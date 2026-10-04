//! The rule modules' state that lives with chunks, and where it meets the chunk extras holder
//! ([`super::extra`], PLAN.md 10.7).
//!
//! Three modules own a chunk extras kind each: block entities ([`super::block_entity`]),
//! entities ([`super::entity`]) and scheduled block updates ([`super::update`]). Each keeps its
//! typed state for the loaded chunks and offers the same three operations, which the chunk
//! holder calls here:
//!
//! - `load(key, list)`: the chunk's saved list arrived (or came back from memory); build the
//!   typed state from it (block entities also check their blocks, which the chunk holds).
//! - `take_changed()`: before a save snapshot, every chunk whose typed state changed since the
//!   last call, as the full NBT list to store; the module clears its own marks.
//! - `unload(key)`: the chunk leaves; drop the typed state and return the list if it changed
//!   since the last `take_changed` (`None`: the holder's copy is still the truth).
//!
//! A module never touches the holder itself; this file copies lists both ways.

use super::extra::Extras;
use super::view::Key;
use super::{block_entity, entity, update};
use crate::custom::Registry;
use crate::save::ChunkExtras;
use crate::storage::ChunkKind;
use crate::world::World;

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

impl Rules {
    /// A chunk and its extras are loaded (from the file or from memory). Saved items are read
    /// with the world's custom content (a world without it, in tests, reads vanilla items only).
    pub(super) fn load(&mut self, key: Key, extras: &ChunkExtras, world: &World) {
        let Self {
            block_entities,
            entities,
            updates,
        } = self;
        let vanilla = Registry::default();
        let custom = world.custom().unwrap_or(&vanilla);
        block_entities.load(key, extras.get(ChunkKind::BlockEntities), world, custom);
        entities.load(key, extras.get(ChunkKind::Entities), custom);
        updates.load(key, extras.get(ChunkKind::ScheduledUpdates));
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
