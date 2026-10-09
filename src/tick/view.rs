//! What each player sees of its world: the chunks around it, the chunk loads they need, the
//! sub-chunk request answers, the block changes it is sent and its fake blocks.
//!
//! Chunks go out in the sub-chunk request system, as BDS sends them (P6): a `LevelChunk` with the
//! biomes, then the client asks for the sub-chunks it wants. A chunk stays loaded while a player
//! sees it, it is a spawn chunk or a ticket holds it ([`Context::hold_chunks`]); one nobody holds
//! unloads in the next view phase (PMMP unloads
//! at its world tick too), with or without [`ChunkUnload`] listeners, so that listening changes
//! nothing; an unloaded chunk with unsaved changes waits in memory for the next commit (`save`).
//!
//! Fake blocks are blocks one player is shown instead of the world's: the user's
//! ([`Context::send_fake_block`]) and, over them, the container blocks of an open menu. They are
//! sent in the output phase, so the last change of a tick wins whatever its order with the
//! world's changes; sent again after every sub-chunk answer that holds them, as the client then
//! forgets them; and a world change at their position is not sent while they stand. The server
//! keeps playing by the world's blocks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tracing::{debug, error, warn};

use super::extra::Extras;
use super::queue::{Message, SessionId};
use super::rules::Rules;
use super::save::{Saving, Unsaved, keep_unloaded};
use super::{Context, Phase, Player, State, dispatch, world_api};
use crate::block;
use crate::event::{ChunkLoad, ChunkUnload, PlayerPostChunkSend, SpawnChange};
use crate::math::Vec3;
use crate::menu::{Error as MenuError, Result as MenuResult};
use crate::player;
use crate::protocol::consts::{BlockUpdateFlag, HeightMapData, SubChunkResult};
use crate::protocol::packet::{
    BlockActorData, LevelChunk, NetworkChunkPublisherUpdate, Packet, SubChunk, SubChunkRequest,
    UpdateBlock, UpdateSubChunkBlocks,
};
use crate::protocol::{BlockChangeEntry, HeightMap, SubChunkEntry};
use crate::save::{Loaded, Request, WorldIndex};
use crate::world::{BlockPos, Chunk, ChunkPos, Kind, NO_HEIGHT, SUB_CHUNKS, World};

/// Chunk loads the save thread may have in flight at once.
pub(super) const MAX_LOADS_IN_FLIGHT: usize = 1024;
/// Chunk loads one tick may ask for.
const MAX_LOADS_PER_TICK: usize = 256;
/// The most chunks [`ChunkUnload`] listeners may keep loaded at once; past it a cancel is
/// ignored, so a listener that cancels every unload cannot hold the whole map in memory.
const MAX_KEPT_CHUNKS: usize = 1024;
/// How long a chunk a listener kept stays loaded before the listeners are asked again: 30
/// seconds, PMMP's unload delay.
const KEEP_TICKS: u64 = 600;
/// Unanswered sub-chunk requests a player may have queued; more are dropped.
const MAX_QUEUED_SUB_CHUNK_REQUESTS: usize = 256;
/// Sub-chunk requests answered per player per tick; the rest wait for the next tick.
const SUB_CHUNK_REQUESTS_PER_TICK: usize = 64;
/// The lowest sub-chunk y index (`SubChunkRequest` positions are absolute indices).
const LOWEST_SUB_CHUNK: i32 = crate::world::MIN_Y >> 4;
/// `UpdateBlock` flags BDS sends (P6): neighbours and network.
pub(super) const UPDATE_FLAGS: u32 =
    BlockUpdateFlag::Neighbours as u32 | BlockUpdateFlag::Network as u32;
/// The most fake blocks one player is shown, and the most changes of what a player is shown one
/// tick takes from the API (clears of fake blocks it had before the tick come on top).
pub(super) const MAX_FAKE_BLOCKS: usize = 4096;

/// A chunk of one of the server's worlds.
pub(super) type Key = (WorldIndex, ChunkPos);

/// The chunks a player sees and what it is sent of them.
#[derive(Debug, Default)]
pub(super) struct View {
    /// The chunk the view is around.
    center: ChunkPos,
    /// The granted chunk radius; 0 until the client asked for one.
    radius: u8,
    /// The block the last publisher update was centered on.
    published: BlockPos,
    /// The chunks of the view, nearest first (then by x and z).
    order: Vec<ChunkPos>,
    /// Every chunk of `order` before it has been sent.
    cursor: usize,
    /// The chunks the client was sent and still sees.
    sent: BTreeSet<ChunkPos>,
    /// Sub-chunk requests waiting for an answer, oldest first.
    requests: VecDeque<SubChunkRequest>,
    /// The user's fake blocks ([`Context::send_fake_block`]), at most [`MAX_FAKE_BLOCKS`].
    fakes: BTreeMap<FakeKey, block::State>,
    /// The container blocks of the open menu, shown over the user's fake blocks.
    menu: [Option<(BlockPos, block::State)>; 2],
    /// What the output phase sends this tick; empty between ticks.
    pub(super) out: Out,
}

/// A fake block's place: its chunk and sub-chunk y first, so one sub-chunk's are a range.
type FakeKey = (ChunkPos, i32, BlockPos);

fn fake_key(pos: BlockPos) -> FakeKey {
    (pos.chunk(), pos.y >> 4, pos)
}

/// The chunk packets of one tick for one player.
#[derive(Debug, Default)]
pub(super) struct Out {
    /// A publisher update: the view or the player's block moved, or the view changed size.
    publish: bool,
    /// Chunks to send, in order.
    chunks: Vec<ChunkPos>,
    /// Sub-chunk requests to answer, in order.
    answers: Vec<SubChunkRequest>,
    /// Positions whose fake block was set or cleared this tick: the output phase sends what they
    /// show then. At most [`MAX_FAKE_BLOCKS`] from sets and menus, plus the clears of fake blocks
    /// the player had before the tick.
    shown: BTreeSet<BlockPos>,
}

impl Out {
    pub(super) fn is_empty(&self) -> bool {
        !self.publish && self.chunks.is_empty() && self.answers.is_empty() && self.shown.is_empty()
    }
}

impl View {
    /// Whether the client was sent chunk `pos` and still sees it.
    pub(super) fn sees(&self, pos: ChunkPos) -> bool {
        self.sent.contains(&pos)
    }

    /// Whether the view has a radius yet.
    pub(super) fn is_open(&self) -> bool {
        self.radius > 0
    }

    /// The granted chunk radius, 0 before the client asked.
    pub(super) fn radius(&self) -> u8 {
        self.radius
    }

    /// Whether every chunk of the view was sent (tests).
    #[cfg(test)]
    pub(super) fn is_complete(&self) -> bool {
        self.sent.len() == self.order.len()
    }

    /// The fake block the player is shown at `pos`, if any: the open menu's, else the user's.
    pub(super) fn shown(&self, pos: BlockPos) -> Option<block::State> {
        self.menu
            .iter()
            .flatten()
            .find(|(p, _)| *p == pos)
            .map(|(_, b)| *b)
            .or_else(|| self.fakes.get(&fake_key(pos)).copied())
    }

    /// The user's fake blocks in sub-chunk `sub_y` of `chunk`.
    fn fakes_in(
        &self,
        chunk: ChunkPos,
        sub_y: i32,
    ) -> impl Iterator<Item = (BlockPos, block::State)> + '_ {
        let low = (chunk, sub_y, BlockPos::new(i32::MIN, i32::MIN, i32::MIN));
        let high = (chunk, sub_y, BlockPos::new(i32::MAX, i32::MAX, i32::MAX));
        self.fakes
            .range(low..=high)
            .map(|(&(_, _, pos), &b)| (pos, b))
    }

    /// Whether the player is shown a fake block in sub-chunk `sub_y` of `chunk`.
    fn shows_in(&self, chunk: ChunkPos, sub_y: i32) -> bool {
        self.fakes_in(chunk, sub_y).next().is_some()
            || self
                .menu
                .iter()
                .flatten()
                .any(|(p, _)| fake_key(*p).0 == chunk && p.y >> 4 == sub_y)
    }

    /// The fake blocks the player is shown in sub-chunk `sub_y` of `chunk`, in position order.
    fn shown_in(&self, chunk: ChunkPos, sub_y: i32) -> Vec<(BlockPos, block::State)> {
        let mut out: Vec<(BlockPos, block::State)> = self.fakes_in(chunk, sub_y).collect();
        for &(pos, b) in self.menu.iter().flatten() {
            if pos.chunk() != chunk || pos.y >> 4 != sub_y {
                continue;
            }
            match out.binary_search_by_key(&pos, |(p, _)| *p) {
                Ok(i) => {
                    if let Some(e) = out.get_mut(i) {
                        e.1 = b;
                    }
                }
                Err(i) => out.insert(i, (pos, b)),
            }
        }
        out
    }

    /// Whether `n` more changes of what the player is shown fit this tick.
    pub(super) fn has_room_to_show(&self, n: usize) -> bool {
        self.out.shown.len().saturating_add(n) <= MAX_FAKE_BLOCKS
    }

    /// Shows the open menu's container blocks (none: the menu closed) over the user's fake
    /// blocks and returns the ones shown before; the output phase sends both the old and the new
    /// positions.
    pub(super) fn set_menu_blocks(
        &mut self,
        blocks: [Option<(BlockPos, block::State)>; 2],
    ) -> [Option<(BlockPos, block::State)>; 2] {
        for (pos, _) in self.menu.iter().chain(&blocks).flatten() {
            self.out.shown.insert(*pos);
        }
        std::mem::replace(&mut self.menu, blocks)
    }

    /// The menu's container blocks shown (tests).
    #[cfg(test)]
    pub(super) fn menu_blocks(&self) -> [Option<(BlockPos, block::State)>; 2] {
        self.menu
    }

    /// How many fake blocks the user set (tests).
    #[cfg(test)]
    pub(super) fn fake_count(&self) -> usize {
        self.fakes.len()
    }

    /// Moves the cursor past the chunks already sent.
    fn advance(&mut self) {
        while self
            .order
            .get(self.cursor)
            .is_some_and(|p| self.sent.contains(p))
        {
            self.cursor = self.cursor.saturating_add(1);
        }
    }

    /// Whether every chunk within `radius` of the center was sent.
    fn has_sent_within(&self, radius: u8) -> bool {
        let r = i32::from(radius);
        self.order
            .get(self.cursor)
            .is_none_or(|p| distance(*p, self.center) > r * r)
    }

    /// A hash of what the view decides, for the state hash of tests.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The order follows from the center and radius; the output is empty between ticks.
        let Self {
            center,
            radius,
            published,
            order: _,
            cursor,
            sent,
            requests,
            fakes,
            menu,
            out: _,
        } = self;
        h.u64(u64::from(*radius));
        for c in [center.x, center.z, published.x, published.y, published.z] {
            h.u64(u64::from(c.cast_unsigned()));
        }
        h.usize(*cursor);
        h.usize(sent.len());
        for p in sent {
            h.u64(u64::from(p.x.cast_unsigned()));
            h.u64(u64::from(p.z.cast_unsigned()));
        }
        h.usize(requests.len());
        for SubChunkRequest {
            dimension,
            position,
            offsets,
        } in requests
        {
            h.u64(u64::from(dimension.cast_unsigned()));
            for c in position {
                h.u64(u64::from(c.cast_unsigned()));
            }
            h.usize(offsets.len());
            for offset in offsets {
                h.raw(&offset.map(i8::cast_unsigned));
            }
        }
        let block = |h: &mut super::Fnv, pos: BlockPos, b: block::State| {
            h.bytes(&[pos.x, pos.y, pos.z].map(i32::to_le_bytes).concat());
            h.u64(u64::from(b.network_id()));
        };
        h.usize(fakes.len());
        for (&(_, _, pos), &b) in fakes {
            block(h, pos, b);
        }
        for m in menu {
            match m {
                Some((pos, b)) => block(h, *pos, *b),
                None => h.u64(0),
            }
        }
    }
}

