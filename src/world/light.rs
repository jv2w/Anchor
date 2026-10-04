//! Server light (PLAN.md 6 "서버 조명", 10.7 "조명"): block light and sky light for game rules,
//! never sent to clients. A port of PMMP 6a7cc02 `src/world/light/*` and the light parts of its
//! `World`, with PMMP's defects fixed (listed on [`Engine`]).
//!
//! A lit chunk holds 4-bit block and sky light per sub-chunk next to its blocks, so a light change
//! never copies a shared block sub-chunk, and the sky height of each column (PMMP's height map):
//! one above the highest block that stops direct sky light. Light is never saved; it is worked out
//! again after a load.
//!
//! The rules are PMMP's. A block spreads its light to its six neighbours, less the neighbour's
//! filter plus 1 (PMMP `LightUpdate::BASE_LIGHT_FILTER`). Block light starts at emitters; sky light
//! is 15 at and above the sky height and comes into the top block from above the world. Light
//! does not enter or leave chunks that are not loaded.

mod nibbles;
mod population;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::block::{self, Facing, State};

use super::chunk::{COLUMNS, SUB_CHUNKS, column, split_y};
use super::index::ChunkIndex;
use super::paletted::offset;
use super::sub_chunk::Entry;
use super::{BlockPos, Chunk, ChunkPos, MAX_Y, MIN_Y};

use nibbles::Nibbles;
use population::Population;

/// The brightest light.
const FULL: u8 = 15;

/// The world time of one day.
const DAY: i64 = 24_000;

/// The sky light reduction from each time of day on (PMMP `World::computeSkyLightReduction` of
/// `computeSunAnglePercentage`, which use `cos`). Each entry is the first tick of the day with
/// that reduction; the test `the_time_table_is_pmmps_formula` recomputes all 24,000 ticks with
/// the deterministic [`crate::math::cos`]. A table keeps platform `cos` out of tick code
/// (PLAN.md 6) and costs a binary search over 23 entries.
const REDUCTION: [(i64, u8); 23] = [
    (0, 0),
    (12_041, 1),
    (12_210, 2),
    (12_377, 3),
    (12_541, 4),
    (12_704, 5),
    (12_866, 6),
    (13_027, 7),
    (13_188, 8),
    (13_348, 9),
    (13_509, 10),
    (13_670, 11),
    (22_331, 10),
    (22_492, 9),
    (22_653, 8),
    (22_813, 7),
    (22_974, 6),
    (23_135, 5),
    (23_297, 4),
    (23_460, 3),
    (23_624, 2),
    (23_791, 1),
    (23_960, 0),
];

/// How much sky light the time of day takes away, 0 (day) to 11 (night) (PMMP
/// `World::getSkyLightReduction`). `time` is the world time in ticks; only `time` mod 24,000
/// counts (PMMP keeps the sign of a negative time; Anchor counts it from the day's start).
pub(crate) fn sky_light_reduction(time: i64) -> u8 {
    let tick = time.rem_euclid(DAY);
    let after = REDUCTION.partition_point(|&(start, _)| start <= tick);
    REDUCTION
        .get(after.saturating_sub(1))
        .map_or(0, |&(_, reduction)| reduction)
}

/// The two kinds of light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Channel {
    /// Light from emitting blocks.
    Block = 0,
    /// Light from the sky.
    Sky = 1,
}

/// The state whose light values an entry has: an unknown state counts as
/// `minecraft:info_update`, the block clients see in its place.
fn state_of(entry: Entry) -> State {
    match entry {
        Entry::Known(state) => state,
        Entry::Unknown(_) => block::INFO_UPDATE,
    }
}

/// The light a block gives off.
fn emission(entry: Entry) -> u8 {
    state_of(entry).light_emission().min(FULL)
}

/// How much light drops when it enters the block: its filter plus 1, at most 15 (PMMP
/// `RuntimeBlockStateRegistry::$lightFilter`).
fn filter(entry: Entry) -> u8 {
    state_of(entry).light_filter().saturating_add(1).min(FULL)
}

/// Whether the block stops direct sky light.
fn blocks_sky(entry: Entry) -> bool {
    state_of(entry).blocks_direct_sky_light()
}

/// The sky height of local column (x, z): one above its highest block that stops direct sky
/// light, or the world bottom (PMMP `SkyLightUpdate::recalculateHeightMapColumn`).
fn sky_height(chunk: &Chunk, x: u8, z: u8) -> i16 {
    let bottom = i16::try_from(MIN_Y).unwrap_or(i16::MIN);
    let Some(top) = chunk.highest_block(x, z) else {
        return bottom;
    };
    (MIN_Y..=top)
        .rev()
        .find(|&y| chunk.entry(x, y, z).is_some_and(blocks_sky))
        .and_then(|y| i16::try_from(y + 1).ok())
        .unwrap_or(bottom)
}

/// The light a block has of itself: block light from its emission; sky light 15 at and above
/// its column's sky height, and for the top block of a column blocked at the top, 15 less its
/// filter (the sky above the world, PMMP `SkyLightUpdate::getEffectiveLight`).
fn source(channel: Channel, entry: Entry, y: i32, height: i32) -> u8 {
    match channel {
        Channel::Block => emission(entry),
        Channel::Sky if y >= height => FULL,
        Channel::Sky if y == MAX_Y => FULL.saturating_sub(filter(entry)),
        Channel::Sky => 0,
    }
}

/// The light of a lit chunk: per sub-chunk and channel 4-bit values, and the sky height of each
/// column.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ChunkLight {
    /// Block light, then sky light, by sub-chunk from the bottom.
    arrays: [[Nibbles; SUB_CHUNKS]; 2],
    /// One above the highest block that stops direct sky light, by [`column`].
    heights: [i16; COLUMNS],
}

impl ChunkLight {
    fn array(&self, channel: Channel, sub: usize) -> Option<&Nibbles> {
        self.arrays.get(channel as usize)?.get(sub)
    }

    fn array_mut(&mut self, channel: Channel, sub: usize) -> Option<&mut Nibbles> {
        self.arrays.get_mut(channel as usize)?.get_mut(sub)
    }

    fn get(&self, channel: Channel, sub: usize, at: u16) -> u8 {
        self.array(channel, sub).map_or(0, |n| n.get(at))
    }

    fn set(&mut self, channel: Channel, sub: usize, at: u16, value: u8) {
        if let Some(n) = self.array_mut(channel, sub) {
            n.set(at, value);
        }
    }

    fn height(&self, column: usize) -> i32 {
        self.heights.get(column).map_or(MIN_Y, |&h| i32::from(h))
    }

    /// The bytes it holds on the heap beside its own size.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.arrays.iter().flatten().map(Nibbles::heap_bytes).sum()
    }
}

/// A word-at-a-time FNV-1a mix for light hashes (tests, PLAN.md 10.7 결정성): cheap enough to
/// hash the light of every loaded chunk after every tick of the determinism runs.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct LightHash(u64);

#[cfg(test)]
impl LightHash {
    pub(crate) fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub(crate) fn word(&mut self, v: u64) {
        self.0 = (self.0 ^ v)
            .wrapping_mul(0x0000_0100_0000_01b3)
            .rotate_left(29);
    }

    fn pos(&mut self, pos: BlockPos) {
        for c in [pos.x, pos.y, pos.z] {
            self.word(u64::from(c.cast_unsigned()));
        }
    }

    fn chunk(&mut self, pos: ChunkPos) {
        self.word(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
    }

    fn len(&mut self, n: usize) {
        self.word(u64::try_from(n).unwrap_or(u64::MAX));
    }

    pub(crate) fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
impl ChunkLight {
    /// Hashes every value and sky height: a uniform cube by its value, a full one by its bytes.
    pub(crate) fn hash_into(&self, h: &mut LightHash) {
        let Self { arrays, heights } = self;
        for n in arrays.iter().flatten() {
            match n {
                Nibbles::Uniform(v) => h.word(0x100 | u64::from(*v)),
                Nibbles::Full(bytes) => {
                    h.word(0x200);
                    for w in bytes.as_chunks::<8>().0 {
                        h.word(u64::from_le_bytes(*w));
                    }
                }
            }
        }
        for height in heights {
            h.word(u64::from(height.cast_unsigned()));
        }
    }
}

impl fmt::Debug for ChunkLight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkLight")
            .field("heap_bytes", &self.heap_bytes())
            .finish_non_exhaustive()
    }
}

