//! What each player sees of its world (PLAN.md 10.6): the chunks around it, the chunk loads they
//! need, the sub-chunk request answers and the block changes it is sent.
//!
//! Chunks go out in the sub-chunk request system, as BDS sends them (P6): a `LevelChunk` with the
//! biomes, then the client asks for the sub-chunks it wants. A chunk stays loaded while a player
//! sees it or it is a spawn chunk; an unloaded chunk with unsaved changes waits in memory for the
//! next commit (`save`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tracing::{debug, error};

use super::extra::Extras;
use super::rules::Rules;
use super::save::{Saving, Unsaved, keep_unloaded};
use super::{Phase, Player, State};
use crate::block;
use crate::math::Vec3;
use crate::protocol::packet::{
    LevelChunk, NetworkChunkPublisherUpdate, Packet, SubChunk, SubChunkRequest, UpdateBlock,
    UpdateSubChunkBlocks,
};
use crate::protocol::{BlockChangeEntry, HeightMap, SubChunkEntry};
use crate::save::{Loaded, Request, WorldIndex};
use crate::world::{BlockPos, Chunk, ChunkPos, Kind, NO_HEIGHT, SUB_CHUNKS, World};

/// Chunk loads the save thread may have in flight at once.
pub(super) const MAX_LOADS_IN_FLIGHT: usize = 1024;
/// Chunk loads one tick may ask for.
const MAX_LOADS_PER_TICK: usize = 256;
/// Unanswered sub-chunk requests a player may have queued; more are dropped.
const MAX_QUEUED_SUB_CHUNK_REQUESTS: usize = 256;
/// Sub-chunk requests answered per player per tick; the rest wait for the next tick.
const SUB_CHUNK_REQUESTS_PER_TICK: usize = 64;
/// The lowest sub-chunk y index (`SubChunkRequest` positions are absolute indices).
const LOWEST_SUB_CHUNK: i32 = -4;
/// `UpdateBlock` flags BDS sends (P6): neighbours and network.
const UPDATE_FLAGS: u32 = 3;
/// `SubChunkEntry` results (gophertunnel `SubChunkResult*`).
const RESULT_SUCCESS: u8 = 1;
const RESULT_CHUNK_NOT_FOUND: u8 = 2;
const RESULT_INDEX_OUT_OF_BOUNDS: u8 = 5;
const RESULT_SUCCESS_ALL_AIR: u8 = 6;
/// Height map types (gophertunnel `HeightMapData*`); 0, none, is the entries' default.
const HEIGHT_MAP_HAS_DATA: u8 = 1;
const HEIGHT_MAP_TOO_HIGH: u8 = 2;
const HEIGHT_MAP_TOO_LOW: u8 = 3;
const HEIGHT_MAP_ALL_COPIED: u8 = 4;

/// A chunk of one of the server's worlds.
pub(super) type Key = (WorldIndex, ChunkPos);

/// The chunks a player sees and what it is sent of them.
#[derive(Debug, Default)]
pub(super) struct View {
    /// The chunk the view is around.
    center: ChunkPos,
    /// The granted chunk radius; 0 until the client asked for one.
    radius: u8,
    /// The chunks of the view, nearest first (then by x and z).
    order: Vec<ChunkPos>,
    /// Every chunk of `order` before it has been sent.
    cursor: usize,
    /// The chunks the client was sent and still sees.
    sent: BTreeSet<ChunkPos>,
    /// Sub-chunk requests waiting for an answer, oldest first.
    requests: VecDeque<SubChunkRequest>,
    /// What the output phase sends this tick; empty between ticks.
    pub(super) out: Out,
}

/// The chunk packets of one tick for one player.
#[derive(Debug, Default)]
pub(super) struct Out {
    /// A publisher update: the view moved or changed size.
    publish: bool,
    /// Chunks to send, in order.
    chunks: Vec<ChunkPos>,
    /// Sub-chunk requests to answer, in order.
    answers: Vec<SubChunkRequest>,
}