super::checkpoint::fields!(View {
    center,
    radius,
    published,
    order,
    cursor,
    sent,
    requests,
    fakes,
    menu;
    out
});
super::checkpoint::fields!(SubChunkRequest {
    dimension,
    position,
    offsets
});

/// The squared horizontal distance between two chunks.
fn distance(a: ChunkPos, b: ChunkPos) -> i32 {
    let (dx, dz) = (a.x.saturating_sub(b.x), a.z.saturating_sub(b.z));
    dx.saturating_mul(dx).saturating_add(dz.saturating_mul(dz))
}

/// The chunks within `radius` of `center` (x² + z² ≤ r²), nearest first, then by x and z.
pub(super) fn disc(center: ChunkPos, radius: u8) -> Vec<ChunkPos> {
    let r = i32::from(radius);
    let mut offsets: Vec<(i32, i32)> = (-r..=r)
        .flat_map(|x| (-r..=r).map(move |z| (x, z)))
        .filter(|&(x, z)| x * x + z * z <= r * r)
        .collect();
    offsets.sort_unstable_by_key(|&(x, z)| (x * x + z * z, x, z));
    offsets
        .into_iter()
        .map(|(x, z)| ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z)))
        .collect()
}

/// The chunk an eye position is in. Positions are finite (checked when they arrive).
pub(super) fn chunk_of(position: Vec3) -> ChunkPos {
    BlockPos::containing(position).chunk()
}

/// The loaded chunks, the chunk loads in flight and the unloaded chunks waiting for a commit.
#[derive(Debug, Default)]
pub(super) struct Chunks {
    /// How many views (and spawn areas) hold each chunk; a chunk is loaded or wanted exactly
    /// while it is here.
    interest: BTreeMap<Key, u32>,
    /// Chunks asked of the save thread and not answered yet.
    pub(super) requested: BTreeSet<Key>,
    /// Unloaded chunks with changes no commit holds yet.
    pub(super) unsaved: BTreeMap<Key, Unsaved>,
    /// Loads to ask for at the end of this tick; empty between ticks.
    batch: Vec<Key>,
    /// The extras of the loaded chunks.
    pub(super) extras: Extras,
    /// The spawn whose 3 × 3 chunks are held for each world, by world index: the world's spawn,
    /// or its old one until the view phase moves the hold ([`move_spawns`]).
    spawns: Vec<BlockPos>,
    /// The chunk events someone listens to, fixed when the server is built ([`hold_spawns`]).
    heard: Heard,
    /// Chunks loaded or brought back from memory this tick, for [`ChunkLoad`] in the view phase
    /// (`true`: the generator made it). Empty unless it is heard, and between ticks.
    loads: BTreeMap<Key, bool>,
    /// Chunks whose last holder let go: still loaded until the view phase unloads them, asking
    /// the [`ChunkUnload`] listeners first ([`announce`]).
    unloading: BTreeSet<Key>,
    /// Chunks a [`ChunkUnload`] listener kept loaded, by the tick they are asked about again;
    /// each holds one interest. At most [`MAX_KEPT_CHUNKS`].
    kept: BTreeSet<(u64, Key)>,
    /// The chunks picked to send this tick, for [`PlayerPostChunkSend`] in the view phase. Empty
    /// unless it is heard, and between ticks.
    sent: Vec<(SessionId, ChunkPos)>,
    /// The chunk tickets ([`Context::hold_chunks`]) by number; each holds one interest in every
    /// chunk of its area. At most [`MAX_TICKETS`], holding at most [`MAX_TICKET_CHUNKS`].
    tickets: BTreeMap<u64, Ticket>,
    /// The number of the next ticket.
    next_ticket: u64,
    /// Each chunk the tickets hold with the number of each ticket that holds it, one pair per
    /// ticket and chunk (at most [`MAX_TICKET_CHUNKS`]): derived from `tickets`, so neither
    /// saved nor hashed.
    by_chunk: BTreeSet<(Key, u64)>,
    /// Chunks the tickets hold that are not loaded yet: asked for in the view phase until they
    /// load (or nobody holds them).
    waiting: BTreeSet<Key>,
    /// Loads in flight for a world that unloaded since they were asked for, by how many: their
    /// answers come first (the save thread answers in order) and are dropped, so a world made
    /// again never gets a chunk of the world before it.
    pub(super) stale: BTreeMap<Key, u32>,
}

/// The chunks a ticket holds: the square of `radius` chunks around `center` in `world`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ticket {
    world: WorldIndex,
    center: ChunkPos,
    radius: u8,
}

super::checkpoint::fields!(Ticket {
    world,
    center,
    radius
});

impl Ticket {
    /// The chunks of its area, row by row.
    fn area(self) -> impl Iterator<Item = ChunkPos> {
        let r = i32::from(self.radius);
        let center = self.center;
        (-r..=r).flat_map(move |x| {
            (-r..=r)
                .map(move |z| ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z)))
        })
    }

    /// How many chunks its area holds.
    fn size(radius: u8) -> usize {
        let side = 2 * usize::from(radius) + 1;
        side * side
    }
}

/// The most chunk tickets at once.
const MAX_TICKETS: usize = 1024;
/// The most chunks the tickets hold together (counted per ticket): the loads in flight times 4.
const MAX_TICKET_CHUNKS: usize = MAX_LOADS_IN_FLIGHT * 4;
/// The largest ticket radius: 17 × 17 chunks.
const MAX_TICKET_RADIUS: u8 = 8;
/// The largest chunk coordinate a ticket may center on: the world border's.
const MAX_CHUNK_COORDINATE: i32 = crate::world::BORDER >> 4;

/// Which chunk events someone listens to or monitors.
#[derive(Debug, Default, Clone, Copy)]
struct Heard {
    load: bool,
    unload: bool,
    sent: bool,
    spawn: bool,
}

