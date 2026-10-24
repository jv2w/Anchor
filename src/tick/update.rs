//! Block updates: scheduled updates, neighbour updates and random ticks, and the block rules
//! they run (crops, saplings, leaves, fire, grass, ice, copper, liquids, falling blocks).
//!
//! Each world runs, once a tick and in this order (PMMP `World::actuallyDoTick`):
//!
//! 1. **Scheduled updates** whose tick has come, in (tick, sequence) order, at most
//!    [`MAX_SCHEDULED_PER_TICK`] (PMMP `scheduleDelayedBlockUpdate`, `onScheduledUpdate`). A
//!    position has at most one pending update: scheduling it again later than the pending one
//!    changes nothing, earlier moves it (PMMP compares the delays instead of the due ticks and
//!    keeps both entries, so an earlier request ran twice: fixed). They are saved with their
//!    chunk (chunk extras kind 3) and continue after a restart (PMMP drops them: fixed).
//! 2. **Neighbour updates**: every block change of the world ([`World::next_changed`]: players,
//!    tasks and rules alike) gives the block and its six neighbours one `onNearbyBlockChange`,
//!    through a first-in first-out queue that skips positions already in it. Changes the rules
//!    make join the same queue, so cascades run in the same tick, at most
//!    [`MAX_NEIGHBOUR_UPDATES_PER_TICK`]; the rest waits for the next tick.
//! 3. **Random ticks**: the chunks within `tick-radius` of a player of the world whose view is open
//!    (as for entities) and whose 3 × 3 neighbourhood is loaded and lit, in position order; in each
//!    sub-chunk that is not air, `random-tick-speed` positions from the engine random stream of the
//!    chunk and tick (PMMP uses the global `mt_rand`; dragonfly a per-world generator that depends
//!    on thread timing: both fixed). Blocks named in `disable-block-ticking` never tick. A custom
//!    block ticks if its definition asks for it, through its behaviour's `on_random_tick`.
//!
//! A rule never reads a chunk that is not loaded as air: it waits (PMMP loads it, or reads air
//! and breaks blocks on chunk borders: fixed). Liquids that need such a block try again after
//! their tick rate.
//!
//! [`World::next_changed`]: crate::world::World::next_changed

mod fire;
mod liquid;
mod plant;
mod props;
mod raise;
mod soil;
mod support;
mod tree;

pub(super) use liquid::{LAVA_TICK_RATE, WATER_TICK_RATE};
pub(super) use plant::{MAX_VINE_AGE, nether_vines_age};
pub(super) use soil::CAULDRON_FULL;
pub(super) use support::{attached_support, static_support};

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tracing::warn;

use super::interact::{self, Destroy};
use super::view::{self, Key};
use super::{Context, State, block_entity, dispatch, entity};
use crate::block::{self, Class, Face, Kind};
use crate::event::{Actor, BlockUpdate};
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::{Compound, Value};
use crate::player;
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::storage::codec::MAX_EXTRA_ENTRIES;
use crate::world::{self, BlockPos, ChunkPos, Entry, MAX_Y, MIN_Y, SUB_CHUNKS, World};

/// The most scheduled updates a world runs in one tick; the rest run in the next ticks, in
/// order (PMMP has no limit). Measured (release): a water flow front costs about 4.3 µs an
/// update, so this keeps a world's scheduled updates under about 18 ms a tick.
const MAX_SCHEDULED_PER_TICK: usize = 4_096;
/// The most neighbour updates a world runs in one tick (PMMP has no limit); measured at about
/// 0.25 µs each in a water flow, about 4 ms.
const MAX_NEIGHBOUR_UPDATES_PER_TICK: usize = 16_384;
/// The most positions a world's neighbour queue holds; more changed blocks wait in the world's
/// change feed. A change queues at most seven positions.
const MAX_NEIGHBOUR_QUEUE: usize = 1 << 18;

/// The saved list keys (Bedrock `PendingTicks` entry names): the position and the engine tick
/// the update is due at.
const KEY_X: &str = "x";
const KEY_Y: &str = "y";
const KEY_Z: &str = "z";
const KEY_TIME: &str = "time";

/// Random stream purposes.
const RANDOM_TICK: u64 = random::tag("random-tick");
const RULE: u64 = random::tag("block-rule");

/// The pending block updates of the loaded chunks.
#[derive(Debug, Default)]
pub(super) struct Updates {
    /// Scheduled updates by (world, due tick, sequence).
    queue: BTreeMap<(WorldIndex, u64, u64), BlockPos>,
    /// The scheduled update of each position, by chunk: (due tick, sequence). At most one per
    /// position of a loaded chunk.
    pending: BTreeMap<(WorldIndex, ChunkPos, BlockPos), (u64, u64)>,
    /// The loaded chunks whose saved lists this module holds.
    loaded: BTreeSet<Key>,
    /// Loaded chunks whose scheduled updates changed since [`Self::take_changed`].
    changed: BTreeSet<Key>,
    /// The sequence number of the next scheduled update.
    next_seq: u64,
    /// The neighbour updates waiting in each world, by world index.
    neighbours: Vec<Neighbours>,
    /// Random streams opened so far: each rule run gets a stream of its own.
    draws: u64,
    /// The chunks [`ticking_chunks`] gave the last world, kept so random ticks do not allocate
    /// every tick: at most the players' discs of the largest `tick-radius`. Neither saved nor
    /// hashed.
    ticking: Vec<ChunkPos>,
    /// What the rules asked other modules for, so tests see it whatever those modules do.
    #[cfg(test)]
    effects: Vec<Effect>,
}

/// A world's neighbour update queue (PMMP `neighbourBlockUpdateQueue` and its index).
#[derive(Debug, Default)]
struct Neighbours {
    /// Positions waiting for `onNearbyBlockChange`, oldest first; at most
    /// [`MAX_NEIGHBOUR_QUEUE`].
    fifo: VecDeque<BlockPos>,
    /// The positions in `fifo`.
    queued: BTreeSet<BlockPos>,
}

/// A request a rule made of another module (tests).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Effect {
    /// The block was destroyed with drops ([`interact::destroy`]).
    Destroyed(BlockPos),
    /// The block started falling ([`entity::fall`]).
    Fell(BlockPos, block::State),
    /// An item was dropped.
    Dropped(BlockPos, Stack),
}

/// A chunk's saved scheduled updates read off the tick ([`Parsed::read`]), for
/// [`Updates::load`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Parsed {
    /// The well-formed entries in the chunk, position and due tick, in saved order.
    entries: Vec<(BlockPos, u64)>,
}

impl Parsed {
    /// Reads chunk `key`'s saved list, off the tick as it needs nothing of the tick's state: at
    /// most [`MAX_EXTRA_ENTRIES`] entries; those outside the chunk or the world height are left
    /// out with a warning.
    pub(super) fn read(key: Key, list: &[Compound]) -> Self {
        let (world, chunk) = key;
        let mut entries = Vec::with_capacity(list.len().min(MAX_EXTRA_ENTRIES));
        let mut skipped = 0usize;
        for entry in list.iter().take(MAX_EXTRA_ENTRIES) {
            match parse(entry) {
                Some((pos, due)) if pos.chunk() == chunk => entries.push((pos, due)),
                _ => skipped += 1,
            }
        }
        if skipped > 0 {
            warn!(
                world,
                x = chunk.x,
                z = chunk.z,
                skipped,
                "malformed scheduled block updates skipped"
            );
        }
        Self { entries }
    }
}

impl Updates {
    /// A chunk's saved list arrived ([`mod@super::rules`]), read off the tick: its scheduled
    /// updates are pending again, in their saved order. An entry that repeats a position is left
    /// out; the list counts as unchanged until an update of the chunk changes.
    pub(super) fn load(&mut self, key: Key, parsed: Parsed) {
        self.loaded.insert(key);
        let (world, chunk) = key;
        for (pos, due) in parsed.entries {
            if !self.pending.contains_key(&(world, chunk, pos)) {
                self.insert(world, pos, due);
            }
        }
    }

    /// [`Parsed::read`] and [`Self::load`] in one.
    #[cfg(test)]
    pub(super) fn load_list(&mut self, key: Key, list: &[Compound]) {
        self.load(key, Parsed::read(key, list));
    }

    /// A chunk leaves: drops its pending updates; its list if it changed since
    /// [`Self::take_changed`].
    pub(super) fn unload(&mut self, key: Key) -> Option<Vec<Compound>> {
        if !self.loaded.remove(&key) {
            return None;
        }
        let list = self.changed.remove(&key).then(|| self.list(key));
        let (world, chunk) = key;
        let range = (world, chunk, BlockPos::new(i32::MIN, i32::MIN, i32::MIN))
            ..=(world, chunk, BlockPos::new(i32::MAX, i32::MAX, i32::MAX));
        for ((w, _, _), (due, seq)) in self.pending.extract_if(range, |_, _| true) {
            self.queue.remove(&(w, due, seq));
        }
        list
    }

    /// The lists of the chunks that changed since the last call, for a save snapshot.
    pub(super) fn take_changed(&mut self) -> Vec<(Key, Vec<Compound>)> {
        let changed = std::mem::take(&mut self.changed);
        changed
            .into_iter()
            .filter(|key| self.loaded.contains(key))
            .map(|key| (key, self.list(key)))
            .collect()
    }

    /// The saved list of a loaded chunk: its pending updates in (tick, sequence) order.
    fn list(&self, (world, chunk): Key) -> Vec<Compound> {
        let range = (world, chunk, BlockPos::new(i32::MIN, i32::MIN, i32::MIN))
            ..=(world, chunk, BlockPos::new(i32::MAX, i32::MAX, i32::MAX));
        let mut entries: Vec<((u64, u64), BlockPos)> = self
            .pending
            .range(range)
            .map(|(&(_, _, pos), &order)| (order, pos))
            .collect();
        entries.sort_unstable();
        entries
            .into_iter()
            .map(|((due, _), pos)| {
                let mut c = Compound::new();
                c.insert(KEY_X.to_owned(), Value::Int(pos.x));
                c.insert(KEY_Y.to_owned(), Value::Int(pos.y));
                c.insert(KEY_Z.to_owned(), Value::Int(pos.z));
                c.insert(
                    KEY_TIME.to_owned(),
                    Value::Long(i64::try_from(due).unwrap_or(i64::MAX)),
                );
                c
            })
            .collect()
    }

