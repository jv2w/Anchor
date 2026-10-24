//! The rule modules' state that lives with chunks, and where it meets the chunk extras holder
//! ([`super::extra`]).
//!
//! Three modules own a chunk extras kind each: block entities ([`super::block_entity`]),
//! entities ([`super::entity`]) and scheduled block updates ([`super::update`]). Each keeps its
//! typed state for the loaded chunks and offers the same three operations, which the chunk
//! holder calls here:
//!
//! - `load(key, read)`: the chunk's saved list arrived (or came back from memory), already read
//!   into the module's typed form ([`ChunkRules`]); put it in the store. The read needs nothing
//!   of the tick's state (block entities check their blocks, which the chunk holds), so it runs
//!   on the save thread's CPU pool as the chunk loads, one list per pool item; only a chunk that
//!   comes back from memory is read on the tick ([`Rules::read`]).
//! - `take_changed()`: before a save snapshot, every chunk whose typed state changed since the
//!   last call, as the full NBT list to store; the module clears its own marks.
//! - `unload(key)`: the chunk leaves; drop the typed state and return the list if it changed
//!   since the last `take_changed` (`None`: the holder's copy is still the truth).
//!
//! A module never touches the holder itself; this file copies lists both ways.

use super::entity::EntityTypes;
use super::extra::Extras;
use super::view::Key;
use super::{block_entity, entity, update};
use crate::custom::Registry;
use crate::save::ChunkExtras;
use crate::storage::ChunkKind;
use crate::world::{Chunk, World};

/// The chunk-bound state of the rule modules.
#[derive(Debug, Default)]
pub(super) struct Rules {
    /// Containers, furnaces, signs… by position.
    pub(super) block_entities: block_entity::BlockEntities,
    /// Every non-player entity of every world.
    pub(super) entities: entity::Entities,
    /// Scheduled, neighbour and random block updates.
    pub(super) updates: update::Updates,
}

/// A chunk's extras read into the rule modules' typed form, for [`Rules::load`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChunkRules {
    block_entities: block_entity::Parsed,
    entities: Vec<entity::Saved>,
    updates: update::Parsed,
}

/// One extras kind of a chunk read ([`ChunkRules::read_kind`]): one CPU pool item of a load.
#[derive(Debug)]
pub(crate) struct ChunkRulesPart(Part);

#[derive(Debug)]
enum Part {
    BlockEntities(block_entity::Parsed),
    Entities(Vec<entity::Saved>),
    Updates(update::Parsed),
}

impl ChunkRules {
    /// Reads every kind of `extras`, the extras of chunk `key` whose blocks are `blocks`, with the
    /// server's custom content `custom` (saved items) and custom entity types `types`; `None` if
    /// a list is damaged ([`Self::read_kind`]).
    pub(crate) fn read(
        key: Key,
        extras: &ChunkExtras,
        blocks: &Chunk,
        custom: &Registry,
        types: &EntityTypes,
    ) -> Option<Self> {
        let mut rules = Self::default();
        for kind in ChunkKind::EXTRAS {
            rules.add(Self::read_kind(key, kind, extras, blocks, custom, types)?);
        }
        Some(rules)
    }

    /// Reads the `kind` list of `extras` as [`Self::read`] does. An empty list reads as
    /// [`ChunkRules::default`] has it, so a load reads only the lists that have entries. `None`
    /// for a damaged list: a block entity list keeping more entries as read than a chunk may
    /// ([`block_entity::Parsed::read`]).
    pub(crate) fn read_kind(
        key: Key,
        kind: ChunkKind,
        extras: &ChunkExtras,
        blocks: &Chunk,
        custom: &Registry,
        types: &EntityTypes,
    ) -> Option<ChunkRulesPart> {
        let list = extras.get(kind);
        Some(ChunkRulesPart(match kind {
            ChunkKind::BlockEntities => {
                let block_of = |pos: crate::world::BlockPos| {
                    let (x, z) = pos.local();
                    blocks.block(x, pos.y, z)
                };
                Part::BlockEntities(block_entity::Parsed::read(key, list, block_of, custom)?)
            }
            ChunkKind::Entities => Part::Entities(entity::read(key.1, list, types, custom)),
            ChunkKind::ScheduledUpdates => Part::Updates(update::Parsed::read(key, list)),
        }))
    }

    /// Takes in a kind read by [`Self::read_kind`].
    pub(crate) fn add(&mut self, part: ChunkRulesPart) {
        match part.0 {
            Part::BlockEntities(read) => self.block_entities = read,
            Part::Entities(read) => self.entities = read,
            Part::Updates(read) => self.updates = read,
        }
    }
}