impl Chunks {
    /// Writes what lasts between ticks; the batch, the loads and the sent chunks are empty then,
    /// and the events heard are fixed at build.
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
        let Self {
            interest,
            requested,
            unsaved,
            batch: _,
            extras,
            spawns,
            heard: _,
            loads: _,
            unloading,
            kept,
            sent: _,
            tickets,
            next_ticket,
            by_chunk: _,
            waiting,
            stale,
        } = self;
        interest.encode(w);
        requested.encode(w);
        w.len(unsaved.len());
        for (key, u) in unsaved {
            key.encode(w);
            u.checkpoint(w);
        }
        extras.checkpoint(w);
        spawns.encode(w);
        unloading.encode(w);
        kept.encode(w);
        tickets.encode(w);
        next_ticket.encode(w);
        waiting.encode(w);
        stale.encode(w);
    }

    /// Reads what [`Self::checkpoint`] wrote into the chunks of a new state (which keeps the
    /// events it heard); `ground` gives what the server file or the generator has.
    pub(super) fn restore(
        &mut self,
        r: &mut super::checkpoint::Reader<'_>,
        ground: &mut super::checkpoint::Ground,
    ) -> super::checkpoint::Result<()> {
        use super::checkpoint::{Codec, Error};
        let interest = Codec::decode(r)?;
        let requested = Codec::decode(r)?;
        let n = r.len()?;
        let mut unsaved = BTreeMap::new();
        for _ in 0..n {
            let key = Key::decode(r)?;
            let chunk = ground.chunks.remove(&key).ok_or(Error::Format(
                "unsaved terrain the checkpoint does not keep",
            ))?;
            let restored = Unsaved::restore(r, chunk, ground.extras.remove(&key))?;
            if unsaved.insert(key, restored).is_some() {
                return Err(Error::Format("repeated unsaved chunk"));
            }
        }
        *self = Self {
            interest,
            requested,
            unsaved,
            batch: Vec::new(),
            extras: Extras::restore(r, &mut ground.extras)?,
            spawns: Codec::decode(r)?,
            heard: self.heard,
            loads: BTreeMap::new(),
            unloading: Codec::decode(r)?,
            kept: Codec::decode(r)?,
            sent: Vec::new(),
            tickets: Codec::decode(r)?,
            next_ticket: Codec::decode(r)?,
            by_chunk: BTreeSet::new(),
            waiting: Codec::decode(r)?,
            stale: Codec::decode(r)?,
        };
        for (&n, ticket) in &self.tickets {
            self.by_chunk
                .extend(ticket.area().map(|pos| ((ticket.world, pos), n)));
        }
        Ok(())
    }

    /// Whether no chunk is held, wanted, in flight or unsaved (for leak tests).
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.requested.is_empty()
            && self.unsaved.is_empty()
            && self.batch.is_empty()
            && self.loads.is_empty()
            && self.unloading.is_empty()
            && self.kept.is_empty()
            && self.sent.is_empty()
            && self.tickets.is_empty()
            && self.by_chunk.is_empty()
            && self.waiting.is_empty()
            && self.stale.is_empty()
    }

    /// The chunks with loads in flight: those asked for and those of unloaded worlds (a chunk of
    /// an unloaded world counts once, however many of its loads are in flight: only unloading the
    /// same world again before an answer repeats one, and the save thread's bounded queues bound
    /// those).
    fn in_flight(&self) -> usize {
        self.requested.len().saturating_add(self.stale.len())
    }

    /// The chunks held, wanted or in flight, for tests.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.interest.len()
    }

    /// Whether `key` is held, wanted or in flight, for tests.
    #[cfg(test)]
    pub(super) fn holds(&self, key: Key) -> bool {
        self.interest.contains_key(&key)
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The batch, the loads and the sent chunks are empty between ticks; the events heard
        // are fixed.
        let Self {
            interest,
            requested,
            unsaved,
            batch: _,
            extras,
            spawns,
            heard: _,
            loads: _,
            unloading,
            kept,
            sent: _,
            tickets,
            next_ticket,
            by_chunk: _,
            waiting,
            stale,
        } = self;
        let key = |h: &mut super::Fnv, &(w, p): &Key| {
            h.usize(w);
            h.u64(u64::from(p.x.cast_unsigned()));
            h.u64(u64::from(p.z.cast_unsigned()));
        };
        h.usize(interest.len());
        for (k, &n) in interest {
            key(h, k);
            h.u64(u64::from(n));
        }
        h.usize(requested.len());
        for k in requested {
            key(h, k);
        }
        h.usize(unsaved.len());
        for (k, u) in unsaved {
            key(h, k);
            u.hash_into(h);
        }
        extras.hash_into(h);
        h.usize(spawns.len());
        for p in spawns {
            h.bytes(&[p.x, p.y, p.z].map(i32::to_le_bytes).concat());
        }
        h.usize(unloading.len());
        for k in unloading {
            key(h, k);
        }
        h.usize(kept.len());
        for (due, k) in kept {
            h.u64(*due);
            key(h, k);
        }
        h.usize(tickets.len());
        for (n, t) in tickets {
            h.u64(*n);
            key(h, &(t.world, t.center));
            h.u64(u64::from(t.radius));
        }
        h.u64(*next_ticket);
        h.usize(waiting.len());
        for k in waiting {
            key(h, k);
        }
        h.usize(stale.len());
        for (k, n) in stale {
            key(h, k);
            h.u64(u64::from(*n));
        }
    }
}

/// Adds one holder of `key`. A chunk unloaded with unsaved changes comes back from memory, its
/// terrain and extras with their dirty marks, so a load never reads a file older than the tick's
/// data.
fn hold(worlds: &mut [World], chunks: &mut Chunks, rules: &mut Rules, key: Key) {
    let count = chunks.interest.entry(key).or_insert(0);
    *count = count.saturating_add(1);
    if *count > 1 {
        return;
    }
    if chunks.unloading.remove(&key) {
        // Let go and held again before the view phase asked about it: it never left.
        return;
    }
    if let Some(Unsaved { chunk, extras, .. }) = chunks.unsaved.remove(&key)
        && let Some(world) = worlds.get_mut(key.0)
    {
        world.restore_chunk(key.1, chunk);
        rules.load(key, extras.extras(), world);
        chunks.extras.chunk_restored(key, extras);
        if chunks.heard.load {
            chunks.loads.entry(key).or_insert(false);
        }
    }
}

/// Removes one holder of `key`; after the last one the chunk stays loaded until the view phase
/// unloads it ([`announce`]).
fn release(chunks: &mut Chunks, key: Key) {
    let Some(count) = chunks.interest.get_mut(&key) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count > 0 {
        return;
    }
    chunks.interest.remove(&key);
    chunks.unloading.insert(key);
}

/// Unloads a chunk nobody holds, with its extras. A chunk of a persistent world whose changes no
/// commit holds yet stays in memory until one does.
fn unload(worlds: &mut [World], chunks: &mut Chunks, rules: &mut Rules, saving: &Saving, key: Key) {
    rules.unload(key, &mut chunks.extras);
    let extras = chunks.extras.chunk_unloaded(key);
    let Some(world) = worlds.get_mut(key.0) else {
        return;
    };
    let persistent = world.kind() == Kind::Persistent;
    if let Some(chunk) = world.remove_chunk(key.1)
        && persistent
        && let Some(unsaved) = keep_unloaded(saving, &key, chunk, extras.unwrap_or_default())
    {
        chunks.unsaved.insert(key, unsaved);
    }
}

/// Numbers the worlds by their place in the list ([`World::id`]) and holds the spawn chunks of
/// every world (3 × 3 around the spawn, as PMMP keeps them), so they stay loaded without players.
///
/// It also notes which chunk events someone listens to: listeners are fixed when the server is
/// built, and the chunk code that runs on split borrows of the state cannot ask.
pub(super) fn hold_spawns(state: &mut State) {
    state.chunks.heard = Heard {
        load: dispatch::heard::<ChunkLoad>(state),
        unload: dispatch::heard::<ChunkUnload>(state),
        sent: dispatch::heard::<PlayerPostChunkSend>(state),
        spawn: dispatch::heard::<SpawnChange>(state),
    };
    for (i, w) in state.worlds.iter_mut().enumerate() {
        w.set_id(crate::world::Id::of(i));
    }
    let spawns: Vec<BlockPos> = state.worlds.iter().map(World::spawn).collect();
    for (world, &spawn) in spawns.iter().enumerate() {
        for pos in spawn_area(spawn.chunk()) {
            hold(
                &mut state.worlds,
                &mut state.chunks,
                &mut state.rules,
                (world, pos),
            );
        }
    }
    state.chunks.spawns = spawns;
}

/// The 3 × 3 chunks around `center`.
fn spawn_area(center: ChunkPos) -> impl Iterator<Item = ChunkPos> {
    (-1..=1).flat_map(move |x| {
        (-1..=1).map(move |z| ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z)))
    })
}

/// Moves the held spawn chunks of each world whose spawn moved to another chunk
/// ([`World::set_spawn`]): the new ones are held first, so chunks both areas share stay loaded,
/// then the old ones are released. Returns the worlds whose spawn moved, with the spawn before,
/// while [`SpawnChange`] is heard.
fn move_spawns(
    worlds: &mut [World],
    chunks: &mut Chunks,
    rules: &mut Rules,
) -> Vec<(WorldIndex, BlockPos)> {
    let mut moved = Vec::new();
    for world in 0..chunks.spawns.len() {
        let (Some(&held), Some(now)) = (
            chunks.spawns.get(world),
            worlds.get(world).map(World::spawn),
        ) else {
            continue;
        };
        if held == now {
            continue;
        }
        if chunks.heard.spawn {
            moved.push((world, held));
        }
        if held.chunk() != now.chunk() {
            for pos in spawn_area(now.chunk()) {
                hold(worlds, chunks, rules, (world, pos));
            }
            for pos in spawn_area(held.chunk()) {
                release(chunks, (world, pos));
            }
        }
        if let Some(spawn) = chunks.spawns.get_mut(world) {
            *spawn = now;
        }
    }
    moved
}

/// Opens or moves a player's view: the chunks it gains are held, those it loses released and
/// forgotten as sent.
fn set_view(
    worlds: &mut [World],
    chunks: &mut Chunks,
    rules: &mut Rules,
    world: WorldIndex,
    view: &mut View,
    center: ChunkPos,
    radius: u8,
) {
    // `view.order` is the old disc, or empty before the view opens.
    let had_view = !view.order.is_empty();
    let (r, old_r) = (i32::from(radius), i32::from(view.radius));
    // By x, then z: holding a chunk can restore its saved entities, which take ids in turn.
    for x in -r..=r {
        for z in -r..=r {
            let pos = ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z));
            let was_seen = had_view && distance(pos, view.center) <= old_r * old_r;
            if x * x + z * z <= r * r && !was_seen {
                hold(worlds, chunks, rules, (world, pos));
            }
        }
    }
    for &pos in &view.order {
        if distance(pos, center) > r * r {
            release(chunks, (world, pos));
            view.sent.remove(&pos);
        }
    }
    let order = disc(center, radius);
    view.center = center;
    view.radius = radius;
    view.order = order;
    view.cursor = 0;
    view.out.publish = true;
}

/// Releases every chunk a leaving player held.
pub(super) fn close(state: &mut State, player: &mut Player) {
    let world = player.world;
    for &pos in &player.view.order {
        release(&mut state.chunks, (world, pos));
    }
    player.view = View::default();
}