    /// Adds a pending update the position does not have.
    fn insert(&mut self, world: WorldIndex, pos: BlockPos, due: u64) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.queue.insert((world, due, seq), pos);
        self.pending.insert((world, pos.chunk(), pos), (due, seq));
    }

    /// Schedules an update of `pos` at tick `due` in a loaded chunk (PMMP
    /// `scheduleDelayedBlockUpdate`, with due ticks compared). False if nothing changed.
    fn schedule_at(&mut self, world: WorldIndex, pos: BlockPos, due: u64) -> bool {
        let key = (world, pos.chunk());
        if !self.loaded.contains(&key) || !(MIN_Y..=MAX_Y).contains(&pos.y) {
            return false;
        }
        if let Some(&(old, seq)) = self.pending.get(&(world, key.1, pos)) {
            if old <= due {
                return false;
            }
            self.queue.remove(&(world, old, seq));
        }
        self.insert(world, pos, due);
        self.changed.insert(key);
        true
    }

    /// Takes the next update of `world` due by `tick`.
    fn pop_due(&mut self, world: WorldIndex, tick: u64) -> Option<(BlockPos, u64)> {
        let ((_, _, seq), pos) = self
            .queue
            .extract_if((world, 0, 0)..=(world, tick, u64::MAX), |_, _| true)
            .next()?;
        self.pending.remove(&(world, pos.chunk(), pos));
        self.changed.insert((world, pos.chunk()));
        Some((pos, seq))
    }

    /// The neighbour queue of `world`.
    fn neighbours_mut(&mut self, world: WorldIndex) -> &mut Neighbours {
        if self.neighbours.len() <= world {
            self.neighbours
                .resize_with(world.saturating_add(1), Neighbours::default);
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the vector was just grown past `world`"
        )]
        &mut self.neighbours[world]
    }

    /// The number of scheduled updates and queued neighbour updates (leak tests).
    #[cfg(test)]
    pub(super) fn len(&self) -> (usize, usize, usize) {
        (
            self.queue.len(),
            self.pending.len(),
            self.neighbours
                .iter()
                .map(|n| n.fifo.len() + n.queued.len())
                .sum(),
        )
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            queue,
            pending: _,
            loaded,
            changed,
            next_seq,
            neighbours,
            draws,
            ticking: _,
            // A test-only record of what the rules asked other modules for: what those requests
            // did (blocks destroyed, falling blocks, dropped items) is in the hashed state of the
            // world and the other modules.
            effects: _,
        } = self;
        // `pending` is `queue` by position.
        h.usize(queue.len());
        for (&(w, due, seq), pos) in queue {
            h.usize(w);
            h.u64(due);
            h.u64(seq);
            hash_pos(h, *pos);
        }
        for keys in [loaded, changed] {
            h.usize(keys.len());
            for &(w, c) in keys {
                h.usize(w);
                h.u64(u64::from(c.x.cast_unsigned()));
                h.u64(u64::from(c.z.cast_unsigned()));
            }
        }
        h.u64(*next_seq);
        h.u64(*draws);
        h.usize(neighbours.len());
        for n in neighbours {
            let Neighbours { fifo, queued: _ } = n;
            h.usize(fifo.len());
            for &pos in fifo {
                hash_pos(h, pos);
            }
        }
    }
}

#[cfg(test)]
fn hash_pos(h: &mut super::Fnv, pos: BlockPos) {
    for c in [pos.x, pos.y, pos.z] {
        h.u64(u64::from(c.cast_unsigned()));
    }
}

super::checkpoint::fields!(Neighbours { fifo, queued });

/// Every field but the reused chunk list and the test-only record of what the rules asked other
/// modules for.
impl super::checkpoint::Codec for Updates {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            queue,
            pending,
            loaded,
            changed,
            next_seq,
            neighbours,
            draws,
            ticking: _,
            #[cfg(test)]
                effects: _,
        } = self;
        queue.encode(w);
        pending.encode(w);
        loaded.encode(w);
        changed.encode(w);
        next_seq.encode(w);
        neighbours.encode(w);
        draws.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        Ok(Self {
            queue: Codec::decode(r)?,
            pending: Codec::decode(r)?,
            loaded: Codec::decode(r)?,
            changed: Codec::decode(r)?,
            next_seq: Codec::decode(r)?,
            neighbours: Codec::decode(r)?,
            draws: Codec::decode(r)?,
            ticking: Vec::new(),
            #[cfg(test)]
            effects: Vec::new(),
        })
    }
}

/// A saved entry: its position and due tick, if well formed.
fn parse(entry: &Compound) -> Option<(BlockPos, u64)> {
    #[cfg(test)]
    super::rules::reads::add(|r| r.updates += 1);
    let int = |key: &str| match entry.get(key)? {
        Value::Int(v) => Some(*v),
        _ => None,
    };
    let pos = BlockPos::new(int(KEY_X)?, int(KEY_Y)?, int(KEY_Z)?);
    let due = match entry.get(KEY_TIME)? {
        Value::Long(v) => u64::try_from(*v).ok()?,
        _ => return None,
    };
    (MIN_Y..=MAX_Y).contains(&pos.y).then_some((pos, due))
}

/// The block kinds `disable-block-ticking` names: every kind with a state of one of the Bedrock
/// block names (PMMP parses item names to block types; unknown names are left out as PMMP does).
pub(super) fn untickable(names: &[String]) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = block::State::all()
        .filter(|s| names.iter().any(|n| n == s.name()))
        .map(block::State::kind)
        .collect();
    kinds.sort_unstable_by_key(|k| *k as usize);
    kinds.dedup();
    kinds
}

/// Schedules an update of the block at `pos` in `delay` ticks (PMMP
/// `World::scheduleDelayedBlockUpdate`). A position outside the world height or in a chunk that
/// is not loaded is not scheduled.
pub(super) fn schedule(state: &mut State, world: WorldIndex, pos: BlockPos, delay: u32) {
    let due = state.tick.saturating_add(u64::from(delay));
    state.rules.updates.schedule_at(world, pos, due);
}

impl Context<'_> {
    /// Schedules an update of the block at `pos` in `world` in `delay_ticks` ticks (PMMP
    /// `World::scheduleDelayedBlockUpdate`): the block then reacts as to its own scheduled
    /// updates (a liquid flows, a pressed button pops out, fire burns; a custom block does
    /// nothing). It runs in the block update pass of that tick, or of the next tick if this
    /// tick's pass is over. A position has at most one pending update: an earlier request moves
    /// it, a later one changes nothing. Pending updates are saved with their chunk.
    ///
    /// False, changing nothing, if the server has no world `world`, `pos` is outside the world
    /// height or in a chunk that is not loaded, or the position has an update due no later.
    #[doc(alias = "World::scheduleDelayedBlockUpdate")]
    pub fn schedule_block_update(
        &mut self,
        world: world::Id,
        pos: BlockPos,
        delay_ticks: u32,
    ) -> bool {
        let Some(world) = super::world_api::index(self.state, world) else {
            return false;
        };
        let due = self.state.tick.saturating_add(u64::from(delay_ticks));
        self.state.rules.updates.schedule_at(world, pos, due)
    }
}

/// Bone meal used on the block at `pos` (PMMP `onInteract` with `Fertilizer`: crops, stems,
/// saplings, grass, sweet berries, bamboo, cocoa…) by `player`: grows it and returns true if the
/// bone meal is used up. The player is taken to use finite resources (PMMP's chances for saplings
/// of azalea and fungi), and is named in the growth events.
pub(super) fn fertilize(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    player: Option<player::Id>,
) -> bool {
    let mut cx = Cx::new(state, world, pos);
    cx.player = player;
    let Some(block) = cx.get(pos) else {
        return false;
    };
    match block.class() {
        Class::Wheat
        | Class::Carrot
        | Class::Potato
        | Class::Beetroot
        | Class::MelonStem
        | Class::PumpkinStem
        | Class::Sugarcane
        | Class::CocoaBlock
        | Class::SweetBerryBush
        | Class::Bamboo
        | Class::BambooSapling
        | Class::NetherVines
        | Class::CaveVines
        | Class::PitcherCrop
        | Class::DoublePitcherCrop
        | Class::TorchflowerCrop
        | Class::GlowLichen
        | Class::PinkPetals
        | Class::SmallDripleaf
        | Class::BigDripleafHead
        | Class::BigDripleafStem
        | Class::TallGrass
        | Class::Sapling
        | Class::Azalea
        | Class::NetherFungus => plant::fertilize(&mut cx, pos, block),
        Class::Grass | Class::Dirt | Class::Netherrack | Class::Nylium => {
            soil::fertilize(&mut cx, pos, block)
        }
        _ => false,
    }
}

/// A living entity (a player or a mob) landed on the block at `pos` after falling
/// `fall_distance` blocks (PMMP `Farmland::onEntityLand`): farmland may be trampled to dirt,
/// raising [`EntityTrampleFarmland`](crate::event::EntityTrampleFarmland).
pub(super) fn trample(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    fall_distance: f64,
    entity: Actor,
) {
    soil::trample(&mut Cx::new(state, world, pos), pos, fall_distance, entity);
}

/// One tick of block updates: due scheduled updates, neighbour updates of the blocks changed
/// since the last tick, then random ticks.
///
/// Everything runs on the tick thread, in the serial step, so the rules raise their events where
/// they change blocks: random ticks of 20 players at `tick-radius` 4 (318 chunks of a flat world)
/// cost about 76 µs a tick (release), less than handing the candidate positions to the CPU pool
/// would save, and rules change the blocks later candidates read.
pub(super) fn tick(state: &mut State) {
    // The players' chunks, gathered in one pass for every world rather than one pass per world,
    // and only when random ticks run. No update moves a player or changes its world.
    let centers = if state.settings.random_tick_speed == 0 {
        BTreeMap::new()
    } else {
        view::tick_centres(state)
    };
    for world in 0..state.worlds.len() {
        if !state.is_awake(world) {
            continue;
        }
        run_scheduled(state, world);
        run_neighbours(state, world);
        run_random(state, world, centers.get(&world));
    }
}