impl Out {
    pub(super) fn is_empty(&self) -> bool {
        !self.publish && self.chunks.is_empty() && self.answers.is_empty()
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
            order: _,
            cursor,
            sent,
            requests,
            out: _,
        } = self;
        h.u64(u64::from(*radius));
        for c in [center.x, center.z] {
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
    }
}

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
    let [x, _, z] = block_of(position);
    BlockPos::new(x, 0, z).chunk()
}

/// The block an eye position is in.
fn block_of(position: Vec3) -> [i32; 3] {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: positions are finite and inside the world border, which i32 holds"
    )]
    [position.x, position.y, position.z].map(|c| c.floor() as i32)
}

/// The loaded chunks, the chunk loads in flight and the unloaded chunks waiting for a commit.
#[derive(Debug, Default)]
pub(super) struct Chunks {
    /// How many views (and spawn areas) hold each chunk; a chunk is loaded or wanted exactly
    /// while it is here.
    interest: BTreeMap<Key, u32>,
    /// Chunks asked of the save thread and not answered yet.
    pub(super) requested: BTreeSet<Key>,
    /// Unloaded chunks with changes no commit holds yet (PLAN.md 10.6).
    pub(super) unsaved: BTreeMap<Key, Unsaved>,
    /// Loads to ask for at the end of this tick; empty between ticks.
    batch: Vec<Key>,
    /// The extras of the loaded chunks (PLAN.md 10.7).
    pub(super) extras: Extras,
}

impl Chunks {
    /// Whether no chunk is held, wanted, in flight or unsaved (for leak tests).
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.requested.is_empty() && self.unsaved.is_empty() && self.batch.is_empty()
    }

    /// The chunks held, wanted or in flight, for tests.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.interest.len()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The batch is empty between ticks.
        let Self {
            interest,
            requested,
            unsaved,
            batch: _,
            extras,
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
    }
}

/// Adds one holder of `key`. A chunk unloaded with unsaved changes comes back from memory, its
/// terrain and extras with their dirty marks, so a load never reads a file older than the tick's
/// data (PLAN.md 10.6).
fn hold(worlds: &mut [World], chunks: &mut Chunks, rules: &mut Rules, key: Key) {
    let count = chunks.interest.entry(key).or_insert(0);
    *count = count.saturating_add(1);
    if *count > 1 {
        return;
    }
    if let Some(Unsaved { chunk, extras, .. }) = chunks.unsaved.remove(&key)
        && let Some(world) = worlds.get_mut(key.0)
    {
        world.restore_chunk(key.1, chunk);
        rules.load(key, extras.extras(), world);
        chunks.extras.chunk_restored(key, extras);
    }
}

/// Removes one holder of `key`; the last one unloads the chunk and its extras. A chunk of a
/// persistent world whose changes no commit holds yet stays in memory until one does.
fn release(
    worlds: &mut [World],
    chunks: &mut Chunks,
    rules: &mut Rules,
    saving: &Saving,
    key: Key,
) {
    let Some(count) = chunks.interest.get_mut(&key) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count > 0 {
        return;
    }
    chunks.interest.remove(&key);
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

/// Holds the spawn chunks of every world (3 × 3 around the spawn, as PMMP keeps them), so they
/// stay loaded without players.
pub(super) fn hold_spawns(state: &mut State) {
    let spawns: Vec<(WorldIndex, ChunkPos)> = state
        .worlds
        .iter()
        .enumerate()
        .map(|(i, w)| (i, w.spawn().chunk()))
        .collect();
    for (world, center) in spawns {
        for x in -1..=1 {
            for z in -1..=1 {
                let pos = ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z));
                hold(
                    &mut state.worlds,
                    &mut state.chunks,
                    &mut state.rules,
                    (world, pos),
                );
            }
        }
    }
}

