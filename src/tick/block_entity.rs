//! Block entities: containers, signs, bed colours, banners, flower pots, mob heads and the other blocks
//! with data beyond their state, by position, saved with their chunk (chunk extras kind 1, in
//! Bedrock form: `id`, `x`, `y`, `z`, `Items` with `Slot`, `CustomName`, `pairx`/`pairz`…; the same
//! list holds Anchor's own spill entries, [`SPILL_ID`], which an export leaves out).
//!
//! The store keeps a typed [`BlockEntity`] per position of every loaded chunk; a saved entry of an
//! id Anchor does not type is kept as it came ([`Kind::Other`]) and does nothing. A kind with
//! slots keeps them in [`BlockEntity::items`], which the inventory windows read and write; the
//! keys of a kind that Anchor does not read stay in the entity and are written back unchanged.
//!
//! Items that cannot be dropped when a container breaks (the world's entity limit) wait in the
//! chunk as spills ([`Spill`], saved with the chunk) and drop on later ticks. A chunk keeps
//! [`MAX_SPILLS`] spills of [`MAX_SPILL_STACKS`] stacks each; an action that would need room
//! past that is refused before it changes anything ([`can_hold`]).
//!
//! `ChestPair` sits between the steps of pairing two chests; after a listener the step is checked
//! again on the block as the listener left it.

mod api;
mod decor;

use std::collections::{BTreeMap, BTreeSet, btree_map};
use std::ops::Bound;

use tracing::{error, warn};

use super::view::Key;
use super::{SessionId, State, checkpoint, dispatch, entity, interact, inventory, survival};
use crate::block::{self, Class, Face, Facing, PropertyValue};
use crate::block_entity::DyeColour;
use crate::custom::Registry;
use crate::event::ChestPair;
use crate::inventory::CHEST_SLOTS;
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::BlockEventType;
use crate::protocol::packet::{BlockActorData, BlockEvent, Packet};
use crate::save::WorldIndex;
use crate::storage::codec::{MAX_RAW_BLOCK_ENTITIES, MAX_SPILLS};
use crate::world::{BlockPos, ChunkPos};

/// The most spilled stacks dropped per tick, over every world.
const MAX_SPILL_DROPS_PER_TICK: usize = 64;
/// The most stacks a saved spill takes: its slots when read ([`Chunk::read`]).
const MAX_SPILL_STACKS: usize = 2 * CHEST_SLOTS;
/// The id of a spill entry in the saved list (Anchor's own; Bedrock has no such entity).
pub(crate) const SPILL_ID: &str = "AnchorSpill";

/// The block entities of the loaded chunks.
#[derive(Debug, Default)]
pub(super) struct BlockEntities {
    /// The block entities of each loaded chunk.
    chunks: BTreeMap<Key, Chunk>,
    /// Chunks whose block entities changed since [`Self::take_changed`].
    changed: BTreeSet<Key>,
    /// The sessions with a window open on each block (both halves of a double chest).
    viewers: BTreeMap<(WorldIndex, BlockPos), Vec<SessionId>>,
    /// The loaded chunks with spills waiting, so a tick looks only at them.
    spilling: BTreeSet<Key>,
    /// The block entities whose client form changed since [`send_marked`] last sent them.
    marked: BTreeSet<(WorldIndex, BlockPos)>,
}

impl BlockEntities {
    /// Every item the block entities and spills of the loaded chunks hold (for item counts).
    #[cfg(test)]
    pub(super) fn all_items(&self) -> impl Iterator<Item = &Stack> {
        self.chunks.values().flat_map(|c| {
            c.entities
                .values()
                .flat_map(|e| e.items.iter())
                .chain(c.spills.iter().flat_map(|s| s.items.iter()))
        })
    }

    /// What clients get of the block entities of a loaded chunk.
    #[cfg(test)]
    pub(super) fn client_nbt(&self, key: Key) -> Vec<Compound> {
        self.chunks.get(&key).map_or_else(Vec::new, |c| {
            c.entities
                .iter()
                .filter(|(_, e)| e.is_spawnable())
                .map(|(pos, e)| e.spawn_nbt(*pos))
                .collect()
        })
    }

    /// The block entities of the loaded chunks of `world`.
    pub(super) fn count(&self, world: WorldIndex) -> usize {
        let chunks =
            (world, ChunkPos::new(i32::MIN, i32::MIN))..=(world, ChunkPos::new(i32::MAX, i32::MAX));
        self.chunks
            .range(chunks)
            .map(|(_, c)| c.entities.len())
            .sum()
    }

    /// The block entity at `pos`.
    pub(super) fn get(&self, world: WorldIndex, pos: BlockPos) -> Option<&BlockEntity> {
        self.chunks.get(&(world, pos.chunk()))?.entities.get(&pos)
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            chunks,
            changed,
            viewers,
            // The chunks with spills, derived from `chunks`.
            spilling: _,
            // Empty between ticks, as the checkpoint leaves it out.
            marked: _,
        } = self;
        h.usize(chunks.len());
        for (&(world, pos), chunk) in chunks {
            let Chunk {
                entities,
                spills,
                raw,
            } = chunk;
            h.usize(world);
            h.u64(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
            h.usize(entities.len());
            for (p, e) in entities {
                h.block_pos(*p);
                e.hash_into(h, *p);
            }
            h.usize(spills.len());
            for s in spills {
                h.block_pos(s.pos);
                h.usize(s.items.len());
                for i in &s.items {
                    inventory::hash_stack(h, i);
                }
            }
            h.usize(raw.len());
            for r in raw {
                super::extra::hash_nbt(h, r);
            }
        }
        h.usize(changed.len());
        for (world, pos) in changed {
            h.usize(*world);
            h.u64(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
        }
        h.usize(viewers.len());
        for ((world, pos), list) in viewers {
            h.usize(*world);
            h.block_pos(*pos);
            h.usize(list.len());
            for s in list {
                h.u64(s.get());
            }
        }
    }

    /// The sizes of every table (for leak tests).
    #[cfg(test)]
    pub(super) fn sizes(&self) -> Sizes {
        Sizes {
            chunks: self.chunks.len(),
            changed: self.changed.len(),
            viewers: self.viewers.len(),
            spilling: self.spilling.len(),
            marked: self.marked.len(),
            spills: self.chunks.values().map(|c| c.spills.len()).sum(),
        }
    }

    /// The sessions with a window open on the block at `pos`.
    pub(super) fn viewers(&self, world: WorldIndex, pos: BlockPos) -> &[SessionId] {
        self.viewers
            .get(&(world, pos))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Appends to `out` the client form of the block entities in sub-chunk `index` (0 is the
    /// lowest) of a loaded chunk, as network little-endian NBT roots one after another: what a
    /// sub-chunk answer carries after the blocks. Nothing when there are none. `pos` is within
    /// the world border and `index` below [`SUB_CHUNKS`](crate::world::SUB_CHUNKS) (a sub-chunk a
    /// player asks for, `view::encode`).
    ///
    /// Each root is built as a [`Compound`] (owned keys) once for each answer built: an answer is
    /// cached per sub-chunk and a change marks its block entity once a tick ([`send_marked`]).
    pub(super) fn write_client_bytes(
        &self,
        world: WorldIndex,
        pos: ChunkPos,
        index: usize,
        out: &mut Vec<u8>,
    ) {
        let Some(chunk) = self.chunks.get(&(world, pos)) else {
            return;
        };
        let west = pos.x * 16;
        #[expect(
            clippy::expect_used,
            reason = "invariant: index is below SUB_CHUNKS (24)"
        )]
        let bottom =
            crate::world::MIN_Y + i32::try_from(index).expect("a sub-chunk index fits i32") * 16;
        // Positions order by x, then y, then z: the sub-chunk's part of one x is one range, and
        // walking x up sends the roots in the chunk's order.
        let slabs = (west..=west + 15).flat_map(|x| {
            let slab = BlockPos::new(x, bottom, i32::MIN)..=BlockPos::new(x, bottom + 15, i32::MAX);
            chunk.entities.range(slab)
        });
        for (p, e) in slabs {
            #[cfg(test)]
            tests::count_client_entity();
            if !e.is_spawnable() {
                continue;
            }
            let start = out.len();
            let mut w = crate::protocol::Writer::new(std::mem::take(out), 0);
            let written = w.nbt(&e.spawn_nbt(*p), crate::nbt::Encoding::NetworkLittleEndian);
            *out = w.into_inner();
            if written.is_err() {
                // A failed write leaves part of the root: cut it so the roots after it read.
                out.truncate(start);
                warn!(
                    x = p.x,
                    y = p.y,
                    z = p.z,
                    "a block entity is too large to send"
                );
            }
        }
    }

    /// Records that `session` opened a window on the block at `pos`; returns the number of
    /// viewers.
    pub(super) fn add_viewer(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        session: SessionId,
    ) -> usize {
        let list = self.viewers.entry((world, pos)).or_default();
        if !list.contains(&session) {
            list.push(session);
        }
        list.len()
    }

    /// Fills every slot of the spills of `pos`'s loaded chunk with `stack` (for tests of the cap).
    #[cfg(test)]
    pub(super) fn fill_spills(&mut self, world: WorldIndex, pos: BlockPos, stack: &Stack) {
        let spill = Spill {
            pos,
            items: vec![stack.clone(); MAX_SPILL_STACKS],
        };
        let key = (world, pos.chunk());
        self.chunks.get_mut(&key).unwrap().spills = vec![spill; MAX_SPILLS];
        self.spilling.insert(key);
    }

    /// The block entity at `pos` to change; its chunk is marked changed.
    pub(super) fn get_mut(&mut self, world: WorldIndex, pos: BlockPos) -> Option<&mut BlockEntity> {
        let key = (world, pos.chunk());
        let entity = self.chunks.get_mut(&key)?.entities.get_mut(&pos)?;
        self.changed.insert(key);
        Some(entity)
    }

    /// [`Parsed::read`] and [`Self::load`] in one, the blocks read from `world` (none when it
    /// does not hold the chunk, so none is checked).
    #[cfg(test)]
    pub(super) fn load_list(
        &mut self,
        key: Key,
        list: &[Compound],
        world: &crate::world::World,
        custom: &Registry,
    ) {
        let parsed = Parsed::read(key, list, |pos| world.block(pos), custom);
        self.load(key, parsed.expect("the list is not damaged"));
    }

    /// Takes in a chunk's saved list, which arrived ([`mod@super::rules`]) read off the tick: its
    /// block entities enter the store, and the chunk is saved again if reading removed any. A
    /// chest paired with one that is not paired back is unpaired ([`Self::unpair_orphans`]).
    pub(super) fn load(&mut self, key: Key, parsed: Parsed) {
        let Parsed { chunk, removed } = parsed;
        if chunk.spills.is_empty() {
            self.spilling.remove(&key);
        } else {
            self.spilling.insert(key);
        }
        self.chunks.insert(key, chunk);
        if removed {
            self.changed.insert(key);
        } else {
            self.changed.remove(&key);
        }
        self.unpair_orphans(key);
    }

    /// Records that `session` closed its window on the block at `pos`; returns the number left.
    pub(super) fn remove_viewer(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        session: SessionId,
    ) -> usize {
        let Some(list) = self.viewers.get_mut(&(world, pos)) else {
            return 0;
        };
        list.retain(|s| *s != session);
        let left = list.len();
        if left == 0 {
            self.viewers.remove(&(world, pos));
        }
        left
    }

    /// Keeps items of a broken block to drop later, in the chunk of `pos`.
    pub(super) fn spill(&mut self, world: WorldIndex, pos: BlockPos, items: Vec<Stack>) {
        let key = (world, pos.chunk());
        let Some(chunk) = self.chunks.get_mut(&key) else {
            error!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "container items lost: the chunk is gone"
            );
            return;
        };
        spill_unfit(&mut chunk.spills, pos, items);
        self.changed.insert(key);
        self.spilling.insert(key);
    }

    /// Takes the lists of the chunks that changed since the last call, for a save snapshot.
    pub(super) fn take_changed(&mut self) -> Vec<(Key, Vec<Compound>)> {
        let changed = std::mem::take(&mut self.changed);
        changed
            .into_iter()
            .filter_map(|key| Some((key, self.chunks.get(&key)?.write())))
            .collect()
    }

    /// Drops the state of a chunk that leaves; gives its list if it changed since
    /// [`Self::take_changed`].
    pub(super) fn unload(&mut self, key: Key) -> Option<Vec<Compound>> {
        self.spilling.remove(&key);
        let chunk = self.chunks.remove(&key)?;
        self.changed.remove(&key).then(|| chunk.write())
    }

    /// Whether the loaded chunk of `pos` can keep `stacks` more waiting stacks from one
    /// destruction ([`spills_take`]).
    fn has_spill_room(&self, world: WorldIndex, pos: BlockPos, stacks: usize) -> bool {
        self.chunks
            .get(&(world, pos.chunk()))
            .is_some_and(|c| spills_take(&c.spills, stacks))
    }

    /// Whether the chest at `at`, paired with `partner`, has a partner that is loaded and does
    /// not point back at it.
    fn is_orphan(&self, world: WorldIndex, at: BlockPos, partner: BlockPos) -> bool {
        self.is_loaded(world, partner)
            && !matches!(
                self.get(world, partner).map(|e| &e.kind),
                Some(Kind::Chest { pair: Some(back) }) if *back == at
            )
    }

    /// Whether the block entities of the chunk of `pos` are loaded.
    fn is_loaded(&self, world: WorldIndex, pos: BlockPos) -> bool {
        self.chunks.contains_key(&(world, pos.chunk()))
    }

