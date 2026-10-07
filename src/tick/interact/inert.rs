//! A player breaking an inert block ([`block::Inert`]): PMMP `Player::breakBlock` with the same
//! checks as for an Anchor state, but no drops of its own and no rule of the block: it becomes air.

use tracing::debug;

use super::{
    Actor, Event, Hit, State, USE, adventure_allows, break_done, center, finish_break_with, held,
    level_event, return_items, spawn_drops, stop_breaking, stream, tool_after_break,
};
use crate::block::{self, Inert};
use crate::config::GameMode;
use crate::event::BlockBreak;
use crate::item::Stack;
use crate::tick::{dispatch, entity};
use crate::world::BlockPos;

/// The client says it broke the inert block `block` at `pos`, in reach. False (the caller
/// resyncs) for a spectator, an adventure player whose tool's `CanDestroy` does not name it, a
/// block that cannot be broken, a break before its time when `counted`, or a [`BlockBreak`]
/// listener that cancels it. The event names [`block::INFO_UPDATE`] in its place (the block
/// Anchor stands in for states it does not implement) and starts with no drops; drops and
/// experience a listener gives are dropped.
pub(super) fn break_block(
    state: &mut State,
    mut actor: Actor,
    pos: BlockPos,
    block: Inert,
    counted: bool,
) -> bool {
    let session = actor.session;
    let tool = held(state, session);
    if !allowed(&actor, &tool, block) {
        stop_breaking(state, session);
        return false;
    }
    let mut insta_break = !counted;
    let mut event = None;
    if dispatch::heard::<BlockBreak>(state) {
        let mut e = BlockBreak {
            player: crate::player::Id::new(session),
            world: crate::world::Id::of(actor.world),
            pos,
            block: block::INFO_UPDATE,
            item: tool.clone(),
            insta_break,
            drops: Vec::new(),
            xp_drop_amount: 0,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            stop_breaking(state, session);
            return false;
        }
        if dispatch::listened::<BlockBreak>(state) {
            // A listener may have moved the player, changed the block or taken the tool.
            let again = super::actor(state, session).filter(|a| {
                a.world == actor.world
                    && a.can_interact(center(pos), a.reach())
                    && held(state, session) == tool
                    && Hit::at(state, a.world, pos) == Some(Hit::Inert(block))
                    && allowed(a, &tool, block)
            });
            let Some(again) = again else {
                stop_breaking(state, session);
                return false;
            };
            actor = again;
        }
        insta_break = e.insta_break;
        event = Some(e);
    }
    if counted && !insta_break && !break_done(state, session, &actor, pos) {
        debug!(session = session.get(), "break before its time");
        stop_breaking(state, session);
        return false;
    }
    let (drops, xp) = event
        .as_ref()
        .map_or((Vec::new(), 0), |e| (e.drops.clone(), e.xp_drop_amount));
    let broke = finish_break_with(state, &actor, pos, |state| {
        destroy(state, &actor, pos, block, &tool, drops, xp)
    });
    if let Some(e) = event.filter(|_| broke) {
        dispatch::watch(state, &e);
    }
    broke
}

/// PMMP `World::useBreakOn`'s checks for a player: not a spectator, `CanDestroy` in adventure
/// mode, and a breakable block with finite resources.
fn allowed(actor: &Actor, tool: &Stack, block: Inert) -> bool {
    actor.mode != GameMode::Spectator
        && (actor.mode != GameMode::Adventure || adventure_allows(tool, "CanDestroy", block.name()))
        && (!actor.finite() || block.break_info().is_breakable())
}

/// The block becomes air with its block entity gone, the viewers see it break, the listener's
/// drops fall and the tool wears. False (nothing changes) if the drops find no room for
/// entities or the block cannot be set.
fn destroy(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    block: Inert,
    tool: &Stack,
    drops: Vec<Stack>,
    xp: u32,
) -> bool {
    if !entity::has_room(state, actor.world, drops.len()) {
        debug!("break refused: no room for the drops");
        return false;
    }
    // No particle event: `Particle` names Anchor states only.
    let id = block.network_id().cast_signed();
    level_event(
        state,
        actor.world,
        center(pos),
        Event::ParticlesDestroyBlock,
        id,
    );
    super::super::block_entity::removed(state, actor.world, pos);
    if let Some(w) = state.worlds.get_mut(actor.world)
        && let Err(e) = w.set_block(pos, block::AIR)
    {
        debug!(?pos, %e, "broken block not set");
    }
    spawn_drops(state, actor.world, pos, drops, xp);
    let mut rng = stream(state, actor.world, USE, pos);
    let instant = block.break_info().breaks_instantly();
    let worn = tool_after_break(&state.settings, tool, instant, &mut rng);
    return_items(state, actor, tool, worn, Vec::new());
    true
}
