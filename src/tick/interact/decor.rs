//! Using a sign: the click opens its editor, and the block entity keeps the text.

use super::open_sign_editor;
use super::using::{Use, sign_side};
use crate::block_entity::{SIGN_LINES, Side};
use crate::save::WorldIndex;
use crate::tick::State;
use crate::tick::block_entity::{self, BlockEntity, Kind};
use crate::world::BlockPos;

/// What a sign's block entity holds of its text: the lines of each side and whether it is waxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignSides {
    /// The front side's lines.
    pub(super) front: [String; SIGN_LINES],
    /// The back side's lines.
    pub(super) back: [String; SIGN_LINES],
    /// A waxed sign takes no edits.
    pub(super) waxed: bool,
}

/// Uses a sign: a waxed sign does nothing, any other opens the editor on the side the player
/// faces. Always true: the click is taken.
pub(super) fn sign(state: &mut State, u: &Use) -> bool {
    let world = u.actor.world;
    let waxed = matches!(
        entity_at(state, world, u.pos).map(|e| &e.kind),
        Some(Kind::Sign(sign)) if sign.waxed
    );
    if !waxed {
        let side = sign_side(u.block, u.actor.feet, u.pos);
        open_sign_editor(state, u.actor.session, world, u.pos, side);
    }
    true
}

/// The text of both sides of the sign at `pos`; `None` without a sign block entity.
pub(super) fn sign_sides(state: &State, world: WorldIndex, pos: BlockPos) -> Option<SignSides> {
    let BlockEntity {
        kind: Kind::Sign(sign),
        ..
    } = entity_at(state, world, pos)?
    else {
        return None;
    };
    Some(SignSides {
        front: sign.front.lines.clone(),
        back: sign.back.lines.clone(),
        waxed: sign.waxed,
    })
}

/// Stores the lines of one side of the sign at `pos` (the lines change, the colour and glow
/// stay); the caller checked that the sign takes the edit.
pub(super) fn write_sign(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    side: Side,
    lines: [String; SIGN_LINES],
) {
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = state.rules.block_entities.get_mut(world, pos)
    else {
        return;
    };
    let text = match side {
        Side::Front => &mut sign.front,
        Side::Back => &mut sign.back,
    };
    text.lines = lines;
    block_entity::mark_spawn(state, world, pos);
}

/// The block entity at `pos`.
fn entity_at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<&BlockEntity> {
    state.rules.block_entities.get(world, pos)
}