    /// The positions of the block entities of a loaded chunk, in order.
    fn positions(&self, world: WorldIndex, pos: ChunkPos) -> impl Iterator<Item = BlockPos> + '_ {
        self.chunks
            .get(&(world, pos))
            .into_iter()
            .flat_map(|c| c.entities.keys().copied())
    }

    /// Puts a block entity at `pos` of a loaded chunk, where there is none.
    fn insert(&mut self, world: WorldIndex, pos: BlockPos, entity: BlockEntity) {
        let key = (world, pos.chunk());
        let Some(chunk) = self.chunks.get_mut(&key) else {
            return;
        };
        self.changed.insert(key);
        chunk.entities.insert(pos, entity);
    }

    /// Removes the block entity at `pos`.
    fn remove(&mut self, world: WorldIndex, pos: BlockPos) -> Option<BlockEntity> {
        let key = (world, pos.chunk());
        let entity = self.chunks.get_mut(&key)?.entities.remove(&pos)?;
        self.changed.insert(key);
        Some(entity)
    }

    /// Unpairs the chests of chunk `key` and of its four neighbours whose partner is in a loaded
    /// chunk and does not pair back. A pair is a fact of two chests that point at each other; a
    /// half whose partner was broken, replaced or unpaired while its own chunk was not loaded
    /// (or was never a chest) would otherwise stay paired for ever and never pair again. A
    /// partner in a chunk that is not loaded is left: the check runs again when that chunk loads.
    fn unpair_orphans(&mut self, key: Key) {
        let (world, chunk) = key;
        let near = [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .filter_map(|(dx, dz)| {
                Some((
                    world,
                    ChunkPos::new(chunk.x.checked_add(dx)?, chunk.z.checked_add(dz)?),
                ))
            });
        let mut orphans = Vec::new();
        for near_key in near {
            let Some(near_chunk) = self.chunks.get(&near_key) else {
                continue;
            };
            for (&at, e) in &near_chunk.entities {
                if let Kind::Chest {
                    pair: Some(partner),
                } = e.kind
                    && self.is_orphan(world, at, partner)
                {
                    orphans.push(at);
                }
            }
        }
        for at in orphans {
            let chunk_key = (world, at.chunk());
            if let Some(Kind::Chest { pair }) = self
                .chunks
                .get_mut(&chunk_key)
                .and_then(|c| c.entities.get_mut(&at))
                .map(|e| &mut e.kind)
            {
                *pair = None;
            }
            self.changed.insert(chunk_key);
            self.marked.insert((world, at));
        }
    }

    /// Changes the block entity at `pos` with `f`; its chunk is marked changed when `f` gives
    /// `Some` (`None`: `f` changed nothing).
    fn update<T>(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        f: impl FnOnce(&mut BlockEntity) -> Option<T>,
    ) -> Option<T> {
        let key = (world, pos.chunk());
        let out = f(self.chunks.get_mut(&key)?.entities.get_mut(&pos)?)?;
        self.changed.insert(key);
        Some(out)
    }
}

/// Every field but the chunks with spills, which are `chunks` again, and the marked block
/// entities, which are empty between ticks as the outboxes are (the boundary sends what it marks).
///
/// Each block entity goes in its saved form, built when the checkpoint is taken: the cost is one
/// [`BlockEntity::write`] for each loaded block entity, which are the containers and the few
/// decorations Anchor keeps. A cache of the saved forms would double their memory and need every
/// write through [`BlockEntities::get_mut`] to drop its entry.
impl super::checkpoint::Codec for BlockEntities {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            chunks,
            changed,
            viewers,
            spilling: _,
            marked: _,
        } = self;
        chunks.encode(w);
        changed.encode(w);
        viewers.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        let chunks: BTreeMap<Key, Chunk> = Codec::decode(r)?;
        let spilling = chunks
            .iter()
            .filter(|(_, c)| !c.spills.is_empty())
            .map(|(&k, _)| k)
            .collect();
        Ok(Self {
            chunks,
            changed: Codec::decode(r)?,
            viewers: Codec::decode(r)?,
            spilling,
            marked: BTreeSet::new(),
        })
    }
}

/// The sizes of the store's tables ([`BlockEntities::sizes`], for leak tests).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Sizes {
    /// The loaded chunks.
    pub(super) chunks: usize,
    /// The chunks changed since the last save.
    pub(super) changed: usize,
    /// The viewer entries.
    pub(super) viewers: usize,
    /// The chunks with spills waiting.
    pub(super) spilling: usize,
    /// The block entities marked to be sent.
    pub(super) marked: usize,
    /// The waiting spills.
    pub(super) spills: usize,
}

/// One chunk's block entities. Its saved list, the three together, stays within what a list
/// holds (`storage::codec::MAX_BLOCK_ENTITY_ENTRIES`): one entity per block, at most
/// [`MAX_SPILLS`] spills and at most [`MAX_RAW_BLOCK_ENTITIES`] raw entries.
#[derive(Debug, Clone, PartialEq, Default)]
struct Chunk {
    entities: BTreeMap<BlockPos, BlockEntity>,
    /// Container contents waiting for room in the world to drop.
    spills: Vec<Spill>,
    /// Saved entries without a valid position in this chunk, or past the spills a chunk keeps,
    /// kept as they came.
    raw: Vec<Compound>,
}

impl Chunk {
    /// The chunk of a saved list; `None` if the list keeps more raw entries than a chunk may
    /// ([`MAX_RAW_BLOCK_ENTITIES`]), which no list Anchor wrote or imported does. A second entry
    /// at one position is dropped with a warning: kept, it would become the block entity once
    /// the first is gone, and its items would come back.
    fn read(key: Key, list: &[Compound], custom: &Registry) -> Option<Self> {
        let mut chunk = Self::default();
        for nbt in list {
            let Some(pos) = saved_pos(nbt, key) else {
                #[cfg(test)]
                crate::tick::rules::reads::add(|r| r.clones += 1);
                chunk.raw.push(nbt.clone());
                continue;
            };
            let spill = nbt.get("id") == Some(&Value::String(SPILL_ID.to_owned()));
            if spill && chunk.spills.len() >= MAX_SPILLS {
                #[cfg(test)]
                crate::tick::rules::reads::add(|r| r.clones += 1);
                chunk.raw.push(nbt.clone());
            } else if spill {
                let mut items = vec![Stack::EMPTY; MAX_SPILL_STACKS];
                if let Some(Value::List(List::Compound(list))) = nbt.get("Items") {
                    read_items(list, pos, custom, &mut items);
                }
                items.retain(|s| !s.is_empty());
                if !items.is_empty() {
                    chunk.spills.push(Spill { pos, items });
                }
            } else if let btree_map::Entry::Vacant(free) = chunk.entities.entry(pos) {
                #[cfg(test)]
                crate::tick::rules::reads::add(|r| r.clones += 1);
                free.insert(BlockEntity::read(nbt.clone(), pos, custom));
            } else {
                // Safety exception to "corrupt data is neither read nor overwritten": kept as
                // read, the second would take the place once the first is broken and drop its
                // items again (an item copy); setting the whole chunk aside would leave a hole
                // in an imported world. So it is dropped, and the next save leaves it out.
                warn!(
                    x = pos.x,
                    y = pos.y,
                    z = pos.z,
                    "a second saved block entity at one position; it is dropped"
                );
            }
        }
        (chunk.raw.len() <= MAX_RAW_BLOCK_ENTITIES).then_some(chunk)
    }

    /// The saved list: the block entities, the spills, then the raw entries.
    fn write(&self) -> Vec<Compound> {
        let mut list = Vec::with_capacity(self.entities.len() + self.spills.len() + self.raw.len());
        list.extend(self.entities.iter().map(|(pos, e)| e.write(*pos)));
        for spill in &self.spills {
            list.push(Compound::from([
                ("id".to_owned(), Value::String(SPILL_ID.to_owned())),
                ("x".to_owned(), Value::Int(spill.pos.x)),
                ("y".to_owned(), Value::Int(spill.pos.y)),
                ("z".to_owned(), Value::Int(spill.pos.z)),
                ("Items".to_owned(), write_items(&spill.items)),
            ]));
        }
        list.extend(self.raw.iter().cloned());
        list
    }
}

/// Each block entity by its position and saved form, which holds every field, then the spills
/// and the raw entries.
impl super::checkpoint::Codec for Chunk {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            entities,
            spills,
            raw,
        } = self;
        w.len(entities.len());
        for (&pos, e) in entities {
            pos.encode(w);
            e.write(pos).encode(w);
        }
        spills.encode(w);
        raw.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::{Codec, Error};
        let n = r.len()?;
        let mut entities = BTreeMap::new();
        for _ in 0..n {
            let pos = BlockPos::decode(r)?;
            let nbt = Compound::decode(r)?;
            let e = BlockEntity::read(nbt, pos, r.custom);
            if entities.insert(pos, e).is_some() {
                return Err(Error::Format("repeated block entity"));
            }
        }
        Ok(Self {
            entities,
            spills: Codec::decode(r)?,
            raw: Codec::decode(r)?,
        })
    }
}

/// Items of a broken container that could not be dropped yet.
#[derive(Debug, Clone, PartialEq)]
struct Spill {
    pos: BlockPos,
    items: Vec<Stack>,
}

checkpoint::fields!(Spill { pos, items });

/// A chunk's saved block entities read off the tick ([`Parsed::read`]), for
/// [`BlockEntities::load`].
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Parsed {
    chunk: Chunk,
    /// Whether reading took something out of the saved list, so the chunk is saved again.
    removed: bool,
}

impl Parsed {
    /// Reads chunk `key`'s saved list, off the tick as it needs nothing of the tick's state;
    /// `block_of` gives the chunk's block at a position of it (`None` for one not known) and
    /// `custom` is the server's custom content (saved custom items read back through it).
    ///
    /// A block entity whose block does not take it (a list written outside Anchor) is removed
    /// with a warning; what it held waits as a spill and drops in the next ticks, so no item is
    /// lost. With the chunk's spills full it is kept as it is. A shulker box's stacks its slots
    /// do not take (a list written outside Anchor, or with stacks a slot does not take) wait as
    /// a spill too, so the box with its items is always one stack ([`spill_unfit`]); with no
    /// room for them they stay in the box.
    ///
    /// `None` for a damaged list: one keeping more raw entries than a chunk may ([`Chunk::read`]).
    pub(super) fn read(
        key: Key,
        list: &[Compound],
        block_of: impl Fn(BlockPos) -> Option<block::State>,
        custom: &Registry,
    ) -> Option<Self> {
        let mut chunk = Chunk::read(key, list, custom)?;
        let mut removed = chunk.entities.len() + chunk.spills.len() + chunk.raw.len() != list.len();
        let stray: Vec<BlockPos> = chunk
            .entities
            .iter()
            .filter(|(pos, e)| block_of(**pos).is_some_and(|b| !fits(&e.kind, b)))
            .map(|(pos, _)| *pos)
            .collect();
        removed |= !stray.is_empty();
        for pos in stray {
            if chunk.spills.len() >= MAX_SPILLS {
                break;
            }
            let Some(mut entity) = chunk.entities.remove(&pos) else {
                continue;
            };
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                id = entity.kind.id(),
                "a saved block entity does not belong to its block; removed, its items dropped"
            );
            let mut items = Vec::new();
            take_contents(&mut entity, &mut items);
            items.extend(entity.items.into_iter().filter(|s| !s.is_empty()));
            if !items.is_empty() {
                chunk.spills.push(Spill { pos, items });
            }
        }
        for (pos, entity) in &mut chunk.entities {
            // With no room for them, the stacks stay in the box.
            if !spills_take(&chunk.spills, entity.unfit_count()) {
                continue;
            }
            let unfit = entity.take_unfit();
            if !unfit.is_empty() {
                spill_unfit(&mut chunk.spills, *pos, unfit);
                removed = true;
            }
        }
        Some(Self { chunk, removed })
    }
}

/// A block entity.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BlockEntity {
    pub(super) kind: Kind,
    /// The slots (27 for a chest; [`Kind::slots`]), empty for kinds without slots.
    pub(super) items: Vec<Stack>,
    /// `CustomName`, of a kind that takes one ([`Kind::is_nameable`]).
    pub(super) name: Option<String>,
    /// The other saved keys (`Lock`, `isMovable`…), written back unchanged.
    rest: Compound,
}

impl BlockEntity {
    /// Whether clients get it. A kind Anchor does not type goes when it has an id, as the blocks
    /// drawn from one show nothing without it.
    pub(super) fn is_spawnable(&self) -> bool {
        match &self.kind {
            Kind::Other(_) => !self.kind.id().is_empty(),
            _ => true,
        }
    }

    /// The `Lock` key a held item's custom name must match to open the container.
    pub(super) fn lock(&self) -> Option<&str> {
        match self.rest.get("Lock") {
            Some(Value::String(lock)) => Some(lock),
            _ => None,
        }
    }

    /// The other half of a double chest.
    pub(super) fn pair(&self) -> Option<BlockPos> {
        match self.kind {
            Kind::Chest { pair } => pair,
            _ => None,
        }
    }