/// Runs the due scheduled updates of `world`.
fn run_scheduled(state: &mut State, world: WorldIndex) {
    for _ in 0..MAX_SCHEDULED_PER_TICK {
        let Some((pos, seq)) = state.rules.updates.pop_due(world, state.tick) else {
            break;
        };
        let mut cx = Cx::new(state, world, pos);
        cx.seq = seq;
        if let Some(block) = cx.get(pos) {
            on_scheduled(&mut cx, pos, block);
        }
    }
}

/// Runs the neighbour updates of `world`: the changed blocks queue themselves and their
/// neighbours, and the queue runs in order.
fn run_neighbours(state: &mut State, world: WorldIndex) {
    for n in 0..MAX_NEIGHBOUR_UPDATES_PER_TICK {
        refill(state, world);
        let queue = state.rules.updates.neighbours_mut(world);
        let Some(pos) = queue.fifo.pop_front() else {
            break;
        };
        queue.queued.remove(&pos);
        // PMMP `BlockUpdateEvent`: cancelling skips the reaction of the block and of the
        // entities in it.
        let mut update = None;
        if dispatch::heard::<BlockUpdate>(state) {
            let mut cx = Cx::new(state, world, pos);
            if let Some(block) = cx.get(pos) {
                let asked = cx.ask(
                    [pos],
                    |cx| BlockUpdate {
                        world: cx.world_id(),
                        pos,
                        block,
                        cancelled: false,
                    },
                    BlockUpdate::is_cancelled,
                );
                let Ok(event) = asked else {
                    continue;
                };
                update = event;
            }
        }
        // Entities resting on or in the block see the change too (PMMP
        // `Entity::onNearbyBlockChange` through the world's neighbour update).
        super::entity::block_changed(state, world, pos);
        let mut cx = Cx::new(state, world, pos);
        cx.seq = n as u64;
        if let Some(block) = cx.get(pos) {
            on_nearby(&mut cx, pos, block);
        }
        cx.tell(update);
    }
}

/// Moves changed blocks of `world` from its change feed into its neighbour queue while there
/// is room (PMMP `internalNotifyNeighbourBlockUpdate`: the block, then its six neighbours). A
/// changed block with a block entity first checks that the entity still belongs to it
/// ([`block_entity::block_changed`]), before any neighbour update runs.
fn refill(state: &mut State, world: WorldIndex) {
    let State { worlds, rules, .. } = state;
    let Some(w) = worlds.get_mut(world) else {
        return;
    };
    let queue = rules.updates.neighbours_mut(world);
    // Allocates only when a changed block has a block entity.
    let mut with_entity = Vec::new();
    while queue.fifo.len() + 7 <= MAX_NEIGHBOUR_QUEUE {
        let Some(pos) = w.next_changed() else {
            break;
        };
        if rules.block_entities.get(world, pos).is_some() {
            with_entity.push(pos);
        }
        let around = std::iter::once(Some(pos)).chain(Face::ALL.map(|f| pos.step(f)));
        for p in around.flatten() {
            if (MIN_Y..=MAX_Y).contains(&p.y) && queue.queued.insert(p) {
                queue.fifo.push_back(p);
            }
        }
    }
    for pos in with_entity {
        block_entity::block_changed(state, world, pos);
    }
}

/// Random ticks of `world` (PMMP `World::tickChunks`) around `centers`, the chunks its players
/// stand in.
fn run_random(state: &mut State, world: WorldIndex, centers: Option<&BTreeSet<ChunkPos>>) {
    let speed = state.settings.random_tick_speed;
    let radius = state.settings.view(world).tick_radius;
    if speed == 0 || radius == 0 {
        return;
    }
    let Some(world_seed) = state.worlds.get(world).map(World::seed) else {
        return;
    };
    let mut chunks = std::mem::take(&mut state.rules.updates.ticking);
    ticking_chunks(state, world, radius, centers, &mut chunks);
    for &chunk in &chunks {
        let k = random::key(
            RANDOM_TICK,
            &[
                world as u64,
                u64::from(chunk.x.cast_unsigned()),
                u64::from(chunk.z.cast_unsigned()),
            ],
        );
        let mut rng = Random::new(world_seed, state.tick, k);
        for index in 0..SUB_CHUNKS {
            let filled = state
                .worlds
                .get(world)
                .and_then(|w| w.chunk(chunk))
                .is_some_and(|c| c.sub_chunk(index).is_some());
            if !filled {
                continue;
            }
            let base_y = MIN_Y + i32::try_from(index).unwrap_or(0) * 16;
            let mut bits = 0;
            for i in 0..speed {
                if i % 5 == 0 {
                    bits = rng.next_u64();
                }
                let pos = random_position(chunk, base_y, bits);
                bits >>= 12;
                let mut cx = Cx::new(state, world, pos);
                if let Some(block) = cx.get(pos)
                    && cx.ticks_randomly(block)
                {
                    on_random(&mut cx, pos, block);
                }
            }
        }
    }
    state.rules.updates.ticking = chunks;
}

/// The position 12 random bits pick in a sub-chunk: x, then y, then z, 4 bits each (PMMP).
fn random_position(chunk: ChunkPos, base_y: i32, bits: u64) -> BlockPos {
    let at = |shift: u32| i32::try_from((bits >> shift) & 15).unwrap_or(0);
    BlockPos::new(
        chunk.x.wrapping_mul(16).wrapping_add(at(0)),
        base_y + at(4),
        chunk.z.wrapping_mul(16).wrapping_add(at(8)),
    )
}

/// Fills `chunks` with the chunks of `world` that take random ticks, in order: within `radius`
/// of `centers` ([`view::Disc::around`]), the chunks of the players in the world whose view is
/// open ([`view::tick_centres`]), with their 3 × 3 neighbourhood loaded and lit. Players in one
/// chunk share a center.
fn ticking_chunks(
    state: &State,
    world: WorldIndex,
    radius: u8,
    centers: Option<&BTreeSet<ChunkPos>>,
    chunks: &mut Vec<ChunkPos>,
) {
    chunks.clear();
    let (Some(w), Some(centers)) = (state.worlds.get(world), centers) else {
        return;
    };
    for &c in centers {
        chunks.extend(state.chunks.disc.around(c, radius));
    }
    chunks.sort_unstable();
    chunks.dedup();
    // A chunk that is not loaded is not lit.
    chunks.retain(|c| {
        (-1..=1).all(|dx| {
            (-1..=1)
                .all(|dz| w.is_chunk_lit(ChunkPos::new(c.x.wrapping_add(dx), c.z.wrapping_add(dz))))
        })
    });
}

/// A scheduled update reached the block (PMMP `onScheduledUpdate`).
fn on_scheduled(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    match block.class() {
        Class::Water | Class::Lava => liquid::on_scheduled(cx, pos, block),
        Class::Fire => fire::on_random(cx, pos, block),
        Class::FrostedIce => soil::frosted_ice(cx, pos, block),
        Class::Coral | Class::FloorCoralFan | Class::WallCoralFan | Class::CoralBlock => {
            soil::coral_check(cx, pos, block);
        }
        Class::StoneButton | Class::WoodenButton => support::release_button(cx, pos, block),
        Class::BigDripleafHead => plant::dripleaf_tilt(cx, pos, block),
        _ => {}
    }
}

/// A block at or next to `pos` changed (PMMP `onNearbyBlockChange`).
fn on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    match block.class() {
        Class::Air => {}
        Class::Water | Class::Lava => liquid::on_nearby(cx, pos, block),
        Class::Fire => fire::on_nearby(cx, pos, block),
        Class::SoulFire => fire::soul_on_nearby(cx, pos),
        Class::Leaves => soil::leaves_on_nearby(cx, pos, block),
        Class::Farmland | Class::GrassPath => soil::trample_check(cx, pos),
        Class::FrostedIce => cx.schedule_random(pos, 20, 40),
        Class::Coral | Class::FloorCoralFan | Class::WallCoralFan | Class::CoralBlock => {
            soil::coral_on_nearby(cx, pos, block);
        }
        Class::ConcretePowder => soil::concrete_powder(cx, pos, block),
        Class::RedstoneOre => soil::redstone_ore_on_nearby(cx, pos, block),
        Class::Cauldron | Class::WaterCauldron | Class::PotionCauldron => {
            soil::cauldron_on_nearby(cx, pos, block);
        }
        Class::Campfire | Class::SoulCampfire => soil::campfire_on_nearby(cx, pos, block),
        Class::Bed => support::bed_on_nearby(cx, pos, block),
        Class::MelonStem | Class::PumpkinStem => plant::stem_on_nearby(cx, pos, block),
        Class::Sugarcane => plant::sugarcane_on_nearby(cx, pos, block),
        _ => support::on_nearby(cx, pos, block),
    }
}

/// A random tick reached the block (PMMP `onRandomTick`); a custom block's goes to its
/// behaviour.
fn on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if block.custom_block().is_some() {
        interact::random_tick(cx.state, cx.world, pos, block);
        return;
    }
    match block.class() {
        Class::Fire => fire::on_random(cx, pos, block),
        Class::Grass => soil::grass(cx, pos),
        Class::Mycelium => soil::mycelium(cx, pos),
        Class::Nylium => soil::nylium(cx, pos, block),
        Class::Ice => soil::ice(cx, pos),
        Class::FrostedIce => soil::frosted_ice(cx, pos, block),
        Class::SnowLayer => soil::snow_layer(cx, pos),
        Class::Farmland => soil::farmland(cx, pos, block),
        Class::Leaves => soil::leaves_on_random(cx, pos, block),
        Class::RedstoneOre => soil::redstone_ore_on_random(cx, pos, block),
        Class::BuddingAmethyst => soil::budding_amethyst(cx, pos),
        _ if soil::is_oxidizable(block) => soil::oxidize(cx, pos, block),
        _ => plant::on_random(cx, pos, block),
    }
}

/// What a rule sees of the server: the world it runs in, with the reads, writes and requests
/// to other modules the rules need.
pub(super) struct Cx<'a> {
    state: &'a mut State,
    world: WorldIndex,
    /// The position the rule runs for (its random stream).
    at: BlockPos,
    /// A number that tells apart two runs at the same position in one tick.
    seq: u64,
    /// The player whose bone meal runs the rule, named in its growth events.
    player: Option<player::Id>,
}