/// Grants a chunk radius: the view opens or resizes around the player.
pub(super) fn set_radius(state: &mut State, player: &mut Player, radius: u8) {
    let center = chunk_of(player.position);
    set_view(
        &mut state.worlds,
        &mut state.chunks,
        &mut state.rules,
        player.world,
        &mut player.view,
        center,
        radius,
    );
}

/// The player of `id` if it is in the game (its saved data has loaded).
fn in_game(state: &mut State, id: player::Id) -> Option<&mut Player> {
    state
        .players
        .get_mut(&id.session())
        .filter(|p| p.phase != Phase::Loading)
}

impl Context<'_> {
    /// Shows `block` at `pos` to `player` instead of the block of its world there, until the
    /// fake block is cleared or the player changes world or leaves. The client is told at the
    /// end of the tick, again whenever it is sent the sub-chunk again, and is not told of the
    /// world's changes there while the fake block stands. A menu's container blocks show over
    /// it while the menu is open.
    ///
    /// Only the client sees it: the server keeps playing by the world's block, so the player
    /// collides with, breaks and uses the real one, and a refused interaction there may show the
    /// real block again.
    ///
    /// Fails, changing nothing, if the player is not in the game, `pos` is outside the world
    /// height, the player has 4,096 fake blocks, or 4,096 fake block changes wait to be sent to
    /// it this tick.
    #[doc(alias = "World::createBlockUpdatePackets")]
    pub fn send_fake_block(
        &mut self,
        player: player::Id,
        pos: BlockPos,
        block: block::State,
    ) -> MenuResult<()> {
        if !World::is_in_world(pos) {
            return Err(MenuError::OutsideWorld);
        }
        let view = &mut in_game(self.state, player).ok_or(MenuError::Offline)?.view;
        let key = fake_key(pos);
        let had = view.fakes.get(&key).copied();
        if had == Some(block) {
            return Ok(());
        }
        if had.is_none() && view.fakes.len() >= MAX_FAKE_BLOCKS {
            return Err(MenuError::Full);
        }
        if !view.out.shown.contains(&pos) && !view.has_room_to_show(1) {
            return Err(MenuError::Busy);
        }
        view.fakes.insert(key, block);
        view.out.shown.insert(pos);
        Ok(())
    }

    /// The fake block `player` is shown at `pos` ([`send_fake_block`](Self::send_fake_block)),
    /// if any.
    pub fn fake_block(&self, player: player::Id, pos: BlockPos) -> Option<block::State> {
        let p = self.state.players.get(&player.session())?;
        p.view.fakes.get(&fake_key(pos)).copied()
    }

    /// Clears the fake block `player` is shown at `pos`: by the end of the tick it sees the
    /// world's block there again. False if the player is not in the game or has no fake block
    /// there.
    pub fn clear_fake_block(&mut self, player: player::Id, pos: BlockPos) -> bool {
        let Some(p) = in_game(self.state, player) else {
            return false;
        };
        if p.view.fakes.remove(&fake_key(pos)).is_none() {
            return false;
        }
        p.view.out.shown.insert(pos);
        restore_block_data(self.state, player.session(), pos);
        true
    }

    /// Clears every fake block `player` is shown. False if the player is not in the game.
    pub fn clear_fake_blocks(&mut self, player: player::Id) -> bool {
        let Some(p) = in_game(self.state, player) else {
            return false;
        };
        let fakes = std::mem::take(&mut p.view.fakes);
        for &(_, _, pos) in fakes.keys() {
            p.view.out.shown.insert(pos);
        }
        for &(_, _, pos) in fakes.keys() {
            restore_block_data(self.state, player.session(), pos);
        }
        true
    }
}

/// Sends `session` the client data of the block entity at `pos` once the player sees the world's
/// block there again (after a fake block over it is cleared): the client lost it when the fake
/// block replaced the block. It goes out after the block itself, which the output phase sends.
pub(super) fn restore_block_data(state: &mut State, session: super::SessionId, pos: BlockPos) {
    let State { players, rules, .. } = state;
    let Some(player) = players.get_mut(&session) else {
        return;
    };
    if player.view.shown(pos).is_some() || !player.view.sees(pos.chunk()) {
        return;
    }
    let Some(entity) = rules
        .block_entities
        .get(player.world, pos)
        .filter(|e| e.is_spawnable())
    else {
        return;
    };
    let packet = Packet::BlockActorData(BlockActorData {
        position: [pos.x, pos.y, pos.z],
        nbt_data: entity.spawn_nbt(pos),
    });
    player.send_packet(packet);
}

/// Queues a sub-chunk request for the output phase; a player with too many waiting loses it.
pub(super) fn queue_request(player: &mut Player, request: SubChunkRequest) {
    if player.view.requests.len() >= MAX_QUEUED_SUB_CHUNK_REQUESTS {
        debug!(
            runtime_id = player.runtime_id,
            "too many sub-chunk requests waiting; one dropped"
        );
        return;
    }
    player.view.requests.push_back(request);
}

/// A load the save thread answered: the terrain goes into its world and the extras that came
/// with it to their holder ([`Extras::chunk_loaded`]). A chunk nobody holds any more is dropped;
/// a damaged one is quarantined, extras included.
pub(super) fn loaded(state: &mut State, world: WorldIndex, pos: ChunkPos, loaded: Loaded) {
    let key = (world, pos);
    if let Some(n) = state.chunks.stale.get_mut(&key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            state.chunks.stale.remove(&key);
        }
        return;
    }
    let generated = matches!(loaded, Loaded::Generated(..));
    state.chunks.requested.remove(&key);
    if !state.chunks.interest.contains_key(&key) {
        return;
    }
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    match loaded {
        Loaded::Disk(chunk, stored) | Loaded::Generated(chunk, stored) => {
            w.insert_chunk(pos, *chunk);
            state.rules.load(key, &stored.extras, w);
            state.chunks.extras.chunk_loaded(key, stored.extras);
            if state.chunks.heard.load {
                state.chunks.loads.insert(key, generated);
            }
        }
        Loaded::Damaged => {
            state.chunks.extras.chunk_unloaded(key);
            error!(
                world = w.name(),
                x = pos.x,
                z = pos.z,
                "saved chunk is damaged; it is shown empty, kept as saved and never changed"
            );
            w.quarantine(pos);
        }
    }
}

/// The view phase (serial, after the inputs and tasks): the worlds made, loaded and unloaded
/// ([`world_api::step`]), then opens the menus that waited for their blocks and closes those
/// their player left behind ([`super::inventory::tick_menus`]), moves each view to its player and
/// its granted radius, picks the chunks to send and the sub-chunk requests to answer this tick, and asks for the
/// chunk loads they, the spawns and the tickets need. Returns the requests for the save thread:
/// the worlds it does not know yet (taken after the last listener that can make one before the
/// loads, so every load names a world the save thread knows), then the loads. A world the
/// chunk events make waits for the next view phase or snapshot.
pub(super) fn update(state: &mut State) -> Vec<Request> {
    world_api::step(state);
    super::inventory::tick_menus(state);
    let State {
        settings,
        players,
        worlds,
        chunks,
        rules,
        ..
    } = state;
    for player in players.values_mut() {
        if player.view.is_open() {
            let limits = settings.view(player.world);
            let center = chunk_of(player.position);
            // The radius granted since the last view phase (`spawn::request_chunk_radius`).
            let radius = player.chunk_radius.unwrap_or(player.view.radius);
            if center != player.view.center || radius != player.view.radius {
                set_view(
                    worlds,
                    chunks,
                    rules,
                    player.world,
                    &mut player.view,
                    center,
                    radius,
                );
            }
            // BDS sends a publisher update every tick (P6), so the client's chunk area follows the
            // player block by block rather than a chunk at a time; repeats of a block are left out.
            let at = BlockPos::containing(player.position);
            if player.view.out.publish || at != player.view.published {
                player.view.out.publish = true;
                player.view.published = at;
            }
            let per_tick = usize::try_from(limits.chunks_per_tick).unwrap_or(usize::MAX);
            pick(worlds, chunks, player, per_tick);
            let spawn_radius = limits.spawn_radius.min(player.view.radius);
            if player.phase == Phase::Joined && player.view.has_sent_within(spawn_radius) {
                player.pending.spawn_status = true;
                player.phase = Phase::Spawning;
            }
        }
        let take = player.view.requests.len().min(SUB_CHUNK_REQUESTS_PER_TICK);
        if take > 0 {
            player.view.out.answers = player.view.requests.drain(..take).collect();
        }
    }
    let moved = move_spawns(worlds, chunks, rules);
    request_spawn_loads(worlds, chunks);
    request_ticket_loads(worlds, chunks);
    let batch = std::mem::take(&mut chunks.batch);
    let deleted = world_api::take_deleted_worlds(state);
    let new = world_api::take_new_worlds(state);
    announce(state, &moved);
    let mut requests = Vec::with_capacity(3);
    if !deleted.is_empty() {
        requests.push(Request::DeleteWorlds(deleted));
    }
    if !new.is_empty() {
        requests.push(Request::Worlds(new));
    }
    if !batch.is_empty() {
        requests.push(Request::Chunks(batch));
    }
    requests
}