    /// What clients get of it: id, position, the custom name and the kind's client keys (a
    /// chest's pair, a sign's text, a bed's colour…). A kind Anchor does not type goes as saved:
    /// the client draws some blocks (a decorated pot, a bell) only from their block entity's
    /// keys, which Anchor cannot pick out.
    pub(super) fn spawn_nbt(&self, pos: BlockPos) -> Compound {
        if matches!(self.kind, Kind::Other(_)) {
            return self.write(pos);
        }
        let mut nbt = Compound::from([
            ("id".to_owned(), Value::String(self.kind.id().to_owned())),
            ("x".to_owned(), Value::Int(pos.x)),
            ("y".to_owned(), Value::Int(pos.y)),
            ("z".to_owned(), Value::Int(pos.z)),
        ]);
        self.write_kind(Form::Client, &mut nbt);
        if let Some(name) = &self.name {
            nbt.insert("CustomName".to_owned(), Value::String(name.clone()));
        }
        nbt
    }

    /// Reads a saved compound at `pos` (its `x`, `y`, `z`, already checked).
    fn read(mut nbt: Compound, pos: BlockPos, custom: &Registry) -> Self {
        #[cfg(test)]
        crate::tick::rules::reads::add(|r| r.block_entities += 1);
        for key in ["x", "y", "z"] {
            nbt.remove(key);
        }
        let Some(Value::String(id)) = nbt.get("id").cloned() else {
            return Self::new(Kind::Other(nbt));
        };
        let kind = match id.as_str() {
            "Chest" => Kind::Chest { pair: None },
            "Barrel" => Kind::Barrel,
            "ShulkerBox" => Kind::ShulkerBox { facing: 1 },
            "EnderChest" => Kind::EnderChest,
            "Sign" | "HangingSign" => {
                Kind::Sign(Box::new(decor::Sign::read(&mut nbt, id == "HangingSign")))
            }
            "Bed" => Kind::Bed {
                colour: decor::read_bed(&mut nbt),
            },
            "Banner" => Kind::Banner(decor::Banner::read(&mut nbt)),
            "FlowerPot" => Kind::FlowerPot {
                plant: decor::read_plant(&mut nbt),
            },
            "Skull" => {
                let (skull, rotation) = decor::read_skull(&mut nbt);
                Kind::Skull { skull, rotation }
            }
            _ => return Self::new(Kind::Other(nbt)),
        };
        nbt.remove("id");
        let mut items = vec![Stack::EMPTY; kind.slots()];
        if kind.has_item_list()
            && let Some(Value::List(List::Compound(list))) = nbt.remove("Items")
        {
            read_items(&list, pos, custom, &mut items);
        }
        let mut entity = Self {
            kind,
            items,
            name: None,
            rest: Compound::new(),
        };
        // A name over the limit, or on a kind without one, stays as saved and is not shown, so
        // every name is one a stack may carry ([`with_block_data`]).
        if entity.kind.is_nameable()
            && let Some(Value::String(name)) = nbt.get("CustomName")
            && !name.is_empty()
            && name.len() <= crate::block_entity::MAX_NAME_BYTES
        {
            entity.name = Some(name.clone());
            nbt.remove("CustomName");
        }
        match &mut entity.kind {
            Kind::Chest { pair } => {
                // A pair must be a horizontal neighbour.
                if let (Some(Value::Int(x)), Some(Value::Int(z))) =
                    (nbt.get("pairx"), nbt.get("pairz"))
                {
                    let (x, z) = (*x, *z);
                    let next = (x == pos.x && z.abs_diff(pos.z) == 1)
                        || (z == pos.z && x.abs_diff(pos.x) == 1);
                    if next {
                        *pair = Some(BlockPos::new(x, pos.y, z));
                    }
                }
                nbt.remove("pairx");
                nbt.remove("pairz");
            }
            Kind::ShulkerBox { facing } => {
                if let Some(Value::Byte(f)) = nbt.remove("facing") {
                    *facing = f.min(5);
                }
            }
            _ => {}
        }
        entity.rest = nbt;
        entity
    }

    /// A new block entity of `kind` with its slots empty.
    fn new(kind: Kind) -> Self {
        let slots = kind.slots();
        Self {
            kind,
            items: vec![Stack::EMPTY; slots],
            name: None,
            rest: Compound::new(),
        }
    }

    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv, pos: BlockPos) {
        let Self {
            kind: _,
            items: _,
            name: _,
            rest: _,
        } = self;
        // The saved form holds every field.
        super::extra::hash_nbt(h, &self.write(pos));
    }

    /// The number of stacks [`Self::take_unfit`] would take.
    fn unfit_count(&self) -> usize {
        if !matches!(self.kind, Kind::ShulkerBox { .. }) {
            return 0;
        }
        self.items
            .iter()
            .filter(|s| !s.can_enter_shulker_box())
            .count()
    }

    /// The saved compound at `pos`.
    fn write(&self, pos: BlockPos) -> Compound {
        let mut nbt = match &self.kind {
            Kind::Other(nbt) => nbt.clone(),
            kind => {
                let mut nbt = self.rest.clone();
                nbt.insert("id".to_owned(), Value::String(kind.id().to_owned()));
                if kind.has_item_list() {
                    nbt.insert("Items".to_owned(), write_items(&self.items));
                }
                self.write_kind(Form::Saved, &mut nbt);
                nbt
            }
        };
        nbt.insert("x".to_owned(), Value::Int(pos.x));
        nbt.insert("y".to_owned(), Value::Int(pos.y));
        nbt.insert("z".to_owned(), Value::Int(pos.z));
        if let Some(name) = &self.name {
            nbt.insert("CustomName".to_owned(), Value::String(name.clone()));
        }
        nbt
    }

    /// Writes the keys of the kind's own data in `form` (most of them saves and clients share).
    fn write_kind(&self, form: Form, nbt: &mut Compound) {
        match &self.kind {
            Kind::Chest { pair: Some(p) } => {
                nbt.insert("pairx".to_owned(), Value::Int(p.x));
                nbt.insert("pairz".to_owned(), Value::Int(p.z));
            }
            Kind::ShulkerBox { facing } => {
                nbt.insert("facing".to_owned(), Value::Byte(*facing));
            }
            Kind::Sign(s) if form == Form::Client => s.spawn(nbt),
            Kind::Sign(s) => s.write(nbt),
            Kind::Bed { colour } => {
                nbt.insert("color".to_owned(), Value::Byte(*colour));
            }
            Kind::Banner(b) => b.write(nbt),
            Kind::FlowerPot { plant } => decor::write_plant(plant.as_ref(), form, nbt),
            Kind::Skull { skull, rotation } => decor::write_skull(*skull, *rotation, nbt),
            _ => {}
        }
    }

    /// Takes the stacks a shulker box's slots do not take ([`Stack::can_enter_shulker_box`]) out of
    /// it: what a box item made by a developer held, or a list written outside Anchor. None for
    /// other kinds.
    fn take_unfit(&mut self) -> Vec<Stack> {
        if !matches!(self.kind, Kind::ShulkerBox { .. }) {
            return Vec::new();
        }
        self.items
            .iter_mut()
            .filter(|s| !s.can_enter_shulker_box())
            .map(std::mem::take)
            .collect()
    }
}

/// What a block entity is, with the data of its kind.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    /// A chest or trapped chest, and the chest it pairs with into a double chest.
    Chest {
        /// The other half of a double chest.
        pair: Option<BlockPos>,
    },
    /// A barrel.
    Barrel,
    /// A shulker box and the face it opens toward (Bedrock `facing`, 0 down … 5 east).
    ShulkerBox {
        /// The `facing` byte.
        facing: u8,
    },
    /// An ender chest: its contents are each player's own.
    EnderChest,
    /// A sign or hanging sign.
    Sign(Box<decor::Sign>),
    /// A bed and its colour (the `DyeColour` ID, the bed item's meta).
    Bed {
        /// The colour.
        colour: u8,
    },
    /// A banner.
    Banner(decor::Banner),
    /// A flower pot and its plant.
    FlowerPot {
        /// The plant, if any.
        plant: Option<decor::Plant>,
    },
    /// A mob head: its type and its rotation on the floor (sixteenths).
    Skull {
        /// The `SkullType` byte ([`decor::SKULL_TYPES`]).
        skull: u8,
        /// The rotation, below [`interact::props::ROTATION_STEPS`].
        rotation: u8,
    },
    /// A kind no module types: the saved compound without `x`, `y`, `z`, kept as it came.
    Other(Compound),
}

impl Kind {
    /// The Bedrock block entity id.
    fn id(&self) -> &str {
        match self {
            Self::Chest { .. } => "Chest",
            Self::Barrel => "Barrel",
            Self::ShulkerBox { .. } => "ShulkerBox",
            Self::EnderChest => "EnderChest",
            Self::Sign(s) if s.hanging => "HangingSign",
            Self::Sign(_) => "Sign",
            Self::Bed { .. } => "Bed",
            Self::Banner(_) => "Banner",
            Self::FlowerPot { .. } => "FlowerPot",
            Self::Skull { .. } => "Skull",
            Self::Other(nbt) => match nbt.get("id") {
                Some(Value::String(id)) => id,
                _ => "",
            },
        }
    }

    /// Whether it takes a custom name.
    fn is_nameable(&self) -> bool {
        self.has_item_list()
    }

    /// Whether the kind's slots are saved as a `Slot`-numbered `Items` list.
    fn has_item_list(&self) -> bool {
        matches!(
            self,
            Self::Chest { .. } | Self::Barrel | Self::ShulkerBox { .. }
        )
    }

    /// The kind a block state gets a block entity of; `None` for a block without one.
    fn of(state: block::State) -> Option<Self> {
        let classes = state.kind().classes();
        let is = |c: Class| classes.contains(&c);
        Some(if is(Class::Chest) {
            Self::Chest { pair: None }
        } else if is(Class::Barrel) {
            Self::Barrel
        } else if is(Class::ShulkerBox) {
            // Bedrock's state has no facing; `placed` takes the clicked face.
            Self::ShulkerBox { facing: 1 }
        } else if is(Class::EnderChest) {
            Self::EnderChest
        } else if is(Class::BaseSign) {
            let sign = decor::Sign {
                hanging: is_hanging_sign(state),
                ..decor::Sign::default()
            };
            Self::Sign(Box::new(sign))
        } else if is(Class::Bed) {
            Self::Bed {
                colour: decor::DEFAULT_BED_COLOUR,
            }
        } else if is(Class::BaseBanner) {
            Self::Banner(decor::Banner::default())
        } else if is(Class::FlowerPot) {
            Self::FlowerPot { plant: None }
        } else if is(Class::MobHead) {
            Self::Skull {
                skull: skull_type(state),
                rotation: 0,
            }
        } else {
            return None;
        })
    }

    /// The number of slots ([`BlockEntity::items`]): the kind decides it, a write never changes
    /// it.
    fn slots(&self) -> usize {
        match self {
            Self::Chest { .. } | Self::Barrel | Self::ShulkerBox { .. } => CHEST_SLOTS,
            Self::EnderChest
            | Self::Sign(_)
            | Self::Bed { .. }
            | Self::Banner(_)
            | Self::FlowerPot { .. }
            | Self::Skull { .. }
            | Self::Other(_) => 0,
        }
    }
}

/// Which form of a block entity a write makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// The saved form, which the chunk keeps.
    Saved,
    /// The form clients get.
    Client,
}

/// Creates the block entity of the block at `pos`, if the block has one, after `session` (or the
/// world when `None`) placed it from `item` against `face` (the clicked face).
///
/// The new entity takes the item's custom name if its kind has one and the data its item carries:
/// a shulker box's items and the face it opens toward, a bed's colour, a banner's colour and
/// patterns, a floor mob head's rotation from the player's yaw. A chest pairs with a chest of the
/// same kind and facing beside it that is not paired yet, trying the counter-clockwise side first.
/// A block entity already at `pos` (one nothing removed) is replaced, everything it held dropped
/// first ([`discard`]). The players who see the chunk get its client data.
pub(super) fn placed(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    session: Option<SessionId>,
    item: &Stack,
    face: Face,
) {
    if create(
        state,
        world,
        pos,
        session,
        item,
        face,
        entity::Raise::Events,
    ) {
        mark_spawn(state, world, pos);
    }
}

/// Handles the change of the block at `pos` (the world's change feed): removes a block entity the
/// new block does not take (the type registers another kind, or none) and drops everything it
/// held: its slots and a shulker box's items; a new block that takes one and has none gets an
/// empty one, as [`missing`] makes it. A sign's text or a banner's patterns go with their block
/// entity, and the old block entity stays readable until this runs, in the tick's block change
/// phase after the change.
///
/// Breaking ([`interact::destroy_deferring_hooks`]) and placing ([`placed`]) do their block entities
/// themselves; this catches every other change: a task's or a command's `World::set_block`, an
/// explosion. A generator's blocks are no change: they get theirs on first use ([`missing`],
/// [`Context::block_entity_mut`](crate::Context::block_entity_mut), [`own_item`]). A block entity of a kind Anchor does not type
/// ([`Kind::Other`]) belongs to blocks Anchor cannot tell, so it goes only when its block becomes
/// air.
pub(super) fn block_changed(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    match state.rules.block_entities.get(world, pos) {
        Some(entity) if fits(&entity.kind, block) => {}
        Some(_) if !can_hold(state, world, pos, stacks_held(state, world, pos)) => {
            // Its items have nowhere to wait: the entity stays as it is, as one read from a
            // chunk does.
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "a block entity does not belong to its block; kept, its items have no room to drop"
            );
        }
        Some(_) => {
            discard(state, world, pos);
            missing(state, world, pos);
        }
        None => missing(state, world, pos),
    }
}