/// Opens or moves a player's view: the chunks it gains are held, those it loses released and
/// forgotten as sent.
#[expect(
    clippy::too_many_arguments,
    reason = "performance: split borrows of the tick state let the player loop hold a player and the chunk tables at once"
)]
fn set_view(
    worlds: &mut [World],
    chunks: &mut Chunks,
    rules: &mut Rules,
    saving: &Saving,
    world: WorldIndex,
    view: &mut View,
    center: ChunkPos,
    radius: u8,
) {
    let order = disc(center, radius);
    let old: BTreeSet<ChunkPos> = view.order.iter().copied().collect();
    let new: BTreeSet<ChunkPos> = order.iter().copied().collect();
    for &pos in new.difference(&old) {
        hold(worlds, chunks, rules, (world, pos));
    }
    for &pos in old.difference(&new) {
        release(worlds, chunks, rules, saving, (world, pos));
        view.sent.remove(&pos);
    }
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
        release(
            &mut state.worlds,
            &mut state.chunks,
            &mut state.rules,
            &state.saving,
            (world, pos),
        );
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
        &state.saving,
        player.world,
        &mut player.view,
        center,
        radius,
    );
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
/// a damaged one is quarantined, extras included (PLAN.md 10.6, 10.7).
pub(super) fn loaded(state: &mut State, world: WorldIndex, pos: ChunkPos, loaded: Loaded) {
    let key = (world, pos);
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

/// The view phase (serial, after the inputs and tasks): moves each view to its player, picks the
/// chunks to send and the sub-chunk requests to answer this tick, and asks for the chunk loads
/// they need. Returns the load request, if any.
pub(super) fn update(state: &mut State) -> Option<Request> {
    let per_tick = usize::try_from(state.settings.chunks_per_tick).unwrap_or(usize::MAX);
    let spawn_radius = state.settings.spawn_radius;
    let State {
        players,
        worlds,
        chunks,
        rules,
        saving,
        ..
    } = state;
    for player in players.values_mut() {
        if player.view.is_open() {
            let center = chunk_of(player.position);
            if center != player.view.center {
                let radius = player.view.radius;
                set_view(
                    worlds,
                    chunks,
                    rules,
                    saving,
                    player.world,
                    &mut player.view,
                    center,
                    radius,
                );
            }
            pick(worlds, chunks, player, per_tick);
            let spawn_radius = spawn_radius.min(player.view.radius);
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
    request_spawn_loads(worlds, chunks);
    let batch = std::mem::take(&mut chunks.batch);
    (!batch.is_empty()).then_some(Request::Chunks(batch))
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
        || chunks.requested.len() >= MAX_LOADS_IN_FLIGHT
    {
        return;
    }
    chunks.requested.insert(key);
    chunks.batch.push(key);
}

/// Asks for the held spawn chunks that are not loaded.
fn request_spawn_loads(worlds: &[World], chunks: &mut Chunks) {
    for (world, w) in worlds.iter().enumerate() {
        let center = w.spawn().chunk();
        for x in -1..=1 {
            for z in -1..=1 {
                let pos = ChunkPos::new(center.x.saturating_add(x), center.z.saturating_add(z));
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
}

/// The encoded sub-chunks this tick's answers need, encoded once for every player who asked
/// (PLAN.md 10.6): `None` for a sub-chunk of air.
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

/// The chunk packets of one player this tick, in BDS's order (P6): the publisher update, the
/// chunks, then the sub-chunk answers.
pub(super) fn packets(world: &World, player: &Player, encoded: &Encoded) -> Vec<Packet> {
    let view = &player.view;
    let mut out = Vec::with_capacity(
        usize::from(view.out.publish) + view.out.chunks.len() + view.out.answers.len(),
    );
    if view.out.publish {
        let [x, y, z] = block_of(player.position);
        out.push(Packet::NetworkChunkPublisherUpdate(
            NetworkChunkPublisherUpdate {
                position: [x, y, z],
                radius: u32::from(view.radius) << 4,
                saved_chunks: Vec::new(),
            },
        ));
    }
    let empty = Chunk::new();
    for &pos in &view.out.chunks {
        let chunk = world.chunk(pos).unwrap_or(&empty);
        out.push(Packet::LevelChunk(LevelChunk {
            position: [pos.x, pos.z],
            sub_chunk_limit: Some(i32::from(chunk.sub_chunk_limit())),
            raw_payload: chunk.level_chunk_payload(),
            ..LevelChunk::default()
        }));
    }
    for request in &view.out.answers {
        out.push(Packet::SubChunk(answer(
            world,
            player.world,
            view,
            request,
            encoded,
        )));
    }
    out
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
    let failed = |result| SubChunkEntry {
        offset,
        result,
        ..SubChunkEntry::default()
    };
    let Some((pos, index)) = target(request, offset) else {
        return failed(RESULT_INDEX_OUT_OF_BOUNDS);
    };
    if !view.sent.contains(&pos) {
        return failed(RESULT_CHUNK_NOT_FOUND);
    }
    let empty = Chunk::new();
    let chunk = world.chunk(pos).unwrap_or(&empty);
    let (height_map_type, height_map_data) = height_map(chunk, index);
    let render_height_map_type = if height_map_type == HEIGHT_MAP_HAS_DATA {
        HEIGHT_MAP_ALL_COPIED
    } else {
        height_map_type
    };
    let payload = encoded
        .get(&(world_index, pos, index))
        .and_then(Option::as_ref);
    SubChunkEntry {
        offset,
        result: if payload.is_some() {
            RESULT_SUCCESS
        } else {
            RESULT_SUCCESS_ALL_AIR
        },
        raw_payload: payload.cloned(),
        height_map_type,
        height_map_data,
        render_height_map_type,
        render_height_map_data: None,
        blob_hash: None,
    }
}

/// The height map of the sub-chunk at `index` (P6): per column the highest block's y relative to
/// the sub-chunk's bottom, plus 1; 16 if it is in a higher sub-chunk, -1 if lower or the column
/// is air. If every column is higher or every one lower, the type says so without data.
fn height_map(chunk: &Chunk, index: usize) -> (u8, Option<HeightMap>) {
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
        (HEIGHT_MAP_TOO_HIGH, None)
    } else if all_lower {
        (HEIGHT_MAP_TOO_LOW, None)
    } else {
        (HEIGHT_MAP_HAS_DATA, Some(map))
    }
}

/// The block changes of a tick grouped by sub-chunk: (world, chunk, sub-chunk y) to the changed
/// blocks with their state, in position order.
pub(super) type Changes = BTreeMap<(WorldIndex, ChunkPos, i32), Vec<(BlockPos, block::State)>>;

/// Takes the block changes of every world.
pub(super) fn take_changes(worlds: &mut [World]) -> Changes {
    let mut changes = Changes::new();
    for (i, world) in worlds.iter_mut().enumerate() {
        for (pos, state) in world.take_changes() {
            changes
                .entry((i, pos.chunk(), pos.y >> 4))
                .or_default()
                .push((pos, state));
        }
    }
    changes
}

/// The block change packets for a player: for each changed sub-chunk of a chunk it was sent, an
/// `UpdateBlock` for one block or an `UpdateSubChunkBlocks` for several (BDS, P6).
pub(super) fn block_packets(player: &Player, changes: &Changes) -> Vec<Packet> {
    let mut out = Vec::new();
    for ((world, pos, sub_y), blocks) in changes {
        if *world != player.world || !player.view.sent.contains(pos) {
            continue;
        }
        match blocks.as_slice() {
            [(at, state)] => out.push(Packet::UpdateBlock(UpdateBlock {
                position: [at.x, at.y, at.z],
                new_block_runtime_id: state.network_id(),
                flags: UPDATE_FLAGS,
                layer: 0,
            })),
            _ => out.push(Packet::UpdateSubChunkBlocks(UpdateSubChunkBlocks {
                position: [
                    pos.x.saturating_mul(16),
                    sub_y.saturating_mul(16),
                    pos.z.saturating_mul(16),
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
    out
}
