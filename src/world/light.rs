//! Server light: block light and sky light for game rules, never sent to clients.
//!
//! A lit chunk holds 4-bit block and sky light per sub-chunk next to its blocks, so a light change
//! never copies a shared block sub-chunk, and the sky height of each column: one above the highest
//! block that stops direct sky light. Light is never saved; it is worked out again after a load.
//!
//! A block spreads its light to its six neighbours, less the neighbour's filter plus 1. Block
//! light starts at emitters; sky light is 15 at and above the sky height and comes into the top
//! block from above the world. Light is worked out in steps under a budget of units per tick
//! ([`Engine::step`]), and a chunk is lit alone first (`populate`, which the save thread does off
//! the tick for a chunk it loads). Light does not enter or leave chunks that are not loaded.

mod nibbles;
mod population;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use super::chunk::{COLUMNS, SUB_CHUNKS, column, split_y};
use super::index::Chunks;
use super::paletted::{Derive, offset};
use super::sub_chunk::Entry;
use super::{BlockPos, Chunk, ChunkPos, MAX_Y, MIN_Y, tick_of_day};
use crate::block::{self, Facing};
use crate::config::Lighting;
use crate::tick::checkpoint::{self, Codec, Error, Reader, Writer};
use nibbles::Nibbles;
use population::Population;

/// The pairs across the 4 faces of a chunk.
const ALL_FACE_PAIRS: u32 = 4 * FACE_PAIRS;
/// The sky height of a column with no block that stops direct sky light: the world bottom.
const BOTTOM: i16 = -64;
/// The pairs across one face of a chunk: all 24 sub-chunks.
const FACE_PAIRS: u32 = 24 * SUB_FACE_PAIRS;
/// The brightest light.
const FULL: u8 = 15;
/// The sky light reduction from each time of day on.
///
/// The reduction follows the sun's angle through a cosine. Each entry is the first tick of the day
/// with that reduction; the test `the_time_table_follows_the_sun_angle_formula` recomputes all
/// 24,000 ticks with the deterministic [`crate::math::cos`]. A table keeps platform `cos` out of
/// tick code and costs a binary search over 23 entries.
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
/// The pairs across one face of a chunk in one sub-chunk: 16 × 16.
const SUB_FACE_PAIRS: u32 = 256;

const _: () = assert!(BOTTOM as i32 == MIN_Y);
const _: () = assert!(SUB_CHUNKS == 24);

/// The light work of a world.
///
/// Only the tick uses it; [`Self::step`] does the queued work under a budget of units (one per
/// block looked at), in a fixed order, so the result does not depend on threads or timing.
///
/// How it keeps the light right:
/// 1. A budget bounds each tick's work and the rest waits.
/// 2. Every face between lit chunks is spread both ways.
/// 3. A population works on a snapshot and the changes after it are replayed.
/// 4. A block keeps its own light and spreads it again when a neighbour's light is removed.
/// 5. Every darkened block is looked past, so a block lit from another side stays lit.
/// 6. The light an unloaded chunk gave its neighbours is taken out.
/// 7. Queues are bounded (an overflow lights the chunk again) and the time correction is a table,
///    not a platform `cos`.
#[derive(Clone)]
pub(crate) struct Engine {
    /// How light is worked out.
    mode: Mode,
    /// The bounds on the queues.
    limits: Limits,
    /// Changed blocks waiting to be looked at.
    changes: VecDeque<BlockPos>,
    /// Blocks to recalculate from their neighbours.
    recalc: VecDeque<Node>,
    /// Blocks whose light is gone, to look past.
    removal: VecDeque<Node>,
    /// Blocks to spread their light from.
    spread: VecDeque<Node>,
    /// Chunks that left, whose light is still being taken out of their neighbours.
    retired: VecDeque<Retired>,
    /// Chunks whose work did not fit a queue, to be lit again.
    overflowed: BTreeSet<ChunkPos>,
    /// Chunk tasks by order number.
    tasks: BTreeMap<u64, Task>,
    /// The order number in `tasks` of each (chunk, kind).
    index: BTreeMap<(ChunkPos, Kind), u64>,
    /// The order number of the next task.
    next_task: u64,
}