/// Creates an empty block entity at `pos` for a block without one: a block that takes one
/// ([`block_changed`]), or a container block with a window opening on it, or a sign with an editor
/// opening on it, without it (a block a generator made). Clients are not sent it: the client
/// makes a block's block entity with the block, and the official server sends no `BlockActorData`
/// when a window opens; a chest that pairs sends both halves as on placing.
pub(super) fn missing(state: &mut State, world: WorldIndex, pos: BlockPos) {
    create(
        state,
        world,
        pos,
        None,
        &Stack::EMPTY,
        Face::Up,
        entity::Raise::Events,
    );
}

/// Creates the block entity of the block at `pos` as [`placed`] describes; false if the block
/// has none. A chest's pairing raises [`ChestPair`] only as `raise` says.
fn create(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    session: Option<SessionId>,
    item: &Stack,
    face: Face,
    raise: entity::Raise,
) -> bool {
    let Some(block) = block_at(state, world, pos) else {
        return false;
    };
    let Some(kind) = Kind::of(block) else {
        return false;
    };
    if state.rules.block_entities.get(world, pos).is_some() {
        discard(state, world, pos);
    }
    let yaw = session
        .and_then(|s| state.players.get(&s))
        .map(|p| survival::rotation(p).0);
    let mut entity = BlockEntity::new(kind);
    if entity.kind.is_nameable() {
        entity.name = Some(item.custom_name().to_owned()).filter(|n| !n.is_empty());
    }
    let tag = item.tag();
    let mut unfit = Vec::new();
    match &mut entity.kind {
        Kind::ShulkerBox { facing } => {
            *facing = face.index();
            if let Some(Value::List(List::Compound(items))) =
                tag.as_ref().and_then(|t| t.get("Items"))
            {
                read_items(items, pos, &state.custom, &mut entity.items);
            }
            unfit = entity.take_unfit();
        }
        Kind::Bed { colour } => {
            if let Ok(c) = u8::try_from(item.meta())
                && DyeColour::from_id(c).is_some()
            {
                *colour = c;
            }
        }
        Kind::Banner(banner) => {
            banner.base = i32::from(item.meta()).min(15);
            if let Some(tag) = &tag {
                if let Some(Value::List(List::Compound(list))) = tag.get("Patterns") {
                    banner.patterns = decor::read_patterns(list);
                }
                if let Some(Value::Int(ty)) = tag.get("Type") {
                    banner.ty = *ty;
                }
            }
        }
        Kind::Skull { rotation, .. } => {
            if face == Face::Up
                && let Some(yaw) = yaw
            {
                *rotation = interact::props::rotation_nearest(yaw);
            }
        }
        _ => {}
    }
    let is_chest = matches!(entity.kind, Kind::Chest { .. });
    // `discard` above emptied `pos`: nothing is replaced here.
    state.rules.block_entities.insert(world, pos, entity);
    if is_chest {
        pair_chest(state, world, pos, block, session, raise);
    }
    drop_all(state, world, pos.to_center(), unfit);
    true
}

/// Pairs the new chest at `pos` with a free chest of the same kind and facing beside it, raising
/// [`ChestPair`] as `raise` says. After a listener the chests must be as they were, and the hand
/// of `session` (the player placing, whose interaction goes on with it) the same stack.
fn pair_chest(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    session: Option<SessionId>,
    raise: entity::Raise,
) {
    let Some(facing) = cardinal(block) else {
        return;
    };
    let held = session.map(|s| inventory::held_of(state, s));
    // The counter-clockwise side is tried first; that side's chest is the right half.
    for (side, clockwise) in [
        (facing.clockwise().opposite(), false),
        (facing.clockwise(), true),
    ] {
        let Some(other) = beside(pos, side) else {
            continue;
        };
        let Some(other_block) = block_at(state, world, other) else {
            continue;
        };
        let pairable = |state: &State| {
            session.map(|s| inventory::held_of(state, s)) == held
                && block_at(state, world, pos) == Some(block)
                && block_at(state, world, other) == Some(other_block)
                && chests_pair(state, world, pos, other)
        };
        if !pairable(state) {
            continue;
        }
        let raised = raise == entity::Raise::Events && dispatch::heard::<ChestPair>(state);
        let mut event = raised.then(|| {
            let (left, right) = if clockwise {
                (other, pos)
            } else {
                (pos, other)
            };
            ChestPair {
                world: crate::world::Id::of(world),
                left,
                right,
                cancelled: false,
            }
        });
        if let Some(e) = &mut event {
            dispatch::call(state, e);
            if e.cancelled || (dispatch::listened::<ChestPair>(state) && !pairable(state)) {
                continue;
            }
        }
        let entities = &mut state.rules.block_entities;
        for (at, with) in [(pos, other), (other, pos)] {
            if let Some(BlockEntity {
                kind: Kind::Chest { pair },
                ..
            }) = entities.get_mut(world, at)
            {
                *pair = Some(with);
            }
        }
        mark_spawn(state, world, pos);
        mark_spawn(state, world, other);
        if let Some(e) = event {
            dispatch::watch(state, &e);
        }
        return;
    }
}

/// Whether the chests at `pos` and `other` may pair into a double chest: both single chests of the
/// same block facing the same way, `other` beside `pos` on its left or right.
fn chests_pair(state: &State, world: WorldIndex, pos: BlockPos, other: BlockPos) -> bool {
    let single = |at| {
        matches!(
            state.rules.block_entities.get(world, at).map(|e| &e.kind),
            Some(Kind::Chest { pair: None })
        )
    };
    let (Some(block), Some(other_block)) =
        (block_at(state, world, pos), block_at(state, world, other))
    else {
        return false;
    };
    let Some(facing) = cardinal(block) else {
        return false;
    };
    let beside_it = [facing.clockwise(), facing.clockwise().opposite()]
        .into_iter()
        .any(|side| beside(pos, side) == Some(other));
    single(pos)
        && single(other)
        && beside_it
        && other_block.kind() == block.kind()
        && cardinal(other_block) == Some(facing)
}

/// Whether `block` has a block entity.
pub(super) fn takes_entity(block: block::State) -> bool {
    Kind::of(block).is_some()
}

/// Whether a destruction at `pos` whose items come to `stacks` loses none: they all find room as
/// entities, or the chunk has room to keep them waiting. An action that fails this changes
/// nothing.
pub(super) fn can_hold(state: &State, world: WorldIndex, pos: BlockPos, stacks: usize) -> bool {
    entity::has_room(state, world, stacks)
        || state
            .rules
            .block_entities
            .has_spill_room(world, pos, stacks)
}

/// The stacks the block entity at `pos` holds in its slots.
pub(super) fn stacks_held(state: &State, world: WorldIndex, pos: BlockPos) -> usize {
    state
        .rules
        .block_entities
        .get(world, pos)
        .map_or(0, |e| e.items.iter().filter(|s| !s.is_empty()).count())
}

/// Removes the block entity at `pos` whose block is gone, dropping everything it held (its
/// slots and what [`take_contents`] takes, which [`removed`] leaves to the breaking code).
fn discard(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let mut contents = Vec::new();
    if let Some(e) = state.rules.block_entities.get_mut(world, pos) {
        take_contents(e, &mut contents);
    }
    removed(state, world, pos);
    drop_all(state, world, pos.to_center(), contents);
}

/// Removes the block entity of the block at `pos`, which is gone (broken, replaced, exploded),
/// closes the windows on it (without an event: the block is gone, and a listener could open it
/// again) and drops what it held as items.
///
/// A shulker box keeps the contents that fit it: they leave in the dropped box's NBT, which the
/// breaking code takes from [`with_block_data`] before calling this; the others drop. A paired
/// chest's other half becomes a single chest. Items that cannot be dropped now (the world's
/// entity limit) wait as a spill in the chunk and drop on later ticks.
pub(super) fn removed(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        return;
    };
    let pair = entity.pair();
    for session in window_viewers(state, world, pos) {
        inventory::close_window_without_event(state, session);
    }
    let Some(entity) = state.rules.block_entities.remove(world, pos) else {
        return;
    };
    if let Some(other) = pair {
        if let Some(BlockEntity {
            kind: Kind::Chest { pair },
            ..
        }) = state.rules.block_entities.get_mut(world, other)
        {
            *pair = None;
        }
        mark_spawn(state, world, other);
    }
    let mut items = entity.items;
    if matches!(entity.kind, Kind::ShulkerBox { .. }) {
        items.retain(|s| !s.can_enter_shulker_box());
    }
    drop_all(state, world, pos.to_center(), items);
}

/// The sessions with a window open on the block at `pos`; a double chest window covers both
/// halves.
pub(super) fn window_viewers(state: &State, world: WorldIndex, pos: BlockPos) -> Vec<SessionId> {
    let entities = &state.rules.block_entities;
    let mut out = entities.viewers(world, pos).to_vec();
    if let Some(other) = entities.get(world, pos).and_then(BlockEntity::pair) {
        for s in entities.viewers(world, other) {
            if !out.contains(s) {
                out.push(*s);
            }
        }
    }
    out
}

/// Drops `stacks` at `at`; what cannot drop now (the world's entity limit) waits as a spill in
/// the chunk of `at`, so no item is lost.
pub(super) fn drop_all(state: &mut State, world: WorldIndex, at: Vec3, stacks: Vec<Stack>) {
    let mut left = Vec::new();
    for stack in stacks.into_iter().filter(|s| !s.is_empty()) {
        if !inventory::drop_stack(state, world, at, stack.clone()) {
            left.push(stack);
        }
    }
    if !left.is_empty() {
        state
            .rules
            .block_entities
            .spill(world, BlockPos::containing(at), left);
    }
}

/// Whether a block entity of `kind` belongs to `block`: the kind the block's type takes, by its
/// Bedrock id.
fn fits(kind: &Kind, block: block::State) -> bool {
    match (kind, Kind::of(block)) {
        (Kind::Other(_), None) => block != block::AIR,
        (_, None) => false,
        (kind, Some(new)) => kind.id() == new.id(),
    }
}

/// Takes out what a block entity holds that [`removed`] does not drop: a shulker box's slots
/// (they leave with a broken box's item).
fn take_contents(entity: &mut BlockEntity, out: &mut Vec<Stack>) {
    if matches!(entity.kind, Kind::ShulkerBox { .. }) {
        out.extend(entity.items.iter_mut().map(std::mem::take));
    }
    out.retain(|s| !s.is_empty());
}

/// The item a block whose item form is its block entity's turns into (a bed and a banner): the
/// bed of its colour, the banner of its colour with its patterns. A block without its block
/// entity (a generator's block, never used) turns into what an empty one gives, as if it was
/// made on this first use. `None` for other blocks.
pub(super) fn own_item(state: &State, world: WorldIndex, pos: BlockPos) -> Option<Stack> {
    let empty = Compound::new();
    let fresh;
    let kind = if let Some(entity) = state.rules.block_entities.get(world, pos) {
        &entity.kind
    } else {
        fresh = Kind::of(block_at(state, world, pos)?)?;
        &fresh
    };
    match kind {
        Kind::Bed { colour } => {
            let t = item::Type::from_name("minecraft:bed", u16::from(*colour), None, &empty)?;
            Some(Stack::new(t, 1))
        }
        Kind::Banner(b) => {
            let mut tag = Compound::new();
            if !b.patterns.is_empty() {
                tag.insert("Patterns".to_owned(), decor::patterns_value(&b.patterns));
            }
            if b.ty != 0 {
                tag.insert("Type".to_owned(), Value::Int(b.ty));
            }
            let meta = u16::try_from(b.base).unwrap_or(0);
            let t = item::Type::from_name("minecraft:banner", meta, None, &tag)?;
            Stack::from_parts(t, 1, meta, (!tag.is_empty()).then_some(tag)).ok()
        }
        _ => None,
    }
}

/// `stack` (the item form of the block at `pos`, without extra data) with the block entity's
/// data an item keeps: a shulker box's items that fit it ([`Stack::can_enter_shulker_box`]) and
/// custom name, a container's custom name. The stack unchanged for other blocks.
pub(super) fn with_block_data(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    stack: Stack,
) -> Stack {
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        return stack;
    };
    let Some(item) = stack.item() else {
        return stack;
    };
    let mut tag = stack.tag().unwrap_or_default();
    if matches!(entity.kind, Kind::ShulkerBox { .. }) {
        // A loaded box keeps the stacks it does not take while its chunk has no room to spill
        // them ([`Parsed::read`]); they drop on their own when it goes ([`removed`]).
        let fitting: Vec<Stack> = entity
            .items
            .iter()
            .map(|s| {
                if s.can_enter_shulker_box() {
                    s.clone()
                } else {
                    Stack::EMPTY
                }
            })
            .collect();
        if fitting.iter().any(|s| !s.is_empty()) {
            tag.insert("Items".to_owned(), write_items(&fitting));
        }
    }
    if let Some(name) = &entity.name {
        let display = Compound::from([("Name".to_owned(), Value::String(name.clone()))]);
        tag.insert("display".to_owned(), Value::Compound(display));
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: a block entity's name is at most block_entity::MAX_NAME_BYTES and \
                  only the stacks that fit a shulker box are taken, so 27 of them with the name \
                  are one stack (item::MAX_NBT_BYTES)"
    )]
    Stack::from_parts(item, stack.count(), stack.meta(), Some(tag))
        .expect("a block's item with its block entity's data is within the stack limits")
}