/// Lights `chunk` alone, as if every neighbour were dark (PMMP `LightPopulationTask`). A pure
/// function of the chunk, for the CPU pool; the tick connects it to its neighbours when it
/// loads.
pub(crate) fn populate(chunk: &Chunk) -> ChunkLight {
    let mut population = Population::new(chunk.clone(), 0);
    let mut units = u32::MAX;
    while !population.run(&mut units) {
        units = u32::MAX;
    }
    population.finish().0
}

/// Where a block's light lives: its chunk, sub-chunk, offset and column.
#[derive(Debug, Clone, Copy)]
struct Spot {
    chunk: ChunkPos,
    sub: usize,
    at: u16,
    column: usize,
}

impl Spot {
    /// The spot of `pos`, if it is in the world height.
    fn of(pos: BlockPos) -> Option<Self> {
        let (sub, ly) = split_y(pos.y)?;
        let (x, z) = pos.local();
        Some(Self {
            chunk: pos.chunk(),
            sub,
            at: offset(x, ly, z),
            column: column(x, z),
        })
    }
}

/// The six neighbours of `pos` that are valid positions (any height).
fn neighbours(pos: BlockPos) -> impl Iterator<Item = BlockPos> {
    [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 0, 1),
        (0, 0, -1),
        (0, 1, 0),
        (0, -1, 0),
    ]
    .into_iter()
    .filter_map(move |(dx, dy, dz)| {
        Some(BlockPos::new(
            pos.x.checked_add(dx)?,
            pos.y.checked_add(dy)?,
            pos.z.checked_add(dz)?,
        ))
    })
}

/// A neighbour of a block: in the same chunk (with where its light lives), across a chunk
/// face, or outside the world height.
#[derive(Debug, Clone, Copy)]
enum Near {
    Same(BlockPos, Spot),
    Across(BlockPos),
    Out,
}

/// The six neighbours of the block at `pos`, whose spot is `spot`, worked out without looking
/// up a chunk.
fn near(pos: BlockPos, spot: Spot) -> [Near; 6] {
    let (lx, lz, ly) = (spot.at >> 8, (spot.at >> 4) & 15, spot.at & 15);
    let same = |dx: i32, dy: i32, dz: i32, sub: usize, at: u16, column: usize| {
        Near::Same(
            BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz),
            Spot {
                chunk: spot.chunk,
                sub,
                at,
                column,
            },
        )
    };
    let across = |dx: i32, dz: i32| match (pos.x.checked_add(dx), pos.z.checked_add(dz)) {
        (Some(x), Some(z)) => Near::Across(BlockPos::new(x, pos.y, z)),
        _ => Near::Out,
    };
    let (sub, at, col) = (spot.sub, spot.at, spot.column);
    [
        if lx < 15 {
            same(1, 0, 0, sub, at + 256, col + 1)
        } else {
            across(1, 0)
        },
        if lx > 0 {
            same(-1, 0, 0, sub, at - 256, col - 1)
        } else {
            across(-1, 0)
        },
        if lz < 15 {
            same(0, 0, 1, sub, at + 16, col + 16)
        } else {
            across(0, 1)
        },
        if lz > 0 {
            same(0, 0, -1, sub, at - 16, col - 16)
        } else {
            across(0, -1)
        },
        if ly < 15 {
            same(0, 1, 0, sub, at + 1, col)
        } else if sub + 1 < SUB_CHUNKS {
            same(0, 1, 0, sub + 1, at - 15, col)
        } else {
            Near::Out
        },
        if ly > 0 {
            same(0, -1, 0, sub, at - 1, col)
        } else if sub > 0 {
            same(0, -1, 0, sub - 1, at + 15, col)
        } else {
            Near::Out
        },
    ]
}

/// The chunk beside `pos` toward `side`.
fn beside(pos: ChunkPos, side: Facing) -> Option<ChunkPos> {
    let (dx, dz) = side.offset();
    Some(ChunkPos::new(
        pos.x.checked_add(dx)?,
        pos.z.checked_add(dz)?,
    ))
}

/// The pair of block positions `i` (0 to 6,143: sub-chunk × 256 + across × 16 + y) across the
/// face of chunk `pos` toward `side`: the block in the chunk and the block beside it.
fn face_pair(pos: ChunkPos, side: Facing, i: u32) -> Option<(BlockPos, BlockPos)> {
    let sub = i32::try_from(i >> 8).ok()?;
    let across = i32::try_from((i >> 4) & 15).ok()?;
    let y = MIN_Y + sub * 16 + i32::try_from(i & 15).ok()?;
    let (bx, bz) = (pos.x.checked_mul(16)?, pos.z.checked_mul(16)?);
    let (inside, outside) = match side {
        Facing::East => ((15, across), (16, across)),
        Facing::West => ((0, across), (-1, across)),
        Facing::South => ((across, 15), (across, 16)),
        Facing::North => ((across, 0), (across, -1)),
    };
    let at = |(x, z): (i32, i32)| Some(BlockPos::new(bx.checked_add(x)?, y, bz.checked_add(z)?));
    Some((at(inside)?, at(outside)?))
}

/// Pairs across one chunk face: 24 sub-chunks of 16 × 16.
const FACE_PAIRS: u32 = 24 * 256;

/// Bounds on the engine's queues (CLAUDE.md 2.2).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Changed blocks waiting to be looked at.
    pub(crate) changes: usize,
    /// Blocks to recalculate from their neighbours.
    pub(crate) recalc: usize,
    /// Nodes in each of the removal and spread queues.
    pub(crate) nodes: usize,
    /// Unloaded or reset chunks whose light is still being taken out of their neighbours.
    pub(crate) retired: usize,
    /// Blocks a chunk being lit on the tick may change before it starts again.
    pub(crate) population_changes: usize,
}

impl Limits {
    /// The engine's bounds: at most about 9.5 MB of queued blocks per world (16 bytes each), and
    /// 256 retired chunks' light (typically a few KB each, 98 KB at most).
    pub(crate) const DEFAULT: Self = Self {
        changes: 16_384,
        recalc: 65_536,
        nodes: 262_144,
        retired: 256,
        population_changes: 4_096,
    };
}

/// A queued block: its position, channel and, in the removal queue, the light it had.
#[derive(Debug, Clone, Copy)]
struct Node {
    pos: BlockPos,
    channel: Channel,
    level: u8,
}

/// The kind of a chunk task: one of each per chunk at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Reset,
    Populate,
    Border(u8),
}

/// Work on a whole chunk, done in order after the node queues are empty.
#[derive(Debug)]
enum Task {
    /// Retire the chunk's light and light it again alone (after a queue overflow).
    Reset(ChunkPos),
    /// Light the chunk alone from a snapshot.
    Populate(ChunkPos, Box<Population>),
    /// Spread light both ways across the face toward a side; the next pair to look at.
    Border(ChunkPos, Facing, u32),
}

/// The light of a chunk that left (unloaded or reset), kept until what it lit in its neighbours
/// is taken out. `cursor` walks the 4 faces' pairs.
#[derive(Debug)]
struct Retired {
    chunk: ChunkPos,
    light: Box<ChunkLight>,
    cursor: u32,
}

/// The light work of a world (PLAN.md 10.7). Only the tick uses it; [`Self::step`] does the
/// queued work under a budget of units (one per block looked at), in a fixed order, so the
/// result does not depend on threads or timing.
///
/// PMMP defects fixed here:
/// 1. PMMP does every queued update at the end of the tick, however many: here a budget bounds
///    each tick and the rest waits.
/// 2. PMMP lights a chunk alone and never lets light cross into it from loaded neighbours or out
///    of it: here every face between lit chunks is spread both ways.
/// 3. PMMP drops updates in a chunk whose light is being worked out and installs a result that
///    may be out of date (its TODO): here a population works on a snapshot and the changes after
///    it are replayed.
/// 4. PMMP's removal sets a weaker emitter beside a removed light to 0 and never gives its light
///    back: here a block keeps its own light and spreads it again.
/// 5. PMMP's removal does not look past a block that had light 1, so such a block lit from
///    another side stays dark: here every darkened block is looked past.
/// 6. PMMP keeps the light an unloaded chunk gave its neighbours, which later changes can no
///    longer take out: here it is taken out.
/// 7. PMMP's queues are unbounded and its time correction uses platform `cos`: here queues are
///    bounded (an overflow lights the chunk again) and the correction is a table.
pub(crate) struct Engine {
    enabled: bool,
    limits: Limits,
    changes: VecDeque<BlockPos>,
    recalc: VecDeque<Node>,
    removal: VecDeque<Node>,
    spread: VecDeque<Node>,
    retired: VecDeque<Retired>,
    /// Chunks whose work did not fit a queue, to be lit again.
    overflowed: BTreeSet<ChunkPos>,
    /// Chunk tasks by order number, and the order number of each (chunk, kind).
    tasks: BTreeMap<u64, Task>,
    index: BTreeMap<(ChunkPos, Kind), u64>,
    next_task: u64,
}