impl Engine {
    /// An engine with dynamic light and the default bounds.
    pub(crate) fn new() -> Self {
        Self {
            mode: Mode::Dynamic,
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

    /// Hashes the work waiting: the queues in order, the overflowed chunks and the chunk tasks.
    #[cfg(test)]
    pub(crate) fn hash_into(&self, h: &mut Digest) {
        let Self {
            mode,
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
        h.word(match mode {
            Mode::Dynamic => 0,
            Mode::OnLoad => 1,
            Mode::Fixed(level) => 2 + u64::from(*level),
        });
        h.len(changes.len());
        for &pos in changes {
            h.pos(pos);
        }
        for queue in [recalc, removal, spread] {
            h.len(queue.len());
            for node in queue {
                h.pos(node.pos);
                h.word(node.channel as u64);
                h.word(u64::from(node.level));
            }
        }
        h.len(retired.len());
        for chunk in retired {
            h.chunk(chunk.chunk);
            h.word(u64::from(chunk.cursor));
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

    /// The bytes its queued work holds, about.
    ///
    /// These are a node for each queued block, the light of each retired chunk and the working
    /// memory of each chunk being lit.
    pub(crate) fn heap_bytes(&self) -> usize {
        let nodes = self.changes.len() + self.recalc.len() + self.removal.len() + self.spread.len();
        let retired: usize = self
            .retired
            .iter()
            .map(|retired| retired.light.heap_bytes())
            .sum();
        let populations = self
            .tasks
            .values()
            .filter(|task| matches!(task, Task::Populate(..)))
            .count();
        nodes
            .saturating_mul(size_of::<Node>())
            .saturating_add(retired)
            .saturating_add(populations.saturating_mul(Population::WORK_BYTES))
    }

    /// Whether the chunk is loaded and lit (always, with [`Mode::Fixed`]).
    pub(crate) fn is_lit(&self, chunks: &Chunks, pos: ChunkPos) -> bool {
        chunks
            .get(pos)
            .is_some_and(|chunk| !self.is_calculating() || chunk.light().is_some())
    }

    /// Whether light is worked out at all: not [`Mode::Fixed`].
    pub(crate) fn is_calculating(&self) -> bool {
        !matches!(self.mode, Mode::Fixed(_))
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

    /// The light of `channel` at `pos`, or `None` where there is none.
    ///
    /// There is none if the chunk is not loaded or not lit yet, or `pos` is outside the world
    /// height. With [`Mode::Fixed`], block light is 0 and sky light the fixed level.
    pub(crate) fn light(&self, chunks: &Chunks, pos: BlockPos, channel: Channel) -> Option<u8> {
        let (block, sky) = self.lights(chunks, pos)?;
        Some(match channel {
            Channel::Block => block,
            Channel::Sky => sky,
        })
    }

    /// The block light and the sky light at `pos`, from one lookup of the chunk, or `None` where
    /// [`Self::light`] has none.
    pub(crate) fn lights(&self, chunks: &Chunks, pos: BlockPos) -> Option<(u8, u8)> {
        let spot = Spot::of(pos)?;
        let chunk = chunks.get(spot.chunk)?;
        if let Mode::Fixed(level) = self.mode {
            return Some((0, level));
        }
        let light = chunk.light()?;
        Some((
            light.get(Channel::Block, spot.sub, spot.at),
            light.get(Channel::Sky, spot.sub, spot.at),
        ))
    }

    /// How light is worked out (setting `lighting`).
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Follows a block change in a loaded chunk.
    ///
    /// The block at `pos` of `chunk` changed from `old` to `new`. Only [`Mode::Dynamic`] follows
    /// block changes.
    pub(crate) fn changed(&mut self, chunk: &Chunk, pos: BlockPos, old: Entry, new: Entry) {
        if self.mode != Mode::Dynamic || old.derive() == new.derive() {
            return;
        }
        let at = pos.chunk();
        if let Some(seq) = self.index.get(&(at, Kind::Populate))
            && let Some(Task::Populate(_, population)) = self.tasks.get_mut(seq)
        {
            if !population.note(pos, self.limits.population_changes) {
                **population = Population::new(chunk.clone());
            }
            return;
        }
        if chunk.light().is_some() {
            if self.changes.len() < self.limits.changes {
                self.changes.push_back(pos);
            } else {
                self.add_task(at, Kind::Reset, Task::Reset(at));
            }
        }
    }

    /// Replaces the queue bounds (tests force overflows with small ones).
    #[cfg(test)]
    pub(crate) fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Sets how light is worked out for the chunks at `positions` (in order).
    ///
    /// Turning the work off ([`Mode::Fixed`]) drops all light and work; turning it on lights
    /// every loaded chunk again. Between [`Mode::Dynamic`] and [`Mode::OnLoad`] the light stays.
    pub(crate) fn set_mode(
        &mut self,
        chunks: &mut Chunks,
        positions: impl Iterator<Item = ChunkPos>,
        mode: Mode,
    ) {
        let was_calculating = self.is_calculating();
        self.mode = mode;
        let is_calculating = self.is_calculating();
        if is_calculating == was_calculating {
            return;
        }
        self.changes.clear();
        self.recalc.clear();
        self.removal.clear();
        self.spread.clear();
        self.retired.clear();
        self.overflowed.clear();
        self.tasks.clear();
        self.index.clear();
        for pos in positions {
            if let Some(chunk) = chunks.get_mut(pos) {
                chunk.take_light();
            }
            if is_calculating {
                self.loaded(chunks, pos);
            }
        }
    }

    /// Takes in a chunk just put at `pos`.
    ///
    /// Its light is connected to its lit neighbours, or it is lit on the tick if it came without
    /// light (loads and chunks from memory come lit from the CPU pool). That work is bounded by
    /// the budget of [`Self::step`] per tick.
    pub(crate) fn loaded(&mut self, chunks: &mut Chunks, pos: ChunkPos) {
        let Some(chunk) = chunks.get_mut(pos) else {
            return;
        };
        if !self.is_calculating() {
            chunk.take_light();
            return;
        }
        if chunk.light().is_some() {
            self.connect(chunks, pos);
        } else {
            let population = Population::new(chunk.clone());
            self.add_task(
                pos,
                Kind::Populate,
                Task::Populate(pos, Box::new(population)),
            );
        }
    }

    /// Does queued work until it is done or `budget` units are spent.
    ///
    /// It returns the units spent. The order is: chunks marked as overflowed (no units), removal,
    /// recalculation, spread, changed blocks, retired chunks, chunk tasks, so light that lost its
    /// source goes before light spreads again. A column scan is not split, so the last item may
    /// look at up to one column (384 blocks) more than was left, counted as the rest of the
    /// budget.
    pub(crate) fn step(&mut self, chunks: &mut Chunks, budget: u32) -> u32 {
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
                left = left.saturating_sub(self.apply_change(chunks, pos));
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

    /// Takes in the chunk at `pos` that just left, with its light.
    ///
    /// Its tasks go, and the light it gave its lit neighbours is taken out (or, with too many
    /// chunks already retiring, those neighbours are lit again).
    pub(crate) fn unloaded(&mut self, chunks: &Chunks, pos: ChunkPos, light: Option<Box<Levels>>) {
        if !self.is_calculating() {
            return;
        }
        self.forget(pos);
        if let Some(light) = light {
            self.retire(chunks, pos, light);
        }
    }

    /// Whether neither of two chunks can light the other through sub-chunk `sub`.
    ///
    /// That is so when both have uniform light there in both channels, differing by at most 1.
    fn flat_face(chunks: &Chunks, a: ChunkPos, b: ChunkPos, sub: usize) -> bool {
        let uniform = |pos, channel| {
            chunks
                .get(pos)
                .and_then(Chunk::light)
                .and_then(|light| light.array(channel, sub))
                .and_then(Nibbles::uniform)
        };
        Channel::ALL.into_iter().all(|channel| {
            matches!((uniform(a, channel), uniform(b, channel)), (Some(x), Some(y)) if x.abs_diff(y) <= 1)
        })
    }

    /// Whether the chunk at `pos` is loaded and has light.
    fn has_light(chunks: &Chunks, pos: ChunkPos) -> bool {
        chunks.get(pos).is_some_and(|chunk| chunk.light().is_some())
    }

    /// Whether the node queues and the changed blocks are all empty.
    fn is_queue_empty(&self) -> bool {
        self.removal.is_empty()
            && self.recalc.is_empty()
            && self.spread.is_empty()
            && self.changes.is_empty()
    }

    /// The light of `channel` at `pos`, if its chunk is lit.
    // `level`, `relax` and `relax_at` run for every block light spreads to. Without the hint the
    // compiler kept them as calls once `step` grew, and lighting a newly loaded chunk cost about
    // 40% more.
    #[inline]
    fn level(chunks: &Chunks, pos: BlockPos, channel: Channel) -> Option<u8> {
        let spot = Spot::of(pos)?;
        Some(
            chunks
                .get(spot.chunk)?
                .light()?
                .get(channel, spot.sub, spot.at),
        )
    }

    /// Looks past a block whose light `node.level` is gone.
    ///
    /// Each neighbour may have had its light from it.
    fn remove_around(&mut self, chunks: &mut Chunks, node: Node) {
        let Some(spot) = Spot::of(node.pos) else {
            return;
        };
        let around = near(node.pos, spot);
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            for adjacent in around {
                if let Near::Same(target, target_spot) = adjacent {
                    self.remove_at(chunk, target, target_spot, node.channel, node.level);
                }
            }
        }
        for adjacent in around {
            if let Near::Across(target) = adjacent {
                self.remove(chunks, target, node.channel, node.level);
            }
        }
    }

    /// Spreads a queued block's light to its neighbours.
    fn spread_from(&mut self, chunks: &mut Chunks, node: Node) {
        let Some(spot) = Spot::of(node.pos) else {
            return;
        };
        let around = near(node.pos, spot);
        let Some(chunk) = chunks.get_mut(spot.chunk) else {
            return;
        };
        let level = chunk
            .light()
            .map_or(0, |light| light.get(node.channel, spot.sub, spot.at));
        if level <= 1 {
            return;
        }
        for adjacent in around {
            if let Near::Same(target, target_spot) = adjacent {
                self.relax_at(chunk, target, target_spot, node.channel, level);
            }
        }
        for adjacent in around {
            if let Near::Across(target) = adjacent {
                self.relax(chunks, target, node.channel, level);
            }
        }
    }

    /// Works out a block's light again from its own light and its neighbours.
    ///
    /// The difference is queued.
    fn recalculate(&mut self, chunks: &mut Chunks, pos: BlockPos, channel: Channel) {
        let Some(spot) = Spot::of(pos) else {
            return;
        };
        let around = near(pos, spot);
        // Neighbours in other chunks are read first, before the block's chunk is held to write;
        // the others are read from that chunk's light without looking it up again.
        let across = around
            .into_iter()
            .filter_map(|adjacent| match adjacent {
                Near::Same(..) | Near::Out => None,
                Near::Across(target) => Self::level(chunks, target, channel),
            })
            .max();
        let Some(chunk) = chunks.get_mut(spot.chunk) else {
            return;
        };
        let values = chunk.light_values_at(spot.sub, spot.at);
        let Some(light) = chunk.light_mut() else {
            return;
        };
        let own = source(channel, values, pos.y, light.height(spot.column));
        let old = light.get(channel, spot.sub, spot.at);
        let brightest = around
            .into_iter()
            .filter_map(|adjacent| match adjacent {
                Near::Same(_, same) => Some(light.get(channel, same.sub, same.at)),
                Near::Across(_) | Near::Out => None,
            })
            .chain(across)
            .max()
            .unwrap_or(0);
        let new = own.max(brightest.saturating_sub(values.filter));
        if new == old {
            return;
        }
        light.set(channel, spot.sub, spot.at, new);
        if new < old {
            self.push_removal(pos, channel, old);
        }
        if new > 1 {
            self.push_spread(pos, channel);
        }
    }

    /// Follows a changed block and returns the units spent.
    ///
    /// That is one, and one per block a column scan looked at. The block's column's sky height
    /// follows it, and the block and the part of the column whose direct sky light changed are
    /// queued for recalculation.
    ///
    /// The height changes only when the block now stops sky light above the old height (it rises
    /// to the block) or the block just under the old height no longer does (it drops to the next
    /// one below, the one case that scans). Every block changed since the heights were right has
    /// its change queued, and each is applied against the chunk as it is now, so after the last
    /// one the height is the column's.
    fn apply_change(&mut self, chunks: &mut Chunks, pos: BlockPos) -> u32 {
        let Some(spot) = Spot::of(pos) else {
            return 1;
        };
        let (x, z) = pos.local();
        let Some(chunk) = chunks.get_mut(spot.chunk) else {
            return 1;
        };
        let Some(old) = chunk.light().map(|light| light.height(spot.column)) else {
            return 1;
        };
        let blocks = chunk.light_values_at(spot.sub, spot.at).blocks_sky;
        let (new, looked) = if blocks && pos.y >= old {
            (pos.y + 1, 0)
        } else if !blocks && pos.y + 1 == old {
            let (height, looked) = sky_height(chunk, x, z, pos.y - 1);
            (i32::from(height), looked)
        } else {
            (old, 0)
        };
        if new != old
            && let Some(stored) = chunk
                .light_mut()
                .and_then(|light| light.heights.get_mut(spot.column))
        {
            *stored = stored_height(new);
        }
        let spent = 1 + looked;
        let heights = old.min(new)..old.max(new).min(MAX_Y + 1);
        let count = 2 + heights.len();
        if self.recalc.len().saturating_add(count) > self.limits.recalc {
            self.reset(chunks, spot.chunk);
            return spent;
        }
        for channel in Channel::ALL {
            self.recalc.push_back(Node {
                pos,
                channel,
                level: 0,
            });
        }
        for y in heights {
            self.recalc.push_back(Node {
                pos: BlockPos::new(pos.x, y, pos.z),
                channel: Channel::Sky,
                level: 0,
            });
        }
        spent
    }

    /// Takes a retired chunk's light out of its neighbours, pair by pair.
    ///
    /// It goes across the 4 faces while the node queues are empty, and returns whether it is
    /// done.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a cursor below the pairs of the 4 faces is on one of them"
    )]
    fn take_out(&mut self, chunks: &mut Chunks, retired: &mut Retired, left: &mut u32) -> bool {
        while *left > 0 && self.is_queue_empty() {
            if retired.cursor >= ALL_FACE_PAIRS {
                return true;
            }
            *left -= 1;
            let side = Facing::ALL
                .get(index_of(retired.cursor / FACE_PAIRS))
                .copied()
                .expect("the cursor is on one of the 4 faces");
            let i = retired.cursor % FACE_PAIRS;
            let sub = index_of(i / SUB_FACE_PAIRS);
            let Some((other, other_light)) = beside(retired.chunk, side)
                .and_then(|pos| chunks.get(pos))
                .and_then(|chunk| chunk.light().map(|light| (chunk, light)))
            else {
                retired.cursor = (retired.cursor / FACE_PAIRS + 1) * FACE_PAIRS;
                continue;
            };
            // A sub-chunk face where no block beside can have had its light from the retired
            // side: the retired side is dark, or the side beside is dark, full (only sources
            // are 15) or as bright throughout.
            let unrelated = |channel| {
                let had = retired.light.array(channel, sub).and_then(Nibbles::uniform);
                let has = other_light.array(channel, sub).and_then(Nibbles::uniform);
                matches!(had, Some(0 | 1))
                    || matches!(has, Some(0 | FULL))
                    || matches!((had, has), (Some(a), Some(b)) if b >= a)
            };
            if i.is_multiple_of(SUB_FACE_PAIRS) && Channel::ALL.into_iter().all(unrelated) {
                retired.cursor += SUB_FACE_PAIRS;
                continue;
            }
            retired.cursor += 1;
            let Some((inside, outside)) = face_pair(retired.chunk, side, i) else {
                continue;
            };
            let (Some(inside_spot), Some(outside_spot)) = (Spot::of(inside), Spot::of(outside))
            else {
                continue;
            };
            // Light at `outside` (in `other`) that the retired block may have given is removed
            // unless `outside` is brighter, dark or lit by itself. Removing one channel never
            // changes the other, so both are decided from this lookup.
            let taken = Channel::ALL.map(|channel| {
                let had = retired.light.get(channel, inside_spot.sub, inside_spot.at);
                let current = other_light.get(channel, outside_spot.sub, outside_spot.at);
                let own = || {
                    let values = other.light_values_at(outside_spot.sub, outside_spot.at);
                    let height = other_light.height(outside_spot.column);
                    source(channel, values, outside.y, height)
                };
                (had >= 2 && current < had && current > own()).then_some((channel, had))
            });
            for (channel, had) in taken.into_iter().flatten() {
                self.remove(chunks, outside, channel, had);
            }
        }
        retired.cursor >= ALL_FACE_PAIRS
    }

    /// Works on the chunk task `task`, which was taken out under `seq`.
    ///
    /// It puts the task back if it is not done.
    fn run_task(&mut self, chunks: &mut Chunks, seq: u64, task: Task, left: &mut u32) {
        match task {
            Task::Reset(pos) => {
                *left -= 1;
                self.index.remove(&(pos, Kind::Reset));
                let lit_beside = Facing::ALL
                    .into_iter()
                    .filter_map(|side| beside(pos, side))
                    .any(|other| Self::has_light(chunks, other));
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
                let population = Population::new(snapshot);
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
                for block in changed {
                    if self.changes.len() < self.limits.changes {
                        self.changes.push_back(block);
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

    /// Spreads light both ways across the face of `pos` toward `side`, pair by pair.
    ///
    /// It starts at `cursor` and goes on while the node queues are empty, and returns whether it
    /// is done.
    fn cross(
        &mut self,
        chunks: &mut Chunks,
        pos: ChunkPos,
        side: Facing,
        cursor: &mut u32,
        left: &mut u32,
    ) -> bool {
        let Some(other) = beside(pos, side) else {
            return true;
        };
        if !Self::has_light(chunks, pos) || !Self::has_light(chunks, other) {
            return true;
        }
        while *left > 0 && self.is_queue_empty() {
            if *cursor >= FACE_PAIRS {
                return true;
            }
            *left -= 1;
            let i = *cursor;
            let sub = index_of(i / SUB_FACE_PAIRS);
            if i.is_multiple_of(SUB_FACE_PAIRS) && Self::flat_face(chunks, pos, other, sub) {
                *cursor += SUB_FACE_PAIRS;
                continue;
            }
            *cursor += 1;
            let Some((inside, outside)) = face_pair(pos, side, i) else {
                continue;
            };
            // Both channels of a block from one lookup: relaxing one channel never changes the
            // other.
            let levels = |block| {
                let spot = Spot::of(block)?;
                let light = chunks.get(spot.chunk)?.light()?;
                Some(Channel::ALL.map(|channel| (channel, light.get(channel, spot.sub, spot.at))))
            };
            let (Some(from_inside), Some(from_outside)) = (levels(inside), levels(outside)) else {
                continue;
            };
            for ((channel, level_inside), (_, level_outside)) in
                from_inside.into_iter().zip(from_outside)
            {
                if level_inside >= 2 {
                    self.relax(chunks, outside, channel, level_inside);
                }
                if level_outside >= 2 {
                    self.relax(chunks, inside, channel, level_outside);
                }
            }
        }
        *cursor >= FACE_PAIRS
    }

    /// Queues spreading across each face between the chunk at `pos` and a lit neighbour.
    fn connect(&mut self, chunks: &Chunks, pos: ChunkPos) {
        for side in Facing::ALL {
            if beside(pos, side).is_some_and(|other| Self::has_light(chunks, other)) {
                self.add_task(pos, Kind::Border(side.bit()), Task::Border(pos, side, 0));
            }
        }
    }

    /// Keeps `light`, which left `pos`, until what it lit in its lit neighbours is taken out.
    ///
    /// With no lit neighbour it is dropped; with too many chunks retiring, the neighbours are lit
    /// again instead.
    fn retire(&mut self, chunks: &Chunks, pos: ChunkPos, light: Box<Levels>) {
        let lit = Facing::ALL
            .map(|side| beside(pos, side).filter(|&other| Self::has_light(chunks, other)));
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
            for other in lit.into_iter().flatten() {
                self.reset(chunks, other);
            }
        }
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

    /// Does [`Self::remove_at`] for a block of any loaded chunk.
    fn remove(&mut self, chunks: &mut Chunks, target: BlockPos, channel: Channel, old: u8) {
        let Some(spot) = Spot::of(target) else {
            return;
        };
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            self.remove_at(chunk, target, spot, channel, old);
        }
    }

    /// Handles the light at `target` (in `chunk`) that may have come from a lost neighbour light.
    ///
    /// The neighbour's light `old` is gone. The light at `target` is darkened to its own light and
    /// queued for removal, or, if it is brighter (lit from elsewhere) or its own source, queued to
    /// spread again: the own light stays and every darkened block is looked past.
    fn remove_at(
        &mut self,
        chunk: &mut Chunk,
        target: BlockPos,
        spot: Spot,
        channel: Channel,
        old: u8,
    ) {
        let values = chunk.light_values_at(spot.sub, spot.at);
        let Some(light) = chunk.light_mut() else {
            return;
        };
        let current = light.get(channel, spot.sub, spot.at);
        if current == 0 {
            return;
        }
        let own = source(channel, values, target.y, light.height(spot.column));
        if current < old && current > own {
            light.set(channel, spot.sub, spot.at, own);
            self.push_removal(target, channel, current);
            if own > 1 {
                self.push_spread(target, channel);
            }
        } else if current > 1 {
            self.push_spread(target, channel);
        }
    }

    /// Does [`Self::relax_at`] for a block of any loaded chunk.
    #[inline]
    fn relax(&mut self, chunks: &mut Chunks, target: BlockPos, channel: Channel, level: u8) {
        let Some(spot) = Spot::of(target) else {
            return;
        };
        if let Some(chunk) = chunks.get_mut(spot.chunk) {
            self.relax_at(chunk, target, spot, channel, level);
        }
    }

    /// Raises `target` (in `chunk`) to `level` less its filter if that is brighter.
    ///
    /// It queues the block to spread.
    #[inline]
    fn relax_at(
        &mut self,
        chunk: &mut Chunk,
        target: BlockPos,
        spot: Spot,
        channel: Channel,
        level: u8,
    ) {
        let Some(current) = chunk
            .light()
            .map(|light| light.get(channel, spot.sub, spot.at))
        else {
            return;
        };
        // Every filter is at least 1 (`Entry::derive`), so a block this bright cannot be raised:
        // its block is not looked up.
        if current + 1 >= level {
            return;
        }
        let potential = level.saturating_sub(chunk.light_values_at(spot.sub, spot.at).filter);
        if potential > current
            && let Some(light) = chunk.light_mut()
        {
            light.set(channel, spot.sub, spot.at, potential);
            if potential > 1 {
                self.push_spread(target, channel);
            }
        }
    }

    /// Marks a lit chunk to be lit again from scratch: work about it did not fit a queue.
    fn reset(&mut self, chunks: &Chunks, pos: ChunkPos) {
        if Self::has_light(chunks, pos) {
            self.add_task(pos, Kind::Reset, Task::Reset(pos));
        }
    }

    /// Adds `task` of `kind` for the chunk at `pos` unless one of that kind waits already.
    fn add_task(&mut self, pos: ChunkPos, kind: Kind, task: Task) {
        if self.index.contains_key(&(pos, kind)) {
            return;
        }
        let seq = self.next_task;
        self.next_task += 1;
        self.tasks.insert(seq, task);
        self.index.insert((pos, kind), seq);
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
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("mode", &self.mode)
            .field("changes", &self.changes.len())
            .field("recalc", &self.recalc.len())
            .field("removal", &self.removal.len())
            .field("spread", &self.spread.len())
            .field("retired", &self.retired.len())
            .field("tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

/// The checkpoint form of the replay tool.
///
/// Light depends on what was loaded when, so a replay takes it from the checkpoint. Every field
/// is kept but the bounds, which are constants, and the index, which is `tasks` again. Reading
/// refuses work that breaks the bounds or the order numbers.
impl Codec for Engine {
    fn encode(&self, w: &mut Writer) {
        let Self {
            mode,
            // The bounds are constants: reading gives [`Limits::DEFAULT`].
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
        mode.encode(w);
        changes.encode(w);
        recalc.encode(w);
        removal.encode(w);
        spread.encode(w);
        retired.encode(w);
        overflowed.encode(w);
        tasks.encode(w);
        next_task.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        let limits = Limits::DEFAULT;
        let mode: Mode = Codec::decode(r)?;
        let changes: VecDeque<BlockPos> = Codec::decode(r)?;
        let recalc: VecDeque<Node> = Codec::decode(r)?;
        let removal: VecDeque<Node> = Codec::decode(r)?;
        let spread: VecDeque<Node> = Codec::decode(r)?;
        let retired: VecDeque<Retired> = Codec::decode(r)?;
        let overflowed: BTreeSet<ChunkPos> = Codec::decode(r)?;
        let tasks: BTreeMap<u64, Task> = Codec::decode(r)?;
        let next_task: u64 = Codec::decode(r)?;
        let level_fits = !matches!(mode, Mode::Fixed(level) if level > FULL);
        let queues_fit = changes.len() <= limits.changes
            && recalc.len() <= limits.recalc
            && removal.len() <= limits.nodes
            && spread.len() <= limits.nodes
            && retired.len() <= limits.retired;
        let numbers_fit = next_task != u64::MAX
            && tasks
                .keys()
                .next_back()
                .is_none_or(|&last| last < next_task);
        if !(level_fits && queues_fit && numbers_fit) {
            return Err(Error::Format("light engine"));
        }
        let mut index = BTreeMap::new();
        for (&seq, task) in &tasks {
            let key = match task {
                Task::Reset(pos) => (*pos, Kind::Reset),
                Task::Populate(pos, _) => (*pos, Kind::Populate),
                Task::Border(pos, side, _) => (*pos, Kind::Border(side.bit())),
            };
            if index.insert(key, seq).is_some() {
                return Err(Error::Format("repeated light task"));
            }
        }
        Ok(Self {
            mode,
            limits,
            changes,
            recalc,
            removal,
            spread,
            retired,
            overflowed,
            tasks,
            index,
            next_task,
        })
    }
}

/// How a world's light is worked out (setting `lighting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Light worked out and following block changes (`"dynamic"`).
    Dynamic,
    /// Light worked out for each chunk that loads (`"on-load"`).
    ///
    /// Block changes leave it as it is.
    OnLoad,
    /// Nothing worked out (`"fixed"`, `fixed-light-level`).
    ///
    /// Every block has this sky light, at most 15, and no block light.
    Fixed(u8),
}

impl Mode {
    /// The mode of the settings `lighting` and `fixed-light-level`.
    pub(crate) fn of(lighting: Lighting, fixed_level: u8) -> Self {
        match lighting {
            Lighting::Dynamic => Self::Dynamic,
            Lighting::OnLoad => Self::OnLoad,
            Lighting::Fixed => Self::Fixed(fixed_level.min(FULL)),
        }
    }
}

checkpoint::variants!(Mode {
    0 => Dynamic,
    1 => OnLoad,
    2 => Fixed(level),
});

/// Bounds on the engine's queues.
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
    /// The engine's bounds.
    ///
    /// These are at most about 9.5 MB of queued blocks per world (16 bytes each), and 256 retired
    /// chunks' light (typically a few KB each, 98 KB at most).
    pub(crate) const DEFAULT: Self = Self {
        changes: 16_384,
        recalc: 65_536,
        nodes: 262_144,
        retired: 256,
        population_changes: 4096,
    };
}

/// A queued block: its position, channel and, in the removal queue, the light it had.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// The block.
    pos: BlockPos,
    /// The channel of the light.
    channel: Channel,
    /// The light the block had, in the removal queue.
    level: u8,
}

checkpoint::fields!(Node {
    pos,
    channel,
    level
});

/// The kind of a chunk task: one of each per chunk at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// The chunk's light retired and worked out again alone.
    Reset,
    /// The chunk lit alone.
    Populate,
    /// Light spread across the face toward a side (its bit).
    Border(u8),
}

/// Work on a whole chunk, done in order after the node queues are empty.
#[derive(Debug, Clone)]
enum Task {
    /// The chunk's light retired and worked out again alone (after a queue overflow).
    Reset(ChunkPos),
    /// The chunk lit alone from a snapshot.
    Populate(ChunkPos, Box<Population>),
    /// Light spread both ways across the face toward a side, with the next pair to look at.
    Border(ChunkPos, Facing, u32),
}

checkpoint::variants!(Task {
    0 => Reset(pos),
    1 => Populate(pos, population),
    2 => Border(pos, side, at),
});

/// The light of a chunk that left (unloaded or reset).
///
/// It is kept until what it lit in the chunk's neighbours is taken out.
#[derive(Debug, Clone)]
struct Retired {
    /// The chunk that left.
    chunk: ChunkPos,
    /// The light it had.
    light: Box<Levels>,
    /// The next pair to look at, walking the pairs of the 4 faces.
    cursor: u32,
}

checkpoint::fields!(Retired {
    chunk,
    light,
    cursor
});

/// The light of a lit chunk.
///
/// It is 4-bit values per sub-chunk and channel, and the sky height of each column.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Levels {
    /// Block light, then sky light, by sub-chunk from the bottom.
    arrays: [[Nibbles; SUB_CHUNKS]; Channel::COUNT],
    /// One above the highest block that stops direct sky light, by [`column()`].
    heights: [i16; COLUMNS],
}

impl Levels {
    /// Hashes every value and sky height: a uniform cube by its value, a full one by its bytes.
    #[cfg(test)]
    pub(crate) fn hash_into(&self, h: &mut Digest) {
        let Self { arrays, heights } = self;
        for nibbles in arrays.iter().flatten() {
            match nibbles {
                Nibbles::Uniform(value) => h.word(0x100 | u64::from(*value)),
                Nibbles::Full(bytes) => {
                    h.word(0x200);
                    for word in bytes.as_chunks::<8>().0 {
                        h.word(u64::from_le_bytes(*word));
                    }
                }
            }
        }
        for height in heights {
            h.word(u64::from(height.cast_unsigned()));
        }
    }

    /// The bytes it holds on the heap beside its own size.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.arrays.iter().flatten().map(Nibbles::heap_bytes).sum()
    }

    /// The values of `channel` in sub-chunk `sub`, if the sub-chunk is in the world.
    fn array(&self, channel: Channel, sub: usize) -> Option<&Nibbles> {
        self.arrays.get(channel as usize)?.get(sub)
    }

    /// The light of `channel` at offset `at` of sub-chunk `sub`.
    fn get(&self, channel: Channel, sub: usize, at: u16) -> u8 {
        self.array(channel, sub)
            .map_or(0, |nibbles| nibbles.get(at))
    }

    /// The sky height of column index `column`.
    fn height(&self, column: usize) -> i32 {
        self.heights.get(column).map_or(MIN_Y, |&h| i32::from(h))
    }

    /// The values of `channel` in sub-chunk `sub`, to change.
    fn array_mut(&mut self, channel: Channel, sub: usize) -> Option<&mut Nibbles> {
        self.arrays.get_mut(channel as usize)?.get_mut(sub)
    }

    /// Sets the light of `channel` at offset `at` of sub-chunk `sub`.
    fn set(&mut self, channel: Channel, sub: usize, at: u16, value: u8) {
        if let Some(nibbles) = self.array_mut(channel, sub) {
            nibbles.set(at, value);
        }
    }
}

impl fmt::Debug for Levels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Levels")
            .field("heap_bytes", &self.heap_bytes())
            .finish_non_exhaustive()
    }
}

checkpoint::fields!(Levels { arrays, heights });

/// The two kinds of light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Channel {
    /// Light from emitting blocks.
    Block = 0,
    /// Light from the sky.
    Sky = 1,
}

impl Channel {
    /// The number of kinds of light.
    pub(crate) const COUNT: usize = 2;

    /// Both kinds of light, in order.
    pub(crate) const ALL: [Self; Self::COUNT] = [Self::Block, Self::Sky];
}

checkpoint::variants!(Channel {
    0 => Block,
    1 => Sky,
});

/// The light values of a block.
///
/// A sub-chunk's palette keeps them beside each entry ([`Derive`]), so the light work reads them
/// by the slot's palette index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Values {
    /// The light the block gives off.
    emission: u8,
    /// The light that drops when light enters the block: its filter plus 1, at most 15.
    filter: u8,
    /// Whether the block stops direct sky light.
    blocks_sky: bool,
}

/// Where a block's light lives: its chunk, sub-chunk, offset and column.
#[derive(Debug, Clone, Copy)]
struct Spot {
    /// The block's chunk.
    chunk: ChunkPos,
    /// The index of the block's sub-chunk.
    sub: usize,
    /// The block's offset in its sub-chunk.
    at: u16,
    /// The block's column index.
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

/// A neighbour of a block.
///
/// It is in the same chunk (with where its light lives), across a chunk face, or outside the
/// world height.
#[derive(Debug, Clone, Copy)]
enum Near {
    /// A block of the same chunk, with its spot.
    Same(BlockPos, Spot),
    /// A block of the chunk beside.
    Across(BlockPos),
    /// Nothing: above or below the world.
    Out,
}

/// A hash for the light hashes of the determinism tests.
///
/// It is a word-at-a-time FNV-1a mix, cheap enough to hash the light of every loaded chunk after
/// every tick of the determinism runs.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct Digest(u64);

#[cfg(test)]
impl Digest {
    /// A hash of nothing yet.
    pub(crate) fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// The hash of everything mixed in.
    pub(crate) fn finish(&self) -> u64 {
        self.0
    }

    /// Mixes in a word.
    pub(crate) fn word(&mut self, v: u64) {
        self.0 = (self.0 ^ v)
            .wrapping_mul(0x0000_0100_0000_01b3)
            .rotate_left(29);
    }

    /// Mixes in a block position.
    fn pos(&mut self, pos: BlockPos) {
        for c in [pos.x, pos.y, pos.z] {
            self.word(u64::from(c.cast_unsigned()));
        }
    }

    /// Mixes in a chunk position.
    fn chunk(&mut self, pos: ChunkPos) {
        self.word(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
    }

    /// Mixes in a length.
    fn len(&mut self, n: usize) {
        self.word(u64::try_from(n).unwrap_or(u64::MAX));
    }
}

/// The light values of an entry are its state's.
///
/// An unknown state counts as `minecraft:info_update`, the block clients see in its place.
impl Derive for Entry {
    type Derived = Values;

    fn derive(self) -> Values {
        let (emission, filter, blocks_sky) = match self {
            Entry::Known(s) => (
                s.light_emission(),
                s.light_filter(),
                s.blocks_direct_sky_light(),
            ),
            Entry::Unknown(_) => {
                let s = block::INFO_UPDATE;
                (
                    s.light_emission(),
                    s.light_filter(),
                    s.blocks_direct_sky_light(),
                )
            }
        };
        Values {
            emission: emission.min(FULL),
            filter: filter.saturating_add(1).min(FULL),
            blocks_sky,
        }
    }
}

/// How much sky light the time of day takes away, 0 (day) to 11 (night).
///
/// `time` is the world time in ticks; only `time` mod 24,000 counts, and a negative time counts
/// from the day's start.
pub(crate) fn sky_light_reduction(time: i64) -> u8 {
    let tick = i64::from(tick_of_day(time));
    let after = REDUCTION.partition_point(|&(start, _)| start <= tick);
    REDUCTION
        .get(after.saturating_sub(1))
        .map_or(0, |&(_, reduction)| reduction)
}

/// The light of `chunk` worked out alone, as if every neighbour were dark.
///
/// It is a pure function of the chunk, for the CPU pool; the tick connects it to its neighbours
/// when it loads.
pub(crate) fn populate(chunk: &Chunk) -> Levels {
    let mut population = Population::new(chunk.clone());
    let mut units = u32::MAX;
    while !population.run(&mut units) {
        units = u32::MAX;
    }
    population.finish().0
}

/// The light a block has of itself.
///
/// Block light comes from its emission; sky light is 15 at and above its column's sky height, and
/// for the top block of a column blocked at the top, 15 less its filter (the sky above the
/// world).
fn source(channel: Channel, values: Values, y: i32, height: i32) -> u8 {
    match channel {
        Channel::Block => values.emission,
        Channel::Sky if y >= height => FULL,
        Channel::Sky if y == MAX_Y => FULL.saturating_sub(values.filter),
        Channel::Sky => 0,
    }
}

/// The sky height of local column (x, z) from its blocks at and below `top`.
///
/// It is one above the highest block that stops direct sky light, or the world bottom, and the
/// number of blocks looked at.
fn sky_height(chunk: &Chunk, x: u8, z: u8, top: i32) -> (i16, u32) {
    let Some(top) = chunk.highest_block(x, z).map(|height| height.min(top)) else {
        return (BOTTOM, 0);
    };
    let mut looked = 0;
    for y in (MIN_Y..=top).rev() {
        looked += 1;
        if chunk
            .light_values(x, y, z)
            .is_some_and(|values| values.blocks_sky)
        {
            return (stored_height(y + 1), looked);
        }
    }
    (BOTTOM, looked)
}

/// A sky height as a chunk's light stores it.
#[expect(
    clippy::expect_used,
    reason = "invariant: a sky height is at most one above the world top"
)]
fn stored_height(height: i32) -> i16 {
    i16::try_from(height).expect("a sky height fits 16 bits")
}

/// The six neighbours of the block at `pos`.
///
/// Its spot is `spot`, so they are worked out without looking up a chunk.
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

/// The block positions of pair `i` across the face of chunk `pos` toward `side`.
///
/// They are the block in the chunk and the block beside it. `i` runs from 0 to 6,143 as
/// sub-chunk × 256 + across × 16 + y.
fn face_pair(pos: ChunkPos, side: Facing, i: u32) -> Option<(BlockPos, BlockPos)> {
    let sub = i32::try_from(i / SUB_FACE_PAIRS).ok()?;
    let across = i32::try_from((i % SUB_FACE_PAIRS) / 16).ok()?;
    let y = MIN_Y + sub * 16 + i32::try_from(i % 16).ok()?;
    let (bx, bz) = (pos.x.checked_mul(16)?, pos.z.checked_mul(16)?);
    let (inside, outside) = match side {
        Facing::North => ((across, 0), (across, -1)),
        Facing::East => ((15, across), (16, across)),
        Facing::South => ((across, 15), (across, 16)),
        Facing::West => ((0, across), (-1, across)),
    };
    let at = |(x, z): (i32, i32)| Some(BlockPos::new(bx.checked_add(x)?, y, bz.checked_add(z)?));
    Some((at(inside)?, at(outside)?))
}

/// A cursor or count of the light work as an index.
#[expect(
    clippy::expect_used,
    reason = "invariant: a cursor or count of the light work fits a usize"
)]
fn index_of(value: u32) -> usize {
    usize::try_from(value).expect("a light cursor fits a usize")
}