/// Puts `stack` into slot `index` of the block entity at `pos` outside a window (the inventory
/// API). Ok(false) if the slot held `stack` already; `NoSlot` without such a block entity or
/// slot, `NotAccepted` for a stack a shulker box does not take. Only a write that changes the
/// slot marks the chunk changed. The caller tells the window viewers.
pub(super) fn set_slot(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    index: usize,
    stack: Stack,
) -> crate::inventory::Result<bool> {
    let entities = &mut state.rules.block_entities;
    let entity = entities
        .get(world, pos)
        .ok_or(crate::inventory::Error::NoSlot)?;
    if matches!(entity.kind, Kind::ShulkerBox { .. }) && !stack.can_enter_shulker_box() {
        return Err(crate::inventory::Error::NotAccepted);
    }
    let slot = entity
        .items
        .get(index)
        .ok_or(crate::inventory::Error::NoSlot)?;
    if *slot == stack {
        return Ok(false);
    }
    entities.update(world, pos, |entity| {
        *entity.items.get_mut(index)? = stack;
        Some(())
    });
    Ok(true)
}

/// Runs one tick of block entities: spilled items of broken containers drop when the world has
/// room, at most [`MAX_SPILL_DROPS_PER_TICK`] stacks a tick, in chunk order.
pub(super) fn tick(state: &mut State) {
    let mut budget = MAX_SPILL_DROPS_PER_TICK;
    // The world at its entity cap this tick: its other chunks wait too, other worlds' do not.
    let mut full = None;
    // In order, without a copy of the set: each step finds the next chunk after the last.
    let mut next = state.rules.block_entities.spilling.first().copied();
    while let Some(key) = next
        && budget > 0
    {
        next = state
            .rules
            .block_entities
            .spilling
            .range((Bound::Excluded(key), Bound::Unbounded))
            .next()
            .copied();
        if full == Some(key.0) {
            // The rest of the full world's chunks wait: go on at the next world.
            let last = (key.0, ChunkPos::new(i32::MAX, i32::MAX));
            next = state
                .rules
                .block_entities
                .spilling
                .range((Bound::Excluded(last), Bound::Unbounded))
                .next()
                .copied();
            continue;
        }
        let entities = &mut state.rules.block_entities;
        if entities
            .chunks
            .get(&key)
            .is_none_or(|c| c.spills.is_empty())
        {
            entities.spilling.remove(&key);
            continue;
        }
        while budget > 0 {
            let Some((pos, stack)) = state
                .rules
                .block_entities
                .chunks
                .get(&key)
                .and_then(|c| c.spills.first())
                .and_then(|s| Some((s.pos, s.items.last()?.clone())))
            else {
                break;
            };
            budget -= 1;
            if !inventory::drop_stack(state, key.0, pos.to_center(), stack) {
                full = Some(key.0);
                break;
            }
            let entities = &mut state.rules.block_entities;
            if let Some(chunk) = entities.chunks.get_mut(&key) {
                if let Some(first) = chunk.spills.first_mut() {
                    first.items.pop();
                    if first.items.is_empty() {
                        chunk.spills.remove(0);
                    }
                }
                entities.changed.insert(key);
            }
        }
    }
}

/// Sends a chest-like lid event for `viewers` people looking in: chests, ender chests and shulker
/// boxes animate their lid; a barrel shows `open_bit`.
pub(super) fn animate(state: &mut State, world: WorldIndex, pos: BlockPos, viewers: usize) {
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    if block.is(Class::Barrel) {
        let open = PropertyValue::Byte(u8::from(viewers > 0));
        if let Some(next) = block.with("open_bit", open)
            && next != block
            && let Some(w) = state.worlds.get_mut(world)
            && w.set_block(pos, next).is_err()
        {
            warn!(x = pos.x, y = pos.y, z = pos.z, "could not open the barrel");
        }
        return;
    }
    if block.is(Class::Chest) || block.is(Class::EnderChest) || block.is(Class::ShulkerBox) {
        let packet = Packet::BlockEvent(BlockEvent {
            position: [pos.x, pos.y, pos.z],
            event_type: i32::from(BlockEventType::ChangeChestState),
            event_data: i32::try_from(viewers).unwrap_or(i32::MAX),
        });
        state.broadcast(world, pos.chunk(), &packet, None);
    }
}

/// Plays the open sound (`viewers` above 0) or the close sound of the chest-like block at `pos`
/// at its center; other blocks play none.
pub(super) fn lid_sound(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    viewers: usize,
    raise: entity::Raise,
) {
    use crate::world::Sound as S;
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    let (open, close) = if block.is(Class::Chest) {
        (S::ChestOpen, S::ChestClose)
    } else if block.is(Class::EnderChest) {
        (S::EnderChestOpen, S::EnderChestClose)
    } else if block.is(Class::Barrel) {
        (S::BarrelOpen, S::BarrelClose)
    } else if block.is(Class::ShulkerBox) {
        (S::ShulkerBoxOpen, S::ShulkerBoxClose)
    } else {
        return;
    };
    let sound = if viewers > 0 { open } else { close };
    super::emit::sound_as(state, world, pos.to_center(), sound, raise);
}

/// Marks the block entity at `pos` for [`send_marked`] to send its client form at the end of the
/// tick: a block entity changed many times in one tick is built and sent once.
pub(super) fn mark_spawn(state: &mut State, world: WorldIndex, pos: BlockPos) {
    state.rules.block_entities.marked.insert((world, pos));
}

/// Sends the client form of every block entity marked this tick ([`mark_spawn`]), as it is now,
/// to every player that sees its chunk. Runs after everything in the tick that can change a block
/// entity.
pub(super) fn send_marked(state: &mut State) {
    let marked = std::mem::take(&mut state.rules.block_entities.marked);
    for (world, pos) in marked {
        let Some(entity) = state
            .rules
            .block_entities
            .get(world, pos)
            .filter(|e| e.is_spawnable())
        else {
            continue;
        };
        let packet = Packet::BlockActorData(BlockActorData {
            position: [pos.x, pos.y, pos.z],
            nbt_data: entity.spawn_nbt(pos),
        });
        state.broadcast(world, pos.chunk(), &packet, None);
    }
}

/// Reads an `Items` list into `slots` by `Slot` (custom items through `custom`). An entry of an
/// unknown item ([`inventory::read_saved_stack`]), in no slot, or in a slot an earlier entry
/// took is dropped, and gone from the chunk's next save: the log says where it was and which
/// item it was.
fn read_items(items: &[Compound], pos: BlockPos, custom: &Registry, slots: &mut [Stack]) {
    for entry in items {
        let slot = match entry.get("Slot") {
            Some(Value::Byte(s)) => usize::from(*s),
            _ => usize::MAX,
        };
        let item = match entry.get("Name") {
            Some(Value::String(name)) => name.as_str(),
            _ => "",
        };
        let Some(target) = slots.get_mut(slot) else {
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                slot,
                item,
                "a container item is in no slot; it is dropped and gone from the next save"
            );
            continue;
        };
        if !target.is_empty() {
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                slot,
                item,
                "a container item is in a taken slot; it is dropped and gone from the next save"
            );
            continue;
        }
        #[cfg(test)]
        crate::tick::rules::reads::add(|r| r.stacks += 1);
        if let Some(stack) = inventory::read_saved_stack(
            entry,
            custom,
            format_args!("container at {} {} {}, slot {slot}", pos.x, pos.y, pos.z),
        ) {
            *target = stack;
        }
    }
}

/// An `Items` list of the non-empty slots, with `Slot`.
fn write_items(slots: &[Stack]) -> Value {
    let list = (0_u8..)
        .zip(slots)
        .filter(|(_, s)| !s.is_empty())
        .map(|(slot, s)| {
            let mut nbt = s.to_nbt();
            nbt.insert("Slot".to_owned(), Value::Byte(slot));
            nbt
        })
        .collect();
    Value::List(List::Compound(list))
}

/// The position a saved entry names, if it is in the chunk of `key`.
fn saved_pos(nbt: &Compound, key: Key) -> Option<BlockPos> {
    let coordinate = |k: &str| match nbt.get(k) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let pos = BlockPos::new(coordinate("x")?, coordinate("y")?, coordinate("z")?);
    (pos.chunk() == key.1 && (crate::world::MIN_Y..=crate::world::MAX_Y).contains(&pos.y))
        .then_some(pos)
}

/// Keeps `items`, the stacks of a broken container or a loaded shulker box at `pos` that wait to
/// drop, in `spills`: as spills of their own of at most [`MAX_SPILL_STACKS`] stacks each (what a
/// saved spill takes) while [`MAX_SPILLS`] allows, then in the slots the waiting spills have free
/// (dropped where those wait), so the list stays within what it holds. What finds no slot,
/// past [`MAX_SPILLS`] times [`MAX_SPILL_STACKS`] stacks in one chunk, is lost with an error; the
/// callers that can choose check [`spills_take`] first.
fn spill_unfit(spills: &mut Vec<Spill>, pos: BlockPos, mut items: Vec<Stack>) {
    while !items.is_empty() && spills.len() < MAX_SPILLS {
        let rest = items.split_off(MAX_SPILL_STACKS.min(items.len()));
        spills.push(Spill { pos, items });
        items = rest;
    }
    for spill in spills.iter_mut() {
        if items.is_empty() {
            return;
        }
        let free = MAX_SPILL_STACKS.saturating_sub(spill.items.len());
        spill.items.extend(items.drain(..free.min(items.len())));
    }
    if items.is_empty() {
        return;
    }
    error!(
        x = pos.x,
        y = pos.y,
        z = pos.z,
        lost = items.len(),
        "container items lost: too many spills waiting"
    );
}

/// Whether calls of [`spill_unfit`] that keep `stacks` stacks in all lose none: the new spills
/// [`MAX_SPILLS`] allows and the free slots of the waiting ones take them.
fn spills_take(spills: &[Spill], stacks: usize) -> bool {
    let free: usize = spills
        .iter()
        .map(|s| MAX_SPILL_STACKS.saturating_sub(s.items.len()))
        .sum();
    let new = MAX_SPILLS.saturating_sub(spills.len()) * MAX_SPILL_STACKS;
    free + new >= stacks
}

/// The `SkullType` byte of a mob head block, by its name ([`decor::SKULL_TYPES`]); a skeleton
/// skull's for another name.
fn skull_type(state: block::State) -> u8 {
    (0_u8..)
        .zip(decor::SKULL_TYPES)
        .find(|(_, name)| *name == state.name())
        .map_or(0, |(id, _)| id)
}

/// Whether a sign block hangs (block entity `HangingSign`).
fn is_hanging_sign(state: block::State) -> bool {
    state.is(Class::CeilingCenterHangingSign)
        || state.is(Class::CeilingEdgesHangingSign)
        || state.is(Class::WallHangingSign)
}

/// The horizontal facing of a chest-like block state (`minecraft:cardinal_direction`).
fn cardinal(state: block::State) -> Option<Facing> {
    let face = interact::props::facing_in(
        state,
        "minecraft:cardinal_direction",
        interact::props::Encoding::Cardinal,
    )?;
    Facing::ALL.into_iter().find(|f| f.face() == face)
}

/// The block one step toward `facing`.
fn beside(pos: BlockPos, facing: Facing) -> Option<BlockPos> {
    let (dx, dz) = facing.offset();
    Some(BlockPos::new(
        pos.x.checked_add(dx)?,
        pos.y,
        pos.z.checked_add(dz)?,
    ))
}

/// The block at `pos` of `world`, if loaded.
fn block_at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<block::State> {
    state.worlds.get(world)?.block(pos)
}

#[cfg(test)]
pub(in crate::tick) mod tests {
    use super::*;

    use crate::event::Listeners;
    use crate::item::Kind as ItemKind;
    use crate::world::{Chunk as Terrain, ChunkPos, Kind as WorldKind, World};

    fn chest_entity(items: &[(usize, Stack)], pair: Option<BlockPos>) -> BlockEntity {
        let mut e = BlockEntity::new(Kind::Chest { pair });
        for (slot, s) in items {
            e.items[*slot] = s.clone();
        }
        e.name = Some("Loot".to_owned());
        e.rest
            .insert("Lock".to_owned(), Value::String("key".to_owned()));
        e
    }

    #[test]
    fn a_saved_chunk_reads_and_writes_back_byte_for_byte() {
        let key = (0, ChunkPos::new(1, -2));
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let mut chunk = Chunk::default();
        let a = BlockPos::new(16, 3, -32);
        let b = BlockPos::new(17, 3, -32);
        chunk.entities.insert(
            a,
            chest_entity(&[(0, stick.clone()), (26, stick.clone())], Some(b)),
        );
        chunk.entities.insert(b, chest_entity(&[], Some(a)));
        chunk.entities.insert(
            BlockPos::new(18, 3, -32),
            BlockEntity::read(
                Compound::from([
                    ("id".to_owned(), Value::String("Furnace".to_owned())),
                    ("BurnTime".to_owned(), Value::Short(7)),
                ]),
                BlockPos::new(18, 3, -32),
                &Registry::default(),
            ),
        );
        chunk.spills.push(Spill {
            pos: BlockPos::new(19, 3, -31),
            items: vec![stick.clone()],
        });
        let list = chunk.write();
        let read = Chunk::read(key, &list, &Registry::default()).unwrap();
        assert_eq!(read, chunk);
        assert_eq!(read.write(), list);
        // The chunk's blocks are not loaded: nothing is checked against them.
        let world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        assert_eq!(
            store.unload(key),
            None,
            "unchanged chunks hand nothing back"
        );
        store.load_list(key, &list, &world, &Registry::default());
        assert!(store.get_mut(0, a).is_some());
        assert_eq!(store.take_changed(), vec![(key, list.clone())]);
        assert_eq!(store.take_changed(), Vec::new());
        assert_eq!(store.unload(key), None);
        assert_eq!(store.sizes(), Sizes::default());
    }