impl Rules {
    /// Reads a chunk's extras on the tick, as the save thread reads a loaded chunk's: for an
    /// unsaved chunk that comes back from memory (and the tests' stand-ins of the save thread).
    /// Saved items are read with `world`'s custom content (a world without it, in tests, reads
    /// vanilla items only).
    #[expect(
        clippy::expect_used,
        reason = "invariant: a list in memory was read at its load or written by its module, so it is not damaged"
    )]
    pub(super) fn read(
        &self,
        key: Key,
        extras: &ChunkExtras,
        blocks: &Chunk,
        world: &World,
    ) -> ChunkRules {
        // Built only for such a world: a registry builds the vanilla creative inventory, which
        // on every chunk load made a walking player's chunk loads cost milliseconds.
        let vanilla;
        let custom = if let Some(custom) = world.custom() {
            custom
        } else {
            vanilla = Registry::default();
            &vanilla
        };
        ChunkRules::read(key, extras, blocks, custom, self.entities.types())
            .expect("invariant: a list in memory is not damaged")
    }

    /// A chunk and its extras are loaded (from the file or from memory), `read` from `extras`.
    pub(super) fn load(&mut self, key: Key, read: ChunkRules, extras: &ChunkExtras) {
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        let ChunkRules {
            block_entities: read_block_entities,
            entities: read_entities,
            updates: read_updates,
        } = read;
        block_entities.load(key, read_block_entities);
        entities.load(key, read_entities, extras.get(ChunkKind::Entities));
        updates.load(key, read_updates);
    }

    /// A chunk unloads: changed typed state goes back to the holder before it lets the extras go.
    pub(super) fn unload(&mut self, key: Key, holder: &mut Extras) {
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        let lists = [
            (ChunkKind::BlockEntities, block_entities.unload(key)),
            (ChunkKind::Entities, entities.unload(key)),
            (ChunkKind::ScheduledUpdates, updates.unload(key)),
        ];
        for (kind, list) in lists {
            if let Some(list) = list
                && let Some(held) = holder.get_mut(key, kind)
            {
                *held = list;
            }
        }
    }

    /// Before a snapshot: every changed chunk's typed state goes to the holder, marked dirty.
    pub(super) fn flush(&mut self, holder: &mut Extras) {
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        let changed = [
            (ChunkKind::BlockEntities, block_entities.take_changed()),
            (ChunkKind::Entities, entities.take_changed()),
            (ChunkKind::ScheduledUpdates, updates.take_changed()),
        ];
        for (kind, lists) in changed {
            for (key, list) in lists {
                if let Some(held) = holder.get_mut(key, kind) {
                    *held = list;
                }
            }
        }
    }

    /// Writes every module's typed state.
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        block_entities.encode(w);
        entities.checkpoint(w);
        updates.encode(w);
    }

    /// Reads what [`Self::checkpoint`] wrote; the custom entity types stay.
    pub(super) fn restore(
        &mut self,
        r: &mut super::checkpoint::Reader<'_>,
    ) -> super::checkpoint::Result<()> {
        use super::checkpoint::Codec;
        self.block_entities = Codec::decode(r)?;
        self.entities.restore(r)?;
        self.updates = Codec::decode(r)?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        block_entities.hash_into(h);
        entities.hash_into(h);
        updates.hash_into(h);
    }
}

/// What reading chunk extras did on the current thread (tests): the counting tests see which
/// thread reads. Per thread, as tests run on threads side by side.
#[cfg(test)]
pub(super) mod reads {
    use std::cell::Cell;

    /// The reads counted since the last [`take`].
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(in crate::tick) struct Reads {
        /// Saved block entities read (`BlockEntity::read`).
        pub(in crate::tick) block_entities: usize,
        /// Saved entity compounds read (`saved::load`).
        pub(in crate::tick) entities: usize,
        /// Saved scheduled update entries read (`update::parse`).
        pub(in crate::tick) updates: usize,
        /// Saved stacks read by containers, spills and item entities (`Stack::from_nbt_with`).
        pub(in crate::tick) stacks: usize,
        /// Saved compounds of the extras lists cloned.
        pub(in crate::tick) clones: usize,
    }

    thread_local! {
        static READS: Cell<Reads> = const {
            Cell::new(Reads {
                block_entities: 0,
                entities: 0,
                updates: 0,
                stacks: 0,
                clones: 0,
            })
        };
    }

    /// Counts a read on this thread.
    pub(in crate::tick) fn add(count: impl FnOnce(&mut Reads)) {
        READS.with(|r| {
            let mut reads = r.get();
            count(&mut reads);
            r.set(reads);
        });
    }

    /// The reads counted on this thread since the last call.
    pub(in crate::tick) fn take() -> Reads {
        READS.with(Cell::take)
    }
}

#[cfg(test)]
mod tests;
