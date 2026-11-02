//! Actions of a player on blocks, items and entities: breaking, clicking and using.
//!
//! Each goes through the same rules and events as the player's own click; the rules live in the
//! tick's interaction module. [`BreakBlock`], [`InteractEntity`] and [`UseItemOn`] need their
//! target within reach: 7 blocks of the player's eyes (13 in creative) and in front of where the
//! player looks, or nothing happens.

use super::Action;
use crate::block::Face;
use crate::math::Vec3;
use crate::world::BlockPos;
use crate::{entity, player};

/// The player left-clicks the block at `pos` on `face`.
///
/// `pos` is in the world the player is in when the action runs. The player starts breaking the
/// block, after [`PlayerInteract`](crate::event::PlayerInteract).
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

impl Action for AttackBlock {}

/// The player breaks the block at `pos` now with the held item.
///
/// It is as if the break time had passed ([`BlockBreak`](crate::event::BlockBreak) starts with
/// `insta_break` set). The position is in the world the player is in when the action runs, and
/// the player's open window closes first, as for the player's own break. Drops and tool wear are
/// the player's own break's.
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

impl Action for BreakBlock {}

/// The player right-clicks the entity `entity`.
///
/// It comes after [`PlayerEntityInteract`](crate::event::PlayerEntityInteract). A villager NPC
/// opens trading. A custom entity's
/// [`on_interact`](crate::entity::Behaviour::on_interact) runs unless a listener cancels the
/// event. The target is never a player: a right-click on another player does nothing and raises
/// no event.
#[doc(alias = "Player::interactEntity")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InteractEntity {
    pub(crate) player: player::Id,
    pub(crate) entity: entity::Id,
    pub(crate) click: Vec3,
}

impl InteractEntity {
    /// `player` right-clicks `entity` at `click`, relative to the entity's position.
    ///
    /// A part of `click` that is not finite counts as 0.
    pub fn new(player: player::Id, entity: entity::Id, click: Vec3) -> Self {
        Self {
            player,
            entity,
            click,
        }
    }
}

impl Action for InteractEntity {}

/// The player uses the held item in the air.
///
/// It throws the item, puts armour on, or starts eating or drawing a bow (with
/// [`PlayerItemUse`](crate::event::PlayerItemUse)). It never releases an item the player has
/// drawn.
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

impl Action for UseItem {}

/// The player right-clicks the block at `pos` on `face` with the held item.
///
/// `pos` is in the world the player is in when the action runs. The block reacts, or the item is
/// used on it or placed (with [`PlayerInteract`](crate::event::PlayerInteract) and the events
/// that follow).
#[doc(alias = "Player::interactBlock")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UseItemOn {
    pub(crate) player: player::Id,
    pub(crate) pos: BlockPos,
    pub(crate) face: Face,
    pub(crate) click: Vec3,
}

impl UseItemOn {
    /// `player` right-clicks the block at `pos` on `face`, at `click` on the face.
    ///
    /// Each part of `click` is 0 to 1 (other values are clamped, a value that is not finite
    /// counts as 0).
    pub fn new(player: player::Id, pos: BlockPos, face: Face, click: Vec3) -> Self {
        Self {
            player,
            pos,
            face,
            click,
        }
    }
}

impl Action for UseItemOn {}
