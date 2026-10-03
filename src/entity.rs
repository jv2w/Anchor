//! Entities: what lives in a world besides blocks and players (PLAN.md 10.7).
//!
//! The tick owns every entity; code outside a tick keeps only an [`Id`], which stays valid until
//! the entity is gone and is never reused for another entity.

/// An entity: its slot in the world's entity store and the slot's generation, so a stale `Id`
/// of a removed entity never finds the entity that took the slot later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id {
    index: u32,
    generation: u32,
}

impl Id {
    /// The ID of slot `index` in its `generation`.
    pub(crate) const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The slot in the entity store.
    pub(crate) const fn index(self) -> u32 {
        self.index
    }

    /// The generation of the slot this entity holds.
    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }
}