/// The six neighbours of `pos` that are valid positions (any height).
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::State;
    use crate::custom::Registry;
    use crate::protocol::Rng;
    use crate::tick::Customs;
    use crate::world::chunk::tests::plain_states;
    use crate::world::{DAY, Flat, Generator, PLAINS, Paletted, SubChunk, Void, World};

    /// Blocks in a chunk, indexed by [`cell`].
    const CELLS: usize = 384 * 256;

    fn cell(x: u8, y: i32, z: u8) -> usize {
        usize::try_from(y - MIN_Y).unwrap() * 256 + column(x, z)
    }

    /// One state for each combination of light values in the table, without neighbour rules.
    ///
    /// The values are the emission, the filter and whether the state stops direct sky light.
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

    /// Loads `chunk` at `pos` into `w`, to be lit on the tick.
    fn load(w: &mut World, pos: ChunkPos, chunk: Chunk) {
        w.insert_chunk(pos, chunk);
    }

    /// Loads `chunk` at `pos` into `w`, lit on the "pool" as the save thread does.
    fn load_lit(w: &mut World, pos: ChunkPos, mut chunk: Chunk) {
        chunk.populate_light();
        w.insert_chunk(pos, chunk);
    }

    /// A world with the chunks of x and z from `-r` to `r` from `generator`.
    ///
    /// `build` sets blocks in each chunk first (local x, z). Every other chunk is lit on the pool,
    /// the rest on the tick.
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
                if (x + z).rem_euclid(2) == 0 {
                    load_lit(&mut w, pos, chunk);
                } else {
                    load(&mut w, pos, chunk);
                }
            }
        }
        w
    }

    /// Steps the light with `budget` per tick until no work is left.
    ///
    /// It asserts that no tick spends more than the budget and that every tick with work left
    /// spends some, and returns the ticks.
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

    /// The light of every loaded chunk worked out from scratch, independently of the engine.
    ///
    /// It is a plain breadth-first spread over all loaded chunks from the sources (emitters;
    /// sky light 15 at and above one over each column's highest sky blocker, and from above the
    /// world into the top block), dropping by filter + 1 per block and never entering a chunk
    /// that is not loaded. It holds each column's sky height too.
    fn reference(w: &World) -> BTreeMap<ChunkPos, ([Vec<u8>; 2], Vec<i32>)> {
        let mut out = BTreeMap::new();
        let mut queue = VecDeque::new();
        for cp in w.loaded_chunks() {
            let chunk = w.chunk(cp).unwrap();
            let mut arrays = [vec![0_u8; CELLS], vec![0_u8; CELLS]];
            let mut heights = vec![MIN_Y; COLUMNS];
            for x in 0..16 {
                for z in 0..16 {
                    let top = (MIN_Y..=MAX_Y)
                        .rev()
                        .find(|&y| chunk.entry(x, y, z).unwrap().derive().blocks_sky)
                        .map_or(MIN_Y, |y| y + 1);
                    heights[column(x, z)] = top;
                    for y in MIN_Y..=MAX_Y {
                        let e = chunk.entry(x, y, z).unwrap();
                        let i = cell(x, y, z);
                        arrays[0][i] = e.derive().emission;
                        arrays[1][i] = if y >= top {
                            15
                        } else if y == MAX_Y {
                            15 - e.derive().filter
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
                let potential = v.saturating_sub(chunk.entry(qx, q.y, qz).unwrap().derive().filter);
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
                        for (c, channel) in Channel::ALL.into_iter().enumerate() {
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

    /// A random position of the chunks from -1 to 1.
    ///
    /// It is mostly near the flat ground, some at the top of the world (tall sky shadows and the
    /// block lit from above the world).
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
        settle(&mut w, 4096);
        assert_matches_reference(&w);
        let mut rng = Rng(0x00e1_1001);
        let budget = 2000;
        let mut waiting = 0;
        for round in 1..=10_000_usize {
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
            if round == 5000 {
                settle(&mut w, budget);
                assert_matches_reference(&w);
            }
        }
        let ticks = settle(&mut w, budget);
        assert_matches_reference(&w);
        assert!(waiting > 100 && ticks > 1, "work waited for later ticks");
    }

    fn find(name: &str) -> State {
        State::all()
            .find(|s| s.name() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    fn water() -> State {
        State::find(
            "minecraft:water",
            &[("liquid_depth", block::PropertyValue::Int(0))],
        )
        .unwrap()
    }

    /// A world of caves, lit and settled.
    ///
    /// It is stone from y 0 to 63 in the chunks from -1 to 1, with a tunnel along x at y 8, z 8
    /// from x -12 to 28 (across the chunk borders at x 0 and 16) and a shaft open to the sky at
    /// x 8, z 24 down to y 9.
    fn caves() -> World {
        let stone = find("minecraft:stone");
        let mut w = world_with(&Void, 1, |pos, chunk| {
            for x in 0..16_u8 {
                for z in 0..16_u8 {
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
        settle(&mut w, 1000);
        w
    }

    // api: World::getBlockLightAt
    #[test]
    fn block_light_follows_the_spread_rules() {
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
                Some(14_u8.saturating_sub(d.try_into().unwrap()))
            );
        }
        // Water filters 2: light drops by 3 into it, then by 1 again.
        w.set_block(at(-5), water).unwrap();
        settle(&mut w, 50);
        assert_eq!(w.block_light(at(-6)), Some(10));
        assert_eq!(w.block_light(at(-5)), Some(7));
        assert_eq!(w.block_light(at(-4)), Some(6));
        // A stronger emitter beside a weaker one, then removed: the weaker keeps its light.
        w.set_block(at(-11), glowstone).unwrap();
        settle(&mut w, 1000);
        assert_eq!(w.block_light(at(-10)), Some(14));
        w.set_block(at(-11), block::AIR).unwrap();
        settle(&mut w, 1000);
        assert_eq!(w.block_light(at(-10)), Some(14));
        assert_eq!(w.block_light(at(-11)), Some(13));
        // Two torches whose light meets at 1 in x 3; the first removed, x 3 keeps 1 from the
        // second (a block darkened from 1 is looked past too).
        w.set_block(at(-5), block::AIR).unwrap();
        w.set_block(at(16), torch).unwrap();
        settle(&mut w, 1000);
        assert_eq!(
            [2, 3, 4].map(|x| w.block_light(at(x))),
            [Some(2), Some(1), Some(2)]
        );
        w.set_block(at(-10), block::AIR).unwrap();
        w.set_block(at(-11), block::AIR).unwrap();
        settle(&mut w, 1000);
        assert_eq!(
            [2, 3, 4].map(|x| w.block_light(at(x))),
            [Some(0), Some(1), Some(2)]
        );
        assert_matches_reference(&w);
    }

    // api: World::getPotentialBlockSkyLightAt
    #[test]
    fn sky_light_follows_the_spread_rules() {
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
        settle(&mut w, 1000);
        assert_eq!(
            w.sky_light(shaft(9)),
            Some(15),
            "glass keeps direct sky light"
        );
        // Water stops direct sky light: 15 less 3 in it, then 1 less per block below.
        w.set_block(shaft(40), water).unwrap();
        settle(&mut w, 1000);
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
        settle(&mut w, 1000);
        assert_eq!(w.sky_light(shaft(40)), Some(14));
        assert_eq!(w.sky_light(shaft(30)), Some(4));
        w.set_block(shaft(40), block::AIR).unwrap();
        settle(&mut w, 1000);
        assert_eq!(w.sky_light(shaft(9)), Some(15));
        assert_eq!(height(&w), 9);
        // A sky blocker in the top block is lit from above the world.
        w.set_block(BlockPos::new(8, MAX_Y, 24), water).unwrap();
        settle(&mut w, 1000);
        assert_eq!(w.sky_light(BlockPos::new(8, MAX_Y, 24)), Some(12));
        assert_eq!(height(&w), MAX_Y + 1);
        assert_eq!(w.sky_light(BlockPos::new(8, MAX_Y + 1, 24)), None);
        assert_eq!(w.block_light(BlockPos::new(8, 0, 40)), None, "not loaded");
        assert_matches_reference(&w);
    }

    fn world(generator: &dyn Generator, r: i32) -> World {
        world_with(generator, r, |_, _| {})
    }

    #[test]
    fn overflowing_queues_light_chunks_again_and_still_converge() {
        let states = light_states();
        let mut w = world(&Flat, 1);
        w.light.set_limits(Limits {
            changes: 32,
            recalc: 512,
            nodes: 8192,
            retired: 1,
            population_changes: 8,
        });
        settle(&mut w, 4096);
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
            w.step_light(3000);
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
                    w.insert_chunk(p, c);
                }
            }
        }
        assert!(
            resets > 10,
            "overflows marked chunks to be lit again ({resets})"
        );
        settle(&mut w, 3000);
        assert_matches_reference(&w);
    }

    #[test]
    fn unloading_drops_work_and_takes_out_the_light_it_gave() {
        let glowstone = find("minecraft:glowstone");
        let mut w = world(&Flat, 1);
        settle(&mut w, 4096);
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
        settle(&mut w, 1000);
        assert_eq!(w.block_light(BlockPos::new(15, -60, 5)), Some(0));
        assert_matches_reference(&w);
        // Put back without light, it is lit on the tick and its light crosses again.
        w.insert_chunk(pos, chunk);
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
        settle(&mut w, 1000);
        assert_eq!(w.block_light(BlockPos::new(15, -60, 5)), Some(14));
        assert_eq!(w.block_light(BlockPos::new(20, -60, 10)), Some(14));
        assert_matches_reference(&w);
        // Unloading everything with work pending leaves nothing behind.
        w.set_block(BlockPos::new(0, -60, 0), glowstone).unwrap();
        w.step_light(10);
        let all: Vec<ChunkPos> = w.loaded_chunks().collect();
        for p in all {
            w.remove_chunk(p);
        }
        settle(&mut w, 1000);
        assert!(w.is_light_idle());
        assert_eq!(w.loaded_chunks().count(), 0);
    }

    // api: World::getRealBlockSkyLightAt, World::getFullLightAt
    #[test]
    fn lighting_off_sees_an_open_sky_everywhere() {
        let torch = find("minecraft:torch");
        let mut w = world(&Flat, 1);
        w.set_lighting(Mode::Fixed(15));
        assert!(!w.light.is_calculating());
        assert!(w.is_light_idle());
        let under = BlockPos::new(3, MIN_Y, 3);
        w.set_block(BlockPos::new(3, -60, 3), torch).unwrap();
        assert!(w.is_light_idle(), "changes make no work");
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(0));
        assert_eq!(w.sky_light(under), Some(15));
        w.set_time(18_000);
        assert_eq!(w.real_sky_light(under), Some(4), "midnight");
        w.set_time(6000);
        assert_eq!(w.full_light(under), Some(15));
        assert_eq!(w.sky_light(BlockPos::new(48, 0, 0)), None, "not loaded");
        assert!(w.is_chunk_lit(ChunkPos::new(0, 0)));
        assert!(!w.is_chunk_lit(ChunkPos::new(3, 0)));
        let chunk = Chunk::generate(&Flat, 3, ChunkPos::new(2, 0));
        load_lit(&mut w, ChunkPos::new(2, 0), chunk);
        assert!(
            w.chunk(ChunkPos::new(2, 0)).unwrap().light().is_none(),
            "pool light is dropped"
        );
        // On again: every chunk is lit on the tick.
        w.set_lighting(Mode::Dynamic);
        assert!(!w.is_chunk_lit(ChunkPos::new(0, 0)));
        settle(&mut w, 4096);
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(14));
        assert_eq!(w.sky_light(under), Some(0));
        w.set_time(18_000);
        assert_eq!(w.full_light(BlockPos::new(3, -60, 4)), Some(13));
        assert_matches_reference(&w);
    }

    /// `lighting = "on-load"`: chunks are lit as they load, and block changes queue no work.
    #[test]
    fn on_load_lighting_lights_chunks_but_not_block_changes() {
        let torch = find("minecraft:torch");
        let mut w = world(&Flat, 1);
        w.set_lighting(Mode::OnLoad);
        settle(&mut w, 4096);
        let under = BlockPos::new(3, MIN_Y, 3);
        assert_eq!(w.sky_light(under), Some(0), "calculated");
        let lit = w.light_hash();
        w.set_block(BlockPos::new(3, -60, 3), torch).unwrap();
        assert!(w.is_light_idle(), "changes make no work");
        assert_eq!(w.light_hash(), lit);
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(0));
        let pos = ChunkPos::new(2, 0);
        load(&mut w, pos, Chunk::generate(&Flat, 3, pos));
        settle(&mut w, 4096);
        assert!(w.is_chunk_lit(pos), "a loaded chunk is lit");
        assert_eq!(w.sky_light(BlockPos::new(35, MIN_Y, 3)), Some(0));
    }

    /// `lighting = "fixed"`: no work, the fixed level as sky light and no block light.
    #[test]
    fn fixed_lighting_gives_its_sky_light_everywhere() {
        let torch = find("minecraft:torch");
        let mut w = world(&Flat, 1);
        w.set_lighting(Mode::Fixed(7));
        assert!(w.is_light_idle());
        let under = BlockPos::new(3, MIN_Y, 3);
        w.set_block(BlockPos::new(3, -60, 3), torch).unwrap();
        assert!(w.is_light_idle(), "changes make no work");
        assert_eq!(w.block_light(BlockPos::new(3, -60, 3)), Some(0));
        assert_eq!(w.sky_light(under), Some(7));
        w.set_time(18_000);
        assert_eq!(w.real_sky_light(under), Some(0), "midnight");
        assert!(w.is_chunk_lit(ChunkPos::new(0, 0)));
        assert_eq!(
            Mode::of(Lighting::Fixed, 7),
            Mode::Fixed(7),
            "the setting's level"
        );
    }

    #[test]
    fn the_time_table_follows_the_sun_angle_formula() {
        use crate::math::{clamp, cos};
        use std::f64::consts::PI;
        for time in 0..DAY {
            let progress = f64::from(i32::try_from(time).unwrap()) / 24_000.0;
            let sun = progress + if progress < 0.25 { 0.75 } else { -0.25 };
            #[expect(
                clippy::manual_midpoint,
                reason = "consistency: the sun-angle formula as it is written"
            )]
            let diff = ((1.0 - (cos(sun * PI) + 1.0) / 2.0) - sun) / 3.0;
            let angle = sun + diff;
            let percentage = clamp(-(cos(angle * 2.0 * PI) * 2.0 - 0.5), 0.0, 1.0);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "consistency: the formula's integer cast of a value from 0 to 11"
            )]
            let want = (percentage * 11.0) as u8;
            assert_eq!(sky_light_reduction(time), want, "time {time}");
        }
        assert_eq!(sky_light_reduction(-1), sky_light_reduction(23_999));
        assert_eq!(sky_light_reduction(5 * DAY + 18_000), 11);
        assert_eq!(sky_light_reduction(6000), 0);
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
        let mut tick = Population::new(chunk.clone());
        let mut steps = 0;
        loop {
            let mut units = 1000;
            steps += 1;
            if tick.run(&mut units) {
                break;
            }
            assert_eq!(units, 0);
        }
        assert!(steps > 10);
        assert_eq!(tick.finish().0, pool);
    }

    /// A tick of light work charges every block a column scan looks at.
    ///
    /// The chunk is lit on the tick under a roof that lets the sky through, glass at the top of
    /// every column. Each column's sky height looks at the whole column, 384 blocks, and each
    /// block looked at is a unit of the tick's light budget.
    #[test]
    fn a_tick_of_light_work_charges_every_block_its_column_scans_look_at() {
        let clear = plain_states()
            .into_iter()
            .find(|s| *s != block::AIR && s.light_filter() == 0 && !s.blocks_direct_sky_light())
            .unwrap();
        let mut chunk = Chunk::new();
        for x in 0..16 {
            for z in 0..16 {
                chunk.set_block(x, MAX_Y, z, clear).unwrap();
            }
        }
        let mut w = World::new(
            "w",
            crate::world::Kind::Temporary,
            0,
            BlockPos::new(0, 0, 0),
        );
        load(&mut w, ChunkPos::new(0, 0), chunk);
        let mut units = 0_u64;
        while !w.is_light_idle() {
            units += u64::from(w.step_light(u32::MAX));
        }
        let scans = 256 * 384;
        assert!(
            units >= scans,
            "{units} units for {scans} blocks of column scans"
        );
    }

    /// A chunk of `count` sub-chunks of stone from the bottom.
    ///
    /// Each sub-chunk holds one block value for the whole cube.
    fn stone_cubes(count: usize) -> Chunk {
        let stone = Entry::Known(find("minecraft:stone"));
        let sub_chunks = std::array::from_fn(|index| {
            (index < count).then(|| {
                SubChunk::from_layers([Paletted::uniform(stone), Paletted::uniform(Entry::AIR)])
            })
        });
        let biomes = std::array::from_fn(|_| Paletted::uniform(PLAINS));
        Chunk::from_parts(sub_chunks, biomes, Vec::new()).unwrap()
    }

    #[test]
    fn a_sub_chunk_of_one_block_without_light_takes_one_unit_to_scan() {
        let stone = find("minecraft:stone");
        let mut set_one_by_one = Chunk::new();
        for x in 0..16 {
            for z in 0..16 {
                for y in MIN_Y..MIN_Y + 48 {
                    set_one_by_one.set_block(x, y, z, stone).unwrap();
                }
            }
        }
        let mut population = Population::new(stone_cubes(3));
        let mut units = u32::MAX;
        assert!(population.run(&mut units));
        let spent = u32::MAX - units;
        assert!(
            spent < 4096,
            "{spent} units for three sub-chunks of one block"
        );
        assert_eq!(population.finish().0, populate(&set_one_by_one));
    }

    /// The checkpoint bytes of `value`.
    pub(super) fn encoded<T: Codec>(value: &T) -> Vec<u8> {
        let mut w = Writer::default();
        value.encode(&mut w);
        w.into_parts().0
    }

    /// The checkpoint form of `value`, read back.
    pub(super) fn read_back<T: Codec>(value: &T) -> checkpoint::Result<T> {
        let bytes = encoded(value);
        let custom = Registry::default();
        let entities = Customs::default();
        T::decode(&mut Reader::new(&bytes, &custom, &entities))
    }

    #[test]
    fn the_light_engine_is_read_back_from_its_checkpoint() {
        let mut w = world(&Flat, 1);
        w.step_light(500);
        assert!(!w.is_light_idle(), "work is waiting");
        let back = read_back(&w.light).unwrap();
        assert_eq!(encoded(&back), encoded(&w.light));
    }

    #[test]
    fn a_checkpoint_the_light_engine_cannot_run_from_is_refused() {
        let tampers: [fn(&mut Engine); 4] = [
            |engine| engine.mode = Mode::Fixed(16),
            |engine| engine.next_task = u64::MAX,
            |engine| {
                let pos = ChunkPos::new(0, 0);
                engine.add_task(pos, Kind::Reset, Task::Reset(pos));
                engine.next_task = 0;
            },
            |engine| {
                let over = Limits::DEFAULT.changes + 1;
                engine
                    .changes
                    .extend((0..over).map(|_| BlockPos::new(0, 0, 0)));
            },
        ];
        for tamper in tampers {
            let mut engine = Engine::new();
            tamper(&mut engine);
            assert!(read_back(&engine).is_err());
        }
    }

    /// Measures light costs.
    ///
    /// Run it with
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
        for (name, chunk, n) in [("flat", &flat, 2000), ("dense random", &dense, 20)] {
            let start = std::time::Instant::now();
            let mut bytes = 0;
            for _ in 0..n {
                bytes = populate(chunk).heap_bytes();
            }
            eprintln!(
                "populate {name}: {:.1} us, light memory {} B heap + {} B",
                us(start.elapsed(), n),
                bytes,
                size_of::<Levels>() + size_of::<Box<Levels>>()
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
            let mut units = 0_u64;
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
        let changes = 4000;
        let mut units = 0_u64;
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
        let mut units = 0_u64;
        for x in -2..=2 {
            let c = w.remove_chunk(ChunkPos::new(x, 0)).unwrap();
            w.insert_chunk(ChunkPos::new(x, 0), c);
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
