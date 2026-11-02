//! A player breaking an inert block ([`block::Inert`]): PMMP `Player::breakBlock` with the same
//! checks as for an Anchor state, but no drops of its own and no rule of the block: it becomes air.

use tracing::debug;

use super::{
    Actor, Event, Hit, State, break_done, center, drops_fit, finish_break_with, held, level_event,
    player_may_break, spawn_drops, stop_breaking_at, wear_after_break,
};
use crate::block::{self, Inert};
use crate::event::BlockBreak;
use crate::item::Stack;
use crate::tick::{block_entity, dispatch};
use crate::world::BlockPos;

/// Breaks the inert block `block` at `pos`, in reach, that the client says it broke. False (the
/// caller resyncs) for a spectator, an adventure player whose tool's `CanDestroy` does not name
/// it, a block that cannot be broken, a break before its time when `counted`, or a [`BlockBreak`]
/// listener that cancels it. The event names [`block::INFO_UPDATE`] in its place (the block
/// Anchor stands in for states it does not implement) and starts with no drops; drops and
/// xp a listener gives are dropped.
pub(super) fn break_block(
    state: &mut State,
    mut actor: Actor,
    pos: BlockPos,
    block: Inert,
    counted: bool,
) -> bool {
    let session = actor.session;
    let tool = held(state, session);
    if !player_may_break(&actor, &tool, block.name(), block.break_info()) {
        stop_breaking_at(state, &actor, pos);
        return false;
    }
    if counted && !break_done(state, session, &actor, pos) {
        debug!(session = session.get(), "break before its time");
        stop_breaking_at(state, &actor, pos);
        return false;
    }
    let mut event = None;
    if dispatch::heard::<BlockBreak>(state) {
        let mut e = BlockBreak {
            player: crate::player::Id::new(session),
            world: crate::world::Id::of(actor.world),
            pos,
            block: block::INFO_UPDATE,
            item: tool.clone(),
            insta_break: !counted,
            drops: Vec::new(),
            xp_drop_amount: 0,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            stop_breaking_at(state, &actor, pos);
            return false;
        }
        if dispatch::listened::<BlockBreak>(state) {
            // A listener may have moved the player, changed the block or taken the tool.
            let again = super::actor(state, session).filter(|a| {
                a.world == actor.world
                    && a.can_interact(center(pos), a.reach())
                    && held(state, session) == tool
                    && Hit::at(state, a.world, pos) == Some(Hit::Inert(block))
                    && player_may_break(a, &tool, block.name(), block.break_info())
            });
            let Some(again) = again else {
                stop_breaking_at(state, &actor, pos);
                return false;
            };
            actor = again;
        }
        event = Some(e);
    }
    let (drops, xp) = event
        .as_ref()
        .map_or((Vec::new(), 0), |e| (e.drops.clone(), e.xp_drop_amount));
    let broke = finish_break_with(state, &actor, pos, |state| {
        destroy(state, &actor, pos, block, &tool, drops, xp).then_some(())
    })
    .is_some();
    if let Some(e) = event.filter(|_| broke) {
        dispatch::watch(state, &e);
    }
    broke
}

/// Destroys the inert block: it becomes air with its block entity gone, the viewers see it break,
/// the listener's drops fall and the tool wears. False (nothing changes) if the drops find no
/// room for entities.
#[expect(
    clippy::expect_used,
    reason = "invariant: the block was just read from a loaded chunk, and no user code ran since"
)]
fn destroy(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    block: Inert,
    tool: &Stack,
    drops: Vec<Stack>,
    xp: u32,
) -> bool {
    if !drops_fit(state, actor.world, drops.len()) {
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
    block_entity::removed(state, actor.world, pos);
    state
        .worlds
        .get_mut(actor.world)
        .expect("the world of a break is loaded")
        .set_block(pos, block::AIR)
        .expect("a broken block is in a loaded chunk inside the world");
    spawn_drops(state, actor.world, pos, drops, xp);
    wear_after_break(state, actor, pos, tool, block.break_info());
    true
}