impl<'a> Cx<'a> {
    fn new(state: &'a mut State, world: WorldIndex, at: BlockPos) -> Self {
        Self {
            state,
            world,
            at,
            seq: 0,
            player: None,
        }
    }

    #[inline]
    fn w(&self) -> Option<&World> {
        self.state.worlds.get(self.world)
    }

    /// The block at `pos`, or `None` if its chunk is not loaded or `pos` is outside the world
    /// height. A state Anchor does not know reads as bedrock: solid, full and unchanged.
    #[inline]
    pub(super) fn get(&self, pos: BlockPos) -> Option<block::State> {
        match self.w()?.entry(pos)? {
            Entry::Known(s) => Some(s),
            Entry::Unknown(_) | Entry::Inert(..) => Some(block::BEDROCK),
        }
    }

    /// The block at `pos`, air outside the world height (PMMP `getBlockAt`), or `None` if its
    /// chunk is not loaded: for rules that read the blocks around a block, which may stand at the
    /// world's top or bottom.
    #[inline]
    pub(super) fn loaded_block(&self, pos: BlockPos) -> Option<block::State> {
        if (MIN_Y..=MAX_Y).contains(&pos.y) {
            self.get(pos)
        } else {
            Some(block::AIR)
        }
    }

    /// The block at `pos`, air where [`Self::get`] has none (PMMP `getBlockAt`, which reads air
    /// outside the world): for rules that only look for a kind of block.
    #[inline]
    pub(super) fn block(&self, pos: Option<BlockPos>) -> block::State {
        pos.and_then(|p| self.get(p)).unwrap_or(block::AIR)
    }

    /// Whether `pos` is in the world height and its chunk is loaded.
    #[inline]
    pub(super) fn is_loaded(&self, pos: BlockPos) -> bool {
        self.get(pos).is_some()
    }

    /// Sets a block (PMMP `setBlock`): neighbours are updated. False if it could not be set.
    pub(super) fn set(&mut self, pos: BlockPos, s: block::State) -> bool {
        self.state
            .worlds
            .get_mut(self.world)
            .is_some_and(|w| w.set_block(pos, s).is_ok())
    }

    /// Sets a block without neighbour updates (PMMP `setBlock` with `update` false).
    pub(super) fn set_quietly(&mut self, pos: BlockPos, s: block::State) -> bool {
        self.state
            .worlds
            .get_mut(self.world)
            .is_some_and(|w| w.set_block_quietly(pos, s))
    }

    /// Destroys the block with drops, as broken by hand (PMMP `useBreakOn` without a player).
    pub(super) fn destroy(&mut self, pos: BlockPos) {
        #[cfg(test)]
        self.state
            .rules
            .updates
            .effects
            .push(Effect::Destroyed(pos));
        interact::destroy(self.state, self.world, pos, Destroy::Rules);
    }

    /// Turns the block into a falling block entity and removes it (PMMP `FallableTrait`); the
    /// block stays when the entity cannot be made.
    pub(super) fn fall(&mut self, pos: BlockPos, s: block::State) {
        #[cfg(test)]
        self.state.rules.updates.effects.push(Effect::Fell(pos, s));
        let _fell = entity::fall(self.state, self.world, pos, s);
    }

    /// Drops an item at the block's corner plus `offset` (PMMP `dropItem`).
    pub(super) fn drop_item(&mut self, pos: BlockPos, stack: Stack) {
        #[cfg(test)]
        self.state
            .rules
            .updates
            .effects
            .push(Effect::Dropped(pos, stack.clone()));
        let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        entity::drop_item(self.state, self.world, at, stack);
    }

    /// Schedules an update of `pos` in `delay` ticks.
    pub(super) fn schedule(&mut self, pos: BlockPos, delay: u32) {
        schedule(self.state, self.world, pos, delay);
    }

    /// Schedules an update of `pos` in `lo..=hi` ticks (PMMP `mt_rand(lo, hi)`).
    pub(super) fn schedule_random(&mut self, pos: BlockPos, lo: u32, hi: u32) {
        let mut rng = self.rng();
        let delay = rng.range(
            i32::try_from(lo).unwrap_or(0),
            i32::try_from(hi).unwrap_or(0),
        );
        self.schedule(pos, u32::try_from(delay).unwrap_or(lo));
    }

    /// A random stream of its own for this rule run (PMMP `mt_rand`).
    pub(super) fn rng(&mut self) -> Random {
        let updates = &mut self.state.rules.updates;
        let draw = updates.draws;
        updates.draws = updates.draws.wrapping_add(1);
        let seed = self.w().map_or(0, World::seed);
        let p = self.at;
        let k = random::key(
            RULE,
            &[
                self.world as u64,
                u64::from(p.x.cast_unsigned()),
                u64::from(p.y.cast_unsigned()),
                u64::from(p.z.cast_unsigned()),
                self.seq,
                draw,
            ],
        );
        Random::new(seed, self.state.tick, k)
    }

    /// The block light at `pos` (0 where not lit yet).
    pub(super) fn block_light(&self, pos: BlockPos) -> u8 {
        self.w().and_then(|w| w.block_light(pos)).unwrap_or(0)
    }

    /// The brightest light at `pos` now, at the world's time of day (PMMP `getFullLightAt`; 0
    /// where not lit yet).
    pub(super) fn full_light(&self, pos: BlockPos) -> u8 {
        self.w()
            .and_then(|w| w.full_light(pos, w.time_ticks()))
            .unwrap_or(0)
    }

    /// The brightest light at `pos` whatever the time of day (PMMP `getPotentialLightAt`).
    pub(super) fn potential_light(&self, pos: BlockPos) -> u8 {
        let Some(w) = self.w() else {
            return 0;
        };
        let sky = w.sky_light(pos).unwrap_or(0);
        sky.max(w.block_light(pos).unwrap_or(0))
    }

    /// The difficulty as PMMP numbers it (peaceful 0 to hard 3).
    pub(super) fn difficulty(&self) -> u8 {
        super::world_api::difficulty(self.state, self.world).id()
    }