impl Engine {
    /// An engine with light on and the default bounds.
    pub(crate) fn new() -> Self {
        Self {
            enabled: true,
            limits: Limits::DEFAULT,
            changes: VecDeque::new(),
            recalc: VecDeque::new(),
            removal: VecDeque::new(),
            spread: VecDeque::new(),
            retired: VecDeque::new(),
            overflowed: BTreeSet::new(),
            tasks: BTreeMap::new(),
            index: BTreeMap::new(),
            next_task: 0,
        }
    }

    /// Whether light is worked out (setting `lighting`).
    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Replaces the queue bounds (tests force overflows with small ones).
    #[cfg(test)]
    pub(crate) fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Hashes the work waiting: the queues in order, the overflowed chunks and the chunk tasks.
    #[cfg(test)]
    pub(crate) fn hash_into(&self, h: &mut LightHash) {
        let Self {
            enabled,
            // A setting (tests force overflows with small bounds).
            limits: _,
            changes,
            recalc,
            removal,
            spread,
            retired,
            overflowed,
            tasks,
            // The order number of each task in `tasks`, by its chunk and kind.
            index: _,
            next_task,
        } = self;
        h.word(u64::from(*enabled));
        h.len(changes.len());
        for &pos in changes {
            h.pos(pos);
        }
        for queue in [recalc, removal, spread] {
            h.len(queue.len());
            for n in queue {
                h.pos(n.pos);
                h.word(n.channel as u64);
                h.word(u64::from(n.level));
            }
        }
        h.len(retired.len());
        for r in retired {
            h.chunk(r.chunk);
            h.word(u64::from(r.cursor));
        }
        h.len(overflowed.len());
        for &pos in overflowed {
            h.chunk(pos);
        }
        h.len(tasks.len());
        for (&seq, task) in tasks {
            h.word(seq);
            match task {
                Task::Reset(pos) => {
                    h.word(0);
                    h.chunk(*pos);
                }
                Task::Populate(pos, _) => {
                    h.word(1);
                    h.chunk(*pos);
                }
                Task::Border(pos, facing, at) => {
                    h.word(2 + *facing as u64);
                    h.chunk(*pos);
                    h.word(u64::from(*at));
                }
            }
        }
        h.word(*next_task);
    }

