//! Using a sign: the click opens its editor, and the block entity keeps the text.

use super::open_sign_editor;
use super::using::{Use, sign_front};
use crate::save::WorldIndex;
use crate::tick::State;
use crate::tick::block_entity::{self, BlockEntity, Kind};
use crate::tile::SIGN_LINES;
use crate::world::BlockPos;

/// What a sign's block entity holds of its text: the lines of each side and whether it is waxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignText {
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
        let front = sign_front(u.block, u.actor.feet, u.pos);
        open_sign_editor(state, u.actor.session, world, u.pos, front);
    }
    true
}

/// Reads the text of the sign at `pos`; `None` without a sign block entity.
pub(super) fn sign_text(state: &State, world: WorldIndex, pos: BlockPos) -> Option<SignText> {
    let BlockEntity {
        kind: Kind::Sign(sign),
        ..
    } = entity_at(state, world, pos)?
    else {
        return None;
    };
    Some(SignText {
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
    front: bool,
    lines: [String; SIGN_LINES],
) {
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = state.rules.block_entities.get_mut(world, pos)
    else {
        return;
    };
    let side = if front {
        &mut sign.front
    } else {
        &mut sign.back
    };
    side.lines = lines;
    block_entity::mark_spawn(state, world, pos);
}

/// Finds the block entity at `pos`.
fn entity_at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<&BlockEntity> {
    state.rules.block_entities.get(world, pos)
}