/// The end of the view phase: the chunk events of the tick, each built only when someone listens.
/// In order: [`ChunkLoad`] for the chunks loaded or brought back from memory that are still
/// loaded; [`PlayerPostChunkSend`] for the chunks picked to send (the output phase sends them at
/// the end of this tick, before any packet a listener queues); [`SpawnChange`] for each world
/// whose spawn moved; then the chunks nobody holds unload, after [`ChunkUnload`].
fn announce(state: &mut State, moved: &[(WorldIndex, BlockPos)]) {
    let heard = state.chunks.heard;
    if heard.load {
        for ((world, chunk), new_chunk) in std::mem::take(&mut state.chunks.loads) {
            if !state
                .worlds
                .get(world)
                .is_some_and(|w| w.is_chunk_loaded(chunk))
            {
                continue;
            }
            let mut e = ChunkLoad {
                world: crate::world::Id::of(world),
                chunk,
                new_chunk,
            };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
    if heard.sent {
        // Taken while the listeners run, and given back with its capacity afterwards.
        let mut sent = std::mem::take(&mut state.chunks.sent);
        for (session, chunk) in sent.drain(..) {
            if !state
                .players
                .get(&session)
                .is_some_and(|p| p.view.sees(chunk))
            {
                continue;
            }
            let mut e = PlayerPostChunkSend {
                player: player::Id::new(session),
                chunk,
            };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
        sent.append(&mut state.chunks.sent);
        state.chunks.sent = sent;
    }
    for &(world, previous_spawn) in moved {
        let mut e = SpawnChange {
            world: crate::world::Id::of(world),
            previous_spawn,
        };
        dispatch::call(state, &mut e);
        dispatch::watch(state, &e);
    }
    unload_released(state);
}

/// Unloads each chunk nobody holds: those let go since the last view phase and those kept
/// [`KEEP_TICKS`] ago, each after asking [`ChunkUnload`] while it is heard. A cancelled unload keeps the chunk loaded, held by its keep
/// alone, while fewer than [`MAX_KEPT_CHUNKS`] are kept; every other chunk unloads. A chunk held
/// again meanwhile, or not loaded (a load in flight), unloads or stays without asking.
fn unload_released(state: &mut State) {
    let tick = state.tick;
    while let Some(&(due, key)) = state.chunks.kept.first()
        && due <= tick
    {
        state.chunks.kept.pop_first();
        let chunks = &mut state.chunks;
        if let Some(count) = chunks.interest.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                chunks.interest.remove(&key);
                chunks.unloading.insert(key);
            }
        }
    }
    for key in std::mem::take(&mut state.chunks.unloading) {
        if state.chunks.interest.contains_key(&key) {
            continue;
        }
        if !state
            .worlds
            .get(key.0)
            .is_some_and(|w| w.is_chunk_loaded(key.1))
        {
            unload_now(state, key);
            continue;
        }
        if !state.chunks.heard.unload {
            unload_now(state, key);
            continue;
        }
        let mut e = ChunkUnload {
            world: crate::world::Id::of(key.0),
            chunk: key.1,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        // Listeners cannot hold a chunk, but the check is the rule's: a held chunk stays.
        if !state.chunks.interest.contains_key(&key) {
            if e.cancelled && state.chunks.kept.len() < MAX_KEPT_CHUNKS {
                state.chunks.interest.insert(key, 1);
                state
                    .chunks
                    .kept
                    .insert((tick.saturating_add(KEEP_TICKS), key));
            } else {
                if e.cancelled {
                    warn!(
                        kept = MAX_KEPT_CHUNKS,
                        x = key.1.x,
                        z = key.1.z,
                        "too many chunks kept loaded by listeners; this one unloads"
                    );
                }
                unload_now(state, key);
            }
        }
        dispatch::watch(state, &e);
    }
}

/// [`unload`] on the whole state.
fn unload_now(state: &mut State, key: Key) {
    let State {
        worlds,
        chunks,
        rules,
        saving,
        ..
    } = state;
    unload(worlds, chunks, rules, saving, key);
}

/// Picks up to `per_tick` loaded chunks of the view to send, nearest first, and asks for loads
/// of the unloaded ones it passes.
fn pick(worlds: &[World], chunks: &mut Chunks, player: &mut Player, per_tick: usize) {
    let view = &mut player.view;
    view.advance();
    let Some(world) = worlds.get(player.world) else {
        return;
    };
    // Looking past what can be sent this tick finds the next loads early, within a bound.
    let window = per_tick.saturating_mul(4);
    let mut seen = 0;
    for &pos in view.order.get(view.cursor..).unwrap_or_default() {
        if seen >= window || view.out.chunks.len() >= per_tick {
            break;
        }
        if view.sent.contains(&pos) {
            continue;
        }
        seen += 1;
        if world.is_chunk_loaded(pos) || world.is_quarantined(pos) {
            view.out.chunks.push(pos);
            view.sent.insert(pos);
            if chunks.heard.sent {
                chunks.sent.push((player.session, pos));
            }
        } else {
            want(chunks, (player.world, pos));
        }
    }
    view.advance();
}

/// Asks for a load of `key` this tick unless it is in flight or the bounds are reached.
fn want(chunks: &mut Chunks, key: Key) {
    if chunks.requested.contains(&key)
        || chunks.batch.len() >= MAX_LOADS_PER_TICK
        || chunks.in_flight() >= MAX_LOADS_IN_FLIGHT
    {
        return;
    }
    chunks.requested.insert(key);
    chunks.batch.push(key);
}

/// Asks for the held spawn chunks that are not loaded.
fn request_spawn_loads(worlds: &[World], chunks: &mut Chunks) {
    for (world, w) in worlds.iter().enumerate() {
        for pos in spawn_area(w.spawn().chunk()) {
            let key = (world, pos);
            if chunks.interest.contains_key(&key)
                && !w.is_chunk_loaded(pos)
                && !w.is_quarantined(pos)
            {
                want(chunks, key);
            }
        }
    }
}

/// Asks for the chunks the tickets hold that are not loaded; one that loaded (or that nobody
/// holds any more) stops waiting.
fn request_ticket_loads(worlds: &[World], chunks: &mut Chunks) {
    if chunks.waiting.is_empty() {
        return;
    }
    for key in std::mem::take(&mut chunks.waiting) {
        let Some(w) = worlds.get(key.0) else {
            continue;
        };
        if chunks.interest.contains_key(&key)
            && !w.is_chunk_loaded(key.1)
            && !w.is_quarantined(key.1)
        {
            want(chunks, key);
            chunks.waiting.insert(key);
        }
    }
}

/// Holds the chunks a world needs once it is made or loaded: its 3 × 3 spawn chunks.
pub(super) fn hold_world(state: &mut State, world: WorldIndex) {
    let Some(spawn) = state.worlds.get(world).map(World::spawn) else {
        return;
    };
    for pos in spawn_area(spawn.chunk()) {
        hold(
            &mut state.worlds,
            &mut state.chunks,
            &mut state.rules,
            (world, pos),
        );
    }
    match state.chunks.spawns.get_mut(world) {
        Some(held) => *held = spawn,
        // Worlds are numbered in order, so a new one is the next.
        None => state.chunks.spawns.push(spawn),
    }
}

/// Lets go of everything a world that unloads holds (its players are gone already): its spawn
/// chunks, its tickets, the chunks listeners kept; its loads in flight become stale. Then every
/// chunk unloads as any chunk does, its unsaved changes waiting in memory for a commit.
pub(super) fn unload_world(state: &mut State, world: WorldIndex) {
    let chunks = &mut state.chunks;
    let tickets: Vec<u64> = chunks
        .tickets
        .iter()
        .filter(|(_, t)| t.world == world)
        .map(|(&n, _)| n)
        .collect();
    for n in tickets {
        release_ticket(chunks, n);
    }
    if let Some(&spawn) = chunks.spawns.get(world) {
        for pos in spawn_area(spawn.chunk()) {
            release(chunks, (world, pos));
        }
    }
    let all =
        (world, ChunkPos::new(i32::MIN, i32::MIN))..=(world, ChunkPos::new(i32::MAX, i32::MAX));
    let held: Vec<Key> = chunks
        .interest
        .range(all.clone())
        .map(|(&k, _)| k)
        .collect();
    for key in held {
        chunks.interest.remove(&key);
    }
    chunks.kept.retain(|(_, k)| k.0 != world);
    chunks.unloading.retain(|k| k.0 != world);
    chunks.waiting.retain(|k| k.0 != world);
    let asked: Vec<Key> = chunks.requested.range(all).copied().collect();
    for key in asked {
        chunks.requested.remove(&key);
        let n = chunks.stale.entry(key).or_insert(0);
        *n = n.saturating_add(1);
    }
    let loaded: Vec<ChunkPos> = state
        .worlds
        .get(world)
        .map(|w| w.loaded_chunks().collect())
        .unwrap_or_default();
    for pos in loaded {
        unload_now(state, (world, pos));
    }
}

/// Lets go of the chunks of ticket `n`; false if there is no such ticket.
fn release_ticket(chunks: &mut Chunks, n: u64) -> bool {
    let Some(ticket) = chunks.tickets.remove(&n) else {
        return false;
    };
    for pos in ticket.area() {
        release(chunks, (ticket.world, pos));
        chunks.by_chunk.remove(&((ticket.world, pos), n));
    }
    true
}

impl Context<'_> {
    /// Keeps the chunks of `world` within `radius` of `center` (a square of 2 × `radius` + 1
    /// chunks a side) loaded, whether a player is near or not, until
    /// [`release_chunks`](Self::release_chunks) lets the ticket go (PMMP
    /// `World::registerChunkLoader`, for a square at once). The chunks that are not loaded load
    /// from the next view phase on, as players' chunks do: [`ChunkLoad`] says when each has.
    /// A world that unloads drops its tickets.
    ///
    /// Fails, holding nothing, with [`world::Error::WorldNotLoaded`](crate::world::Error) for a
    /// world that is not loaded, [`OutOfRange`](crate::world::Error::OutOfRange) for a center
    /// outside the world border, and [`Full`](crate::world::Error::Full) for a radius over 8,
    /// past 1,024 tickets, or past 4,096 chunks the tickets hold together.
    #[doc(alias = "World::registerChunkLoader")]
    #[doc(alias = "World::loadChunk")]
    #[doc(alias = "World::requestChunkPopulation")]
    #[doc(alias = "World::orderChunkPopulation")]
    pub fn hold_chunks(
        &mut self,
        world: crate::world::Id,
        center: ChunkPos,
        radius: u8,
    ) -> crate::world::Result<crate::world::Ticket> {
        use crate::world::Error;
        let w = world_api::index(self.state, world).ok_or(Error::WorldNotLoaded)?;
        let limit = MAX_CHUNK_COORDINATE.unsigned_abs();
        if center.x.unsigned_abs() > limit || center.z.unsigned_abs() > limit {
            return Err(Error::OutOfRange);
        }
        let held = self.state.chunks.by_chunk.len();
        if radius > MAX_TICKET_RADIUS
            || self.state.chunks.tickets.len() >= MAX_TICKETS
            || held.saturating_add(Ticket::size(radius)) > MAX_TICKET_CHUNKS
        {
            return Err(Error::Full);
        }
        let ticket = Ticket {
            world: w,
            center,
            radius,
        };
        let State {
            worlds,
            chunks,
            rules,
            ..
        } = &mut *self.state;
        let n = chunks.next_ticket;
        chunks.next_ticket = n.saturating_add(1);
        for pos in ticket.area() {
            hold(worlds, chunks, rules, (w, pos));
            chunks.by_chunk.insert(((w, pos), n));
            if worlds
                .get(w)
                .is_some_and(|world| !world.is_chunk_loaded(pos))
            {
                chunks.waiting.insert((w, pos));
            }
        }
        chunks.tickets.insert(n, ticket);
        Ok(crate::world::Ticket::new(n))
    }

    /// Lets the chunks of `ticket` go (PMMP `World::unregisterChunkLoader`): those nobody else
    /// holds unload in the view phase, as players' chunks do. False if the ticket was let go
    /// already or its world unloaded.
    #[doc(alias = "World::unregisterChunkLoader")]
    pub fn release_chunks(&mut self, ticket: crate::world::Ticket) -> bool {
        release_ticket(&mut self.state.chunks, ticket.get())
    }

    /// The tickets that hold chunk `chunk` of `world`, in the order they were given (PMMP
    /// `World::getChunkLoaders`).
    #[doc(alias = "World::getChunkLoaders")]
    #[doc(alias = "World::isChunkInUse")]
    pub fn chunk_tickets(
        &self,
        world: crate::world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = crate::world::Ticket> + '_ {
        let key = (world.index(), chunk);
        self.state
            .chunks
            .by_chunk
            .range((key, 0)..=(key, u64::MAX))
            .map(|&(_, n)| crate::world::Ticket::new(n))
    }
}

/// The encoded sub-chunks this tick's answers need, encoded once for every player who asked: `None`
/// for a sub-chunk of air.
pub(super) type Encoded = BTreeMap<(WorldIndex, ChunkPos, usize), Option<Vec<u8>>>;

/// The sub-chunks the answers of this tick read, each once.
pub(super) fn wanted_sub_chunks(state: &State) -> Vec<(WorldIndex, ChunkPos, usize)> {
    let mut keys = BTreeSet::new();
    for player in state.players.values() {
        let Some(world) = state.worlds.get(player.world) else {
            continue;
        };
        for request in &player.view.out.answers {
            for offset in &request.offsets {
                if let Some((pos, index)) = target(request, *offset)
                    && player.view.sent.contains(&pos)
                    && world.chunk(pos).is_some()
                {
                    keys.insert((player.world, pos, index));
                }
            }
        }
    }
    keys.into_iter().collect()
}

/// Encodes one wanted sub-chunk: its blocks, then the client form of its block entities as
/// network NBT (dragonfly `subChunkEntry`; PMMP sends them with the chunk the same way), so a
/// sign shows its text and a chest its pair as soon as the sub-chunk arrives. `None` for an
/// all-air sub-chunk, which holds no block entity.
pub(super) fn encode(
    worlds: &[World],
    blocks: &super::block_entity::BlockEntities,
    key: (WorldIndex, ChunkPos, usize),
) -> Option<Vec<u8>> {
    let (world, pos, index) = key;
    let mut payload = worlds.get(world)?.chunk(pos)?.sub_chunk_payload(index)?;
    payload.extend_from_slice(&blocks.client_bytes(world, pos, index));
    Some(payload)
}

/// The chunk and the sub-chunk index (0 is the lowest) a request offset names, or `None` if the
/// index is outside the world height.
fn target(request: &SubChunkRequest, offset: [i8; 3]) -> Option<(ChunkPos, usize)> {
    let [x, y, z] = request.position;
    let pos = ChunkPos::new(
        x.saturating_add(i32::from(offset[0])),
        z.saturating_add(i32::from(offset[2])),
    );
    let index = y
        .saturating_add(i32::from(offset[1]))
        .saturating_sub(LOWEST_SUB_CHUNK);
    let index = usize::try_from(index).ok().filter(|&i| i < SUB_CHUNKS)?;
    Some((pos, index))
}

/// Puts in `out` the chunk packets of one player this tick, in BDS's order (P6): the publisher
/// update, the chunks, then the sub-chunk answers, each followed by the fake blocks it holds (the
/// client forgets them with the sub-chunk it replaces); then the positions whose fake block was
/// set or cleared this tick, with what they show now.
pub(super) fn packets(world: &World, player: &Player, encoded: &Encoded, out: &mut Vec<Message>) {
    let view = &player.view;
    out.reserve(
        usize::from(view.out.publish)
            + view.out.chunks.len()
            + view.out.answers.len()
            + view.out.shown.len(),
    );
    if view.out.publish {
        let BlockPos { x, y, z } = view.published;
        out.push(Message::Packet(Packet::NetworkChunkPublisherUpdate(
            NetworkChunkPublisherUpdate {
                position: [x, y, z],
                radius: u32::from(view.radius) << 4,
                saved_chunks: Vec::new(),
            },
        )));
    }
    let mut empty = None;
    for &pos in &view.out.chunks {
        let chunk: &Chunk = match world.chunk(pos) {
            Some(chunk) => chunk,
            None => empty.get_or_insert_with(Chunk::new),
        };
        out.push(Message::Packet(Packet::LevelChunk(LevelChunk {
            position: [pos.x, pos.z],
            sub_chunk_limit: Some(i32::from(chunk.sub_chunk_limit())),
            raw_payload: chunk.level_chunk_payload(),
            ..LevelChunk::default()
        })));
    }
    for request in &view.out.answers {
        out.push(Message::Packet(Packet::SubChunk(answer(
            world,
            player.world,
            view,
            request,
            encoded,
        ))));
        if request.dimension != 0 {
            continue;
        }
        for &offset in &request.offsets {
            if let Some((pos, index)) = target(request, offset)
                && view.sent.contains(&pos)
            {
                let sub_y = i32::try_from(index).unwrap_or(0) + LOWEST_SUB_CHUNK;
                if view.shows_in(pos, sub_y) {
                    out.extend(
                        update_packet(pos, sub_y, &view.shown_in(pos, sub_y)).map(Message::Packet),
                    );
                }
            }
        }
    }
    let mut changed: BTreeMap<(ChunkPos, i32), Vec<(BlockPos, block::State)>> = BTreeMap::new();
    for &pos in &view.out.shown {
        if !view.sent.contains(&pos.chunk()) {
            continue;
        }
        if let Some(block) = view.shown(pos).or_else(|| world.block(pos)) {
            changed
                .entry((pos.chunk(), pos.y >> 4))
                .or_default()
                .push((pos, block));
        }
    }
    for ((chunk, sub_y), blocks) in &changed {
        out.extend(update_packet(*chunk, *sub_y, blocks).map(Message::Packet));
    }
}

/// The packet that sets `blocks` of sub-chunk `sub_y` of `chunk` on a client: an `UpdateBlock`
/// for one block, an `UpdateSubChunkBlocks` for several (BDS, P6); none for none.
fn update_packet(
    chunk: ChunkPos,
    sub_y: i32,
    blocks: &[(BlockPos, block::State)],
) -> Option<Packet> {
    match blocks {
        [] => None,
        [(at, state)] => Some(Packet::UpdateBlock(UpdateBlock {
            position: [at.x, at.y, at.z],
            new_block_runtime_id: state.network_id(),
            flags: UPDATE_FLAGS,
            layer: 0,
        })),
        _ => Some(Packet::UpdateSubChunkBlocks(UpdateSubChunkBlocks {
            position: [
                chunk.x.saturating_mul(16),
                sub_y.saturating_mul(16),
                chunk.z.saturating_mul(16),
            ],
            blocks: blocks
                .iter()
                .map(|(at, state)| BlockChangeEntry {
                    block_pos: [at.x, at.y, at.z],
                    block_runtime_id: state.network_id(),
                    flags: UPDATE_FLAGS,
                    synced_update_entity_unique_id: u64::MAX,
                    synced_update_type: 0,
                })
                .collect(),
            extra: Vec::new(),
        })),
    }
}

/// The answer to one sub-chunk request: one entry per offset, in order (dragonfly
/// `ViewSubChunks`, values from P6).
fn answer(
    world: &World,
    world_index: WorldIndex,
    view: &View,
    request: &SubChunkRequest,
    encoded: &Encoded,
) -> SubChunk {
    let mut entries = Vec::with_capacity(request.offsets.len());
    // A request for another dimension gets no entries (dragonfly): the overworld is the only one.
    if request.dimension == 0 {
        for &offset in &request.offsets {
            entries.push(entry(world, world_index, view, request, offset, encoded));
        }
    }
    SubChunk {
        cache_enabled: false,
        dimension: request.dimension,
        position: request.position,
        sub_chunk_entries: entries,
    }
}

fn entry(
    world: &World,
    world_index: WorldIndex,
    view: &View,
    request: &SubChunkRequest,
    offset: [i8; 3],
    encoded: &Encoded,
) -> SubChunkEntry {
    // The height map types not set stay 0, `HeightMapData::None`, the entries' default.
    let failed = |result: SubChunkResult| SubChunkEntry {
        offset,
        result: u8::from(result),
        ..SubChunkEntry::default()
    };
    let Some((pos, index)) = target(request, offset) else {
        return failed(SubChunkResult::IndexOutOfBounds);
    };
    if !view.sent.contains(&pos) {
        return failed(SubChunkResult::ChunkNotFound);
    }
    let (height_map_type, height_map_data) = match world.chunk(pos) {
        Some(chunk) => height_map(chunk, index),
        None => height_map(&Chunk::new(), index),
    };
    let render_height_map_type = if height_map_type == HeightMapData::HasData {
        HeightMapData::AllCopied
    } else {
        height_map_type
    };
    let payload = encoded
        .get(&(world_index, pos, index))
        .and_then(Option::as_ref);
    SubChunkEntry {
        offset,
        result: u8::from(if payload.is_some() {
            SubChunkResult::Success
        } else {
            SubChunkResult::SuccessAllAir
        }),
        raw_payload: payload.cloned(),
        height_map_type: u8::from(height_map_type),
        height_map_data,
        render_height_map_type: u8::from(render_height_map_type),
        render_height_map_data: None,
        blob_hash: None,
    }
}

/// The height map of the sub-chunk at `index` (P6): per column the highest block's y relative to
/// the sub-chunk's bottom, plus 1; 16 if it is in a higher sub-chunk, -1 if lower or the column
/// is air. If every column is higher or every one lower, the type says so without data.
fn height_map(chunk: &Chunk, index: usize) -> (HeightMapData, Option<HeightMap>) {
    let bottom = (i32::try_from(index).unwrap_or(0) + LOWEST_SUB_CHUNK) * 16;
    let mut map: HeightMap = [[0; 16]; 16];
    let (mut all_higher, mut all_lower) = (true, true);
    for (i, &height) in chunk.heights().iter().enumerate() {
        let value = if height == NO_HEIGHT || i32::from(height) < bottom {
            all_higher = false;
            -1
        } else if i32::from(height) >= bottom + 16 {
            all_lower = false;
            16
        } else {
            (all_higher, all_lower) = (false, false);
            // 1 to 16: the height is in this sub-chunk.
            i8::try_from(i32::from(height) - bottom + 1).unwrap_or(16)
        };
        if let Some(cell) = map.get_mut(i >> 4).and_then(|row| row.get_mut(i & 15)) {
            *cell = value;
        }
    }
    if all_higher {
        (HeightMapData::TooHigh, None)
    } else if all_lower {
        (HeightMapData::TooLow, None)
    } else {
        (HeightMapData::HasData, Some(map))
    }
}

/// The block changes of a tick by world, each world's grouped by sub-chunk: (chunk, sub-chunk y)
/// to the changed blocks with their state, in position order. Only worlds with changes are in
/// it, so each player reads its own world's alone.
pub(super) type Changes =
    BTreeMap<WorldIndex, BTreeMap<(ChunkPos, i32), Vec<(BlockPos, block::State)>>>;

/// Takes the block changes of every world.
pub(super) fn take_changes(worlds: &mut [World]) -> Changes {
    let mut changes = Changes::new();
    for (i, world) in worlds.iter_mut().enumerate() {
        let mut by_sub_chunk: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (pos, state) in world.take_changes() {
            by_sub_chunk
                .entry((pos.chunk(), pos.y >> 4))
                .or_default()
                .push((pos, state));
        }
        if !by_sub_chunk.is_empty() {
            changes.insert(i, by_sub_chunk);
        }
    }
    changes
}

/// Puts in `out` the block change packets for a player: for each changed sub-chunk of a chunk it
/// was sent, an `UpdateBlock` for one block or an `UpdateSubChunkBlocks` for several (BDS, P6). A
/// change where the player is shown a fake block, or where [`packets`] sends what it shows, is
/// left out.
pub(super) fn block_packets(player: &Player, changes: &Changes, out: &mut Vec<Message>) {
    let view = &player.view;
    let Some(changes) = changes.get(&player.world) else {
        return;
    };
    for ((pos, sub_y), blocks) in changes {
        if !view.sent.contains(pos) {
            continue;
        }
        if !view.shows_in(*pos, *sub_y) && view.out.shown.is_empty() {
            out.extend(update_packet(*pos, *sub_y, blocks).map(Message::Packet));
            continue;
        }
        let real: Vec<(BlockPos, block::State)> = blocks
            .iter()
            .filter(|(at, _)| view.shown(*at).is_none() && !view.out.shown.contains(at))
            .copied()
            .collect();
        out.extend(update_packet(*pos, *sub_y, &real).map(Message::Packet));
    }
}

#[cfg(test)]
mod tests {
    //! The chunk events: loads, unloads and the chunks a listener keeps, the chunks sent to a
    //! player and the spawn moving.

    use std::cell::Cell;

    use super::*;
    use crate::event::{Event, Listeners, Priority};
    use crate::player::Message;
    use crate::protocol::packet::Packet;
    use crate::tick::Output;
    use crate::tick::tests::{join, leave, new_state, radius, received, settings, tick};
    use crate::world::Flat;

    thread_local! {
        /// What the listeners of a test saw, in order.
        static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
        /// Whether the unload listener cancels.
        static KEEP: Cell<bool> = const { Cell::new(true) };
    }

    fn log(line: String) {
        let mut lines = LOG.take();
        lines.push(line);
        LOG.set(lines);
    }

    fn logged() -> Vec<String> {
        LOG.take()
    }

    fn on<E: Event>(listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static) -> Listeners {
        let mut l = Listeners::default();
        l.push(Priority::Normal, listener);
        l
    }

    fn state(listeners: Listeners) -> State {
        LOG.set(Vec::new());
        KEEP.set(true);
        new_state(settings(|_| {}), listeners)
    }

    /// A chunk far from the spawn and its preloaded chunks.
    const FAR: Key = (0, ChunkPos::new(40, 40));

    /// Holds `key` and loads it as the generator made it.
    fn hold_loaded(state: &mut State, key: Key) {
        hold(&mut state.worlds, &mut state.chunks, &mut state.rules, key);
        let chunk = Box::new(Chunk::generate(&Flat, 0, key.1));
        let stored = crate::tick::extra::tests::sample(key.0, key.1);
        loaded(state, key.0, key.1, Loaded::Generated(chunk, stored));
    }

    fn let_go(state: &mut State, key: Key) {
        release(&mut state.chunks, key);
    }

    fn is_loaded(state: &State, key: Key) -> bool {
        state.worlds[key.0].is_chunk_loaded(key.1)
    }

    /// Unload listeners that log, cancel while [`KEEP`] says so, and a monitor that logs what
    /// was applied.
    fn keeping() -> Listeners {
        let mut l = on(|e: &mut ChunkUnload, _: &mut Context<'_>| {
            log(format!("unload {} {}", e.chunk().x, e.chunk().z));
            if KEEP.get() {
                e.cancel();
            }
        });
        l.push_monitor(|e: &ChunkUnload, cx: &Context<'_>| {
            let loaded = cx.world(e.world()).unwrap().is_chunk_loaded(e.chunk());
            log(format!("monitor {loaded}"));
        });
        l
    }

    /// The radius requests of one tick resize the view once, to the last radius: requests that
    /// end on the radius they started with let go of no chunk and send none again.
    #[test]
    fn the_radius_requests_of_a_tick_resize_the_view_once() {
        let mut state = state(Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 6)], &mut output);
        let session = crate::tick::SessionId::new(1);
        for _ in 0..128 {
            if state.players[&session].view.is_complete() {
                break;
            }
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        assert!(state.players[&session].view.is_complete());
        received(&mut rx);
        tick(
            &mut state,
            &mut vec![radius(1, 2, 1), radius(1, 3, 6)],
            &mut output,
        );
        for _ in 0..8 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        let again = received(&mut rx)
            .0
            .iter()
            .filter(|p| matches!(p, Packet::LevelChunk(_)))
            .count();
        assert_eq!(again, 0);
        let view = &state.players[&session].view;
        assert!(view.radius() == 6 && view.is_complete());
    }

    // pmmp: ChunkLoadEvent, ChunkPopulateEvent
    #[test]
    fn chunk_loads_name_new_and_restored_chunks_once() {
        let mut listeners = on(|e: &mut ChunkLoad, cx: &mut Context<'_>| {
            let loaded = cx.world(e.world()).unwrap().is_chunk_loaded(e.chunk());
            log(format!(
                "{} {} {} {loaded}",
                e.chunk().x,
                e.chunk().z,
                e.is_new_chunk()
            ));
        });
        listeners.push_monitor(|e: &ChunkLoad, _: &Context<'_>| {
            log(format!("monitor {}", e.chunk().x));
        });
        let mut state = state(listeners);
        hold_loaded(&mut state, FAR);
        announce(&mut state, &[]);
        assert_eq!(logged(), ["40 40 true true", "monitor 40"]);
        // A change, then the chunk unloads before a save: it comes back from memory.
        let stone = block::State::find("minecraft:stone", &[]).unwrap();
        state.worlds[0]
            .set_block(BlockPos::new(640, -60, 640), stone)
            .unwrap();
        let_go(&mut state, FAR);
        announce(&mut state, &[]);
        assert!(logged().is_empty());
        assert!(!is_loaded(&state, FAR) && state.chunks.unsaved.contains_key(&FAR));
        hold(&mut state.worlds, &mut state.chunks, &mut state.rules, FAR);
        announce(&mut state, &[]);
        assert_eq!(logged(), ["40 40 false true", "monitor 40"]);
        // A chunk let go before the view phase is still loaded there: it is announced, then
        // unloads.
        let other = (0, ChunkPos::new(-40, 40));
        hold_loaded(&mut state, other);
        let_go(&mut state, other);
        announce(&mut state, &[]);
        assert_eq!(logged(), ["-40 40 true true", "monitor -40"]);
        assert!(!is_loaded(&state, other) && state.chunks.loads.is_empty());
    }

    // pmmp: ChunkUnloadEvent
    #[test]
    fn a_cancelled_unload_keeps_the_chunk_until_it_is_asked_again() {
        let mut state = state(keeping());
        hold_loaded(&mut state, FAR);
        let_go(&mut state, FAR);
        assert!(is_loaded(&state, FAR), "the listeners are asked first");
        announce(&mut state, &[]);
        assert_eq!(logged(), ["unload 40 40", "monitor true"]);
        assert!(is_loaded(&state, FAR) && state.chunks.holds(FAR));
        // Asked again only when its time comes.
        state.tick += KEEP_TICKS - 1;
        announce(&mut state, &[]);
        assert!(logged().is_empty());
        state.tick += 1;
        KEEP.set(false);
        announce(&mut state, &[]);
        assert_eq!(logged(), ["unload 40 40", "monitor false"]);
        assert!(!is_loaded(&state, FAR) && !state.chunks.holds(FAR));
        assert!(state.chunks.kept.is_empty() && state.chunks.unloading.is_empty());
    }

    // pmmp: ChunkUnloadEvent
    #[test]
    fn a_chunk_held_again_before_the_view_phase_is_not_asked_about() {
        let mut state = state(keeping());
        hold_loaded(&mut state, FAR);
        let_go(&mut state, FAR);
        hold(&mut state.worlds, &mut state.chunks, &mut state.rules, FAR);
        announce(&mut state, &[]);
        assert!(logged().is_empty());
        assert!(is_loaded(&state, FAR) && state.chunks.unloading.is_empty());
    }

    // pmmp: ChunkUnloadEvent
    #[test]
    fn listeners_keep_at_most_the_bound_of_chunks() {
        let mut state = state(keeping());
        let keys: Vec<Key> = (0..i32::try_from(MAX_KEPT_CHUNKS).unwrap() + 6)
            .map(|i| (0, ChunkPos::new(100 + i, 100)))
            .collect();
        for &key in &keys {
            hold_loaded(&mut state, key);
            let_go(&mut state, key);
        }
        announce(&mut state, &[]);
        assert_eq!(state.chunks.kept.len(), MAX_KEPT_CHUNKS);
        let loaded = keys.iter().filter(|&&k| is_loaded(&state, k)).count();
        assert_eq!(loaded, MAX_KEPT_CHUNKS);
    }

    #[test]
    fn without_unload_listeners_a_chunk_unloads_in_the_view_phase_too() {
        let mut state = state(Listeners::default());
        hold_loaded(&mut state, FAR);
        let_go(&mut state, FAR);
        assert!(is_loaded(&state, FAR), "as with listeners");
        announce(&mut state, &[]);
        assert!(!is_loaded(&state, FAR) && state.chunks.unloading.is_empty());
    }

    // pmmp: ChunkUnloadEvent, PlayerPostChunkSendEvent, ChunkLoadEvent
    #[test]
    fn join_and_leave_cycles_with_chunk_listeners_return_the_tables_to_their_start() {
        let mut listeners = keeping();
        listeners.push(
            Priority::Normal,
            |_: &mut ChunkLoad, _: &mut Context<'_>| {},
        );
        listeners.push(
            Priority::Normal,
            |_: &mut PlayerPostChunkSend, _: &mut Context<'_>| {},
        );
        let mut state = state(listeners);
        KEEP.set(false);
        let mut output = Output::default();
        let start = state.chunks.held();
        for round in 0..3 {
            let (j, _rx) = join(1, round * 10);
            tick(&mut state, &mut vec![j], &mut output);
            tick(
                &mut state,
                &mut vec![radius(1, round * 10 + 1, 6)],
                &mut output,
            );
            tick(&mut state, &mut Vec::new(), &mut output);
            tick(&mut state, &mut vec![leave(1, round * 10 + 2)], &mut output);
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        assert_eq!(state.chunks.held(), start);
        assert!(state.chunks.kept.is_empty() && state.chunks.unloading.is_empty());
        assert!(state.chunks.loads.is_empty() && state.chunks.sent.is_empty());
        // The buffer of sent chunks keeps its capacity across ticks (no allocation per tick).
        assert!(state.chunks.sent.capacity() > 0);
        assert!(logged().iter().any(|l| l == "monitor false"));
    }

    // pmmp: PlayerPostChunkSendEvent, PlayerPostChunkSendEvent::getChunkX, PlayerPostChunkSendEvent::getChunkZ
    #[test]
    fn post_chunk_send_names_each_chunk_sent_once_and_comes_after_it() {
        let mut listeners = on(|e: &mut PlayerPostChunkSend, cx: &mut Context<'_>| {
            log(format!("{} {}", e.chunk().x, e.chunk().z));
            let player = cx.player_mut(e.player()).unwrap();
            player
                .send(Message::Tip(format!("{}", e.chunk().x)))
                .unwrap();
        });
        listeners.push_monitor(|e: &PlayerPostChunkSend, cx: &Context<'_>| {
            let sees = cx.state.players[&e.player().session()].view.sees(e.chunk());
            assert!(sees);
        });
        let mut state = state(listeners);
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        received(&mut rx);
        tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
        let center = state.worlds[0].spawn().chunk();
        let mut expected: Vec<String> = disc(center, 2)
            .into_iter()
            .map(|p| format!("{} {}", p.x, p.z))
            .collect();
        let mut seen = logged();
        assert_eq!(seen, expected, "in sending order");
        seen.sort();
        seen.dedup();
        expected.sort();
        assert_eq!(seen, expected, "each once");
        let packets = received(&mut rx).0;
        let last_chunk = packets
            .iter()
            .rposition(|p| matches!(p, Packet::LevelChunk(_)))
            .unwrap();
        let first_tip = packets
            .iter()
            .position(|p| matches!(p, Packet::Text(_)))
            .unwrap();
        assert!(first_tip > last_chunk);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(logged().is_empty(), "a chunk is sent once");
    }

    // pmmp: SpawnChangeEvent
    #[test]
    fn a_spawn_change_names_the_spawn_before() {
        let mut listeners = on(|e: &mut SpawnChange, cx: &mut Context<'_>| {
            let now = cx.world(e.world()).unwrap().spawn();
            let before = e.previous_spawn();
            log(format!("{} {} {}", before.x, now.x, now.z));
        });
        listeners.push_monitor(|e: &SpawnChange, cx: &Context<'_>| {
            let now = cx.world(e.world()).unwrap().spawn();
            log(format!(
                "monitor {}",
                cx.state.chunks.holds((0, now.chunk()))
            ));
        });
        let mut state = state(listeners);
        let mut output = Output::default();
        let spawn = state.worlds[0].spawn();
        // Within the same chunk, then far away.
        let near = BlockPos::new(spawn.x + 1, spawn.y, spawn.z);
        state.worlds[0].set_spawn(near);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(
            logged(),
            [
                format!("{} {} {}", spawn.x, near.x, near.z),
                "monitor true".to_owned()
            ]
        );
        let far = BlockPos::new(160, spawn.y, 160);
        state.worlds[0].set_spawn(far);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(
            logged(),
            [format!("{} 160 160", near.x), "monitor true".to_owned()]
        );
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(logged().is_empty());
        assert!(state.chunks.holds((0, far.chunk())));
    }

    /// The packets one tick sends to a player who joins and asks for a radius of 1,000 in the
    /// test world `world`, with `edit` applied to the settings.
    fn first_tick(edit: impl FnOnce(&mut crate::Config)) -> (State, Vec<Packet>) {
        let mut state = new_state(settings(edit), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 1_000)], &mut output);
        (state, received(&mut rx).0)
    }

    #[test]
    fn a_world_table_sets_the_view_values_of_its_world() {
        let granted = |packets: &[Packet]| {
            packets.iter().find_map(|p| match p {
                Packet::ChunkRadiusUpdated(c) => Some(c.chunk_radius),
                _ => None,
            })
        };
        let tick_radius = |packets: &[Packet]| {
            packets.iter().find_map(|p| match p {
                Packet::StartGame(s) => Some(s.server_chunk_tick_radius),
                _ => None,
            })
        };
        let chunks = |packets: &[Packet]| {
            packets
                .iter()
                .filter(|p| matches!(p, Packet::LevelChunk(_)))
                .count()
        };
        let (global, sent) = first_tick(|_| {});
        assert_eq!(granted(&sent), Some(16));
        assert_eq!(tick_radius(&sent), Some(4));
        assert!(chunks(&sent) > 7, "the global 256 chunks per tick");
        let (own, sent) = first_tick(|c| {
            c.worlds.insert(
                "world".to_owned(),
                crate::config::WorldSettings {
                    view_distance: Some(6),
                    chunks_per_tick: Some(7),
                    spawn_radius: Some(2),
                    tick_radius: Some(9),
                    spawn: None,
                },
            );
        });
        assert_eq!(granted(&sent), Some(6), "its view distance caps the radius");
        assert_eq!(
            tick_radius(&sent),
            Some(6),
            "its tick radius, at most its view"
        );
        assert_eq!(chunks(&sent), 7, "its chunks per tick");
        let limits = own.settings.view(0);
        assert_eq!((limits.spawn_radius, limits.tick_radius), (2, 6));
        assert_eq!(
            global.settings.view(0),
            own.settings.view(1),
            "other worlds: global"
        );
    }
}
