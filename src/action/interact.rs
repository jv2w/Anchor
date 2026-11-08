//! Actions of a player on blocks, items and entities: breaking, clicking and using.
//!
//! Each goes through the same rules and events as the player's own click; the rules live in the
//! tick's interaction module. [`BreakBlock`] and [`UseItemOn`] need their block within reach: 7
//! blocks of the player's eyes (13 in creative) and in front of where the player looks, or nothing
//! happens. [`InteractEntity`] needs its entity within 8 blocks of the eyes in every game mode,
//! and in front of where the player looks too.

use super::Action;
use crate::block::Face;
use crate::math::Vec3;
use crate::world::BlockPos;
use crate::{entity, player};

/// A player's left click on the block at `pos` on `face`.
///
/// `pos` is in the world the player is in when the action runs. The block must be loaded and
/// within 100 blocks of the player's feet, and the player must not be a spectator. After
/// [`PlayerInteract`](crate::event::PlayerInteract) a survival or adventure player starts breaking
/// the block; a creative player starts nothing, as a creative break arrives as a separate destroy
/// request from the client.
#[doc(alias = "Player::attackBlock")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttackBlock {
    pub(crate) player: player::Id,
    pub(crate) pos: BlockPos,
    pub(crate) face: Face,
}

impl AttackBlock {
    /// A left click by `player` on the block at `pos` on `face`.
    pub fn new(player: player::Id, pos: BlockPos, face: Face) -> Self {
        Self { player, pos, face }
    }
}

impl Action for AttackBlock {}

/// A player's immediate break of the block at `pos` with the held item.
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
    /// A break of the block at `pos` by `player`.
    pub fn new(player: player::Id, pos: BlockPos) -> Self {
        Self { player, pos }
    }
}

impl Action for BreakBlock {}

/// A player's right click on the entity `entity`.
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
    /// A right click by `player` on `entity` at `click`, relative to the entity's position.
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

/// A player's use of the held item in the air.
///
/// It throws the item, puts armor on, or starts eating or drawing a bow (with
/// [`PlayerItemUse`](crate::event::PlayerItemUse)). It never releases an item the player has
/// drawn.
#[doc(alias = "Player::useHeldItem")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UseItem {
    pub(crate) player: player::Id,
}

impl UseItem {
    /// A use of the held item in the air by `player`.
    pub fn new(player: player::Id) -> Self {
        Self { player }
    }
}

impl Action for UseItem {}

/// A player's right click on the block at `pos` on `face` with the held item.
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
    /// A right click by `player` on the block at `pos` on `face`, at `click` on the face.
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