    /// Whether no light work is waiting.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.changes.is_empty()
            && self.recalc.is_empty()
            && self.removal.is_empty()
            && self.spread.is_empty()
            && self.retired.is_empty()
            && self.overflowed.is_empty()
            && self.tasks.is_empty()
            && self.index.is_empty()
    }

    /// Turns light work on or off for the chunks loaded at `loaded` (in order). Off drops all
    /// light and work; on lights every loaded chunk again.
    pub(crate) fn set_enabled(
        &mut self,
        chunks: &mut ChunkIndex,
        loaded: impl Iterator<Item = ChunkPos>,
        enabled: bool,
    ) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        self.changes.clear();
        self.recalc.clear();
        self.removal.clear();
        self.spread.clear();
        self.retired.clear();
        self.overflowed.clear();
        self.tasks.clear();
        self.index.clear();
        for pos in loaded {
            if let Some(chunk) = chunks.get_mut(pos) {
                chunk.take_light();
            }
            if enabled {
                self.loaded(chunks, pos);
            }
        }
    }

    /// The light of `channel` at `pos`, or `None` if its chunk is not loaded or not lit yet, or
    /// `pos` is outside the world height. With light off, block light is 0 and sky light 15.
    pub(crate) fn light(&self, chunks: &ChunkIndex, pos: BlockPos, channel: Channel) -> Option<u8> {
        let spot = Spot::of(pos)?;
        let chunk = chunks.get(spot.chunk)?;
        if !self.enabled {
            return Some(match channel {
                Channel::Block => 0,
                Channel::Sky => FULL,
            });
        }
        Some(chunk.light()?.get(channel, spot.sub, spot.at))
    }

    /// Whether the chunk is loaded and lit (always, with light off).
    pub(crate) fn is_lit(&self, chunks: &ChunkIndex, pos: ChunkPos) -> bool {
        chunks
            .get(pos)
            .is_some_and(|c| !self.enabled || c.light().is_some())
    }

    /// A chunk was just put at `pos`: connect its light to its lit neighbours, or light it on the
    /// tick if it came without light.
    pub(crate) fn loaded(&mut self, chunks: &mut ChunkIndex, pos: ChunkPos) {
        let Some(chunk) = chunks.get_mut(pos) else {
            return;
        };
        if !self.enabled {
            chunk.take_light();
            return;
        }
        if chunk.light().is_some() {
            self.connect(chunks, pos);
        } else {
            let population = Population::new(chunk.clone(), self.limits.population_changes);
            self.add_task(
                pos,
                Kind::Populate,
                Task::Populate(pos, Box::new(population)),
            );
        }
    }

    /// The chunk at `pos` just left, with its light: its tasks go, and the light it gave its lit
    /// neighbours is taken out (or, with too many chunks already retiring, those neighbours are
    /// lit again).
    pub(crate) fn unloaded(
        &mut self,
        chunks: &ChunkIndex,
        pos: ChunkPos,
        light: Option<Box<ChunkLight>>,
    ) {
        if !self.enabled {
            return;
        }
        self.forget(pos);
        if let Some(light) = light {
            self.retire(chunks, pos, light);
        }
    }

    /// The block at `pos` changed from `old` to `new` in a loaded chunk.
    pub(crate) fn changed(&mut self, chunks: &ChunkIndex, pos: BlockPos, old: Entry, new: Entry) {
        if !self.enabled
            || (emission(old), filter(old), blocks_sky(old))
                == (emission(new), filter(new), blocks_sky(new))
        {
            return;
        }
        let at = pos.chunk();
        if let Some(seq) = self.index.get(&(at, Kind::Populate))
            && let Some(Task::Populate(_, population)) = self.tasks.get_mut(seq)
        {
            if !population.note(pos)
                && let Some(chunk) = chunks.get(at)
            {
                **population = Population::new(chunk.clone(), self.limits.population_changes);
            }
            return;
        }
        if chunks.get(at).is_some_and(|c| c.light().is_some()) {
            if self.changes.len() < self.limits.changes {
                self.changes.push_back(pos);
            } else {
                self.reset(chunks, at);
            }
        }
    }

    /// Does queued work until it is done or `budget` units are spent; returns the units spent.
    /// Order: removal, recalculation, spread, changed blocks, retired chunks, chunk tasks, so
    /// light that lost its source goes before light spreads again.
    pub(crate) fn step(&mut self, chunks: &mut ChunkIndex, budget: u32) -> u32 {
        let mut left = budget;
        while left > 0 {
            if let Some(pos) = self.overflowed.pop_first() {
                self.reset(chunks, pos);
            } else if let Some(node) = self.removal.pop_front() {
                left -= 1;
                self.remove_around(chunks, node);
            } else if let Some(node) = self.recalc.pop_front() {
                left -= 1;
                self.recalculate(chunks, node.pos, node.channel);
            } else if let Some(node) = self.spread.pop_front() {
                left -= 1;
                self.spread_from(chunks, node);
            } else if let Some(pos) = self.changes.pop_front() {
                left -= 1;
                self.apply_change(chunks, pos);
            } else if let Some(mut retired) = self.retired.pop_front() {
                if !self.take_out(chunks, &mut retired, &mut left) {
                    self.retired.push_front(retired);
                }
            } else if let Some((seq, task)) = self.tasks.pop_first() {
                self.run_task(chunks, seq, task, &mut left);
            } else {
                break;
            }
        }
        budget - left
    }

    /// The light of `channel` at a spot of a lit chunk.
    fn level(chunks: &ChunkIndex, pos: BlockPos, channel: Channel) -> Option<u8> {
        let spot = Spot::of(pos)?;
        Some(
            chunks
                .get(spot.chunk)?
                .light()?
                .get(channel, spot.sub, spot.at),
        )
    }

    /// Looks past a block whose light `node.level` is gone: each neighbour may have had its
    /// light from it.
    fn remove_around(&mut self, chunks: &mut ChunkIndex, node: Node) {
        let Some(spot) = Spot::of(node.pos) else {
            return;
        };
        let near = near(node.pos, spot);
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            for n in near {
                if let Near::Same(q, at) = n {
                    self.remove_at(chunk, q, at, node.channel, node.level);
                }
            }
        }
        for n in near {
            if let Near::Across(q) = n {
                self.remove(chunks, q, node.channel, node.level);
            }
        }
    }

    /// [`Self::remove_at`] for a block of any loaded chunk.
    fn remove(&mut self, chunks: &mut ChunkIndex, q: BlockPos, channel: Channel, old: u8) {
        let Some(spot) = Spot::of(q) else {
            return;
        };
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            self.remove_at(chunk, q, spot, channel, old);
        }
    }

    /// Light at `q` (in `chunk`) that may have come from a neighbour whose light `old` is gone:
    /// darkened to its own light and queued for removal, or, if it is brighter (lit from
    /// elsewhere) or its own source, queued to spread again (PMMP
    /// `LightUpdate::computeRemoveLight`, fixed: the own light stays and every darkened block is
    /// looked past).
    fn remove_at(&mut self, chunk: &mut Chunk, q: BlockPos, at: Spot, channel: Channel, old: u8) {
        let entry = chunk.entry_at(at.sub, at.at);
        let Some(light) = chunk.light_mut() else {
            return;
        };
        let current = light.get(channel, at.sub, at.at);
        if current == 0 {
            return;
        }
        let own = source(channel, entry, q.y, light.height(at.column));
        if current < old && current > own {
            light.set(channel, at.sub, at.at, own);
            self.push_removal(q, channel, current);
            if own > 1 {
                self.push_spread(q, channel);
            }
        } else if current > 1 {
            self.push_spread(q, channel);
        }
    }

    /// Spreads a queued block's light to its neighbours (PMMP `LightUpdate::execute`).
    fn spread_from(&mut self, chunks: &mut ChunkIndex, node: Node) {
        let Some(spot) = Spot::of(node.pos) else {
            return;
        };
        let near = near(node.pos, spot);
        let Some(chunk) = chunks.get_mut(spot.chunk) else {
            return;
        };
        let level = chunk
            .light()
            .map_or(0, |l| l.get(node.channel, spot.sub, spot.at));
        if level <= 1 {
            return;
        }
        for n in near {
            if let Near::Same(q, at) = n {
                self.relax_at(chunk, q, at, node.channel, level);
            }
        }
        for n in near {
            if let Near::Across(q) = n {
                self.relax(chunks, q, node.channel, level);
            }
        }
    }

    /// [`Self::relax_at`] for a block of any loaded chunk.
    fn relax(&mut self, chunks: &mut ChunkIndex, q: BlockPos, channel: Channel, level: u8) {
        let Some(spot) = Spot::of(q) else {
            return;
        };
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            self.relax_at(chunk, q, spot, channel, level);
        }
    }

    /// Raises `q` (in `chunk`) to `level` less its filter if that is brighter, and queues it to
    /// spread (PMMP `LightUpdate::computeSpreadLight`).
    fn relax_at(&mut self, chunk: &mut Chunk, q: BlockPos, at: Spot, channel: Channel, level: u8) {
        let potential = level.saturating_sub(filter(chunk.entry_at(at.sub, at.at)));
        let Some(light) = chunk.light_mut() else {
            return;
        };
        if potential > light.get(channel, at.sub, at.at) {
            light.set(channel, at.sub, at.at, potential);
            if potential > 1 {
                self.push_spread(q, channel);
            }
        }
    }

    /// Works out a block's light again from its own light and its neighbours, and queues the
    /// difference (PMMP `BlockLightUpdate::recalculateNode`, `SkyLightUpdate::recalculateNode`).
    fn recalculate(&mut self, chunks: &mut ChunkIndex, pos: BlockPos, channel: Channel) {
        let Some(spot) = Spot::of(pos) else {
            return;
        };
        let Some(chunk) = chunks.get(spot.chunk) else {
            return;
        };
        let entry = chunk.entry_at(spot.sub, spot.at);
        let Some(light) = chunk.light() else {
            return;
        };
        let own = source(channel, entry, pos.y, light.height(spot.column));
        let old = light.get(channel, spot.sub, spot.at);
        let brightest = neighbours(pos)
            .filter_map(|q| Self::level(chunks, q, channel))
            .max()
            .unwrap_or(0);
        let new = own.max(brightest.saturating_sub(filter(entry)));
        if new == old {
            return;
        }
        if let Some(light) = chunks.get_mut(spot.chunk).and_then(Chunk::light_mut) {
            light.set(channel, spot.sub, spot.at, new);
        }
        if new < old {
            self.push_removal(pos, channel, old);
        }
        if new > 1 {
            self.push_spread(pos, channel);
        }
    }

    /// A changed block: its column's sky height is worked out again, and the block and the part
    /// of the column whose direct sky light changed are queued for recalculation.
    fn apply_change(&mut self, chunks: &mut ChunkIndex, pos: BlockPos) {
        let Some(spot) = Spot::of(pos) else {
            return;
        };
        let (x, z) = pos.local();
        let Some(chunk) = chunks.get_mut(spot.chunk) else {
            return;
        };
        let height = sky_height(chunk, x, z);
        let Some(light) = chunk.light_mut() else {
            return;
        };
        let old = light.height(spot.column);
        if let Some(h) = light.heights.get_mut(spot.column) {
            *h = height;
        }
        let new = i32::from(height);
        let column = old.min(new)..old.max(new).min(MAX_Y + 1);
        let count = 2 + column.len();
        if self.recalc.len().saturating_add(count) > self.limits.recalc {
            self.reset(chunks, spot.chunk);
            return;
        }
        for channel in [Channel::Block, Channel::Sky] {
            self.recalc.push_back(Node {
                pos,
                channel,
                level: 0,
            });
        }
        for y in column {
            self.recalc.push_back(Node {
                pos: BlockPos::new(pos.x, y, pos.z),
                channel: Channel::Sky,
                level: 0,
            });
        }
    }

    /// Queues a block whose light `level` is gone; if the queue is full its chunk is lit again.
    fn push_removal(&mut self, pos: BlockPos, channel: Channel, level: u8) {
        if self.removal.len() < self.limits.nodes {
            self.removal.push_back(Node {
                pos,
                channel,
                level,
            });
        } else {
            self.overflowed.insert(pos.chunk());
        }
    }

    /// Queues a block to spread its light; if the queue is full its chunk is lit again.
    fn push_spread(&mut self, pos: BlockPos, channel: Channel) {
        if self.spread.len() < self.limits.nodes {
            self.spread.push_back(Node {
                pos,
                channel,
                level: 0,
            });
        } else {
            self.overflowed.insert(pos.chunk());
        }
    }

    /// Marks a lit chunk to be lit again from scratch: work about it did not fit a queue.
    fn reset(&mut self, chunks: &ChunkIndex, pos: ChunkPos) {
        if chunks.get(pos).is_some_and(|c| c.light().is_some()) {
            self.add_task(pos, Kind::Reset, Task::Reset(pos));
        }
    }

    fn add_task(&mut self, pos: ChunkPos, kind: Kind, task: Task) {
        if self.index.contains_key(&(pos, kind)) {
            return;
        }
        let seq = self.next_task;
        self.next_task += 1;
        self.tasks.insert(seq, task);
        self.index.insert((pos, kind), seq);
    }

    /// Drops every task of the chunk at `pos` and the border tasks toward it.
    fn forget(&mut self, pos: ChunkPos) {
        let own = [Kind::Reset, Kind::Populate]
            .into_iter()
            .chain(Facing::ALL.map(|side| Kind::Border(side.bit())))
            .map(|kind| Some((pos, kind)));
        let toward = Facing::ALL.map(|side| {
            beside(pos, side).map(|other| (other, Kind::Border(side.opposite().bit())))
        });
        for key in own.chain(toward).flatten() {
            if let Some(seq) = self.index.remove(&key) {
                self.tasks.remove(&seq);
            }
        }
    }

    /// Queues spreading across each face between the chunk at `pos` and a lit neighbour.
    fn connect(&mut self, chunks: &ChunkIndex, pos: ChunkPos) {
        for side in Facing::ALL {
            if beside(pos, side).is_some_and(|p| chunks.get(p).is_some_and(|c| c.light().is_some()))
            {
                self.add_task(pos, Kind::Border(side.bit()), Task::Border(pos, side, 0));
            }
        }
    }

    /// Keeps `light`, which left `pos`, until what it lit in its lit neighbours is taken out. With
    /// no lit neighbour it is dropped; with too many chunks retiring, the neighbours are lit again
    /// instead.
    fn retire(&mut self, chunks: &ChunkIndex, pos: ChunkPos, light: Box<ChunkLight>) {
        let lit = Facing::ALL.map(|side| {
            beside(pos, side).filter(|&p| chunks.get(p).is_some_and(|c| c.light().is_some()))
        });
        if lit.iter().all(Option::is_none) {
            return;
        }
        if self.retired.len() < self.limits.retired {
            self.retired.push_back(Retired {
                chunk: pos,
                light,
                cursor: 0,
            });
        } else {
            for p in lit.into_iter().flatten() {
                self.reset(chunks, p);
            }
        }
    }

    /// Takes a retired chunk's light out of its neighbours, pair by pair across its 4 faces, while
    /// the node queues are empty. Returns whether it is done.
    fn take_out(&mut self, chunks: &mut ChunkIndex, retired: &mut Retired, left: &mut u32) -> bool {
        let total = FACE_PAIRS * 4;
        while *left > 0 && self.queues_empty() {
            if retired.cursor >= total {
                return true;
            }
            *left -= 1;
            let side = Facing::ALL
                .get(usize::try_from(retired.cursor / FACE_PAIRS).unwrap_or(0))
                .copied()
                .unwrap_or(Facing::North);
            let i = retired.cursor % FACE_PAIRS;
            let sub = usize::try_from(i >> 8).unwrap_or(0);
            let Some(other) = beside(retired.chunk, side)
                .and_then(|p| chunks.get(p))
                .and_then(Chunk::light)
            else {
                retired.cursor = (retired.cursor / FACE_PAIRS + 1) * FACE_PAIRS;
                continue;
            };
            // A sub-chunk face where no block beside can have had its light from the retired
            // side: the retired side is dark, or the side beside is dark, full (only sources
            // are 15) or as bright throughout.
            let unrelated = |c| {
                let had = retired.light.array(c, sub).and_then(Nibbles::uniform);
                let has = other.array(c, sub).and_then(Nibbles::uniform);
                matches!(had, Some(0 | 1))
                    || matches!(has, Some(0 | FULL))
                    || matches!((had, has), (Some(a), Some(b)) if b >= a)
            };
            if i.is_multiple_of(256) && unrelated(Channel::Block) && unrelated(Channel::Sky) {
                retired.cursor += 256;
                continue;
            }
            retired.cursor += 1;
            let Some((p, q)) = face_pair(retired.chunk, side, i) else {
                continue;
            };
            let Some(spot) = Spot::of(p) else {
                continue;
            };
            for channel in [Channel::Block, Channel::Sky] {
                let had = retired.light.get(channel, spot.sub, spot.at);
                if had >= 2 {
                    self.take_out_at(chunks, q, channel, had);
                }
            }
        }
        retired.cursor >= total
    }

    /// Light at `q` that a retired block with light `had` may have given: removed unless `q` is
    /// brighter, dark or lit by itself.
    fn take_out_at(&mut self, chunks: &mut ChunkIndex, q: BlockPos, channel: Channel, had: u8) {
        let Some(spot) = Spot::of(q) else {
            return;
        };
        let Some(chunk) = chunks.get(spot.chunk) else {
            return;
        };
        let entry = chunk.entry_at(spot.sub, spot.at);
        let Some(light) = chunk.light() else {
            return;
        };
        let current = light.get(channel, spot.sub, spot.at);
        let own = source(channel, entry, q.y, light.height(spot.column));
        if current < had && current > own {
            self.remove(chunks, q, channel, had);
        }
    }

    fn queues_empty(&self) -> bool {
        self.removal.is_empty()
            && self.recalc.is_empty()
            && self.spread.is_empty()
            && self.changes.is_empty()
    }

    /// Works on the first chunk task, which was taken out under `seq`; puts it back if it is not
    /// done.
    fn run_task(&mut self, chunks: &mut ChunkIndex, seq: u64, task: Task, left: &mut u32) {
        match task {
            Task::Reset(pos) => {
                *left -= 1;
                self.index.remove(&(pos, Kind::Reset));
                let lit_beside = Facing::ALL
                    .into_iter()
                    .filter_map(|side| beside(pos, side))
                    .any(|p| chunks.get(p).is_some_and(|c| c.light().is_some()));
                if lit_beside && self.retired.len() >= self.limits.retired {
                    // Wait behind the other tasks for a retired slot.
                    self.add_task(pos, Kind::Reset, Task::Reset(pos));
                    return;
                }
                let Some(chunk) = chunks.get_mut(pos) else {
                    return;
                };
                let Some(light) = chunk.take_light() else {
                    return;
                };
                let snapshot = chunk.clone();
                self.forget(pos);
                self.retire(chunks, pos, light);
                let population = Population::new(snapshot, self.limits.population_changes);
                self.add_task(
                    pos,
                    Kind::Populate,
                    Task::Populate(pos, Box::new(population)),
                );
            }
            Task::Populate(pos, mut population) => {
                if !population.run(left) {
                    self.tasks.insert(seq, Task::Populate(pos, population));
                    return;
                }
                self.index.remove(&(pos, Kind::Populate));
                let (light, changed) = population.finish();
                let Some(chunk) = chunks.get_mut(pos) else {
                    return;
                };
                chunk.set_light(light);
                for at in changed {
                    if self.changes.len() < self.limits.changes {
                        self.changes.push_back(at);
                    } else {
                        self.reset(chunks, pos);
                        break;
                    }
                }
                self.connect(chunks, pos);
            }
            Task::Border(pos, side, mut cursor) => {
                if self.cross(chunks, pos, side, &mut cursor, left) {
                    self.index.remove(&(pos, Kind::Border(side.bit())));
                } else {
                    self.tasks.insert(seq, Task::Border(pos, side, cursor));
                }
            }
        }
    }

    /// Spreads light both ways across the face of `pos` toward `side`, pair by pair from
    /// `cursor`, while the node queues are empty. Returns whether it is done.
    fn cross(
        &mut self,
        chunks: &mut ChunkIndex,
        pos: ChunkPos,
        side: Facing,
        cursor: &mut u32,
        left: &mut u32,
    ) -> bool {
        let Some(other) = beside(pos, side) else {
            return true;
        };
        let lit = |p| chunks.get(p).is_some_and(|c| c.light().is_some());
        if !lit(pos) || !lit(other) {
            return true;
        }
        while *left > 0 && self.queues_empty() {
            if *cursor >= FACE_PAIRS {
                return true;
            }
            *left -= 1;
            let i = *cursor;
            let sub = usize::try_from(i >> 8).unwrap_or(0);
            if i.is_multiple_of(256) && Self::flat_face(chunks, pos, other, sub) {
                *cursor += 256;
                continue;
            }
            *cursor += 1;
            let Some((p, q)) = face_pair(pos, side, i) else {
                continue;
            };
            for channel in [Channel::Block, Channel::Sky] {
                let (Some(lp), Some(lq)) = (
                    Self::level(chunks, p, channel),
                    Self::level(chunks, q, channel),
                ) else {
                    continue;
                };
                if lp >= 2 {
                    self.relax(chunks, q, channel, lp);
                }
                if lq >= 2 {
                    self.relax(chunks, p, channel, lq);
                }
            }
        }
        *cursor >= FACE_PAIRS
    }

    /// Whether sub-chunk `sub` of two chunks has uniform light in both channels that differs by
    /// at most 1, so neither side can light the other.
    fn flat_face(chunks: &ChunkIndex, a: ChunkPos, b: ChunkPos, sub: usize) -> bool {
        let uniform = |pos, channel| {
            chunks
                .get(pos)
                .and_then(Chunk::light)
                .and_then(|l| l.array(channel, sub))
                .and_then(Nibbles::uniform)
        };
        [Channel::Block, Channel::Sky].into_iter().all(|channel| {
            matches!((uniform(a, channel), uniform(b, channel)), (Some(x), Some(y)) if x.abs_diff(y) <= 1)
        })
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("enabled", &self.enabled)
            .field("changes", &self.changes.len())
            .field("recalc", &self.recalc.len())
            .field("removal", &self.removal.len())
            .field("spread", &self.spread.len())
            .field("retired", &self.retired.len())
            .field("tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Rng;
    use crate::world::chunk::tests::plain_states;
    use crate::world::{Flat, Generator, Void, World};

    /// Blocks in a chunk, indexed by [`cell`].
    const CELLS: usize = 384 * 256;

    fn cell(x: u8, y: i32, z: u8) -> usize {
        usize::try_from(y - MIN_Y).unwrap() * 256 + column(x, z)
    }

    fn find(name: &str) -> State {
        State::all()
            .find(|s| s.name() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    /// One state for each combination of light values in the table (emission, filter, whether it
    /// stops direct sky light), without neighbour rules.
    fn light_states() -> Vec<State> {
        let mut by_values = BTreeMap::new();
        for s in plain_states() {
            by_values
                .entry((
                    s.light_emission(),
                    s.light_filter(),
                    s.blocks_direct_sky_light(),
                ))
                .or_insert(s);
        }
        by_values.into_values().collect()
    }

    /// Loads `chunk` at `pos`, lit on the "pool" when `pool` (as the save thread does) or else on
    /// the tick.
    fn load(w: &mut World, pos: ChunkPos, mut chunk: Chunk, pool: bool) {
        if pool {
            chunk.populate_light();
        }
        w.insert_chunk(pos, chunk);
    }

    /// A world with the chunks of x and z from `-r` to `r` from `generator`, after `build` set
    /// blocks in each (local x, z); every other chunk is lit on the pool, the rest on the tick.
    fn world_with(
        generator: &dyn Generator,
        r: i32,
        build: impl Fn(ChunkPos, &mut Chunk),
    ) -> World {
        let mut w = World::new("w", crate::world::Kind::Temporary, 3, generator.spawn(3));
        for x in -r..=r {
            for z in -r..=r {
                let pos = ChunkPos::new(x, z);
                let mut chunk = Chunk::generate(generator, 3, pos);
                build(pos, &mut chunk);
                load(&mut w, pos, chunk, (x + z).rem_euclid(2) == 0);
            }
        }
        w
    }

    fn world(generator: &dyn Generator, r: i32) -> World {
        world_with(generator, r, |_, _| {})
    }

    /// Steps the light with `budget` per tick until no work is left. Asserts that no tick spends
    /// more than the budget and that every tick with work left spends some; returns the ticks.
    fn settle(w: &mut World, budget: u32) -> usize {
        let mut ticks = 0;
        while !w.is_light_idle() {
            let used = w.step_light(budget);
            assert!(used <= budget, "a tick spent {used} of {budget}");
            assert!(used > 0, "work is left but a tick did nothing");
            ticks += 1;
        }
        ticks
    }

    /// The light of every loaded chunk worked out from scratch, independently of the engine: a
    /// plain breadth-first spread over all loaded chunks from PMMP's sources (emitters; sky
    /// light 15 at and above one over each column's highest sky blocker, and from above the
    /// world into the top block), dropping by filter + 1 per block and never entering a chunk
    /// that is not loaded. Also each column's sky height.
    fn reference(w: &World) -> BTreeMap<ChunkPos, ([Vec<u8>; 2], Vec<i32>)> {
        let mut out = BTreeMap::new();
        let mut queue = VecDeque::new();
        for cp in w.loaded() {
            let chunk = w.chunk(cp).unwrap();
            let mut arrays = [vec![0u8; CELLS], vec![0u8; CELLS]];
            let mut heights = vec![MIN_Y; COLUMNS];
            for x in 0..16 {
                for z in 0..16 {
                    let top = (MIN_Y..=MAX_Y)
                        .rev()
                        .find(|&y| blocks_sky(chunk.entry(x, y, z).unwrap()))
                        .map_or(MIN_Y, |y| y + 1);
                    heights[column(x, z)] = top;
                    for y in MIN_Y..=MAX_Y {
                        let e = chunk.entry(x, y, z).unwrap();
                        let i = cell(x, y, z);
                        arrays[0][i] = emission(e);
                        arrays[1][i] = if y >= top {
                            15
                        } else if y == MAX_Y {
                            15 - filter(e)
                        } else {
                            0
                        };
                        let pos =
                            BlockPos::new(cp.x * 16 + i32::from(x), y, cp.z * 16 + i32::from(z));
                        for (c, a) in arrays.iter().enumerate() {
                            if a[i] > 1 {
                                queue.push_back((pos, c));
                            }
                        }
                    }
                }
            }
            out.insert(cp, (arrays, heights));
        }
        while let Some((pos, c)) = queue.pop_front() {
            let (x, z) = pos.local();
            let v = out[&pos.chunk()].0[c][cell(x, pos.y, z)];
            for q in neighbours(pos) {
                if !(MIN_Y..=MAX_Y).contains(&q.y) {
                    continue;
                }
                let Some(chunk) = w.chunk(q.chunk()) else {
                    continue;
                };
                let (qx, qz) = q.local();
                let potential = v.saturating_sub(filter(chunk.entry(qx, q.y, qz).unwrap()));
                let slot = &mut out.get_mut(&q.chunk()).unwrap().0[c][cell(qx, q.y, qz)];
                if potential > *slot {
                    *slot = potential;
                    if potential > 1 {
                        queue.push_back((q, c));
                    }
                }
            }
        }
        out
    }

    /// Asserts that every loaded chunk is lit exactly as [`reference`] lights it.
    fn assert_matches_reference(w: &World) {
        for (cp, (arrays, heights)) in reference(w) {
            let light = w.chunk(cp).unwrap().light().expect("every chunk is lit");
            for (c, &height) in heights.iter().enumerate() {
                assert_eq!(light.height(c), height, "sky height of {cp:?} column {c}");
            }
            for x in 0..16 {
                for z in 0..16 {
                    for y in MIN_Y..=MAX_Y {
                        let (s, ly) = split_y(y).unwrap();
                        let at = offset(x, ly, z);
                        for (c, channel) in [Channel::Block, Channel::Sky].into_iter().enumerate() {
                            assert_eq!(
                                light.get(channel, s, at),
                                arrays[c][cell(x, y, z)],
                                "{channel:?} light of {cp:?} at ({x}, {y}, {z})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A random position of the chunks from -1 to 1: mostly near the flat ground, some at the
    /// top of the world (tall sky shadows and the block lit from above the world).
    fn random_pos(rng: &mut Rng) -> BlockPos {
        let y = if rng.below(10) == 0 {
            MAX_Y - i32::try_from(rng.below(24)).unwrap()
        } else {
            MIN_Y + i32::try_from(rng.below(40)).unwrap()
        };
        BlockPos::new(
            i32::try_from(rng.below(48)).unwrap() - 16,
            y,
            i32::try_from(rng.below(48)).unwrap() - 16,
        )
    }

    #[test]
    fn random_changes_match_a_full_recomputation() {
        let states = light_states();
        assert!(states.len() >= 20, "emitters, filters and sky blockers");
        // Chunks that start with emitters and sky shadows at their borders, so their light
        // must cross when they load.
        let mut w = world_with(&Flat, 1, |pos, chunk| {
            let mut rng = Rng(u64::from(pos.x.abs_diff(-7) * 31 + pos.z.abs_diff(-7)));
            for _ in 0..300 {
                let x = u8::try_from(rng.below(16)).unwrap();
                let z = u8::try_from(rng.below(16)).unwrap();
                let y = MIN_Y + i32::try_from(rng.below(40)).unwrap();
                chunk
                    .set_block(x, y, z, states[rng.below(states.len())])
                    .unwrap();
            }
        });
        settle(&mut w, 4_096);
        assert_matches_reference(&w);
        let mut rng = Rng(0x00e1_1001);
        let budget = 2_000;
        let mut waiting = 0;
        for round in 1..=10_000usize {
            let state = if rng.below(3) == 0 {
                block::AIR
            } else {
                states[rng.below(states.len())]
            };
            w.set_block(random_pos(&mut rng), state).unwrap();
            if round % 8 == 0 {
                assert!(w.step_light(budget) <= budget);
                waiting += usize::from(!w.is_light_idle());
            }
            if round == 5_000 {
                settle(&mut w, budget);
                assert_matches_reference(&w);
            }
        }
        let ticks = settle(&mut w, budget);
        assert_matches_reference(&w);
        assert!(waiting > 100 && ticks > 1, "work waited for later ticks");
    }

    fn water() -> State {
        State::find(
            "minecraft:water",
            &[("liquid_depth", block::PropertyValue::Int(0))],
        )
        .unwrap()
    }

    /// Stone from y 0 to 63 in the chunks from -1 to 1, with a tunnel along x at y 8, z 8 from x
    /// -12 to 28 (across the chunk borders at x 0 and 16) and a shaft open to the sky at x 8,
    /// z 24 down to y 9. Lit and settled.
    fn caves() -> World {
        let stone = find("minecraft:stone");
        let mut w = world_with(&Void, 1, |pos, chunk| {
            for x in 0..16u8 {
                for z in 0..16u8 {
                    let wx = pos.x * 16 + i32::from(x);
                    let wz = pos.z * 16 + i32::from(z);
                    for y in 0..64 {
                        let tunnel = y == 8 && wz == 8 && (-12..=28).contains(&wx);
                        let shaft = wx == 8 && wz == 24 && y >= 9;
                        if !tunnel && !shaft {
                            chunk.set_block(x, y, z, stone).unwrap();
                        }
                    }
                }
            }
        });
        settle(&mut w, 1_000);
        w
    }

    // pmmp: World::getBlockLightAt
    #[test]
    fn block_light_follows_pmmp_rules() {
        let torch = find("minecraft:torch");
        let glowstone = find("minecraft:glowstone");
        let water = water();
        let mut w = caves();
        let at = |x| BlockPos::new(x, 8, 8);
        assert_eq!(w.sky_light(at(0)), Some(0), "the tunnel is dark");
        assert_eq!(w.sky_light(BlockPos::new(0, 64, 0)), Some(15), "open sky");
        assert_eq!(
            w.sky_light(BlockPos::new(0, MIN_Y, 0)),
            Some(0),
            "under stone"
        );
        // An emitter of 14 loses 1 per block, across the chunk borders at x 0 and 16.
        w.set_block(at(-10), torch).unwrap();
        let ticks = settle(&mut w, 4);
        assert!(ticks >= 4, "a small budget spreads the work over ticks");
        for d in 0..=14 {
            assert_eq!(
                w.block_light(at(-10 + d)),
                Some(14u8.saturating_sub(d.try_into().unwrap()))
            );
        }
        // Water filters 2: light drops by 3 into it, then by 1 again.
        w.set_block(at(-5), water).unwrap();
        settle(&mut w, 50);
        assert_eq!(w.block_light(at(-6)), Some(10));
        assert_eq!(w.block_light(at(-5)), Some(7));
        assert_eq!(w.block_light(at(-4)), Some(6));
        // A stronger emitter beside a weaker one, then removed: the weaker keeps its light
        // (PMMP darkens it).
        w.set_block(at(-11), glowstone).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.block_light(at(-10)), Some(14));
        w.set_block(at(-11), block::AIR).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.block_light(at(-10)), Some(14));
        assert_eq!(w.block_light(at(-11)), Some(13));
        // Two torches whose light meets at 1 in x 3; the first removed, x 3 keeps 1 from the
        // second (PMMP does not look past a block it darkens from 1, so it stays dark).
        w.set_block(at(-5), block::AIR).unwrap();
        w.set_block(at(16), torch).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(
            [2, 3, 4].map(|x| w.block_light(at(x))),
            [Some(2), Some(1), Some(2)]
        );
        w.set_block(at(-10), block::AIR).unwrap();
        w.set_block(at(-11), block::AIR).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(
            [2, 3, 4].map(|x| w.block_light(at(x))),
            [Some(0), Some(1), Some(2)]
        );
        assert_matches_reference(&w);
    }

    // pmmp: World::getPotentialBlockSkyLightAt
    #[test]
    fn sky_light_follows_pmmp_rules() {
        let leaves = find("minecraft:oak_leaves");
        let glass = find("minecraft:glass");
        let water = water();
        let mut w = caves();
        // The shaft: open sky straight down to its floor, through glass too.
        let shaft = |y| BlockPos::new(8, y, 24);
        for y in 9..80 {
            assert_eq!(w.sky_light(shaft(y)), Some(15), "y {y}");
        }
        w.set_block(shaft(40), glass).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(
            w.sky_light(shaft(9)),
            Some(15),
            "glass keeps direct sky light"
        );
        // Water stops direct sky light: 15 less 3 in it, then 1 less per block below.
        w.set_block(shaft(40), water).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.sky_light(shaft(41)), Some(15));
        assert_eq!(w.sky_light(shaft(40)), Some(12));
        for k in 1..=11 {
            assert_eq!(
                w.sky_light(shaft(40 - k)),
                Some(12 - u8::try_from(k).unwrap())
            );
        }
        let height = |w: &World| {
            let light = w.chunk(ChunkPos::new(0, 1)).unwrap().light().unwrap();
            light.height(column(8, 8))
        };
        assert_eq!(height(&w), 41);
        // Leaves stop direct sky light and filter 0: 1 less per block.
        w.set_block(shaft(40), leaves).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.sky_light(shaft(40)), Some(14));
        assert_eq!(w.sky_light(shaft(30)), Some(4));
        w.set_block(shaft(40), block::AIR).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.sky_light(shaft(9)), Some(15));
        assert_eq!(height(&w), 9);
        // A sky blocker in the top block is lit from above the world.
        w.set_block(BlockPos::new(8, MAX_Y, 24), water).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.sky_light(BlockPos::new(8, MAX_Y, 24)), Some(12));
        assert_eq!(height(&w), MAX_Y + 1);
        assert_eq!(w.sky_light(BlockPos::new(8, MAX_Y + 1, 24)), None);
        assert_eq!(w.block_light(BlockPos::new(8, 0, 40)), None, "not loaded");
        assert_matches_reference(&w);
    }

    #[test]
    fn overflowing_queues_light_chunks_again_and_still_converge() {
        let states = light_states();
        let mut w = world(&Flat, 1);
        w.light.set_limits(Limits {
            changes: 32,
            recalc: 512,
            nodes: 8_192,
            retired: 1,
            population_changes: 8,
        });
        settle(&mut w, 4_096);
        let mut rng = Rng(0x00e1_1002);
        let mut resets = 0;
        for round in 0..40 {
            // A burst of changes in one tick, more than the queues hold.
            for _ in 0..100 {
                let state = if rng.below(3) == 0 {
                    block::AIR
                } else {
                    states[rng.below(states.len())]
                };
                w.set_block(random_pos(&mut rng), state).unwrap();
            }
            resets += w
                .light
                .index
                .keys()
                .filter(|(_, k)| matches!(k, Kind::Reset | Kind::Populate))
                .count();
            w.step_light(3_000);
            // Changes while chunks are being lit again on the tick, and unloads of lit chunks
            // with lit neighbours (more than the retired limit).
            for _ in 0..20 {
                w.set_block(random_pos(&mut rng), states[rng.below(states.len())])
                    .unwrap();
            }
            if round % 10 == 9 {
                let gone: Vec<(ChunkPos, Chunk)> = [ChunkPos::new(0, 0), ChunkPos::new(1, 1)]
                    .into_iter()
                    .map(|p| (p, w.remove_chunk(p).unwrap()))
                    .collect();
                w.step_light(500);
                for (p, c) in gone {
                    w.restore_chunk(p, c);
                }
            }
        }
        assert!(
            resets > 10,
            "overflows marked chunks to be lit again ({resets})"
        );
        settle(&mut w, 3_000);
        assert_matches_reference(&w);
    }

    #[test]
    fn unloading_drops_work_and_takes_out_the_light_it_gave() {
        let glowstone = find("minecraft:glowstone");
        let mut w = world(&Flat, 1);
        settle(&mut w, 4_096);
        // Light near the border of chunk (1, 0), half spread when the chunk unloads.
        w.set_block(BlockPos::new(16, -60, 5), glowstone).unwrap();
        w.set_block(BlockPos::new(17, -62, 5), glowstone).unwrap();
        w.step_light(300);
        assert!(!w.is_light_idle());
        let pos = ChunkPos::new(1, 0);
        let chunk = w.remove_chunk(pos).unwrap();
        assert!(chunk.light().is_none(), "light leaves with the chunk");
        assert!(
            !w.light.index.keys().any(|(p, _)| *p == pos),
            "no task of the unloaded chunk is left"
        );
        assert_eq!(w.light.retired.len(), 1);
        settle(&mut w, 1_000);
        assert_eq!(w.block_light(BlockPos::new(15, -60, 5)), Some(0));
        assert_matches_reference(&w);
        // Back from memory, it is lit on the tick and its light crosses again.
        w.restore_chunk(pos, chunk);
        assert_eq!(
            w.block_light(BlockPos::new(16, -60, 5)),
            None,
            "not lit yet"
        );
        // Changes while it is being lit are replayed after.
        w.step_light(100);
        assert!(!w.is_chunk_lit(pos));
        w.set_block(BlockPos::new(17, -62, 5), block::AIR).unwrap();
        w.set_block(BlockPos::new(20, -60, 9), glowstone).unwrap();
        settle(&mut w, 1_000);
        assert_eq!(w.block_light(BlockPos::new(15, -60, 5)), Some(14));
        assert_eq!(w.block_light(BlockPos::new(20, -60, 10)), Some(14));
        assert_matches_reference(&w);
        // Unloading everything with work pending leaves nothing behind.
        w.set_block(BlockPos::new(0, -60, 0), glowstone).unwrap();
        w.step_light(10);
        let all: Vec<ChunkPos> = w.loaded().collect();
        for p in all {
            w.remove_chunk(p);
        }
        settle(&mut w, 1_000);
        assert!(w.is_light_idle());
        assert_eq!(w.loaded().count(), 0);
    }

    // pmmp: World::getRealBlockSkyLightAt, World::getFullLightAt
    #[test]
    fn lighting_off_sees_an_open_sky_everywhere() {
        let torch = find("minecraft:torch");
        let mut w = world(&Flat, 1);
        w.set_lighting(false);
        assert!(!w.light.is_enabled());
        assert!(w.is_light_idle());
        let under = BlockPos::new(3, MIN_Y, 3);
        w.set_block(BlockPos::new(3, -60, 3), torch).unwrap();
        assert!(w.is_light_idle(), "changes make no work");
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(0));
        assert_eq!(w.sky_light(under), Some(15));
        assert_eq!(w.real_sky_light(under, 18_000), Some(4), "midnight");
        assert_eq!(w.full_light(under, 6_000), Some(15));
        assert_eq!(w.sky_light(BlockPos::new(48, 0, 0)), None, "not loaded");
        assert!(w.is_chunk_lit(ChunkPos::new(0, 0)));
        assert!(!w.is_chunk_lit(ChunkPos::new(3, 0)));
        let chunk = Chunk::generate(&Flat, 3, ChunkPos::new(2, 0));
        load(&mut w, ChunkPos::new(2, 0), chunk, true);
        assert!(
            w.chunk(ChunkPos::new(2, 0)).unwrap().light().is_none(),
            "pool light is dropped"
        );
        // On again: every chunk is lit on the tick.
        w.set_lighting(true);
        assert!(!w.is_chunk_lit(ChunkPos::new(0, 0)));
        settle(&mut w, 4_096);
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(14));
        assert_eq!(w.sky_light(under), Some(0));
        assert_eq!(w.full_light(BlockPos::new(3, -60, 4), 18_000), Some(13));
        assert_matches_reference(&w);
    }

    #[test]
    fn the_time_table_is_pmmps_formula() {
        use crate::math::{clamp, cos};
        use std::f64::consts::PI;
        for time in 0..DAY {
            let progress = f64::from(i32::try_from(time).unwrap()) / 24_000.0;
            let sun = progress + if progress < 0.25 { 0.75 } else { -0.25 };
            #[expect(
                clippy::manual_midpoint,
                reason = "consistency: PMMP's formula as it is written"
            )]
            let diff = ((1.0 - (cos(sun * PI) + 1.0) / 2.0) - sun) / 3.0;
            let angle = sun + diff;
            let percentage = clamp(-(cos(angle * 2.0 * PI) * 2.0 - 0.5), 0.0, 1.0);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "consistency: PMMP's (int) cast of a value from 0 to 11"
            )]
            let want = (percentage * 11.0) as u8;
            assert_eq!(sky_light_reduction(time), want, "time {time}");
        }
        assert_eq!(sky_light_reduction(-1), sky_light_reduction(23_999));
        assert_eq!(sky_light_reduction(5 * DAY + 18_000), 11);
        assert_eq!(sky_light_reduction(6_000), 0);
    }

    /// A chunk with a random state from `states` in every block of every sub-chunk.
    fn dense(states: &[State], seed: u64) -> Chunk {
        let mut rng = Rng(seed);
        let mut chunk = Chunk::new();
        for x in 0..16 {
            for z in 0..16 {
                for y in MIN_Y..=MAX_Y {
                    let s = if rng.below(2) == 0 {
                        block::AIR
                    } else {
                        states[rng.below(states.len())]
                    };
                    chunk.set_block(x, y, z, s).unwrap();
                }
            }
        }
        chunk
    }

    #[test]
    fn the_pool_and_the_tick_light_a_chunk_the_same() {
        let states = light_states();
        let chunk = dense(&states, 0x00e1_1003);
        let pool = populate(&chunk);
        let mut tick = Population::new(chunk.clone(), 0);
        let mut steps = 0;
        loop {
            let mut units = 1_000;
            steps += 1;
            if tick.run(&mut units) {
                break;
            }
            assert_eq!(units, 0);
        }
        assert!(steps > 10);
        assert_eq!(tick.finish().0, pool);
    }

    /// Measures light costs (PLAN.md 10.7 "성능"):
    /// `cargo test --release --lib world::light::tests::light_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        clippy::cast_precision_loss,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn light_cost() {
        let states = light_states();
        let us = |d: std::time::Duration, n: usize| d.as_secs_f64() * 1e6 / n as f64;
        let flat = Chunk::generate(&Flat, 0, ChunkPos::new(0, 0));
        let dense = dense(&states, 7);
        for (name, chunk, n) in [("flat", &flat, 2_000), ("dense random", &dense, 20)] {
            let start = std::time::Instant::now();
            let mut bytes = 0;
            for _ in 0..n {
                bytes = populate(chunk).heap_bytes();
            }
            eprintln!(
                "populate {name}: {:.1} us, light memory {} B heap + {} B",
                us(start.elapsed(), n),
                bytes,
                size_of::<ChunkLight>() + size_of::<Box<ChunkLight>>()
            );
        }
        let torch = find("minecraft:torch");
        let stone = find("minecraft:stone");
        // Each scenario: a block put at a position, the light settled, the block taken away,
        // settled again; positions spread over the 5 x 5 flat chunks.
        let scenarios: [(&str, State, i32); 3] = [
            ("torch in the open", torch, -59),
            ("torch under the ground", torch, -62),
            ("roof over open sky", stone, -40),
        ];
        for (name, state, y) in scenarios {
            let mut w = world(&Flat, 2);
            settle(&mut w, u32::MAX);
            let mut rng = Rng(0x00e1_1004);
            let changes = 400;
            let mut units = 0u64;
            let start = std::time::Instant::now();
            for _ in 0..changes / 2 {
                let pos = BlockPos::new(
                    i32::try_from(rng.below(80)).unwrap() - 32,
                    y,
                    i32::try_from(rng.below(80)).unwrap() - 32,
                );
                for s in [state, block::AIR] {
                    w.set_block(pos, s).unwrap();
                    while !w.is_light_idle() {
                        units += u64::from(w.step_light(u32::MAX));
                    }
                }
            }
            let elapsed = start.elapsed();
            eprintln!(
                "{name}, put and taken away: {:.1} us and {} units per change, {:.1} ns per unit",
                us(elapsed, changes),
                units / changes as u64,
                elapsed.as_secs_f64() * 1e9 / units as f64
            );
        }
        let mut w = world(&Flat, 2);
        settle(&mut w, u32::MAX);
        let mut rng = Rng(0x00e1_1005);
        let changes = 4_000;
        let mut units = 0u64;
        let start = std::time::Instant::now();
        for _ in 0..changes {
            let pos = BlockPos::new(
                i32::try_from(rng.below(80)).unwrap() - 32,
                -64 + i32::try_from(rng.below(12)).unwrap(),
                i32::try_from(rng.below(80)).unwrap() - 32,
            );
            let state = if rng.below(2) == 0 {
                block::AIR
            } else {
                states[rng.below(states.len())]
            };
            w.set_block(pos, state).unwrap();
            while !w.is_light_idle() {
                units += u64::from(w.step_light(u32::MAX));
            }
        }
        let elapsed = start.elapsed();
        eprintln!(
            "random states near the ground: {:.1} us and {} units per change, {:.1} ns per unit",
            us(elapsed, changes),
            units / changes as u64,
            elapsed.as_secs_f64() * 1e9 / units as f64
        );
        let mut w = world(&Flat, 2);
        settle(&mut w, u32::MAX);
        let start = std::time::Instant::now();
        let mut units = 0u64;
        for x in -2..=2 {
            let c = w.remove_chunk(ChunkPos::new(x, 0)).unwrap();
            w.restore_chunk(ChunkPos::new(x, 0), c);
            while !w.is_light_idle() {
                units += u64::from(w.step_light(u32::MAX));
            }
        }
        eprintln!(
            "unload and relight on the tick (flat): {:.1} us and {} units per chunk",
            us(start.elapsed(), 5),
            units / 5
        );
    }
}
