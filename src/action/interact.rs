//! Actions of a player on blocks, items and entities: breaking, clicking and using (PMMP
//! `Player::attackBlock`, `breakBlock`, `interactBlock`, `useHeldItem`, `interactEntity`).
//!
//! Each goes through the same rules and events as the player's own click; the rules live in the
//! tick's interaction module.

use super::Action;
use crate::block::Face;
use crate::math::Vec3;
use crate::world::BlockPos;
use crate::{entity, player};

/// The player left-clicks the block at `pos` on `face`: the block reacts (a dragon egg jumps,
/// fire on that face goes out) or the player starts breaking it, after
/// [`PlayerInteract`](crate::event::PlayerInteract).
#[doc(alias = "Player::attackBlock")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttackBlock {
    pub(crate) player: player::Id,
    pub(crate) pos: BlockPos,
    pub(crate) face: Face,
}

impl AttackBlock {
    /// `player` left-clicks the block at `pos` on `face`.
    pub fn new(player: player::Id, pos: BlockPos, face: Face) -> Self {
        Self { player, pos, face }
    }
}

/// The player breaks the block at `pos` now with the held item, as if the break time had passed
/// ([`BlockBreak`](crate::event::BlockBreak) starts with `insta_break` set). The block must be
/// within the player's reach; drops, experience and tool wear are the player's own break's.
#[doc(alias = "Player::breakBlock")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakBlock {
    pub(crate) player: player::Id,
    pub(crate) pos: BlockPos,
}

impl BreakBlock {
    /// `player` breaks the block at `pos`.
    pub fn new(player: player::Id, pos: BlockPos) -> Self {
        Self { player, pos }
    }
}

/// The player right-clicks the block at `pos` on `face` with the held item: the block reacts,
/// the item is used on it or placed (with [`PlayerInteract`](crate::event::PlayerInteract) and
/// the events that follow). The block must be within the player's reach.
#[doc(alias = "Player::interactBlock")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UseItemOn {
    pub(crate) player: player::Id,
    pub(crate) pos: BlockPos,
    pub(crate) face: Face,
    pub(crate) click: Vec3,
}

impl UseItemOn {
    /// `player` right-clicks the block at `pos` on `face`, at `click` on the face: each part 0
    /// to 1 (other values are clamped, a value that is not a number counts as 0).
    pub fn new(player: player::Id, pos: BlockPos, face: Face, click: Vec3) -> Self {
        Self {
            player,
            pos,
            face,
            click,
        }
    }
}

/// The player uses the held item in the air: throws it, puts armour on, starts eating or drawing
/// a bow (with [`PlayerItemUse`](crate::event::PlayerItemUse)).
#[doc(alias = "Player::useHeldItem")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UseItem {
    pub(crate) player: player::Id,
}

impl UseItem {
    /// `player` uses the held item in the air.
    pub fn new(player: player::Id) -> Self {
        Self { player }
    }
}

/// The player right-clicks the entity `entity` (a villager NPC opens trading), after
/// [`PlayerEntityInteract`](crate::event::PlayerEntityInteract). The entity must be within the
/// player's reach.
#[doc(alias = "Player::interactEntity")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InteractEntity {
    pub(crate) player: player::Id,
    pub(crate) entity: entity::Id,
    pub(crate) click: Vec3,
}

impl InteractEntity {
    /// `player` right-clicks `entity` at `click`, relative to the entity's position (a part
    /// that is not a number counts as 0).
    pub fn new(player: player::Id, entity: entity::Id, click: Vec3) -> Self {
        Self {
            player,
            entity,
            click,
        }
    }
}

impl Action for AttackBlock {}
impl Action for BreakBlock {}
impl Action for UseItemOn {}
impl Action for UseItem {}
impl Action for InteractEntity {}