    /// Whether the block takes random ticks here: PMMP's `ticksRandomly`, copper that can
    /// oxidize (dragonfly), and not disabled by `disable-block-ticking`; a custom block if its
    /// definition asks for them ([`crate::custom::Block::random_ticks`]).
    fn ticks_randomly(&self, block: block::State) -> bool {
        if block.custom_block().is_some() {
            return block.ticks_randomly();
        }
        (block.ticks_randomly() || soil::is_oxidizable(block))
            && self
                .state
                .settings
                .no_random_ticks
                .binary_search_by_key(&(block.kind() as usize), |k| *k as usize)
                .is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{answer, join, moved_to, new_state, radius, settings};
    use super::super::{Output, Phase, boundary, deliver, step};
    use super::*;
    use crate::block::PropertyValue;
    use crate::event::{BlockGrow, BlockSpread, FarmlandHydrationChange, Listeners, Priority};
    use crate::pool::Pool;
    use crate::tick::Context;
    use crate::tick::InputLog;
    use crate::tick::queue::Input;

    /// A test server with light off (every chunk lit, so random ticks need no light work) and
    /// the spawn area loaded, its chunks held by this module.
    fn server() -> State {
        new_state(
            settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
            Listeners::default(),
        )
    }

    fn pool() -> Pool {
        crate::runtime::cpu_pool(1).unwrap()
    }

    /// One tick of block updates and nothing else.
    fn advance(state: &mut State) {
        tick(state);
        state.tick += 1;
    }

    /// A whole tick with `inputs`, as the tick thread runs it, the stand-in save thread
    /// answering.
    fn full_tick(state: &mut State, inputs: Vec<Input>, pool: &Pool, output: &mut Output) {
        let mut batch = inputs;
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut output.answers);
        step(state, &mut batch, &mut answers, pool, output, &mut log);
        deliver(state, output, &mut log);
        boundary(state, output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        output.answers = answer(state, requests);
    }

    fn set(state: &mut State, pos: BlockPos, s: block::State) {
        state.worlds[0].set_block(pos, s).unwrap();
    }

    fn get(state: &State, pos: BlockPos) -> block::State {
        state.worlds[0].block(pos).unwrap()
    }

    fn named(name: &str) -> block::State {
        block::State::all().find(|s| s.name() == name).unwrap()
    }

    fn find(name: &str, props: &[(&str, PropertyValue<'_>)]) -> block::State {
        block::State::find(name, props).unwrap()
    }

    /// The flat world's block at `y`.
    fn ground(y: i32) -> block::State {
        match y {
            -64 => block::BEDROCK,
            -63 | -62 => block::DIRT,
            -61 => block::GRASS_BLOCK,
            _ => block::AIR,
        }
    }

    /// A state as the golden script writes it: name, then `key=value` sorted.
    fn render(s: block::State) -> String {
        let mut properties: Vec<String> = s
            .properties()
            .map(|p| {
                let value = match p.value() {
                    PropertyValue::Byte(b) => b.to_string(),
                    PropertyValue::Int(i) => i.to_string(),
                    PropertyValue::String(v) => v.to_owned(),
                };
                format!("{}={value}", p.name())
            })
            .collect();
        properties.sort();
        format!("{}[{}]", s.name(), properties.join(","))
    }

    /// Every block of the box that is not the ground or air, as the golden script lists them.
    fn dump(state: &State, scene: &str, [x0, x1, y0, y1, z0, z1]: [i32; 6]) -> Vec<String> {
        let mut out = Vec::new();
        for y in y0..=y1 {
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let s = get(state, BlockPos::new(x, y, z));
                    if s != ground(y) && s != block::AIR {
                        out.push(format!("{scene} {x} {y} {z} {}", render(s)));
                    }
                }
            }
        }
        out
    }

    /// The golden lines of a scene (`xtask/data/pmmp/liquid.txt`, from PMMP's own classes).
    fn golden(scene: &str) -> Vec<String> {
        include_str!("../../xtask/data/pmmp/liquid.txt")
            .lines()
            .filter(|l| l.split(' ').next() == Some(scene))
            .map(str::to_owned)
            .collect()
    }

    type Actions = Vec<(u64, BlockPos, block::State)>;

    /// Runs a scene: `actions` set blocks at the start of their tick.
    fn run_scene(actions: &Actions, ticks: u64, mut hook: impl FnMut(&mut State, u64)) -> State {
        let mut state = server();
        for t in 0..ticks {
            for &(at, pos, s) in actions {
                if at == t {
                    set(&mut state, pos, s);
                }
            }
            hook(&mut state, t);
            advance(&mut state);
        }
        state
    }

    fn water() -> block::State {
        Kind::Water.default_state()
    }

    /// The golden scenes of `xtask/php/liquid.php`.
    fn scenes() -> Vec<(&'static str, Actions, u64, [i32; 6])> {
        let p = BlockPos::new;
        let stone = Kind::Stone.default_state();
        let mut platform: Actions = Vec::new();
        for x in 6..=10 {
            for z in 6..=10 {
                platform.push((0, p(x, -56, z), stone));
            }
        }
        platform.push((0, p(8, -55, 8), water()));
        let mut wall: Actions = vec![(0, p(8, -60, 8), water())];
        for z in 4..=12 {
            wall.push((0, p(11, -60, z), stone));
        }
        wall.push((40, p(11, -60, 8), block::AIR));
        vec![
            (
                "source",
                vec![(0, p(8, -60, 8), water())],
                120,
                [-2, 18, -61, -55, -2, 18],
            ),
            ("waterfall", platform, 200, [-2, 18, -61, -50, -2, 18]),
            (
                "lava_water",
                vec![
                    (0, p(4, -60, 8), water()),
                    (0, p(12, -60, 8), Kind::Lava.default_state()),
                ],
                600,
                [-6, 22, -61, -55, -2, 18],
            ),
            (
                "dry_up",
                vec![(0, p(8, -60, 8), water()), (100, p(8, -60, 8), block::AIR)],
                300,
                [-2, 18, -61, -55, -2, 18],
            ),
            ("wall_opened", wall, 160, [-2, 18, -61, -55, -2, 18]),
        ]
    }

    #[test]
    fn liquid_scenes_match_pmmp() {
        for (name, actions, ticks, b) in scenes() {
            let state = run_scene(&actions, ticks, |_, _| {});
            assert_eq!(dump(&state, name, b), golden(name), "scene {name}");
            let (queue, pending, neighbours) = state.rules.updates.len();
            assert_eq!(queue, pending);
            assert_eq!(neighbours, 0, "scene {name}: the neighbour queue drained");
        }
    }

    // pmmp: World::scheduleDelayedBlockUpdate
    #[test]
    fn the_api_schedules_one_update_per_position_and_it_runs_when_due() {
        let mut state = server();
        let pos = BlockPos::new(2, -60, 2);
        let pressed = find(
            "minecraft:stone_button",
            &[
                ("button_pressed_bit", PropertyValue::Byte(1)),
                ("facing_direction", PropertyValue::Int(1)),
            ],
        );
        assert!(state.worlds[0].set_block_quietly(pos, pressed));
        let mut cx = Context { state: &mut state };
        let id = world::Id::DEFAULT;
        assert!(cx.schedule_block_update(id, pos, 5));
        assert!(
            !cx.schedule_block_update(id, pos, 9),
            "a later request changes nothing"
        );
        assert!(
            cx.schedule_block_update(id, pos, 3),
            "an earlier one moves it"
        );
        assert!(!cx.schedule_block_update(id, BlockPos::new(2, MAX_Y + 1, 2), 1));
        assert!(
            !cx.schedule_block_update(id, BlockPos::new(100_000, -60, 0), 1),
            "not loaded"
        );
        assert!(!cx.schedule_block_update(world::Id::new(7).unwrap(), pos, 1));
        let (queue, pending, _) = state.rules.updates.len();
        assert_eq!((queue, pending), (1, 1));
        for _ in 0..3 {
            advance(&mut state);
        }
        assert_eq!(get(&state, pos), pressed, "not due yet");
        advance(&mut state);
        let released = props::with_flag(pressed, "button_pressed_bit", false).unwrap();
        assert_eq!(get(&state, pos), released);
        assert_eq!(state.rules.updates.len().0, 0);
    }

    /// Lights the block on a random tick.
    struct Glow;

    impl crate::custom::Behaviour for Glow {
        fn on_random_tick(
            &self,
            ctx: &mut Context<'_>,
            world: world::Id,
            pos: BlockPos,
            state: block::State,
        ) {
            let lit = state.with("lit", PropertyValue::Byte(1)).unwrap();
            ctx.world_mut(world).unwrap().set_block(pos, lit).unwrap();
        }
    }

    static TICKING: crate::custom::Block = crate::custom::Block::new("test:ticking", "t")
        .with_properties(&[crate::custom::Property::bool("lit")])
        .with_behaviour(&Glow)
        .random_ticks();
    static STILL: crate::custom::Block = crate::custom::Block::new("test:still", "s")
        .with_properties(&[crate::custom::Property::bool("lit")])
        .with_behaviour(&Glow);

    /// A floor of custom blocks around a player, every other one taking random ticks: those get
    /// them through their behaviour, the same way in a second run; the others never do.
    #[test]
    fn custom_blocks_that_ask_for_random_ticks_get_them_deterministically() {
        let run = || {
            let pool = pool();
            let mut registry = crate::custom::Registry::default();
            registry.add_block(&TICKING);
            registry.add_block(&STILL);
            registry.finish().unwrap();
            let registry = std::sync::Arc::new(registry);
            let mut state = server();
            state.custom = std::sync::Arc::clone(&registry);
            for w in &mut state.worlds {
                w.set_custom(std::sync::Arc::clone(&registry));
            }
            let ticking = registry.block_state(&TICKING).unwrap();
            let still = registry.block_state(&STILL).unwrap();
            assert!(ticking.ticks_randomly() && !still.ticks_randomly());
            for x in -16..16 {
                for z in -16..16 {
                    let s = if (x + z) % 2 == 0 { ticking } else { still };
                    set(&mut state, BlockPos::new(x, -60, z), s);
                }
            }
            let mut output = Output::default();
            let (j, rx) = join(1, 0);
            full_tick(&mut state, vec![j, radius(1, 1, 4)], &pool, &mut output);
            let mut hashes = Vec::new();
            for _ in 0..200 {
                full_tick(&mut state, Vec::new(), &pool, &mut output);
                hashes.push(state.hash());
            }
            let mut lit = [0, 0];
            for x in -16..16 {
                for z in -16..16 {
                    let s = get(&state, BlockPos::new(x, -60, z));
                    let def = s.custom_block().unwrap().identifier();
                    if s.value("lit") == Some(PropertyValue::Byte(1)) {
                        lit[usize::from(def == "test:still")] += 1;
                    }
                }
            }
            drop(rx);
            assert!(pool.shutdown());
            (hashes, lit)
        };
        let (first, [ticked, still]) = run();
        assert!(ticked > 0, "no random tick reached a custom block");
        assert_eq!(still, 0, "a block that did not ask got random ticks");
        let (second, lit) = run();
        assert_eq!(first, second);
        assert_eq!(lit, [ticked, 0]);
    }

    #[test]
    fn scheduled_updates_survive_unload_save_and_reload() {
        let (name, actions, ticks, b) = scenes().swap_remove(0);
        let key = (0, ChunkPos::new(0, 0));
        let mut saved = 0;
        // Mid-flow, the chunk's pending updates go to their saved list and come back, as at an
        // unload and a reload from the file, twice; the flow ends as without it.
        let state = run_scene(&actions, ticks, |state, t| {
            if t == 7 || t == 23 {
                let updates = &mut state.rules.updates;
                let list = updates.unload(key).expect("the chunk's updates changed");
                assert!(!list.is_empty());
                saved += list.len();
                assert!(updates.pending.keys().all(|(_, c, _)| *c != key.1));
                updates.load_list(key, &list);
                assert!(updates.take_changed().iter().all(|(k, _)| *k != key));
            }
        });
        assert!(saved > 0);
        assert_eq!(dump(&state, name, b), golden(name));
    }

    #[test]
    fn saved_lists_round_trip_in_order_and_skip_malformed_entries() {
        let mut updates = Updates::default();
        let key = (0, ChunkPos::new(1, -1));
        updates.load_list(key, &[]);
        let at = |x, y, z| BlockPos::new(16 + x, y, -16 + z);
        assert!(updates.schedule_at(0, at(1, 2, 3), 50));
        assert!(updates.schedule_at(0, at(4, 5, 6), 40));
        assert!(updates.schedule_at(0, at(7, 8, 9), 40));
        // Later than pending: kept; earlier: moved (PMMP compares delays and keeps both).
        assert!(!updates.schedule_at(0, at(1, 2, 3), 60));
        assert!(updates.schedule_at(0, at(1, 2, 3), 45));
        assert!(
            !updates.schedule_at(0, BlockPos::new(0, 0, 0), 1),
            "chunk not held"
        );
        assert!(
            !updates.schedule_at(0, at(0, 400, 0), 1),
            "outside the world height"
        );
        let changed = updates.take_changed();
        assert_eq!(changed.len(), 1);
        let (k, list) = &changed[0];
        assert_eq!(*k, key);
        let mut copy = list.clone();
        copy.push(Compound::new());
        let mut outside = list[0].clone();
        outside.insert(KEY_X.to_owned(), Value::Int(0));
        copy.push(outside);
        let mut again = Updates::default();
        again.load_list(key, &copy);
        let order = |u: &mut Updates| -> Vec<(BlockPos, u64)> {
            std::iter::from_fn(|| {
                let (&(_, due, _), _) = u.queue.iter().next()?;
                u.pop_due(0, u64::MAX).map(|(p, _)| (p, due))
            })
            .collect()
        };
        let first = order(&mut updates);
        assert_eq!(
            first,
            [(at(4, 5, 6), 40), (at(7, 8, 9), 40), (at(1, 2, 3), 45)]
        );
        assert_eq!(order(&mut again), first);
        assert!(again.take_changed().iter().all(|(_, l)| l.is_empty()));
    }

    #[test]
    fn unloading_every_chunk_returns_the_tables_to_their_start() {
        let mut state = server();
        let start = state.rules.updates.loaded.len();
        for round in 0..3 {
            for i in 0..20 {
                set(&mut state, BlockPos::new(i * 3, -60, round * 5), water());
            }
            for _ in 0..12 {
                advance(&mut state);
            }
            assert!(!state.rules.updates.queue.is_empty());
            let keys: Vec<Key> = state.rules.updates.loaded.iter().copied().collect();
            for &key in &keys {
                state.rules.updates.unload(key);
            }
            assert_eq!(state.rules.updates.len(), (0, 0, 0));
            assert!(state.rules.updates.loaded.is_empty());
            assert!(state.rules.updates.changed.is_empty());
            for key in keys {
                state.rules.updates.load_list(key, &[]);
            }
            assert_eq!(state.rules.updates.loaded.len(), start);
        }
        // A world feed left over by unloaded chunks drains in a tick.
        advance(&mut state);
        assert_eq!(state.worlds[0].changed_len(), 0);
        assert_eq!(state.rules.updates.len().2, 0);
    }

    /// A cube of redstone ore: touching it lights every block, each light change updating its
    /// neighbours (PMMP `RedstoneOre::onNearbyBlockChange`), over more than one tick's limit.
    #[test]
    fn neighbour_cascades_are_bounded_per_tick_and_complete() {
        let mut state = server();
        let ore = named("minecraft:redstone_ore");
        let lit = named("minecraft:lit_redstone_ore");
        let side = 30;
        let cube: Vec<BlockPos> = (0..side)
            .flat_map(|x| {
                (0..side).flat_map(move |y| (0..side).map(move |z| BlockPos::new(x, -60 + y, z)))
            })
            .collect();
        for &p in &cube {
            state.worlds[0].set_block_quietly(p, ore);
        }
        set(
            &mut state,
            BlockPos::new(-1, -60, 0),
            Kind::Stone.default_state(),
        );
        let count = |state: &State| cube.iter().filter(|p| get(state, **p) == lit).count();
        let mut ticks = 0;
        let mut seen = Vec::new();
        while count(&state) < cube.len() {
            advance(&mut state);
            ticks += 1;
            seen.push(count(&state));
            assert!(ticks < 50, "the cascade never completes: {seen:?}");
        }
        // About seven updates per changed block: more than one tick's limit.
        assert!(ticks > 1, "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
        for _ in 0..5 {
            advance(&mut state);
        }
        assert_eq!(state.rules.updates.len().2, 0);
        assert_eq!(state.worlds[0].changed_len(), 0);
    }

    /// Listeners and monitors that change nothing on the events random ticks raise; the
    /// receiver gets one message per growth.
    fn pass_through() -> (Listeners, tokio::sync::mpsc::Receiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::channel(1 << 16);
        let mut listeners = Listeners::default();
        listeners.push(
            Priority::Normal,
            move |_: &mut BlockGrow, _: &mut Context<'_>| {
                tx.try_send(()).unwrap();
            },
        );
        listeners.push_monitor(|_: &BlockGrow, _: &Context<'_>| {});
        listeners.push(
            Priority::High,
            |_: &mut BlockSpread, _: &mut Context<'_>| {},
        );
        listeners.push(Priority::Low, |_: &mut BlockUpdate, _: &mut Context<'_>| {});
        listeners.push_monitor(|_: &BlockUpdate, _: &Context<'_>| {});
        listeners.push(
            Priority::Normal,
            |_: &mut FarmlandHydrationChange, _: &mut Context<'_>| {},
        );
        (listeners, rx)
    }

    /// A field of wheat on wet farmland around a player: random ticks grow it, the same way
    /// with one pool thread or four, in a second run, and with listeners that change nothing.
    // pmmp: BlockGrowEvent
    #[test]
    fn random_ticks_are_deterministic_and_grow_crops() {
        let run = |threads: usize, listeners: Listeners| {
            let pool = crate::runtime::cpu_pool(threads).unwrap();
            let mut state = new_state(
                settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
                listeners,
            );
            let farmland = find(
                "minecraft:farmland",
                &[("moisturized_amount", PropertyValue::Int(7))],
            );
            let wheat = named("minecraft:wheat");
            for x in -20..20 {
                for z in -20..20 {
                    set(&mut state, BlockPos::new(x, -61, z), farmland);
                    if x % 2 == 0 {
                        set(&mut state, BlockPos::new(x, -60, z), wheat);
                    }
                }
            }
            set(&mut state, BlockPos::new(1, -61, 1), water());
            let mut output = Output::default();
            let (j, rx) = join(1, 0);
            let mut hashes = Vec::new();
            full_tick(&mut state, vec![j, radius(1, 1, 4)], &pool, &mut output);
            for _ in 0..300 {
                full_tick(&mut state, Vec::new(), &pool, &mut output);
                hashes.push(state.hash());
            }
            let mut grown = 0;
            for x in (-20..20).step_by(2) {
                for z in -20..20 {
                    let s = get(&state, BlockPos::new(x, -60, z));
                    grown += props::int(s, "growth").unwrap_or(0);
                }
            }
            drop(rx);
            assert!(pool.shutdown());
            (hashes, grown)
        };
        let (one, grown) = run(1, Listeners::default());
        assert!(grown > 0, "random ticks grew nothing");
        let (four, grown4) = run(4, Listeners::default());
        assert_eq!(one, four);
        assert_eq!(grown, grown4);
        let (listeners, mut growths) = pass_through();
        let (heard, grown_heard) = run(4, listeners);
        assert_eq!(one, heard, "listeners that change nothing change nothing");
        assert_eq!(grown, grown_heard);
        let calls = std::iter::from_fn(|| growths.try_recv().ok()).count();
        assert!(calls > 0 && calls <= usize::try_from(grown).unwrap());
    }

    #[test]
    fn ticking_chunks_are_the_view_discs_with_lit_neighbourhoods() {
        let mut state = server();
        // A loaded area with holes, so some disc chunks lack a loaded neighbour.
        for x in -12..=12 {
            for z in -12..=12 {
                let pos = ChunkPos::new(x, z);
                if (x * 7 + z * 3) % 11 != 0 {
                    state.worlds[0].insert_chunk(pos, world::Chunk::generate(&world::Flat, 0, pos));
                }
            }
        }
        let centers = BTreeSet::from([ChunkPos::new(0, 0), ChunkPos::new(3, -2)]);
        // One list for every radius, as the tick reuses it.
        let mut chunks = Vec::new();
        for radius in 1..=8 {
            let w = &state.worlds[0];
            let expected: Vec<ChunkPos> = centers
                .iter()
                .flat_map(|&c| view::disc(c, radius))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|c| {
                    (-1..=1).all(|dx| {
                        (-1..=1).all(|dz| {
                            let n = ChunkPos::new(c.x + dx, c.z + dz);
                            w.is_chunk_loaded(n) && w.is_chunk_lit(n)
                        })
                    })
                })
                .collect();
            assert!(!expected.is_empty());
            ticking_chunks(&state, 0, radius, Some(&centers), &mut chunks);
            assert_eq!(chunks, expected, "{radius}");
        }
    }

    /// Blocks and entities tick around the same players: one whose view is not open yet (joined,
    /// no chunk radius asked) ticks neither; once it is open, both.
    #[test]
    fn blocks_and_entities_tick_around_the_same_players() {
        let pool = pool();
        let mut state = new_state(
            settings(|c| {
                c.developer.lighting = crate::config::Lighting::Fixed;
                c.developer.random_tick_speed = 4096;
            }),
            Listeners::default(),
        );
        let farmland = find(
            "minecraft:farmland",
            &[("moisturized_amount", PropertyValue::Int(7))],
        );
        let wheat = named("minecraft:wheat");
        for x in 0..4 {
            set(&mut state, BlockPos::new(x, -61, 0), farmland);
            set(&mut state, BlockPos::new(x, -60, 0), wheat);
        }
        let grown = |state: &State| (0..4).any(|x| get(state, BlockPos::new(x, -60, 0)) != wheat);
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        full_tick(&mut state, vec![j], &pool, &mut output);
        full_tick(&mut state, Vec::new(), &pool, &mut output);
        let player = &state.players[&crate::tick::SessionId::new(1)];
        assert!(player.phase != Phase::Loading && !player.view.is_open());
        let start = Vec3::new(0.5, -50.0, 0.5);
        let stick = crate::item::Stack::new(crate::item::Kind::Stick.default_type(), 1);
        let id = Context { state: &mut state }
            .drop_item(world::Id::DEFAULT, start, stick, Some(Vec3::ZERO))
            .unwrap();
        let moved = |state: &mut State| Context { state }.entity(id).unwrap().position() != start;
        for _ in 0..20 {
            full_tick(&mut state, Vec::new(), &pool, &mut output);
        }
        assert_eq!(
            (grown(&state), moved(&mut state)),
            (false, false),
            "view closed"
        );
        full_tick(&mut state, vec![radius(1, 1, 4)], &pool, &mut output);
        for _ in 0..20 {
            full_tick(&mut state, Vec::new(), &pool, &mut output);
        }
        assert_eq!(
            (grown(&state), moved(&mut state)),
            (true, true),
            "view open"
        );
    }

    #[test]
    fn random_positions_are_a_function_of_seed_tick_and_chunk() {
        let positions = |tick: u64, chunk: ChunkPos| {
            let k = random::key(RANDOM_TICK, &[0, u64::from(chunk.x.cast_unsigned()), 0]);
            let mut rng = Random::new(7, tick, k);
            let mut bits = 0;
            (0..3)
                .map(|i| {
                    if i % 5 == 0 {
                        bits = rng.next_u64();
                    }
                    let p = random_position(chunk, -64, bits);
                    bits >>= 12;
                    p
                })
                .collect::<Vec<_>>()
        };
        let c = ChunkPos::new(2, 0);
        assert_eq!(positions(5, c), positions(5, c));
        assert_ne!(positions(5, c), positions(6, c));
        assert!(
            positions(5, c)
                .iter()
                .all(|p| p.chunk() == c && (-64..-48).contains(&p.y))
        );
        // Every position of a sub-chunk comes up about equally often.
        let mut hits = vec![0u32; 4096];
        let mut rng = Random::new(1, 2, 3);
        for _ in 0..409_600 {
            let p = random_position(ChunkPos::new(0, 0), 0, rng.next_u64());
            hits[usize::try_from(p.x + 16 * p.z + 256 * p.y).unwrap()] += 1;
        }
        // Each count is binomial(409600, 1/4096): mean 100, sd 10; 6 sd either side.
        assert!(hits.iter().all(|&h| (40..=160).contains(&h)));
    }

    /// PMMP `CropGrowthHelper::calculateMultiplier` as PMMP writes it, in floating point:
    /// farmland under and around (`None`: not farmland, else wet) and crops around.
    fn pmmp_multiplier(farmland: &[[Option<bool>; 3]; 3], crops: &[[bool; 3]; 3]) -> f64 {
        let mut result = 1.0;
        if let Some(wet) = farmland[1][1] {
            result += if wet { 3.0 } else { 1.0 };
        }
        let (mut x_row, mut z_row, mut improper) = (false, false, false);
        for x in -1i32..=1 {
            for z in -1i32..=1 {
                if x == 0 && z == 0 {
                    continue;
                }
                let i = usize::try_from(x + 1).unwrap();
                let j = usize::try_from(z + 1).unwrap();
                let Some(wet) = farmland[i][j] else {
                    continue;
                };
                result += if wet { 0.75 } else { 0.25 };
                if !improper && crops[i][j] {
                    if x == 0 {
                        if z_row {
                            improper = true;
                        } else {
                            x_row = true;
                        }
                    } else if z == 0 {
                        if x_row {
                            improper = true;
                        } else {
                            z_row = true;
                        }
                    } else {
                        improper = true;
                    }
                }
            }
        }
        if improper {
            result /= 2.0;
        }
        result
    }

    /// The growth chance follows PMMP's formula: the bound matches PMMP's floating point
    /// multiplier for random arrangements, and a fixed-seed sample of `can_grow` hits
    /// 1 / (bound + 1) within 4.5 standard deviations.
    #[test]
    fn crop_growth_follows_pmmps_formula() {
        let mut state = server();
        let wheat = named("minecraft:wheat");
        let farmland = |wet: bool| {
            find(
                "minecraft:farmland",
                &[(
                    "moisturized_amount",
                    PropertyValue::Int(if wet { 7 } else { 0 }),
                )],
            )
        };
        let mut rng = Random::new(11, 0, 0);
        let center = BlockPos::new(30, -60, 30);
        for round in 0..300 {
            let mut lands = [[None; 3]; 3];
            let mut crops = [[false; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    let dx = i32::try_from(i).unwrap() - 1;
                    let dz = i32::try_from(j).unwrap() - 1;
                    let at = center.offset(dx, 0, dz).unwrap();
                    lands[i][j] = match rng.below(3) {
                        0 => None,
                        n => Some(n == 2),
                    };
                    let land = lands[i][j].map_or(block::GRASS_BLOCK, farmland);
                    set(&mut state, at.offset(0, -1, 0).unwrap(), land);
                    crops[i][j] = (i, j) != (1, 1) && rng.below(3) == 0;
                    let plant = if (i, j) == (1, 1) || crops[i][j] {
                        wheat
                    } else {
                        block::AIR
                    };
                    set(&mut state, at, plant);
                }
            }
            let expected = (25.0 / pmmp_multiplier(&lands, &crops)).floor();
            let cx = Cx::new(&mut state, 0, center);
            let bound = plant::growth_bound(&cx, center, wheat).unwrap();
            assert_eq!(
                f64::from(bound),
                expected,
                "round {round}: {lands:?} {crops:?}"
            );
        }
        // A sample: wet farmland, one row: bound 25 / (1 + 3 + 8 × 0.75) = 2.
        for x in -1..=1 {
            for z in -1..=1 {
                set(&mut state, center.offset(x, -1, z).unwrap(), farmland(true));
                let crop = if x == 0 { wheat } else { block::AIR };
                set(&mut state, center.offset(x, 0, z).unwrap(), crop);
            }
        }
        let mut cx = Cx::new(&mut state, 0, center);
        let bound = plant::growth_bound(&cx, center, wheat).unwrap();
        assert_eq!(bound, 2);
        let n = 30_000u32;
        let hits = (0..n)
            .filter(|_| plant::can_grow(&mut cx, center, wheat))
            .count();
        let p = 1.0 / f64::from(bound + 1);
        let mean = p * f64::from(n);
        let sd = (mean * (1.0 - p)).sqrt();
        let got = f64::from(u32::try_from(hits).unwrap());
        assert!((got - mean).abs() <= 4.5 * sd, "{got} vs {mean} ± {sd}");
    }

    #[test]
    fn saplings_grow_at_noon_but_not_at_midnight() {
        // PMMP `Sapling::onRandomTick` needs a full light of 8: the open sky gives 15 at noon and
        // 15 less the night's reduction of 11 at midnight (PMMP `World::computeSkyLightReduction`).
        let grows = |time: i64| {
            let mut state = server();
            state.worlds[0].set_time(time);
            let sapling = named("minecraft:oak_sapling");
            let at = BlockPos::new(20, -60, 20);
            set(&mut state, at, sapling);
            let mut cx = Cx::new(&mut state, 0, at);
            let light = cx.full_light(at);
            for _ in 0..200 {
                on_random(&mut cx, at, sapling);
            }
            (light, get(&state, at) != sapling)
        };
        assert_eq!(grows(6_000), (15, true));
        assert_eq!(grows(18_000), (4, false));
        assert_eq!(grows(24_000 * 7 + 6_000), (15, true), "every day alike");
    }

    #[test]
    fn trees_grow_the_same_from_the_same_tick_and_need_room() {
        let grow = |blocked: bool| {
            let mut state = server();
            let sapling = named("minecraft:oak_sapling");
            let at = BlockPos::new(20, -60, 20);
            set(&mut state, at, sapling);
            if blocked {
                set(
                    &mut state,
                    at.offset(0, 3, 0).unwrap(),
                    Kind::Stone.default_state(),
                );
            }
            state.tick = 1234;
            assert_eq!(fertilize(&mut state, 0, at, None), !blocked);
            let mut blocks = Vec::new();
            for x in 14..27 {
                for y in -61..-45 {
                    for z in 14..27 {
                        let p = BlockPos::new(x, y, z);
                        let s = get(&state, p);
                        if s != ground(y) {
                            blocks.push((p, s));
                        }
                    }
                }
            }
            blocks
        };
        let a = grow(false);
        assert_eq!(a, grow(false));
        let logs = a.iter().filter(|(_, s)| s.kind() == Kind::OakLog).count();
        let leaves = a
            .iter()
            .filter(|(_, s)| s.kind() == Kind::OakLeaves)
            .count();
        assert!((3..=6).contains(&logs), "{logs} logs");
        assert!(leaves > 20, "{leaves} leaves");
        assert!(a.contains(&(BlockPos::new(20, -61, 20), block::DIRT)));
        let b = grow(true);
        assert_eq!(b.len(), 2, "a blocked sapling does not grow: {b:?}");
        // Every kind PMMP grows fits in an open field.
        for kind in [
            crate::world::TreeType::Spruce,
            crate::world::TreeType::Birch,
            crate::world::TreeType::Jungle,
            crate::world::TreeType::Acacia,
            crate::world::TreeType::Azalea,
            crate::world::TreeType::Crimson,
            crate::world::TreeType::Warped,
        ] {
            let mut state = server();
            let at = BlockPos::new(-20, -60, -20);
            let mut cx = Cx::new(&mut state, 0, at);
            assert!(tree::grow(&mut cx, at, kind), "{kind:?}");
        }
    }

    /// Fire on a wooden floor that reaches the edge of the loaded area: it spreads and burns
    /// out, never past the loaded chunks, and each change is in the box fire looks at.
    #[test]
    fn fire_spreads_within_bounds_and_burns_out() {
        let mut state = server();
        let planks = Kind::OakPlanks.default_state();
        // Loaded: chunks -4..=4, blocks -64..=79.
        for x in 40..80 {
            for z in 0..12 {
                set(&mut state, BlockPos::new(x, -60, z), planks);
            }
        }
        set(
            &mut state,
            BlockPos::new(60, -59, 6),
            Kind::Fire.default_state(),
        );
        state.worlds[0].take_changes();
        let fires = |state: &State| {
            (40..80)
                .flat_map(|x| {
                    (0..12).flat_map(move |z| (-60..=-55).map(move |y| BlockPos::new(x, y, z)))
                })
                .filter(|p| get(state, *p).kind() == Kind::Fire)
                .count()
        };
        let mut most = 0;
        let mut ticks = 0;
        while ticks < 20_000 {
            advance(&mut state);
            ticks += 1;
            for pos in state.worlds[0].take_changes().keys() {
                assert!((39..=79).contains(&pos.x), "{pos:?}");
                assert!((-1..=12).contains(&pos.z), "{pos:?}");
                assert!((-61..=-55).contains(&pos.y), "{pos:?}");
            }
            let n = fires(&state);
            most = most.max(n);
            if n == 0 {
                break;
            }
        }
        assert!(most > 1, "the fire spread");
        assert_eq!(fires(&state), 0, "the fire burnt out in {ticks} ticks");
    }

    /// Fire on the lowest layer stands on air outside the world (PMMP `getSide`): with nothing
    /// that burns next to it, fire goes out, and soul fire always does.
    #[test]
    fn fire_on_the_lowest_layer_goes_out_over_the_world_bottom() {
        let mut state = server();
        let fire = BlockPos::new(5, MIN_Y, 5);
        let soul = BlockPos::new(9, MIN_Y, 5);
        for (pos, s) in [
            (fire, Kind::Fire.default_state()),
            (soul, Kind::SoulFire.default_state()),
        ] {
            set(&mut state, pos, block::AIR);
            set(&mut state, pos, s);
        }
        advance(&mut state);
        assert_eq!(get(&state, fire), block::AIR);
        assert_eq!(get(&state, soul), block::AIR);
    }

    /// A flower on the lowest layer stands on air outside the world (PMMP `getSide`), not on
    /// soil: its neighbour update breaks it.
    #[test]
    fn a_flower_on_the_lowest_layer_breaks_over_the_world_bottom() {
        let mut state = server();
        let pos = BlockPos::new(3, MIN_Y, 3);
        set(&mut state, pos, named("minecraft:poppy"));
        advance(&mut state);
        let effects = std::mem::take(&mut state.rules.updates.effects);
        assert!(effects.contains(&Effect::Destroyed(pos)), "{effects:?}");
        assert_eq!(get(&state, pos), block::AIR);
    }

    /// Water above a full dyed cauldron makes it plain water (PMMP
    /// `WaterCauldron::onNearbyBlockChange` clears the custom colour).
    #[test]
    fn water_above_a_dyed_cauldron_clears_its_colour() {
        let mut state = server();
        let pos = BlockPos::new(5, -60, 5);
        let full = find(
            "minecraft:cauldron",
            &[
                ("cauldron_liquid", PropertyValue::String("water")),
                ("fill_level", PropertyValue::Int(CAULDRON_FULL)),
            ],
        );
        set(&mut state, pos, full);
        block_entity::missing(&mut state, 0, pos);
        let colour = |state: &State| match state.rules.block_entities.get(0, pos) {
            Some(block_entity::BlockEntity {
                kind: block_entity::Kind::Cauldron(c),
                ..
            }) => c.colour,
            other => panic!("{other:?}"),
        };
        if let Some(block_entity::BlockEntity {
            kind: block_entity::Kind::Cauldron(c),
            ..
        }) = state.rules.block_entities.get_mut(0, pos)
        {
            c.colour = Some(-1);
        }
        advance(&mut state);
        assert_eq!(colour(&state), Some(-1));
        set(
            &mut state,
            pos.step(Face::Up).unwrap(),
            Kind::Water.default_state(),
        );
        advance(&mut state);
        assert_eq!(get(&state, pos), full);
        assert_eq!(colour(&state), None);
    }

    /// Fire next to TNT primes it (PMMP `TNT::onIncinerate`): the block goes and primed TNT
    /// takes its place, instead of the TNT burning away.
    #[test]
    fn fire_primes_the_tnt_it_burns() {
        let mut state = server();
        let stone = Kind::Stone.default_state();
        let tnt = Kind::Tnt.default_state();
        let fire = BlockPos::new(8, -60, 8);
        set(&mut state, BlockPos::new(8, -61, 8), stone);
        let around = [
            BlockPos::new(9, -60, 8),
            BlockPos::new(7, -60, 8),
            BlockPos::new(8, -60, 9),
            BlockPos::new(8, -60, 7),
            BlockPos::new(8, -59, 8),
        ];
        for p in around {
            set(&mut state, p, tnt);
        }
        set(&mut state, fire, Kind::Fire.default_state());
        let mut primed = None;
        for _ in 0..20_000 {
            advance(&mut state);
            if let Some(&p) = around.iter().find(|p| get(&state, **p) != tnt) {
                primed = Some(p);
                break;
            }
        }
        let p = primed.expect("the fire reached the TNT");
        assert_eq!(get(&state, p), block::AIR, "primed, not burnt into fire");
        let cell = crate::math::Aabb::new(
            Vec3::new(f64::from(p.x), f64::from(p.y), f64::from(p.z)),
            Vec3::new(
                f64::from(p.x) + 1.0,
                f64::from(p.y) + 1.0,
                f64::from(p.z) + 1.0,
            ),
        );
        assert!(
            crate::tick::entity::occupied(&state, 0, cell),
            "primed TNT stands where the block was"
        );
    }

    /// Blocks that lose what holds them break (with drops, through the interaction rules),
    /// sand falls, and nothing breaks next to a chunk that is not loaded.
    #[test]
    fn support_loss_breaks_and_falling_blocks_fall() {
        let mut state = server();
        let stone = Kind::Stone.default_state();
        // Bedrock "west" is PMMP's east facing: the torch hangs on the block to its west.
        let torch = find(
            "minecraft:torch",
            &[("torch_facing_direction", PropertyValue::String("west"))],
        );
        set(&mut state, BlockPos::new(9, -59, 5), stone);
        set(&mut state, BlockPos::new(10, -59, 5), torch);
        set(
            &mut state,
            BlockPos::new(3, -60, 3),
            named("minecraft:poppy"),
        );
        set(
            &mut state,
            BlockPos::new(5, -55, 5),
            named("minecraft:sand"),
        );
        advance(&mut state);
        let effects = std::mem::take(&mut state.rules.updates.effects);
        assert_eq!(
            effects,
            [Effect::Fell(
                BlockPos::new(5, -55, 5),
                named("minecraft:sand")
            )]
        );
        set(&mut state, BlockPos::new(9, -59, 5), block::AIR);
        set(&mut state, BlockPos::new(3, -61, 3), stone);
        advance(&mut state);
        let effects = std::mem::take(&mut state.rules.updates.effects);
        assert!(
            effects.contains(&Effect::Destroyed(BlockPos::new(10, -59, 5))),
            "{effects:?}"
        );
        assert!(
            effects.contains(&Effect::Destroyed(BlockPos::new(3, -60, 3))),
            "{effects:?}"
        );
        // At the edge of the loaded area (blocks up to x 79), a torch on the unloaded side
        // stays.
        let east = find(
            "minecraft:torch",
            &[("torch_facing_direction", PropertyValue::String("east"))],
        );
        set(&mut state, BlockPos::new(79, -59, 0), east);
        advance(&mut state);
        assert!(state.rules.updates.effects.is_empty());
    }

    #[test]
    fn bone_meal_grows_crops_by_pmmp_steps() {
        let mut state = server();
        let at = BlockPos::new(5, -60, 5);
        set(
            &mut state,
            at.offset(0, -1, 0).unwrap(),
            named("minecraft:farmland"),
        );
        set(&mut state, at, named("minecraft:wheat"));
        let mut ages = vec![0];
        while props::int(get(&state, at), "growth") != Some(7) {
            assert!(fertilize(&mut state, 0, at, None));
            ages.push(props::int(get(&state, at), "growth").unwrap());
        }
        assert!(
            ages.windows(2)
                .all(|w| (2..=5).contains(&(w[1] - w[0])) || w[1] == 7),
            "{ages:?}"
        );
        assert!(
            !fertilize(&mut state, 0, at, None),
            "a grown crop takes no bone meal"
        );
        // Grass grows flowers and grass on the grass around.
        assert!(fertilize(&mut state, 0, BlockPos::new(-10, -61, -10), None));
        let grew = (-12..=-8)
            .flat_map(|x| (-12..=-8).map(move |z| BlockPos::new(x, -60, z)))
            .filter(|p| get(&state, *p) != block::AIR)
            .count();
        assert!(grew > 0);
    }

    /// Who lands on farmland in the tests.
    fn lander() -> Actor {
        Actor::Player(player::Id::new(crate::tick::SessionId::new(1)))
    }

    #[test]
    fn landing_on_farmland_tramples_it_by_the_fall() {
        let mut state = server();
        let at = BlockPos::new(2, -61, 2);
        set(&mut state, at, named("minecraft:farmland"));
        trample(&mut state, 0, at, 0.5, lander());
        assert_eq!(get(&state, at), named("minecraft:farmland"));
        trample(&mut state, 0, at, 1.5, lander());
        assert_eq!(get(&state, at), block::DIRT);
    }

    /// Measures random ticks with 20 players at tick radius 4 on the flat world, over a wheat
    /// field without listeners and with a listener on every growth, and a large water flow:
    /// `cargo test --release --lib tick::update::tests::block_update_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn block_update_cost() {
        let pool = pool();
        for (wheat, listening) in [(false, false), (true, false), (true, true)] {
            let (listeners, growths) = pass_through();
            let listeners = if listening {
                listeners
            } else {
                Listeners::default()
            };
            let mut state = new_state(
                settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
                listeners,
            );
            random_tick_cost(&mut state, &pool, wheat, listening);
            drop(growths);
        }
        // Water sources flowing over the open flat area until it settles.
        let mut state = server();
        for i in 0..64 {
            set(
                &mut state,
                BlockPos::new((i % 8) * 16 - 60, -60, (i / 8) * 16 - 60),
                water(),
            );
        }
        let mut worst = 0.0;
        let mut total = 0.0;
        for _ in 0..200 {
            let start = std::time::Instant::now();
            advance(&mut state);
            let took = start.elapsed().as_secs_f64() * 1e3;
            if took > worst {
                worst = took;
            }
            total += took;
        }
        eprintln!("water: 64 sources, 200 ticks: {total:.1} ms in all, worst tick {worst:.2} ms");
    }

    /// The random tick part of [`block_update_cost`] on `state`, with wheat everywhere near the
    /// players or none.
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn random_tick_cost(state: &mut State, pool: &Pool, wheat: bool, listening: bool) {
        let farmland = find(
            "minecraft:farmland",
            &[("moisturized_amount", PropertyValue::Int(7))],
        );
        let crop = named("minecraft:wheat");
        for x in (-64..80).filter(|_| wheat) {
            for z in -64..80 {
                set(state, BlockPos::new(x, -61, z), farmland);
                set(state, BlockPos::new(x, -60, z), crop);
            }
        }
        let mut output = Output::default();
        let mut joins = Vec::new();
        let mut receivers = Vec::new();
        for n in 1..=20u64 {
            let (j, rx) = join(n, 0);
            receivers.push(rx);
            joins.extend([j, radius(n, 1, 6)]);
        }
        full_tick(state, joins, pool, &mut output);
        let mut moves = Vec::new();
        for n in 1..=20i16 {
            let i = n - 1;
            let x = f32::from((i % 5) * 48 - 96);
            let z = f32::from((i / 5) * 48 - 72);
            moves.push(moved_to(u64::from(n.cast_unsigned()), 2, [x, -59.0, z]));
        }
        full_tick(state, moves, pool, &mut output);
        for _ in 0..40 {
            full_tick(state, Vec::new(), pool, &mut output);
        }
        let centers = view::tick_centres(state);
        let mut chunks = Vec::new();
        ticking_chunks(state, 0, 4, centers.get(&0), &mut chunks);
        let chunks = chunks.len();
        let ticks = 200u32;
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            run_random(state, 0, centers.get(&0));
            state.tick += 1;
        }
        let per = start.elapsed().as_secs_f64() * 1e6 / f64::from(ticks);
        eprintln!(
            "random ticks (wheat {wheat}, BlockGrow listener {listening}): {chunks} chunks, {per:.1} us"
        );
        drop(receivers);
    }
}