    /// A world with the chunk at `key` loaded and empty (air).
    fn world_with(key: Key) -> World {
        let mut world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        world.insert_chunk(key.1, Terrain::new());
        world
    }

    #[test]
    fn a_loaded_block_entity_without_its_block_is_removed_and_its_items_wait_to_drop() {
        let key = (0, ChunkPos::new(0, 0));
        let mut world = world_with(key);
        let (chest, furnace, shulker) = (
            BlockPos::new(1, 2, 3),
            BlockPos::new(2, 2, 3),
            BlockPos::new(3, 2, 3),
        );
        world
            .set_block(furnace, block::Kind::Furnace.default_state())
            .unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let mut boxed = BlockEntity::new(Kind::ShulkerBox { facing: 1 });
        boxed.items[0] = stick.clone();
        let legacy = Compound::from([("id".to_owned(), Value::String("Furnace".to_owned()))]);
        let list = vec![
            chest_entity(&[(0, stick.clone()), (26, stick.clone())], None).write(chest),
            BlockEntity::read(legacy, furnace, &Registry::default()).write(furnace),
            boxed.write(shulker),
        ];
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        assert!(store.get(0, chest).is_none(), "air takes no chest");
        assert!(store.get(0, shulker).is_none());
        assert!(store.get(0, furnace).is_some(), "a furnace keeps its own");
        let spills = &store.chunks[&key].spills;
        assert_eq!(
            spills,
            &vec![
                Spill {
                    pos: chest,
                    items: vec![stick.clone(), stick.clone()],
                },
                Spill {
                    pos: shulker,
                    items: vec![stick.clone()],
                },
            ],
            "every item waits to drop"
        );
        assert!(store.spilling.contains(&key));
        let changed = store.take_changed();
        assert_eq!(changed.len(), 1, "the chunk is saved without them");
        // Read back, the spills stay and nothing more is removed.
        let mut again = BlockEntities::default();
        again.load_list(key, &changed[0].1, &world, &Registry::default());
        assert_eq!(again.chunks[&key], store.chunks[&key]);
        assert!(again.take_changed().is_empty());
    }

    /// A saved shulker box holding a stack its slots do not take (a list written outside Anchor)
    /// loads without it: the stack waits as a spill, a name over the limit stays as saved and is
    /// not shown, and the chunk is saved again that way.
    #[test]
    fn a_loaded_shulker_box_spills_what_its_slots_do_not_take() {
        let key = (0, ChunkPos::new(0, 0));
        let mut world = world_with(key);
        let pos = BlockPos::new(1, 2, 3);
        let shulker = block::State::all()
            .find(|b| b.is(Class::ShulkerBox))
            .unwrap();
        world.set_block(pos, shulker).unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let big = stick
            .with_extra(Some(item::Extra {
                nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![1; 4 << 10]))]),
                ..item::Extra::default()
            }))
            .unwrap();
        let mut entity = BlockEntity::new(Kind::ShulkerBox { facing: 1 });
        entity.items[0] = stick.clone();
        entity.items[1] = big.clone();
        let long = "n".repeat(crate::block_entity::MAX_NAME_BYTES + 1);
        entity.name = Some(long.clone());
        let list = vec![entity.write(pos)];
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        let loaded = store.get(0, pos).unwrap();
        assert_eq!(loaded.items[0], stick);
        assert!(loaded.items[1].is_empty());
        assert_eq!(loaded.name, None);
        assert_eq!(
            loaded.rest.get("CustomName"),
            Some(&Value::String(long.clone()))
        );
        assert_eq!(
            store.chunks[&key].spills,
            vec![Spill {
                pos,
                items: vec![big],
            }]
        );
        let changed = store.take_changed();
        assert_eq!(changed.len(), 1, "the chunk is saved without it");
        assert_eq!(
            changed[0].1[0].get("CustomName"),
            Some(&Value::String(long))
        );
    }

    /// The state of a server and the default world's spawn chunk, whose block entities are not
    /// loaded yet ([`load_chunk`]).
    fn server() -> (State, Key) {
        let state = crate::tick::tests::new_state(
            crate::tick::tests::settings(|_| {}),
            Listeners::default(),
        );
        let key = (0, state.worlds[0].spawn().chunk());
        (state, key)
    }

    /// With the chunk's spills full, the stacks a loaded shulker box's slots do not take join a
    /// waiting spill: the box holds only what fits (so its item when broken is one stack), no
    /// item is lost, and the list reads back the same.
    #[test]
    fn a_loaded_shulker_box_spills_what_it_cannot_take_with_the_spills_full() {
        let (mut state, key) = server();
        let pos = state.worlds[0].spawn();
        let shulker = block::State::all()
            .find(|b| b.is(Class::ShulkerBox))
            .unwrap();
        state.worlds[0].set_block(pos, shulker).unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let big = stick
            .with_extra(Some(item::Extra {
                nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![1; 4 << 10]))]),
                ..item::Extra::default()
            }))
            .unwrap();
        let mut entity = BlockEntity::new(Kind::ShulkerBox { facing: 1 });
        entity.items[1] = big;
        let mut list = vec![entity.write(pos)];
        let spill = Spill {
            pos,
            items: vec![stick],
        };
        let spills = Chunk {
            spills: vec![spill; MAX_SPILLS],
            ..Chunk::default()
        };
        list.extend(spills.write());
        let count = |state: &State| -> usize {
            state
                .rules
                .block_entities
                .all_items()
                .map(|s| usize::from(s.count()))
                .sum()
        };
        let all = 5 * (MAX_SPILLS + 1);
        let blocks = &mut state.rules.block_entities;
        blocks.load_list(key, &list, &state.worlds[0], &Registry::default());
        let entity = state.rules.block_entities.get(0, pos).unwrap();
        assert!(entity.items.iter().all(Stack::can_enter_shulker_box));
        assert_eq!(
            state.rules.block_entities.chunks[&key].spills.len(),
            MAX_SPILLS
        );
        assert_eq!(count(&state), all);
        let block::ItemForm::Item(box_item) = shulker.item() else {
            panic!("a shulker box item");
        };
        let broken = with_block_data(&state, 0, pos, Stack::new(box_item, 1));
        assert!(
            broken.tag().is_none_or(|t| !t.contains_key("Items")),
            "the box is empty"
        );
        let list = state
            .rules
            .block_entities
            .unload(key)
            .expect("the chunk changed");
        let blocks = &mut state.rules.block_entities;
        blocks.load_list(key, &list, &state.worlds[0], &Registry::default());
        assert_eq!(count(&state), all, "the list reads back the same");
    }

    /// With every slot of the chunk's spills full, the stacks a loaded shulker box's slots do not
    /// take stay in the box, as a new spill is past [`MAX_SPILLS`]: no item is lost. The box's
    /// item takes only the stacks that fit, so it stays one stack however large the others are,
    /// and the others drop on their own when the box goes.
    #[test]
    fn a_loaded_shulker_box_keeps_what_no_spill_slot_takes() {
        let (mut state, key) = server();
        let pos = state.worlds[0].spawn();
        let shulker = block::State::all()
            .find(|b| b.is(Class::ShulkerBox))
            .unwrap();
        state.worlds[0].set_block(pos, shulker).unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let big = stick
            .with_extra(Some(item::Extra {
                nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![1; 60 << 10]))]),
                ..item::Extra::default()
            }))
            .unwrap();
        let mut entity = BlockEntity::new(Kind::ShulkerBox { facing: 1 });
        entity.items[0] = stick.clone();
        entity.items[1] = big.clone();
        entity.items[2] = big;
        let mut list = vec![entity.write(pos)];
        let spill = Spill {
            pos,
            items: vec![stick; MAX_SPILL_STACKS],
        };
        let spills = Chunk {
            spills: vec![spill; MAX_SPILLS],
            ..Chunk::default()
        };
        list.extend(spills.write());
        let blocks = &mut state.rules.block_entities;
        blocks.load_list(key, &list, &state.worlds[0], &Registry::default());
        let blocks = &state.rules.block_entities;
        assert!(
            blocks
                .get(0, pos)
                .unwrap()
                .items
                .iter()
                .any(|s| !s.can_enter_shulker_box()),
            "the stack stays in the box"
        );
        assert_eq!(blocks.chunks[&key].spills.len(), MAX_SPILLS);
        let count: usize = blocks.all_items().map(|s| usize::from(s.count())).sum();
        assert_eq!(count, 5 * (MAX_SPILL_STACKS * MAX_SPILLS + 3));
        let block::ItemForm::Item(box_item) = shulker.item() else {
            panic!("a shulker box item");
        };
        let broken = with_block_data(&state, 0, pos, Stack::new(box_item, 1));
        let Some(Value::List(List::Compound(kept))) = broken.tag().unwrap().get("Items").cloned()
        else {
            panic!("the box item holds its items");
        };
        assert_eq!(kept.len(), 1, "only the stick that fits");
        let before = state.rules.entities.count(0);
        removed(&mut state, 0, pos);
        assert_eq!(
            state.rules.entities.count(0),
            before + 2,
            "the large stacks drop"
        );
    }

    /// A list keeping more raw entries than a chunk may is damaged; saved spills past the most a
    /// chunk keeps stay raw.
    #[test]
    fn too_many_raw_entries_are_damage_and_spills_past_the_most_stay_raw() {
        let key = (0, ChunkPos::new(0, 0));
        let raw = Compound::from([("id".to_owned(), Value::String("Chest".to_owned()))]);
        let list = vec![raw; MAX_RAW_BLOCK_ENTITIES + 1];
        assert_eq!(Chunk::read(key, &list, &Registry::default()), None);
        let spill = Spill {
            pos: BlockPos::new(1, 2, 3),
            items: vec![Stack::new(ItemKind::Stick.default_type(), 1)],
        };
        let spills = Chunk {
            spills: vec![spill; MAX_SPILLS + 1],
            ..Chunk::default()
        };
        let read = Chunk::read(key, &spills.write(), &Registry::default()).unwrap();
        assert_eq!(read.spills.len(), MAX_SPILLS);
        assert_eq!(read.raw.len(), 1);
    }

    /// A list of as many raw entries as an import keeps, with a block entity made at run time,
    /// is saved and reads back the same.
    #[test]
    fn a_list_full_of_raw_entries_with_a_new_block_entity_saves_and_reads_back() {
        let key = (0, ChunkPos::new(0, 0));
        let world = world_with(key);
        let raw = Compound::from([("id".to_owned(), Value::String("Chest".to_owned()))]);
        let list = vec![raw; crate::storage::codec::MAX_EXTRA_ENTRIES];
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        store.insert(0, BlockPos::new(1, 2, 3), chest_entity(&[], None));
        let changed = store.take_changed();
        let id = crate::save::chunk_id(1, key.1);
        let kind = crate::storage::ChunkKind::BlockEntities;
        let values: Vec<_> = crate::save::seal_pages(id, kind, &changed[0].1)
            .expect("the list is saved")
            .into_iter()
            .map(|(key, value)| match key {
                crate::storage::Key::Chunk(key) => (key.part, value),
                _ => panic!("a chunk value"),
            })
            .collect();
        let (_, stored) = crate::save::read_values(id, &values).expect("the list reads back");
        assert_eq!(stored.extras.get(kind), changed[0].1);
    }

    #[test]
    fn entries_outside_the_chunk_are_kept_raw() {
        let key = (0, ChunkPos::new(0, 0));
        let outside = Compound::from([
            ("id".to_owned(), Value::String("Chest".to_owned())),
            ("x".to_owned(), Value::Int(99)),
            ("y".to_owned(), Value::Int(0)),
            ("z".to_owned(), Value::Int(0)),
        ]);
        let inside = chest_entity(&[], None).write(BlockPos::new(1, 2, 3));
        let list = vec![inside.clone(), outside.clone()];
        let chunk = Chunk::read(key, &list, &Registry::default()).unwrap();
        assert_eq!(chunk.entities.len(), 1);
        assert_eq!(chunk.raw, vec![outside]);
        assert_eq!(
            Chunk::read(key, &chunk.write(), &Registry::default()),
            Some(chunk)
        );
    }

    /// A second entry at one position is dropped and the chunk saved again without it: kept raw,
    /// it became the block entity once the first was gone, and its items came back.
    #[test]
    fn a_second_entry_at_one_position_is_dropped_and_does_not_come_back() {
        let key = (0, ChunkPos::new(0, 0));
        let pos = BlockPos::new(1, 2, 3);
        let mut world = world_with(key);
        world
            .set_block(pos, block::Kind::Chest.default_state())
            .unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let full = chest_entity(&[(0, stick.clone())], None).write(pos);
        let list = vec![full.clone(), full];
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        assert_eq!(store.all_items().filter(|s| !s.is_empty()).count(), 1);
        let saved = store.take_changed();
        assert_eq!(saved.len(), 1, "the chunk is saved again");
        let again = Chunk::read(key, &saved[0].1, &Registry::default()).unwrap();
        assert_eq!((again.entities.len(), again.raw.len()), (1, 0));
    }

    /// The client bytes of sub-chunk `index` of the chunk at `key`.
    fn client_bytes(store: &BlockEntities, key: Key, index: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        store.write_client_bytes(key.0, key.1, index, &mut bytes);
        bytes
    }

    #[test]
    fn clients_get_the_client_keys_of_typed_block_entities_and_other_ones_as_saved() {
        let key = (0, ChunkPos::new(0, 0));
        let (first_half, second_half, bed_at, pot_at) = (
            BlockPos::new(1, 2, 3),
            BlockPos::new(2, 2, 3),
            BlockPos::new(3, 2, 3),
            BlockPos::new(4, 2, 3),
        );
        let bare = BlockPos::new(5, 2, 3);
        let ender = BlockPos::new(6, 2, 3);
        let mut chunk = Chunk::default();
        chunk
            .entities
            .insert(first_half, chest_entity(&[], Some(second_half)));
        chunk
            .entities
            .insert(second_half, chest_entity(&[], Some(first_half)));
        let bed = Compound::from([
            ("id".to_owned(), Value::String("Bed".to_owned())),
            ("color".to_owned(), Value::Byte(3)),
            ("isMovable".to_owned(), Value::Byte(1)),
        ]);
        chunk
            .entities
            .insert(bed_at, BlockEntity::read(bed, bed_at, &Registry::default()));
        // The client draws a decorated pot from its sherds alone.
        let pot = Compound::from([
            ("id".to_owned(), Value::String("DecoratedPot".to_owned())),
            (
                "sherds".to_owned(),
                Value::List(List::String(vec!["minecraft:brick".to_owned(); 4])),
            ),
            ("isMovable".to_owned(), Value::Byte(1)),
        ]);
        chunk.entities.insert(
            pot_at,
            BlockEntity::read(pot.clone(), pot_at, &Registry::default()),
        );
        let no_id = Compound::from([("isMovable".to_owned(), Value::Byte(1))]);
        chunk
            .entities
            .insert(bare, BlockEntity::read(no_id, bare, &Registry::default()));
        // The client draws an ender chest only with its block entity.
        let ender_chest =
            Compound::from([("id".to_owned(), Value::String("EnderChest".to_owned()))]);
        chunk.entities.insert(
            ender,
            BlockEntity::read(ender_chest, ender, &Registry::default()),
        );
        let mut store = BlockEntities::default();
        let world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        store.load_list(key, &chunk.write(), &world, &Registry::default());
        let sent = store.client_nbt(key);
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[0].get("pairx"), Some(&Value::Int(2)));
        assert_eq!(
            sent[0].get("CustomName"),
            Some(&Value::String("Loot".to_owned()))
        );
        assert!(!sent[0].contains_key("Items") && !sent[0].contains_key("Lock"));
        assert_eq!(sent[1].get("pairx"), Some(&Value::Int(1)));
        assert_eq!(sent[2].get("color"), Some(&Value::Byte(3)));
        assert!(!sent[2].contains_key("isMovable"), "only the client keys");
        let mut saved_pot = pot;
        saved_pot.insert("x".to_owned(), Value::Int(pot_at.x));
        saved_pot.insert("y".to_owned(), Value::Int(pot_at.y));
        saved_pot.insert("z".to_owned(), Value::Int(pot_at.z));
        assert_eq!(sent[3], saved_pot);
        assert_eq!(
            sent[4].get("id"),
            Some(&Value::String("EnderChest".to_owned()))
        );
        let bytes = client_bytes(&store, key, usize::try_from((2 + 64) >> 4).unwrap());
        let mut r = crate::protocol::Reader::new(&bytes, 1 << 20, 0);
        for want in &sent {
            assert_eq!(
                &r.nbt(crate::nbt::Encoding::NetworkLittleEndian).unwrap(),
                want
            );
        }
        assert!(client_bytes(&store, key, 0).is_empty());
    }

    /// A saved block entity that fits its saved form but not the network form is left out of
    /// what the client gets, and the ones after it still read.
    #[test]
    fn a_block_entity_too_large_to_send_leaves_the_others_readable() {
        let key = (0, ChunkPos::new(0, 0));
        let (large, chest) = (BlockPos::new(1, 2, 3), BlockPos::new(2, 2, 3));
        // 4 bytes a value saved, 5 sent: past the root limit only in the network form.
        let nbt = Compound::from([
            ("id".to_owned(), Value::String("Bell".to_owned())),
            ("big".to_owned(), Value::IntArray(vec![i32::MAX; 900_000])),
        ]);
        let mut chunk = Chunk::default();
        chunk
            .entities
            .insert(large, BlockEntity::read(nbt, large, &Registry::default()));
        chunk.entities.insert(chest, chest_entity(&[], None));
        let mut store = BlockEntities::default();
        store.chunks.insert(key, chunk);
        let want = store.chunks[&key].entities[&chest].spawn_nbt(chest);
        let bytes = client_bytes(&store, key, usize::try_from((2 + 64) >> 4).unwrap());
        let mut r = crate::protocol::Reader::new(&bytes, 1 << 20, 0);
        assert!(
            r.nbt(crate::nbt::Encoding::NetworkLittleEndian).ok() == Some(want),
            "the chest is the first root"
        );
        assert!(r.nbt(crate::nbt::Encoding::NetworkLittleEndian).is_err());
    }

    thread_local! {
        /// The block entities `write_client_bytes` looked at on this thread.
        static CLIENT_ENTITIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn count_client_entity() {
        CLIENT_ENTITIES.set(CLIENT_ENTITIES.get() + 1);
    }

    /// What `write_client_bytes` gives, from a scan of the whole chunk kept to the sub-chunk.
    fn client_bytes_of_whole_chunk(store: &BlockEntities, key: Key, index: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (p, e) in &store.chunks[&key].entities {
            let sub = usize::try_from((p.y - crate::world::MIN_Y) >> 4).unwrap_or(usize::MAX);
            if sub == index && e.is_spawnable() {
                let mut w = crate::protocol::Writer::new(std::mem::take(&mut bytes), 0);
                w.nbt(&e.spawn_nbt(*p), crate::nbt::Encoding::NetworkLittleEndian)
                    .unwrap();
                bytes = w.into_inner();
            }
        }
        bytes
    }

    /// The client bytes of each sub-chunk look only at that sub-chunk's block entities: 32 for
    /// the whole column, where a scan of the chunk per sub-chunk looks at 24 x 32 = 768. The
    /// bytes are the whole chunk's scan's, roots in the same order.
    #[test]
    fn client_bytes_look_only_at_the_block_entities_of_their_sub_chunk() {
        let key = (0, ChunkPos::new(0, 0));
        let filled = [0, 5, 6, 23];
        let mut chunk = Chunk::default();
        for &index in &filled {
            let bottom = crate::world::MIN_Y + i32::try_from(index).unwrap() * 16;
            for x in [12, 3] {
                for (dy, z) in [(15, 1), (0, 14), (0, 2), (8, 7)] {
                    let pos = BlockPos::new(x, bottom + dy, z);
                    chunk.entities.insert(pos, chest_entity(&[], None));
                }
            }
        }
        let mut store = BlockEntities::default();
        store.chunks.insert(key, chunk);
        CLIENT_ENTITIES.set(0);
        let bytes: Vec<Vec<u8>> = (0..crate::world::SUB_CHUNKS)
            .map(|index| client_bytes(&store, key, index))
            .collect();
        assert_eq!(CLIENT_ENTITIES.replace(0), 32);
        for (index, bytes) in bytes.iter().enumerate() {
            assert_eq!(bytes, &client_bytes_of_whole_chunk(&store, key, index));
            let mut r = crate::protocol::Reader::new(bytes, 1 << 20, 0);
            let roots =
                std::iter::from_fn(|| r.nbt(crate::nbt::Encoding::NetworkLittleEndian).ok())
                    .count();
            assert_eq!(roots, if filled.contains(&index) { 8 } else { 0 });
        }
    }

    #[test]
    fn a_pair_that_is_not_a_neighbour_is_dropped() {
        let pos = BlockPos::new(1, 2, 3);
        let mut nbt = chest_entity(&[], None).write(pos);
        nbt.insert("pairx".to_owned(), Value::Int(5));
        nbt.insert("pairz".to_owned(), Value::Int(3));
        assert_eq!(
            BlockEntity::read(nbt, pos, &Registry::default()).pair(),
            None
        );
    }

    /// A chest paired with a spot that holds no chest (the other half was broken while its chunk
    /// was away, or the world was edited) loads as a single chest, and the chunk is saved again
    /// that way.
    #[test]
    fn a_chest_paired_with_no_chest_loads_single() {
        let key = (0, ChunkPos::new(0, 0));
        let (a, b) = (BlockPos::new(1, 2, 3), BlockPos::new(2, 2, 3));
        let mut world = world_with(key);
        world
            .set_block(a, block::Kind::Chest.default_state())
            .unwrap();
        let list = vec![chest_entity(&[], Some(b)).write(a)];
        let mut store = BlockEntities::default();
        store.load_list(key, &list, &world, &Registry::default());
        assert_eq!(store.get(0, a).unwrap().pair(), None);
        let saved = store.take_changed();
        assert_eq!(saved.len(), 1, "the chunk is saved without the pair");
        assert!(!saved[0].1[0].contains_key("pairx"));
        assert!(
            store.marked.contains(&(0, a)),
            "clients get the single chest"
        );
    }

    /// The other half of a double chest across a chunk edge was broken while its chunk was
    /// unloaded: when that chunk loads again the chest left is single.
    #[test]
    fn a_half_across_a_chunk_edge_unpairs_when_its_partner_is_gone() {
        let (west, east) = ((0, ChunkPos::new(0, 0)), (0, ChunkPos::new(1, 0)));
        let (a, b) = (BlockPos::new(15, 2, 3), BlockPos::new(16, 2, 3));
        let mut world = world_with(west);
        world.insert_chunk(east.1, Terrain::new());
        for at in [a, b] {
            world
                .set_block(at, block::Kind::Chest.default_state())
                .unwrap();
        }
        let list = |of: BlockPos, with: BlockPos| vec![chest_entity(&[], Some(with)).write(of)];
        let mut store = BlockEntities::default();
        store.load_list(west, &list(a, b), &world, &Registry::default());
        assert_eq!(
            store.get(0, a).unwrap().pair(),
            Some(b),
            "its partner is away"
        );
        store.load_list(east, &[], &world, &Registry::default());
        assert_eq!(store.get(0, a).unwrap().pair(), None, "its partner is gone");
        assert!(store.changed.contains(&west));
        // A pair that points back stays.
        let mut both = BlockEntities::default();
        both.load_list(west, &list(a, b), &world, &Registry::default());
        both.load_list(east, &list(b, a), &world, &Registry::default());
        assert_eq!(both.get(0, a).unwrap().pair(), Some(b));
        assert_eq!(both.get(0, b).unwrap().pair(), Some(a));
    }

    /// One entity of every typed kind with data, as Anchor writes it.
    fn every_kind() -> Vec<BlockEntity> {
        let stick = Stack::new(ItemKind::Stick.default_type(), 1);
        let slots = |n: usize| {
            let mut items = vec![Stack::EMPTY; n];
            items[0] = stick.clone();
            items
        };
        let kinds = vec![
            (Kind::Chest { pair: None }, slots(CHEST_SLOTS)),
            (Kind::Barrel, slots(CHEST_SLOTS)),
            (Kind::ShulkerBox { facing: 4 }, slots(CHEST_SLOTS)),
            (Kind::EnderChest, Vec::new()),
            (
                Kind::Sign(Box::new(decor::Sign {
                    front: crate::block_entity::SignText {
                        lines: ["hi".to_owned(), String::new(), String::new(), String::new()],
                        colour: 7,
                        glowing: true,
                    },
                    back: crate::block_entity::SignText::default(),
                    waxed: true,
                    hanging: true,
                })),
                Vec::new(),
            ),
            (Kind::Sign(Box::default()), Vec::new()),
            (Kind::Bed { colour: 3 }, Vec::new()),
            (
                Kind::Banner(decor::Banner {
                    base: 4,
                    patterns: vec![("bo".to_owned(), 2)],
                    ty: 0,
                }),
                Vec::new(),
            ),
            (
                Kind::FlowerPot {
                    plant: Some(decor::Plant::Known(block::Kind::Dandelion.default_state())),
                },
                Vec::new(),
            ),
            (
                Kind::Skull {
                    skull: 4,
                    rotation: 9,
                },
                Vec::new(),
            ),
        ];
        kinds
            .into_iter()
            .map(|(kind, items)| {
                let mut e = BlockEntity::new(kind);
                e.items = items;
                e.rest.insert("isMovable".to_owned(), Value::Byte(1));
                e
            })
            .collect()
    }

    /// Every typed kind reads back from its saved form as itself, of its own id and with its
    /// slot count ([`Kind::slots`]), byte for byte.
    #[test]
    fn every_kind_round_trips_its_saved_form_byte_for_byte() {
        let pos = BlockPos::new(5, 70, -9);
        for e in every_kind() {
            let saved = e.write(pos);
            let read = BlockEntity::read(saved.clone(), pos, &Registry::default());
            assert_eq!(read, e, "{saved:?}");
            assert_eq!(read.items.len(), read.kind.slots(), "{saved:?}");
            assert_eq!(read.write(pos), saved);
            let mut a = crate::protocol::Writer::new(Vec::new(), 0);
            let mut b = crate::protocol::Writer::new(Vec::new(), 0);
            a.nbt(&saved, crate::nbt::Encoding::LittleEndian).unwrap();
            b.nbt(&read.write(pos), crate::nbt::Encoding::LittleEndian)
                .unwrap();
            assert_eq!(a.as_bytes(), b.as_bytes());
            assert!(!matches!(read.kind, Kind::Other(_)), "{saved:?}");
        }
    }

    /// A flower pot's plant Anchor does not know (an imported world's cherry sapling, an inert
    /// block) is kept as it came and shown to clients as itself; one clients do not know either is
    /// kept but not sent.
    #[test]
    fn a_flower_pot_keeps_a_plant_anchor_does_not_know() {
        let pos = BlockPos::new(1, 64, 2);
        let pot = |plant: &Compound| {
            Compound::from([
                ("id".to_owned(), Value::String("FlowerPot".to_owned())),
                ("isMovable".to_owned(), Value::Byte(1)),
                ("PlantBlock".to_owned(), Value::Compound(plant.clone())),
                ("x".to_owned(), Value::Int(pos.x)),
                ("y".to_owned(), Value::Int(pos.y)),
                ("z".to_owned(), Value::Int(pos.z)),
            ])
        };
        let sapling = crate::save::palette_entry(
            "minecraft:cherry_sapling".to_owned(),
            Compound::from([("age_bit".to_owned(), Value::Byte(0))]),
        );
        let modded = Compound::from([
            ("name".to_owned(), Value::String("example:plant".to_owned())),
            ("states".to_owned(), Value::Compound(Compound::new())),
        ]);
        for (plant, shown) in [(sapling, true), (modded, false)] {
            let saved = pot(&plant);
            let read = BlockEntity::read(saved.clone(), pos, &Registry::default());
            assert_eq!(read.write(pos), saved);
            assert_eq!(
                read.spawn_nbt(pos).get("PlantBlock"),
                shown.then_some(&Value::Compound(plant)),
            );
        }
    }

    /// A chunk's block entity list through the commit's encoding (pages, sealed) and the load's
    /// decoding: what a load reads back.
    pub(in crate::tick) fn committed(list: &[Compound]) -> Vec<Compound> {
        use crate::storage::codec::ExtraRecord;
        use crate::storage::seal::{MAX_RECORD_BYTES, seal_encoded, unseal};
        let mut read = Vec::new();
        let kind = crate::storage::ChunkKind::BlockEntities;
        for page in ExtraRecord::pages(kind, list, MAX_RECORD_BYTES).expect("the list encodes") {
            let sealed = seal_encoded(b"key", &page).expect("the page seals");
            let ExtraRecord(entries) =
                unseal::<ExtraRecord<'_>>(b"key", &sealed).expect("the page reads back");
            read.extend(entries.into_owned());
        }
        read
    }

    /// A chunk of 3,000 chests, each full of enchanted named diamond swords (vanilla play): reading
    /// its list back takes more than one record's reader may allocate, so pages cut by bytes
    /// alone did not read back. Every page reads back.
    #[test]
    fn a_chunk_of_chests_full_of_enchanted_swords_reads_back() {
        let sword = Stack::new(ItemKind::DiamondSword.default_type(), 1)
            .with_extra(Some(item::Extra {
                custom_name: Some("Excalibur".to_owned()),
                enchantments: vec![
                    (item::Enchantment::Sharpness, 5),
                    (item::Enchantment::Unbreaking, 3),
                    (item::Enchantment::Vanishing, 1),
                    (item::Enchantment::FireAspect, 2),
                    (item::Enchantment::Knockback, 2),
                    (item::Enchantment::Mending, 1),
                ],
                ..item::Extra::default()
            }))
            .unwrap();
        let list: Vec<Compound> = (0..3000)
            .map(|i| {
                let pos = BlockPos::new(i & 15, -64 + (i >> 8), (i >> 4) & 15);
                chest_entity(
                    &(0..27).map(|s| (s, sword.clone())).collect::<Vec<_>>(),
                    None,
                )
                .write(pos)
            })
            .collect();
        let alloc: usize = list
            .iter()
            .map(|c| {
                let mut w = crate::protocol::Writer::new(Vec::new(), 0);
                w.nbt_within(c, crate::nbt::Encoding::LittleEndian, usize::MAX)
                    .unwrap()
                    .alloc
            })
            .sum();
        assert!(alloc > crate::storage::seal::DECODE_BUDGET, "{alloc}");
        assert_eq!(committed(&list), list);
    }

    fn food(name: &str, count: u8) -> Stack {
        Stack::new(
            item::Type::from_name(name, 0, None, &Compound::new()).unwrap(),
            count,
        )
    }

    /// The saved block entity of kind `id` at `pos` with `food` in slot 0 and 1 and the working
    /// state of its kind.
    fn working(id: &str, pos: BlockPos) -> Compound {
        let item = |slot: u8, name: &str| {
            let mut nbt = food(name, 8).to_nbt();
            nbt.insert("Slot".to_owned(), Value::Byte(slot));
            nbt
        };
        Compound::from([
            ("id".to_owned(), Value::String(id.to_owned())),
            ("x".to_owned(), Value::Int(pos.x)),
            ("y".to_owned(), Value::Int(pos.y)),
            ("z".to_owned(), Value::Int(pos.z)),
            (
                "Items".to_owned(),
                Value::List(List::Compound(vec![
                    item(0, "minecraft:beef"),
                    item(1, "minecraft:coal"),
                ])),
            ),
            ("BurnTime".to_owned(), Value::Short(100)),
            ("CookTime".to_owned(), Value::Short(0)),
            ("primary".to_owned(), Value::Int(1)),
        ])
    }

    /// Block entities that worked in the tick (a furnace with fuel and a smeltable item, a brewing
    /// stand, a campfire, a beacon with an effect) change nothing in any number of ticks: their
    /// saved data stays as it is, the chunk is not marked changed, and the block stays.
    #[test]
    fn working_block_entities_do_nothing_in_the_tick() {
        let (mut state, key) = server();
        let base = BlockPos::new(key.1.x * 16, 0, key.1.z * 16);
        let blocks = [
            ("Furnace", block::Kind::Furnace),
            ("BrewingStand", block::Kind::BrewingStand),
            ("Campfire", block::Kind::Campfire),
            ("Beacon", block::Kind::Beacon),
        ];
        let mut list = Vec::new();
        let mut placed = Vec::new();
        for (i, (id, kind)) in blocks.into_iter().enumerate() {
            let pos = BlockPos::new(base.x + i32::try_from(i).unwrap(), 0, base.z);
            state.worlds[0]
                .set_block(pos, kind.default_state())
                .unwrap();
            list.push(working(id, pos));
            placed.push((pos, state.worlds[0].block(pos)));
        }
        let blocks = &mut state.rules.block_entities;
        blocks.load_list(key, &list, &state.worlds[0], &Registry::default());
        assert!(blocks.take_changed().is_empty());
        let mut output = crate::tick::Output::default();
        for _ in 0..500 {
            crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        }
        let blocks = &mut state.rules.block_entities;
        assert!(blocks.take_changed().is_empty(), "no block entity changed");
        for (pos, block) in placed {
            assert_eq!(state.worlds[0].block(pos), block);
        }
    }

    /// Loads the (empty) block entities of the chunk at `key`.
    fn load_chunk(state: &mut State, key: Key) {
        let world = &state.worlds[key.0];
        state
            .rules
            .block_entities
            .load_list(key, &[], world, &Registry::default());
    }

    /// A block a task set that takes a block entity gets an empty one in the change phase, with
    /// its slots empty: a chest, a sign, an ender chest.
    #[test]
    fn a_block_set_without_its_block_entity_gets_an_empty_one() {
        let (mut state, key) = server();
        load_chunk(&mut state, key);
        let base = BlockPos::new(key.1.x * 16, 0, key.1.z * 16);
        let kinds = [
            block::Kind::Chest,
            block::Kind::OakSign,
            block::Kind::EnderChest,
            block::Kind::Barrel,
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            let pos = BlockPos::new(base.x + i32::try_from(i).unwrap(), 0, base.z);
            state.worlds[0]
                .set_block(pos, kind.default_state())
                .unwrap();
            assert!(state.rules.block_entities.get(0, pos).is_none());
            block_changed(&mut state, 0, pos);
            let entity = state.rules.block_entities.get(0, pos).unwrap();
            assert!(entity.items.iter().all(Stack::is_empty), "{kind:?}");
            assert_eq!(entity.items.len(), entity.kind.slots(), "{kind:?}");
        }
    }

    /// With the chunk's spills at [`MAX_SPILLS`], a broken container's stacks join the free slots
    /// of the waiting spills instead of being lost.
    #[test]
    fn a_broken_container_with_the_spills_full_keeps_its_items() {
        let (mut state, key) = server();
        load_chunk(&mut state, key);
        let pos = state.worlds[0].spawn();
        let stick = food("minecraft:stick", 1);
        let blocks = &mut state.rules.block_entities;
        let full = Spill {
            pos,
            items: vec![stick.clone()],
        };
        blocks.chunks.get_mut(&key).unwrap().spills = vec![full; MAX_SPILLS];
        blocks.spill(0, pos, vec![stick; 5]);
        let count: usize = blocks.all_items().map(|s| usize::from(s.count())).sum();
        assert_eq!(count, MAX_SPILLS + 5);
    }

    /// More stacks than a saved spill takes wait as several spills, so all of them read back.
    #[test]
    fn a_spill_of_more_stacks_than_a_saved_spill_takes_reads_back_whole() {
        let (mut state, key) = server();
        load_chunk(&mut state, key);
        let pos = state.worlds[0].spawn();
        let stacks = 300;
        let blocks = &mut state.rules.block_entities;
        blocks.spill(0, pos, vec![food("minecraft:stick", 1); stacks]);
        let list = blocks.unload(key).expect("the chunk changed");
        blocks.load_list(key, &list, &state.worlds[0], &Registry::default());
        let count: usize = blocks.all_items().map(|s| usize::from(s.count())).sum();
        assert_eq!(count, stacks);
    }

    /// Of two saved stacks in one slot of a container, the first takes the slot and the second
    /// is dropped as an entry in no slot is.
    #[test]
    fn the_first_of_two_saved_stacks_in_one_slot_takes_it() {
        let pos = BlockPos::new(1, 2, 3);
        let first = Stack::new(ItemKind::Stick.default_type(), 5);
        let second = Stack::new(ItemKind::Stick.default_type(), 7);
        let mut nbt = chest_entity(&[(0, first.clone())], None).write(pos);
        let Some(Value::List(List::Compound(items))) = nbt.get_mut("Items") else {
            panic!("a chest with items");
        };
        let mut again = second.to_nbt();
        again.insert("Slot".to_owned(), Value::Byte(0));
        items.push(again);
        let chest = BlockEntity::read(nbt, pos, &Registry::default());
        assert_eq!(chest.items[0], first);
    }

    /// A world at its entity cap keeps its spills waiting; another world's still drop.
    #[test]
    fn a_full_world_holds_back_only_its_own_spills() {
        use crate::tick::entity::{self, MAX_ENTITIES_PER_WORLD};
        let (mut state, key) = server();
        load_chunk(&mut state, key);
        let mut other = World::new("b", WorldKind::Temporary, 1, BlockPos::new(0, 0, 0));
        other.insert_chunk(ChunkPos::new(0, 0), Terrain::new());
        state.add_world(other);
        let other = (1, ChunkPos::new(0, 0));
        load_chunk(&mut state, other);
        let spawn = state.worlds[0].spawn();
        while entity::drop_item(&mut state, 0, spawn.to_center(), food("minecraft:stick", 1)) {}
        assert_eq!(state.rules.entities.count(0), MAX_ENTITIES_PER_WORLD);
        let stick = vec![food("minecraft:stick", 1)];
        let blocks = &mut state.rules.block_entities;
        blocks.spill(0, spawn, stick.clone());
        blocks.spill(1, BlockPos::new(1, 0, 1), stick);
        tick(&mut state);
        let waiting = |key: Key| state.rules.block_entities.chunks[&key].spills.len();
        assert_eq!(waiting(key), 1, "the full world keeps its spill");
        assert_eq!(waiting(other), 0, "the other world's spill dropped");
    }

    /// A saved name on a kind without one is kept as saved and not shown.
    #[test]
    fn a_saved_name_stays_off_kinds_without_one() {
        let pos = BlockPos::new(1, 2, 3);
        let saved = Compound::from([
            ("id".to_owned(), Value::String("Sign".to_owned())),
            ("CustomName".to_owned(), Value::String("x".to_owned())),
        ]);
        let sign = BlockEntity::read(saved, pos, &Registry::default());
        assert_eq!(sign.name, None);
        assert!(!sign.spawn_nbt(pos).contains_key("CustomName"));
        assert_eq!(
            sign.write(pos).get("CustomName"),
            Some(&Value::String("x".to_owned()))
        );
    }

    /// A slot write that changes nothing or that the block entity refuses leaves its chunk
    /// unchanged, so the next save writes nothing again.
    #[test]
    fn calls_that_change_nothing_leave_the_chunk_unchanged() {
        let (mut state, key) = server();
        load_chunk(&mut state, key);
        let pos = state.worlds[0].spawn();
        state
            .rules
            .block_entities
            .insert(0, pos, chest_entity(&[], None));
        assert_eq!(state.rules.block_entities.take_changed().len(), 1);
        assert!(matches!(
            set_slot(&mut state, 0, pos, 0, Stack::EMPTY),
            Ok(false)
        ));
        assert!(matches!(
            set_slot(&mut state, 0, pos, CHEST_SLOTS, Stack::EMPTY),
            Err(crate::inventory::Error::NoSlot)
        ));
        assert!(state.rules.block_entities.take_changed().is_empty());
    }
}
