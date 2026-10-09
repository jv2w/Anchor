//! What players do to blocks and with items: server-authoritative breaking, placing, using items on
//! blocks, in the air and on entities, and the per-block and per-item rules this runs (PMMP
//! `Player::attackBlock`/`continueBreakBlock`/`breakBlock`/`interactBlock`/ `useHeldItem`,
//! `World::useBreakOn`/`useItemOn`, `Block::onInteract`/`place`/`onBreak`/ `getDrops`,
//! `Item::onInteractBlock`/`onClickAir`/`onReleaseUsing`).
//!
//! Breaking is server-authoritative: a break the client predicts is accepted only when the server's
//! own progress for that block (PMMP `SurvivalBlockBreakHandler`, counted per client input tick and
//! limited by server time) has reached it; with `break-time-check = false` the progress is not
//! counted and the client's break is taken at once. A refused break or placement leaves the world
//! unchanged and sends the client the real blocks around it (PMMP `syncBlocksNearby`). Every rule
//! is split into a check that only reads and an apply step; its event goes between them, and the
//! actions in [`crate::action`] run the same rules.

pub(super) mod book;
pub(super) mod breaking;
mod custom;
mod decor;
pub(super) mod drops;
mod inert;
mod place;
pub(super) mod props;
mod rail;
pub(super) mod sign;
mod using;

pub(super) use book::on_book_edit;
pub(super) use custom::random_tick;
pub(super) use rail::ascent as rail_ascent;

use tracing::debug;

use super::survival::EYE_HEIGHT;
use super::view::{UPDATE_FLAGS, chunk_of};
use super::{Context, Phase, Player, SessionId, State, dispatch, entity, inventory, survival};
use crate::action;
use crate::block::{self, Class, Face, Kind, TypeTag};
use crate::config::GameMode;
use crate::event::{BlockBreak, ExhaustCause, InteractAction, PlayerInteract};
use crate::item::{self, Enchantment, Stack};
use crate::math::{self, Vec3};
use crate::protocol::consts::{
    InventoryActionSource, LevelEvent as Event, PlayerAction, ReleaseItemAction, TriggerType,
    UseItemAction, UseItemOnEntityAction,
};
use crate::protocol::packet::{
    BlockActorData, InventoryTransaction, LevelEvent, OpenSign, Packet, PlayerAuthInput,
    UpdateBlock,
};
use crate::protocol::{InventoryTransactionData, PlayerBlockAction, UseItemTransactionData};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{BlockPos, Entry, Particle, Sound};

/// Server ticks a client may be ahead while breaking: client input ticks counted toward a
/// break never exceed the server ticks since it started plus this (one second of lag).
const LAG_TICKS: u64 = 20;
/// The most block actions one `PlayerAuthInput` may carry (PMMP refuses more than 100).
const MAX_BLOCK_ACTIONS: usize = 100;
/// Breaking stops when the player's feet are farther than this from the block (PMMP
/// `SurvivalBlockBreakHandler` `maxPlayerDistance` 16).
const MAX_BREAK_DISTANCE: f64 = 16.0;
/// Interaction reach from the eyes (PMMP `Player::MAX_REACH_DISTANCE_*`).
const REACH_SURVIVAL: f64 = 7.0;
const REACH_CREATIVE: f64 = 13.0;
/// Positions farther than this (squared, from the feet) are ignored outright (PMMP: 100 blocks).
const MAX_ACTION_DISTANCE_SQUARED: f64 = 10_000.0;
/// An identical click within this many ticks is the client repeating itself (PMMP's 100 ms
/// "client spam bug" filter); it is ignored, so a doubled packet places once.
const REPEAT_TICKS: u64 = 2;
/// A `SimulationTick` block click at most this many ticks after the last block click goes on
/// the same held press; after a longer gap the button was let go. The official client floods
/// repeats while the button is down, and a press that starts in the air and reaches a block sends
/// no `PlayerInput` click (`facts.md`), so the gap is the only sign of a new press there.
const PRESS_GAP_TICKS: u64 = 10;
/// Food exhaustion per broken block (PMMP `Player::breakBlock`).
const BREAK_EXHAUSTION: f64 = 0.005;
/// Ticks between punch particles while breaking (PMMP `DEFAULT_FX_INTERVAL_TICKS`).
const FX_INTERVAL: u32 = 5;
/// The random stream purpose of drops.
const DROPS: u64 = random::tag("interact-drops");
/// The random stream purpose of tool wear and placement.
const USE: u64 = random::tag("interact-use");

/// A player's interaction state: the block being broken, the last click and the sign the player
/// may edit. The rotation, sneaking and effects the rules read are the survival rules'.
#[derive(Debug, Default)]
pub(super) struct Breaking {
    /// The block being broken, if any.
    target: Option<Target>,
    /// The block last attacked (PMMP `lastBlockAttacked`): a repeated start on it is ignored.
    last_attacked: Option<BlockPos>,
    /// The last block click and its tick, for the repeat filter.
    last_click: Option<(u64, Click)>,
    /// The right click the player holds or last held on blocks, for
    /// [`PlayerInteract::is_first`].
    press: Press,
    /// The sign this player may write (PMMP `BaseSign::$editorEntityRuntimeId`).
    sign_editor: Option<(WorldIndex, BlockPos)>,
    /// The tick the player started using a bow, trident, goat horn or spyglass (PMMP
    /// `Player::$startAction` for its `Releasable` items; eating is the survival rules').
    using_since: Option<u64>,
}

/// A block being broken (PMMP `SurvivalBlockBreakHandler`).
#[derive(Debug, Clone, PartialEq)]
struct Target {
    world: WorldIndex,
    pos: BlockPos,
    /// The face being hit (punch particles).
    face: Face,
    /// The block when breaking started.
    block: Hit,
    /// The server tick breaking started.
    start: u64,
    /// Client input ticks counted since the start.
    inputs: u32,
    /// The share broken, 1 or more when done.
    progress: f64,
    /// The share one tick breaks now (sent to viewers when it changes).
    speed: f64,
    /// Ticks of effects shown.
    fx: u32,
}

/// A block a player can hit: an Anchor state, or an inert one ([`block::Inert`]).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    Block(block::State),
    Inert(block::Inert),
}

impl Hit {
    /// The block at `pos` of `world`, if its chunk is loaded and players can hit it.
    fn at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<Self> {
        match state.worlds.get(world)?.entry(pos)? {
            Entry::Known(s) => Some(Self::Block(s)),
            Entry::Inert(s, _) => Some(Self::Inert(s)),
            Entry::Unknown(_) => None,
        }
    }

    /// The network ID clients know it by.
    #[cfg(test)]
    fn network_id(self) -> u32 {
        match self {
            Self::Block(s) => s.network_id(),
            Self::Inert(s) => s.network_id(),
        }
    }

    /// How it breaks.
    fn break_info(self) -> block::BreakInfo {
        match self {
            Self::Block(s) => s.break_info(),
            Self::Inert(s) => s.break_info(),
        }
    }

    /// Whether `other` is the same block in any state (breaking goes on through a state change,
    /// as leaves' decay bit).
    fn same_block(self, other: Self) -> bool {
        match (self, other) {
            (Self::Block(a), Self::Block(b)) => a.kind() == b.kind(),
            (Self::Inert(a), Self::Inert(b)) => a.name() == b.name(),
            _ => false,
        }
    }
}

super::checkpoint::variants!(Hit {
    0 => Block(state),
    1 => Inert(state),
});

/// What makes two block clicks the same (PMMP compares face, positions and click position).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Click {
    pos: BlockPos,
    face: i32,
    /// The click position's bits.
    at: [u32; 3],
    /// The player position's bits.
    player: [u32; 3],
}

/// A right click held on blocks: the client repeats its block click while the button is down.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Press {
    /// The tick of its last block click.
    last: u64,
    /// Whether it raised a right click [`PlayerInteract`] yet.
    raised: bool,
}

/// Where a right click on a block comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The player's client: [`PlayerInteract::is_first`] follows the held press.
    Client,
    /// An [`action::UseItemOn`]: a click of its own, never a repeat.
    Action,
}

impl Breaking {
    /// Notes a block click at `tick`: a `SimulationTick` click (`repeated`) soon after the last
    /// one goes on its press; any other click starts a new press.
    fn note_click(&mut self, tick: u64, repeated: bool) {
        let held_on = repeated && tick.saturating_sub(self.press.last) <= PRESS_GAP_TICKS;
        self.press = Press {
            last: tick,
            raised: held_on && self.press.raised,
        };
    }

    /// Whether a right click [`PlayerInteract`] raised now is the first of the held press; the
    /// press has raised one from now on.
    fn first_of_press(&mut self) -> bool {
        !std::mem::replace(&mut self.press.raised, true)
    }

    /// Stops drawing a bow or trident (PMMP `Player::setUsingItem(false)`): the held slot or
    /// item changed, or the player did something else with it.
    pub(super) fn stop_using(&mut self) {
        self.using_since = None;
    }

    /// Whether a bow, trident, goat horn or spyglass is in use (part of PMMP
    /// `Player::isUsingItem`).
    pub(super) fn is_using(&self) -> bool {
        self.using_since.is_some()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            target,
            last_attacked,
            last_click,
            press,
            sign_editor,
            using_since,
        } = self;
        match target {
            Some(t) => {
                let Target {
                    world,
                    pos,
                    face,
                    block,
                    start,
                    inputs,
                    progress,
                    speed,
                    fx,
                } = t;
                h.u64(1);
                h.usize(*world);
                hash_pos(h, *pos);
                h.u64(u64::from(face.index()));
                h.u64(u64::from(block.network_id()));
                h.u64(*start);
                h.u64(u64::from(*inputs));
                h.u64(progress.to_bits());
                h.u64(speed.to_bits());
                h.u64(u64::from(*fx));
            }
            None => h.u64(0),
        }
        h.u64(u64::from(last_attacked.is_some()));
        if let Some(p) = last_attacked {
            hash_pos(h, *p);
        }
        match last_click {
            Some((tick, c)) => {
                h.u64(*tick);
                hash_pos(h, c.pos);
                h.u64(c.face.cast_unsigned().into());
                for v in c.at.iter().chain(&c.player) {
                    h.u64(u64::from(*v));
                }
            }
            None => h.u64(u64::MAX),
        }
        let Press { last, raised } = press;
        h.u64(*last);
        h.u64(u64::from(*raised));
        match sign_editor {
            Some((world, pos)) => {
                h.usize(*world);
                hash_pos(h, *pos);
            }
            None => h.u64(u64::MAX),
        }
        h.u64(using_since.unwrap_or(u64::MAX));
    }
}

#[cfg(test)]
fn hash_pos(h: &mut super::Fnv, p: BlockPos) {
    for c in [p.x, p.y, p.z] {
        h.u64(u64::from(c.cast_unsigned()));
    }
}

super::checkpoint::fields!(Breaking {
    target,
    last_attacked,
    last_click,
    press,
    sign_editor,
    using_since,
});
super::checkpoint::fields!(Press { last, raised });
super::checkpoint::fields!(Target {
    world,
    pos,
    face,
    block,
    start,
    inputs,
    progress,
    speed,
    fx,
});
super::checkpoint::fields!(Click {
    pos,
    face,
    at,
    player
});

/// Why a block is destroyed: decides drops, experience and effects.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Destroy {
    /// A player broke it with `tool` (drops follow the tool, PMMP `Block::getDrops`).
    Player {
        /// Who broke it.
        session: SessionId,
        /// The held item when it broke.
        tool: Stack,
    },
    /// It lost its support or was washed away by a liquid: drops as if broken by hand.
    Rules,
    /// An explosion with `1 / size` drop chance per block (PMMP `Explosion::explodeB`).
    Explosion {
        /// The explosion size.
        size: f64,
    },
}

/// The player as the interaction rules see it.
#[derive(Debug, Clone, Copy)]
struct Actor {
    session: SessionId,
    world: WorldIndex,
    eye: Vec3,
    feet: Vec3,
    yaw: f64,
    /// The look direction (PMMP `Entity::getDirectionVector`).
    look: Vec3,
    mode: GameMode,
    sneaking: bool,
    sneak_pressed: bool,
}

impl Actor {
    /// Whether items and blocks are used up (PMMP `Player::hasFiniteResources`: every mode but
    /// creative).
    fn finite(&self) -> bool {
        self.mode != GameMode::Creative
    }

    /// The reach for blocks.
    fn reach(&self) -> f64 {
        if self.mode == GameMode::Creative {
            REACH_CREATIVE
        } else {
            REACH_SURVIVAL
        }
    }

    /// What placement reads of the player.
    fn placer(&self) -> place::Placer {
        place::Placer {
            feet: self.feet,
            eye: self.eye,
            yaw: self.yaw,
            sneaking: self.sneaking,
        }
    }

    /// PMMP `Player::canInteract`: within `max` blocks of the eyes and not behind the player.
    fn can_interact(&self, target: Vec3, max: f64) -> bool {
        survival::can_interact(self.eye, self.look, target, max)
    }
}

/// The interacting player of `session`, once spawned and while alive (PMMP
/// `DeathPacketHandler`); its rotation and sneaking are the survival rules' (they read
/// `PlayerAuthInput` before interaction does).
fn actor(state: &State, session: SessionId) -> Option<Actor> {
    let player = state.players.get(&session)?;
    if player.phase != Phase::Spawned || !survival::is_alive(player) {
        return None;
    }
    Some(Actor {
        session,
        world: player.world,
        eye: player.position,
        feet: player.position - Vec3::new(0.0, EYE_HEIGHT, 0.0),
        yaw: survival::rotation(player).0,
        look: survival::look_direction(player),
        mode: survival::game_mode(state, player),
        sneaking: survival::is_sneaking(player),
        sneak_pressed: survival::is_sneak_pressed(player),
    })
}

/// The centre of a block.
fn center(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

/// A block position of a packet.
fn block_pos(p: [i32; 3]) -> BlockPos {
    BlockPos::new(p[0], p[1], p[2])
}

/// A face of a packet; `None` for an invalid face (PMMP `validateFacing`).
fn face_of(face: i32) -> Option<Face> {
    Face::from_index(face)
}

/// The engine random stream of `purpose` at `pos` this tick.
fn stream(state: &State, world: WorldIndex, purpose: u64, pos: BlockPos) -> Random {
    let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
    let key = random::key(
        purpose,
        &[
            u64::try_from(world).unwrap_or(u64::MAX),
            u64::from(pos.x.cast_unsigned()),
            u64::from(pos.y.cast_unsigned()),
            u64::from(pos.z.cast_unsigned()),
        ],
    );
    Random::new(seed, state.tick, key)
}

/// `PlayerAuthInput`'s block actions (breaking) and item interaction (using an item on a block).
pub(super) fn on_auth_input(state: &mut State, session: SessionId, packet: &PlayerAuthInput) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase != Phase::Spawned || !survival::is_alive(player) {
        return;
    }
    // The packet is one client tick: breaking in progress advances by one tick.
    advance(state, session);
    if let Some(data) = &packet.item_interaction_data {
        use_item(state, session, data);
    }
    if let Some(actions) = &packet.block_actions {
        block_actions(state, session, actions);
    }
}

/// `InventoryTransaction`: use item (on a block or in the air), use item on entity (attack,
/// interact), release item (bows, food). Its legacy item lists are not trusted.
pub(super) fn on_inventory_transaction(
    state: &mut State,
    session: SessionId,
    packet: &InventoryTransaction,
) {
    match &packet.transaction_data {
        InventoryTransactionData::UseItem(data) => use_item(state, session, data),
        InventoryTransactionData::UseItemOnEntity(data) => {
            if actor(state, session).is_none() {
                return;
            }
            if let Ok(action) = UseItemOnEntityAction::try_from(data.action_type) {
                let [x, y, z] = data.clicked_position.map(|v| f64::from(finite(v)));
                let click = Vec3::new(x, y, z);
                using::on_entity(state, session, data.target_entity_runtime_id, action, click);
            }
        }
        InventoryTransactionData::ReleaseItem(data) => {
            if actor(state, session).is_some()
                && ReleaseItemAction::try_from(data.action_type) == Ok(ReleaseItemAction::Release)
            {
                using::release(state, session);
            }
        }
        // Item moves go through item stack requests; a normal transaction is only the Q key's
        // drop (PMMP `handleNormalTransaction`); a mismatch needs no answer.
        InventoryTransactionData::Normal(_) => legacy_drop(state, session, &packet.actions),
        InventoryTransactionData::Mismatch(_) => {}
    }
}

/// PMMP `handleNormalTransaction`: a drop by the Q key is two actions, the world's drop slot
/// with the count and the main inventory slot with the client's stack; the client's stack must be
/// the server's (network ID, meta, count) and the drop is [`inventory::drop_from_main`]. Anything
/// else (book edits send up to five actions) is ignored.
fn legacy_drop(
    state: &mut State,
    session: SessionId,
    actions: &[crate::protocol::InventoryAction],
) {
    /// PMMP `NetworkInventoryAction::ACTION_MAGIC_SLOT_DROP_ITEM`.
    const DROP_SLOT: u32 = 0;
    if actor(state, session).is_none() || actions.len() != 2 {
        return;
    }
    let (mut slot, mut client, mut count) = (None, None, None);
    for a in actions {
        if a.source_type == u32::from(InventoryActionSource::World) && a.inventory_slot == DROP_SLOT
        {
            count = u8::try_from(a.new_item.stack.count).ok().filter(|c| *c > 0);
        } else if a.source_type == u32::from(InventoryActionSource::Container)
            && a.window_id == Some(0)
        {
            slot = u8::try_from(a.inventory_slot).ok();
            client = Some(&a.old_item.stack);
        } else {
            return;
        }
    }
    let (Some(slot), Some(client), Some(count)) = (slot, client, count) else {
        return;
    };
    let Some(have) = state
        .players
        .get(&session)
        .and_then(|p| p.inventory.main_stack(slot))
    else {
        return;
    };
    let server = have.to_network();
    if server.item_type != client.item_type || server.count != client.count {
        return;
    }
    let _dropped = inventory::drop_from_main(state, session, slot, count);
}

/// A client turned a lectern's page (`LecternUpdate`).
pub(super) fn on_lectern_update(
    state: &mut State,
    session: SessionId,
    packet: &crate::protocol::packet::LecternUpdate,
) {
    decor::on_lectern_update(state, session, packet);
}

/// A client edited a block entity (sign text).
pub(super) fn on_block_actor_data(state: &mut State, session: SessionId, packet: &BlockActorData) {
    let Some(actor) = actor(state, session) else {
        return;
    };
    let pos = block_pos(packet.position);
    if center(pos).distance_squared(actor.feet) > MAX_ACTION_DISTANCE_SQUARED {
        return;
    }
    let Some(block) = state.worlds.get(actor.world).and_then(|w| w.block(pos)) else {
        return;
    };
    if !block.is(Class::BaseSign) {
        return;
    }
    let edit = match sign::parse(&packet.nbt_data) {
        Ok(edit) => edit,
        Err(refused) => {
            debug!(session = session.get(), ?refused, "sign edit refused");
            return;
        }
    };
    let Some(old) = sign_edit_check(state, &actor, pos, block) else {
        resync(state, session, &[pos]);
        return;
    };
    // PMMP `InGamePacketHandler::handleBlockActorData`: one side changes at a time, the front
    // first; a side the client sent unchanged is no edit.
    let (front, mut lines) = if edit.front != old.front {
        (true, edit.front)
    } else if edit.back != old.back {
        (false, edit.back)
    } else {
        return;
    };
    let mut event = None;
    if dispatch::heard::<crate::event::SignChange>(state) {
        let world = crate::world::Id::of(actor.world);
        let old_text = if front {
            old.front.clone()
        } else {
            old.back.clone()
        };
        let mut e = crate::event::SignChange {
            player: crate::player::Id::new(session),
            world,
            pos,
            block,
            old_text,
            new_text: lines,
            side: if front {
                crate::tile::Side::Front
            } else {
                crate::tile::Side::Back
            },
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        // A listener may have moved the player, written or broken the sign.
        let changed = dispatch::listened::<crate::event::SignChange>(state)
            && !self::actor(state, session).is_some_and(|again| {
                again.world == actor.world
                    && sign_edit_check(state, &again, pos, block).as_ref() == Some(&old)
            });
        let checked = (!e.cancelled && !changed)
            .then(|| sign::check(&e.new_text))
            .flatten();
        let Some(checked) = checked else {
            resync(state, session, &[pos]);
            return;
        };
        lines = checked;
        event = Some(e);
    }
    decor::write_sign(state, actor.world, pos, front, lines);
    if let Some(p) = state.players.get_mut(&session) {
        p.breaking.sign_editor = None;
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Whether `actor` may write the sign `block` at `pos` now (PMMP `BaseSign::updateFaceText`: it
/// is the sign's editor and the sign is not waxed); the sign's text if so.
fn sign_edit_check(
    state: &State,
    actor: &Actor,
    pos: BlockPos,
    block: block::State,
) -> Option<decor::SignText> {
    let editor = state
        .players
        .get(&actor.session)
        .is_some_and(|p| p.breaking.sign_editor == Some((actor.world, pos)));
    let here = state.worlds.get(actor.world).and_then(|w| w.block(pos)) == Some(block);
    if !editor || !here {
        return None;
    }
    decor::sign_text(state, actor.world, pos).filter(|s| !s.waxed)
}

/// Destroys the block at `pos` for `cause`: sets air (or the liquid it held), removes its block
/// entity, spawns drops and experience, and plays the break effect. Both halves of a bed, door or
/// double plant go together, so a block group breaks and drops once. An explosion plays no
/// per-block break effect (PMMP `Explosion::explodeB` shows one explosion particle; a large
/// explosion would otherwise fill the viewers' outboxes). False if the position is not loaded or
/// already air.
#[expect(
    clippy::needless_pass_by_value,
    reason = "consistency: every caller hands over the cause it built, so it is taken by value"
)]
pub(super) fn destroy(state: &mut State, world: WorldIndex, pos: BlockPos, cause: Destroy) -> bool {
    let particles = !matches!(cause, Destroy::Explosion { .. });
    destroy_with(state, world, pos, &cause, particles)
}

/// One tick of breaking in progress (cracking progress, creative and survival timing): each
/// breaking player out of range stops, a change of speed is shown, and every fifth tick punch
/// particles (PMMP `SurvivalBlockBreakHandler::update`).
pub(super) fn tick(state: &mut State) {
    let mut next = state.players.keys().next().copied();
    while let Some(session) = next {
        next = state
            .players
            .range((
                std::ops::Bound::Excluded(session),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .map(|(s, _)| *s);
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        let Some(target) = player.breaking.target.clone() else {
            continue;
        };
        let feet = player.position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
        if target.world != player.world
            || feet.distance_squared(center(target.pos)) > MAX_BREAK_DISTANCE * MAX_BREAK_DISTANCE
        {
            stop_breaking(state, session);
            continue;
        }
        let speed = speed(state, player, target.block);
        let changed = (speed - target.speed).abs() > 0.0001;
        // PMMP `BlockPunchParticle`.
        let punch = target.fx % FX_INTERVAL == 0 && target.progress < 1.0;
        if let Some(p) = state.players.get_mut(&session)
            && let Some(t) = &mut p.breaking.target
        {
            t.speed = speed;
            t.fx = t.fx.wrapping_add(1);
        }
        let at = block_vec(target.pos);
        if changed {
            let data = crack_data(speed);
            level_event(state, target.world, at, Event::UpdateBlockCracking, data);
        }
        if punch {
            match target.block {
                Hit::Block(b) => {
                    particle(
                        state,
                        target.world,
                        at,
                        Particle::BlockPunch(b, target.face),
                    );
                }
                // No particle event: `Particle` names Anchor states only.
                Hit::Inert(s) => {
                    let data = crate::world::punch_data(s.network_id(), target.face);
                    level_event(state, target.world, at, Event::ParticlesCrackBlock, data);
                }
            }
        }
    }
}

/// The data of a cracking level event: the share broken per tick in 1/65535ths (PMMP), capped
/// for blocks that break at once (BDS sends 65535).
fn crack_data(speed: f64) -> i32 {
    props::float_to_i32((65_535.0 * math::min(speed, 1.0)).floor())
}

/// A block position as a vector: the block's corner (cracking events are there, a painting
/// hanging in the block stands there).
pub(super) fn block_vec(pos: BlockPos) -> Vec3 {
    Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z))
}

/// Shows a level event to the players who see the chunk of `at`: what BDS sends that no PMMP
/// sound or particle makes (block cracking, bone meal's sparkles, a sign's wax).
pub(super) fn level_event(state: &mut State, world: WorldIndex, at: Vec3, event: Event, data: i32) {
    let packet = Packet::LevelEvent(LevelEvent {
        event_type: event.into(),
        position: at.to_f32(),
        event_data: data,
    });
    state.broadcast(world, chunk_of(at), &packet, None);
}

/// Plays a sound for the game at `at` (PMMP `World::addSound`): see [`super::emit`].
pub(super) fn sound(state: &mut State, world: WorldIndex, at: Vec3, sound: Sound) {
    super::emit::sound(state, world, at, sound);
}

/// Shows a particle for the game at `at` (PMMP `World::addParticle`): see [`super::emit`].
pub(super) fn particle(state: &mut State, world: WorldIndex, at: Vec3, particle: Particle) {
    super::emit::particle(state, world, at, particle);
}

/// Sends `session` again the blocks at `positions` as it is shown them (a refused change): the
/// fake block where it has one (its open menu's or the user's), otherwise the real block (a state
/// Anchor does not know as the chunk sends it).
fn resync(state: &mut State, session: SessionId, positions: &[BlockPos]) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let Some(world) = state.worlds.get(player.world) else {
        return;
    };
    let mut packets = Vec::with_capacity(positions.len());
    for (i, &pos) in positions.iter().enumerate() {
        if positions
            .get(..i)
            .is_some_and(|before| before.contains(&pos))
        {
            continue;
        }
        if !player.view.sees(pos.chunk()) {
            continue;
        }
        let shown = player.view.shown(pos).map(block::State::network_id);
        if let Some(id) = shown.or_else(|| world.entry(pos).map(Entry::network_id)) {
            packets.push(Packet::UpdateBlock(UpdateBlock {
                position: [pos.x, pos.y, pos.z],
                new_block_runtime_id: id,
                flags: UPDATE_FLAGS,
                layer: 0,
            }));
        }
    }
    if let Some(player) = state.players.get_mut(&session) {
        for packet in packets {
            player.send_packet(packet);
        }
    }
}

/// PMMP `syncBlocksNearby`: the blocks around a refused click or break, and around the block
/// next to the clicked face (where a placement went).
fn sync_nearby(state: &mut State, session: SessionId, pos: BlockPos, face: Option<Face>) {
    let Some(actor) = actor(state, session) else {
        return;
    };
    if center(pos).distance_squared(actor.feet) >= MAX_ACTION_DISTANCE_SQUARED {
        return;
    }
    let mut positions: Vec<BlockPos> = Face::ALL.into_iter().filter_map(|f| pos.step(f)).collect();
    match face.and_then(|f| pos.step(f)) {
        Some(side) => positions.extend(Face::ALL.into_iter().filter_map(|f| side.step(f))),
        None => positions.push(pos),
    }
    resync(state, session, &positions);
}

/// The conditions of the player's breaking now (PMMP
/// `SurvivalBlockBreakHandler::calculateBreakProgressPerTick`): on the ground, flying and under
/// water as the survival rules track them, Aqua Affinity on the helmet, Haste and Mining Fatigue.
fn conditions(state: &State, player: &Player) -> breaking::Conditions {
    let helmet = player.inventory.armor().get(item::ArmorSlot::Head.index());
    let level = |l: u32| u8::try_from(l).unwrap_or(u8::MAX);
    breaking::Conditions {
        on_ground: survival::is_on_ground(player),
        flying: survival::is_flying(player),
        underwater: survival::is_underwater(state, player),
        aqua_affinity: helmet.is_some_and(|h| drops::level(h, Enchantment::AquaAffinity) > 0),
        haste: level(survival::haste_level(player)),
        fatigue: level(survival::mining_fatigue_level(player)),
    }
}

/// The share of `block` the player breaks per tick now.
fn speed(state: &State, player: &Player, block: Hit) -> f64 {
    let (tool, conditions) = (player.inventory.held(), conditions(state, player));
    match block {
        Hit::Block(b) => breaking::progress_per_tick(b, tool, conditions),
        Hit::Inert(s) => breaking::inert_progress_per_tick(s, tool, conditions),
    }
}

/// One client tick of breaking in progress: the target's progress grows by its speed, unless
/// the client is more than [`LAG_TICKS`] ahead of the server since breaking started. Progress
/// exists only for the break time check: without it (`break-time-check = false`) nothing is
/// counted.
fn advance(state: &mut State, session: SessionId) {
    if !state.settings.break_time_check {
        return;
    }
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let Some(target) = &player.breaking.target else {
        return;
    };
    let elapsed = state.tick.saturating_sub(target.start);
    if u64::from(target.inputs) >= elapsed.saturating_add(LAG_TICKS) {
        return;
    }
    let speed = speed(state, player, target.block);
    if let Some(p) = state.players.get_mut(&session)
        && let Some(t) = &mut p.breaking.target
    {
        t.inputs = t.inputs.saturating_add(1);
        t.progress += speed;
    }
}

/// The block actions of one `PlayerAuthInput`. An action the server does not accept (or an invalid
/// face) drops every action of the packet (one bad field drops the packet, not the session).
fn block_actions(state: &mut State, session: SessionId, actions: &[PlayerBlockAction]) {
    if actions.len() > MAX_BLOCK_ACTIONS {
        debug!(session = session.get(), "too many block actions");
        return;
    }
    let mut parsed = Vec::with_capacity(actions.len());
    for a in actions {
        let Ok(action) = PlayerAction::try_from(a.action) else {
            debug!(
                session = session.get(),
                action = a.action,
                "unknown block action"
            );
            return;
        };
        let needs_face = matches!(
            action,
            PlayerAction::StartBreak
                | PlayerAction::ContinueDestroyBlock
                | PlayerAction::CrackBreak
                | PlayerAction::PredictDestroyBlock
        );
        let accepted = needs_face
            || matches!(
                action,
                PlayerAction::AbortBreak
                    | PlayerAction::StopBreak
                    | PlayerAction::CreativePlayerDestroyBlock
                    | PlayerAction::InteractWithBlock
            );
        let face = face_of(a.face);
        if !accepted || (needs_face && face.is_none()) {
            debug!(
                session = session.get(),
                ?action,
                "block action not accepted"
            );
            return;
        }
        parsed.push((action, block_pos(a.block_pos), face.unwrap_or(Face::Down)));
    }
    for (action, pos, face) in parsed {
        apply_block_action(state, session, action, pos, face);
    }
}

/// One accepted block action (PMMP `handlePlayerActionFromData`).
fn apply_block_action(
    state: &mut State,
    session: SessionId,
    action: PlayerAction,
    pos: BlockPos,
    face: Face,
) {
    let last = state
        .players
        .get(&session)
        .and_then(|p| p.breaking.last_attacked);
    let set_last = |state: &mut State, last: Option<BlockPos>| {
        if let Some(p) = state.players.get_mut(&session) {
            p.breaking.last_attacked = last;
        }
    };
    match action {
        PlayerAction::StartBreak | PlayerAction::ContinueDestroyBlock => {
            // The client sends a continue for the block it already breaks: not a new start.
            if last == Some(pos) {
                return;
            }
            if !attack_block(state, session, pos, face) {
                sync_nearby(state, session, pos, Some(face));
            }
            set_last(state, Some(pos));
        }
        PlayerAction::CrackBreak => {
            if let Some(p) = state.players.get_mut(&session)
                && let Some(t) = &mut p.breaking.target
                && t.pos == pos
            {
                t.face = face;
            }
            set_last(state, Some(pos));
        }
        PlayerAction::PredictDestroyBlock => {
            if !break_block(state, session, pos, Timing::Counted) {
                sync_nearby(state, session, pos, Some(face));
            }
            set_last(state, None);
        }
        PlayerAction::AbortBreak | PlayerAction::StopBreak => {
            if state
                .players
                .get(&session)
                .and_then(|p| p.breaking.target.as_ref())
                .is_some_and(|t| t.pos == pos)
            {
                stop_breaking(state, session);
            }
            set_last(state, None);
        }
        // Creative breaks come as predicted destroys too; interaction is ignored as PMMP does.
        _ => {}
    }
}

/// Ends the breaking of `session` and tells the viewers (PMMP: the break handler's destructor;
/// `Player::teleport` drops the handler too).
pub(super) fn stop_breaking(state: &mut State, session: SessionId) {
    let Some(target) = state
        .players
        .get_mut(&session)
        .and_then(|p| p.breaking.target.take())
    else {
        return;
    };
    let loaded = state
        .worlds
        .get(target.world)
        .is_some_and(|w| w.is_chunk_loaded(target.pos.chunk()));
    if loaded {
        level_event(
            state,
            target.world,
            block_vec(target.pos),
            Event::StopBlockCracking,
            0,
        );
    }
}

/// PMMP `Player::attackBlock`: a left click on a block ([`PlayerInteract`]) starts breaking it
/// (or acts on it: fire on the clicked face goes out, a dragon egg jumps away). A click on an
/// inert block raises no event: the event names Anchor states only.
fn attack_block(state: &mut State, session: SessionId, pos: BlockPos, face: Face) -> bool {
    let Some(mut actor) = actor(state, session) else {
        return false;
    };
    let Some(target) = left_click(state, &actor, pos) else {
        return false;
    };
    let mut event = None;
    if let Hit::Block(block) = target
        && dispatch::heard::<PlayerInteract>(state)
    {
        let world = crate::world::Id::of(actor.world);
        let mut e = PlayerInteract {
            player: crate::player::Id::new(session),
            world,
            pos,
            block,
            face,
            item: held(state, session),
            touch_vector: Vec3::ZERO,
            action: InteractAction::LeftClickBlock,
            first: true,
            use_item: true,
            use_block: true,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        if dispatch::listened::<PlayerInteract>(state) {
            // A listener may have moved the player or changed the block.
            let again = self::actor(state, session)
                .filter(|a| a.world == actor.world && left_click(state, a, pos) == Some(target));
            let Some(again) = again else {
                return false;
            };
            actor = again;
        }
        event = Some(e);
    }
    let done = attack(state, &actor, pos, face, target);
    if let Some(e) = event.filter(|_| done) {
        dispatch::watch(state, &e);
    }
    done
}

/// PMMP `Player::attackBlock`'s checks: the block is near (100 blocks) and loaded, and the
/// player is not a spectator. The block if so.
fn left_click(state: &State, actor: &Actor, pos: BlockPos) -> Option<Hit> {
    if center(pos).distance_squared(actor.feet) > MAX_ACTION_DISTANCE_SQUARED
        || actor.mode == GameMode::Spectator
    {
        return None;
    }
    Hit::at(state, actor.world, pos)
}

/// The held item of `session` (empty if it is not a player).
fn held(state: &State, session: SessionId) -> Stack {
    state
        .players
        .get(&session)
        .map(|p| p.inventory.held().clone())
        .unwrap_or_default()
}

/// The left click on `target` at `pos` after [`PlayerInteract`] (PMMP `Player::attackBlock`
/// from `Block::onAttack` on).
fn attack(state: &mut State, actor: &Actor, pos: BlockPos, face: Face, target: Hit) -> bool {
    let session = actor.session;
    // PMMP `Block::onAttack`.
    if let Hit::Block(block) = target {
        if block.is(Class::ItemFrame) || block.is(Class::Lectern) {
            let mut rng = stream(state, actor.world, USE, pos);
            if decor::attack(state, actor, pos, &mut rng) {
                return true;
            }
        }
        if block.is(Class::DragonEgg) && actor.mode != GameMode::Creative {
            using::teleport_dragon_egg(state, actor.world, pos, block);
            return true;
        }
    }
    if let Some(side) = pos.step(face) {
        let fire = state
            .worlds
            .get(actor.world)
            .and_then(|w| w.block(side))
            .is_some_and(|s| s.has_type_tag(TypeTag::Fire));
        if fire {
            if let Some(w) = state.worlds.get_mut(actor.world)
                && w.set_block(side, block::AIR).is_err()
            {
                return false;
            }
            sound(state, actor.world, center(side), Sound::FireExtinguish);
            return true;
        }
    }
    if actor.mode == GameMode::Creative {
        return true;
    }
    stop_breaking(state, session);
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let speed = speed(state, player, target);
    let start = state.tick;
    if let Some(p) = state.players.get_mut(&session) {
        p.breaking.target = Some(Target {
            world: actor.world,
            pos,
            face,
            block: target,
            start,
            inputs: 0,
            progress: 0.0,
            speed,
            fx: 0,
        });
    }
    if speed > 0.0 {
        level_event(
            state,
            actor.world,
            block_vec(pos),
            Event::StartBlockCracking,
            crack_data(speed),
        );
    }
    true
}

/// Whether the player's breaking of `pos` is done: the block breaks at once, or the counted
/// progress (with one tick of tolerance, as PMMP's old break-time check) on a block of the same
/// kind reaches the whole (a state change of the block, as leaves' decay bit, keeps it).
fn break_done(state: &State, session: SessionId, actor: &Actor, pos: BlockPos) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let Some(block) = Hit::at(state, actor.world, pos) else {
        return false;
    };
    if block.break_info().breaks_instantly() || speed(state, player, block) >= 1.0 {
        return true;
    }
    player.breaking.target.as_ref().is_some_and(|t| {
        t.world == actor.world
            && t.pos == pos
            && t.block.same_block(block)
            && t.progress + t.speed >= 1.0 - 1e-9
    })
}

/// Whether a break waits for the break time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timing {
    /// The client's break: the server's own progress must have reached it.
    Counted,
    /// A break the API asks for ([`action::BreakBlock`], PMMP `Player::breakBlock`): now.
    Instant,
}

/// PMMP `Player::breakBlock`: the client says it broke the block at `pos`. False (the caller
/// resyncs) when the player cannot reach it, has not broken it long enough (unless
/// `break-time-check = false`), or the world refuses (PMMP `World::useBreakOn`, with
/// [`BlockBreak`]).
fn break_block(state: &mut State, session: SessionId, pos: BlockPos, timing: Timing) -> bool {
    inventory::close_window(state, session);
    let Some(mut actor) = actor(state, session) else {
        return false;
    };
    if !actor.can_interact(center(pos), actor.reach()) {
        debug!(session = session.get(), "break out of reach");
        return false;
    }
    let counted = timing == Timing::Counted && actor.finite() && state.settings.break_time_check;
    if let Some(Hit::Inert(block)) = Hit::at(state, actor.world, pos) {
        return inert::break_block(state, actor, pos, block, counted);
    }
    if !dispatch::heard::<BlockBreak>(state) {
        if counted && !break_done(state, session, &actor, pos) {
            debug!(session = session.get(), "break before its time");
            stop_breaking(state, session);
            return false;
        }
        let tool = held(state, session);
        let plan = break_plan(state, &actor, pos, &tool);
        return finish_break(state, &actor, pos, &tool, plan);
    }
    // With listeners the break time is checked after them: `insta_break` may skip it.
    let tool = held(state, session);
    let Some(mut plan) = break_plan(state, &actor, pos, &tool) else {
        stop_breaking(state, session);
        return false;
    };
    let world = crate::world::Id::of(actor.world);
    let mut e = BlockBreak {
        player: crate::player::Id::new(session),
        world,
        pos,
        block: plan.block,
        item: tool.clone(),
        insta_break: !counted,
        drops: plan.destruction.items.clone(),
        xp_drop_amount: plan.destruction.xp,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled {
        stop_breaking(state, session);
        return false;
    }
    if dispatch::listened::<BlockBreak>(state) {
        // A listener may have moved the player, changed the block or its contents (the drops
        // would copy them), or taken the tool: the break must come out the same.
        let again = self::actor(state, session).filter(|a| {
            a.world == actor.world
                && a.can_interact(center(pos), a.reach())
                && held(state, session) == tool
                && break_plan(state, a, pos, &tool).as_ref() == Some(&plan)
        });
        let Some(again) = again else {
            stop_breaking(state, session);
            return false;
        };
        actor = again;
    }
    if counted && !e.insta_break && !break_done(state, session, &actor, pos) {
        debug!(session = session.get(), "break before its time");
        stop_breaking(state, session);
        return false;
    }
    plan.destruction.items.clone_from(&e.drops);
    plan.destruction.xp = e.xp_drop_amount;
    if !finish_break(state, &actor, pos, &tool, Some(plan)) {
        return false;
    }
    dispatch::watch(state, &e);
    true
}

/// The end of a break whose time is checked: the breaking stops, then the break `plan` (`None`
/// when the checks refused it) is applied and the player gets hungrier. A refused break stops
/// the cracking the viewers see.
fn finish_break(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    tool: &Stack,
    plan: Option<BreakPlan>,
) -> bool {
    finish_break_with(state, actor, pos, |state| {
        plan.is_some_and(|plan| use_break_on(state, actor, pos, tool, plan))
    })
}

/// [`finish_break`] with `apply` breaking the block (false: refused).
fn finish_break_with(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    apply: impl FnOnce(&mut State) -> bool,
) -> bool {
    let session = actor.session;
    let target = state
        .players
        .get_mut(&session)
        .and_then(|p| p.breaking.target.take());
    if !apply(state) {
        // The break handler stays gone, as in PMMP; the client starts again.
        if target.is_some() {
            level_event(
                state,
                actor.world,
                block_vec(pos),
                Event::StopBlockCracking,
                0,
            );
        }
        return false;
    }
    survival::exhaust_by(state, session, BREAK_EXHAUSTION, ExhaustCause::Mining);
    true
}

/// A player's break worked out before anything changes: the block and its destruction.
#[derive(Debug, Clone, PartialEq)]
struct BreakPlan {
    block: block::State,
    destruction: Destruction,
}

/// PMMP `World::useBreakOn`'s checks for a player (PMMP cancels the `BlockBreakEvent` for
/// these: air, a spectator, an unbreakable block with finite resources, adventure mode without
/// `CanDestroy`) and the drops and experience the break gives.
fn break_plan(state: &State, actor: &Actor, pos: BlockPos, tool: &Stack) -> Option<BreakPlan> {
    let target = state.worlds.get(actor.world)?.block(pos)?;
    if target == block::AIR
        || actor.mode == GameMode::Spectator
        || (actor.finite() && !target.break_info().is_breakable())
        || (actor.mode == GameMode::Adventure
            && !adventure_allows(tool, "CanDestroy", target.name()))
    {
        return None;
    }
    let cause = Destroy::Player {
        session: actor.session,
        tool: tool.clone(),
    };
    let destruction = plan_destruction(state, actor.world, pos, &cause)?;
    Some(BreakPlan {
        block: target,
        destruction,
    })
}

/// PMMP `World::useBreakOn` for a player after its checks: the destruction with drops, and the
/// tool's wear (`Item::onDestroyBlock`).
fn use_break_on(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    tool: &Stack,
    plan: BreakPlan,
) -> bool {
    let cause = Destroy::Player {
        session: actor.session,
        tool: tool.clone(),
    };
    if !apply_destruction(state, actor.world, pos, &cause, plan.destruction, true) {
        return false;
    }
    let mut rng = stream(state, actor.world, USE, pos);
    let instant = plan.block.break_info().breaks_instantly();
    let worn = tool_after_break(&state.settings, tool, instant, &mut rng);
    return_items(state, actor, tool, worn, Vec::new());
    true
}

/// The tool after breaking a block that breaks at once or not (PMMP `Item::onDestroyBlock`):
/// pickaxes, axes, shovels and hoes lose 1 durability, swords and tridents 2, on blocks that do
/// not break at once; shears 1 always.
fn tool_after_break(
    settings: &super::Settings,
    tool: &Stack,
    instant: bool,
    rng: &mut Random,
) -> Stack {
    let Some(item) = tool.item() else {
        return tool.clone();
    };
    let digging = item.is(item::Class::Pickaxe)
        || item.is(item::Class::Axe)
        || item.is(item::Class::Shovel)
        || item.is(item::Class::Hoe);
    let amount = if item.is(item::Class::Shears) {
        1
    } else if instant {
        0
    } else if item.is(item::Class::Sword) || item.is(item::Class::Trident) {
        2
    } else {
        u32::from(digging)
    };
    if amount == 0 {
        return tool.clone();
    }
    breaking::damage(settings, tool, amount, rng)
}

/// Whether the stack's adventure-mode list `key` (`CanDestroy`, `CanPlaceOn`) names the block
/// (PMMP parses each entry as a block and compares types; Anchor compares Bedrock names, with
/// or without the `minecraft:` prefix).
fn adventure_allows(stack: &Stack, key: &str, name: &str) -> bool {
    let Some(crate::nbt::Value::List(crate::nbt::List::String(names))) =
        stack.extra().and_then(|e| e.nbt.get(key))
    else {
        return false;
    };
    let short = name.strip_prefix("minecraft:").unwrap_or(name);
    names.iter().any(|n| n == name || n == short)
}

/// PMMP `Player::returnItemsFromAction`: the held stack becomes `new` if the action changed it
/// (in creative only a change of item, not of count or damage) and the hand still holds `old`
/// (a listener during the action may have changed it); `returned` items go to the inventory,
/// the first into the hand if it emptied, and what does not fit is thrown (at the world's entity
/// limit it waits as a spill at the feet, as a broken container's items do).
fn return_items(state: &mut State, actor: &Actor, old: &Stack, new: Stack, returned: Vec<Stack>) {
    let session = actor.session;
    let mut changed = false;
    let emptied = new.is_empty();
    let holds_old = state
        .players
        .get(&session)
        .is_some_and(|p| p.inventory.held() == old);
    if new != *old && holds_old {
        let deducted = new.is_empty() || (new.item() == old.item() && new.extra() == old.extra());
        if !deducted || actor.finite() {
            // A tool worn out (PMMP `ItemBreakSound`).
            if new.is_empty() && old.item().and_then(item::Type::max_durability).is_some() {
                sound(state, actor.world, actor.eye, Sound::ItemBreak);
            }
            if let Some(player) = state.players.get_mut(&session) {
                inventory::set_held(player, new);
            }
            changed = true;
        }
    }
    let mut returned = returned.into_iter();
    if changed
        && emptied
        && let Some(first) = returned.next()
        && let Some(player) = state.players.get_mut(&session)
    {
        inventory::set_held(player, first);
    }
    for stack in returned {
        let left = match state.players.get_mut(&session) {
            Some(player) => inventory::give(player, stack),
            None => stack,
        };
        if !left.is_empty() && !inventory::throw_stack(state, session, left.clone()) {
            let at = BlockPos::containing(actor.feet);
            state
                .rules
                .block_entities
                .spill(actor.world, at, vec![left]);
        }
    }
}

/// The other half of the bed `bed` at `pos` (PMMP `Bed::getOtherHalf`): the head lies one block
/// along the bed's facing from the foot; `None` unless a bed half of the other kind facing the
/// same way is there.
pub(super) fn bed_other_half(
    world: &crate::world::World,
    pos: BlockPos,
    bed: block::State,
) -> Option<(BlockPos, block::State)> {
    let head = props::flag(bed, "head_piece_bit") == Some(true);
    let facing = props::facing(bed)?;
    let other = pos.step(if head { facing.opposite() } else { facing })?;
    let state = world.block(other)?;
    (state.is(Class::Bed)
        && props::flag(state, "head_piece_bit") != Some(head)
        && props::facing(state) == Some(facing))
    .then_some((other, state))
}

/// The blocks a break takes together (PMMP `Block::getAffectedBlocks`): both halves of a bed,
/// door, double plant or small dripleaf.
fn affected_blocks(
    world: &crate::world::World,
    pos: BlockPos,
    block: block::State,
) -> [Option<(BlockPos, block::State)>; 2] {
    let other = |pos: Option<BlockPos>| pos.and_then(|p| world.block(p).map(|s| (p, s)));
    let upper = props::flag(block, "upper_block_bit") == Some(true);
    let vertical = pos.step(if upper { Face::Down } else { Face::Up });
    let second = if block.is(Class::Bed) {
        bed_other_half(world, pos, block)
    } else if block.is(Class::Door) || block.is(Class::SmallDripleaf) {
        other(vertical).filter(|(_, s)| s.kind() == block.kind())
    } else if block.is(Class::DoublePlant) {
        other(vertical).filter(|(_, s)| {
            s.kind() == block.kind() && props::flag(*s, "upper_block_bit") == Some(!upper)
        })
    } else {
        None
    };
    [Some((pos, block)), second]
}

/// Destroys the block at `pos` for `cause` (PMMP `World::useBreakOn` after its checks,
/// `destroyBlockInternal`, `Block::onBreak`, `Explosion::explodeB` per block): drops worked out
/// first, then every affected block removed with its block entity, then drops and experience.
/// A player's break whose drops find no room for entities changes nothing (the world's entity
/// limit).
fn destroy_with(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    cause: &Destroy,
    particles: bool,
) -> bool {
    match plan_destruction(state, world, pos, cause) {
        Some(plan) => apply_destruction(state, world, pos, cause, plan, particles),
        None => false,
    }
}

/// What destroying a block does, worked out before anything changes: the blocks that go
/// together, then the drops (the block entity's contents included) and the experience.
#[derive(Debug, Clone, PartialEq)]
struct Destruction {
    affected: [Option<(BlockPos, block::State)>; 2],
    items: Vec<Stack>,
    xp: u32,
}

/// The destruction of the block at `pos` for `cause`; `None` if the position is not loaded or
/// already air.
fn plan_destruction(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    cause: &Destroy,
) -> Option<Destruction> {
    let w = state.worlds.get(world)?;
    let target = w.block(pos)?;
    if target == block::AIR {
        return None;
    }
    let affected = affected_blocks(w, pos, target);
    let mut rng = stream(state, world, DROPS, pos);
    let (mut out, xp) = drops_of(state, world, cause, &affected, &mut rng);
    if out.from_block_entity > 0 {
        // The item only the block entity names (a bed's colour, a banner's patterns).
        let own = affected
            .iter()
            .flatten()
            .find_map(|(p, _)| super::block_entity::own_item(state, world, *p));
        if let Some(own) = own {
            for _ in 0..out.from_block_entity {
                out.items.push(own.clone());
            }
        }
        out.from_block_entity = 0;
    }
    // A shulker box's items leave in its dropped item (PMMP `ShulkerBox::getDropsForCompatibleTool`).
    for (p, s) in affected.iter().flatten() {
        if !s.is(Class::ShulkerBox) {
            continue;
        }
        if let block::ItemForm::Item(own) = s.item() {
            for item in &mut out.items {
                if item.item() == Some(own) {
                    *item = super::block_entity::with_block_data(state, world, *p, item.clone());
                }
            }
        }
    }
    Some(Destruction {
        affected,
        items: out.items,
        xp,
    })
}

/// Applies a destruction (PMMP `World::useBreakOn` after its checks, `destroyBlockInternal`,
/// `Block::onBreak`): the windows on the affected blocks close, every affected block is removed
/// with its block entity, then the drops and experience. A close listener may change what the drops
/// copy: a player's break then changes nothing, another cause drops what is there now. A player's
/// break whose drops find no room for entities changes nothing (the world's entity limit); drops of
/// other causes that find none wait as a spill.
fn apply_destruction(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    cause: &Destroy,
    mut plan: Destruction,
    particles: bool,
) -> bool {
    let mut viewers = Vec::new();
    for (p, _) in plan.affected.iter().flatten() {
        for s in super::block_entity::window_viewers(state, world, *p) {
            if !viewers.contains(&s) {
                viewers.push(s);
            }
        }
    }
    if !viewers.is_empty() {
        // A player's plan may carry a listener's drops: only a change of the world refuses it.
        let before = matches!(cause, Destroy::Player { .. })
            .then(|| plan_destruction(state, world, pos, cause));
        for session in viewers {
            inventory::close_window(state, session);
        }
        let after = plan_destruction(state, world, pos, cause);
        match (before, after) {
            (Some(before), after) if before != after => {
                debug!("break refused: a window close changed the block");
                return false;
            }
            (Some(_), _) => {}
            (None, Some(after)) => plan = after,
            (None, None) => return false,
        }
    }
    let Destruction {
        affected,
        items,
        xp,
    } = plan;
    if matches!(cause, Destroy::Player { .. }) && !entity::has_room(state, world, items.len()) {
        debug!("break refused: no room for the drops");
        return false;
    }
    for (p, s) in affected.into_iter().flatten() {
        if particles {
            particle(state, world, center(p), Particle::BlockBreak(s));
        }
        super::block_entity::removed(state, world, p);
        on_break(state, world, p, s, cause);
        if s.custom_block().is_some() {
            let session = match cause {
                Destroy::Player { session, .. } => Some(*session),
                _ => None,
            };
            custom::broken(state, session, world, p, s);
        }
    }
    spawn_drops(state, world, pos, items, xp);
    true
}

/// Drops `items` and `xp` at the centre of `pos`; items that find no room for entities wait as a
/// spill.
fn spawn_drops(state: &mut State, world: WorldIndex, pos: BlockPos, items: Vec<Stack>, xp: u32) {
    let at = center(pos);
    let mut left = Vec::new();
    for stack in items {
        if !entity::drop_item(state, world, at, stack.clone()) {
            left.push(stack);
        }
    }
    if !left.is_empty() {
        state.rules.block_entities.spill(world, pos, left);
    }
    if xp > 0 {
        entity::drop_experience(state, world, at, xp);
    }
}

/// The drops and experience of a destruction (PMMP: only players with finite resources get
/// drops and experience, the experience only with `experience`; a block lost by the rules drops
/// as by hand; an explosion drops with chance `1 / size`).
fn drops_of(
    state: &State,
    world: WorldIndex,
    cause: &Destroy,
    affected: &[Option<(BlockPos, block::State)>; 2],
    rng: &mut Random,
) -> (drops::Drops, u32) {
    let mut out = drops::Drops::default();
    let mut xp = 0u32;
    // A decoration's contents come with its drops (a potted plant, a framed item, a record, a
    // lectern's book).
    let mut add = |d: drops::Drops, pos: BlockPos, rng: &mut Random| {
        out.items.extend(d.items);
        out.from_block_entity = out.from_block_entity.saturating_add(d.from_block_entity);
        out.items
            .extend(super::block_entity::drops(state, world, pos, rng));
    };
    match cause {
        Destroy::Player { session, tool } => {
            let finite = state
                .players
                .get(session)
                .is_some_and(|p| survival::game_mode(state, p) != GameMode::Creative);
            if finite {
                let experience = state.settings.has_orbs();
                for (p, s) in affected.iter().flatten() {
                    if s.custom_block().is_some() {
                        let (items, x) = custom::drops_of(state, *s, tool, rng);
                        add(custom_drops(items), *p, rng);
                        if experience {
                            xp = xp.saturating_add(x);
                        }
                        continue;
                    }
                    add(drops::drops(*s, tool, rng), *p, rng);
                    if experience {
                        xp = xp.saturating_add(drops::experience(*s, tool, rng));
                    }
                }
            }
        }
        Destroy::Rules => {
            for (p, s) in affected.iter().flatten() {
                if s.custom_block().is_some() {
                    let (items, _) = custom::drops_of(state, *s, &Stack::EMPTY, rng);
                    add(custom_drops(items), *p, rng);
                    continue;
                }
                add(drops::drops(*s, &Stack::EMPTY, rng), *p, rng);
            }
        }
        Destroy::Explosion { size } => {
            // One roll for the whole group: a door or bed drops once.
            let chance = if *size > 0.0 {
                math::min(100.0, 100.0 / size)
            } else {
                100.0
            };
            if f64::from(rng.range(0, 100)) < chance {
                for (p, s) in affected.iter().flatten() {
                    if s.custom_block().is_some() {
                        // PMMP `Explosion::explodeB`: as by hand with a compatible tool where
                        // the block is explosion harvestable.
                        let (items, _) = custom::drops_of(state, *s, &Stack::EMPTY, rng);
                        add(custom_drops(items), *p, rng);
                    } else if s.kind() != Kind::Tnt {
                        add(drops::explosion(*s, rng), *p, rng);
                    }
                }
            }
        }
    }
    (out, xp)
}

/// The drops of a custom block as the vanilla drop rules give them.
fn custom_drops(items: Vec<Stack>) -> drops::Drops {
    drops::Drops {
        items,
        from_block_entity: 0,
    }
}

/// PMMP `Block::onBreak` per class: ice melts into water when a survival player (or the world)
/// breaks it without Silk Touch, unstable TNT ignites, anything else becomes air.
fn on_break(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    cause: &Destroy,
) {
    let result = match cause {
        Destroy::Explosion { .. } if block.kind() == Kind::Tnt => {
            // PMMP `Explosion::explodeB`: TNT ignites instead of breaking (until primed TNT
            // exists, it stays).
            let _ignited = using::ignite_tnt(state, world, pos);
            return;
        }
        Destroy::Explosion { .. } => block::AIR,
        Destroy::Player { session, tool } => {
            let survival = state.players.get(session).is_some_and(|p| {
                matches!(
                    survival::game_mode(state, p),
                    GameMode::Survival | GameMode::Adventure
                )
            });
            on_break_result(block, survival, tool)
        }
        Destroy::Rules => on_break_result(block, true, &Stack::EMPTY),
    };
    // PMMP `TNT::onBreak`: unstable TNT ignites (it becomes air with the primed entity).
    if result == block::AIR
        && block.kind() == Kind::Tnt
        && props::flag(block, "explode_bit") == Some(true)
        && using::ignite_tnt(state, world, pos)
    {
        return;
    }
    if let Some(w) = state.worlds.get_mut(world)
        && let Err(e) = w.set_block(pos, result)
    {
        debug!(?pos, %e, "broken block not set");
    }
}

/// What a block leaves when broken (PMMP `Ice::onBreak`: water in survival without Silk Touch).
fn on_break_result(block: block::State, survival: bool, tool: &Stack) -> block::State {
    if block.is(Class::Ice)
        && !block.is(Class::FrostedIce)
        && survival
        && drops::level(tool, Enchantment::SilkTouch) == 0
    {
        return Kind::Water.default_state();
    }
    block::AIR
}

/// `UseItem` from `InventoryTransaction` or `PlayerAuthInput` (PMMP `handleUseItemTransaction`).
fn use_item(state: &mut State, session: SessionId, data: &UseItemTransactionData) {
    let Ok(action) = UseItemAction::try_from(data.action_type) else {
        return;
    };
    match action {
        UseItemAction::ClickBlock => click_block(state, session, data),
        UseItemAction::ClickAir => {
            if held_matches(state, session, data) {
                use_in_air(state, session);
            }
        }
        // Breaking is server-authoritative through block actions; attacks come as
        // `UseItemOnEntity`.
        UseItemAction::BreakBlock | UseItemAction::UseAsAttack => {}
    }
}

/// A click in the air (PMMP `handleUseItemTransaction` `ACTION_CLICK_AIR`): drawing stops (a bow
/// or trident starts again), eating or drinking is the survival rules', anything else
/// [`using::use_held_item`].
fn use_in_air(state: &mut State, session: SessionId) {
    let Some(actor) = actor(state, session) else {
        return;
    };
    stop_using(state, session);
    if !survival::use_consumable(state, session) {
        using::use_held_item(state, &actor);
    }
}

/// Whether the client's held item is the server's: the same hotbar slot, network ID and count
/// (BDS ignores block runtime IDs and NBT, P6).
fn held_matches(state: &State, session: SessionId, data: &UseItemTransactionData) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let held = player.inventory.held();
    let stack = &data.held_item.stack;
    let network_id = held.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.hot_bar_slot == i32::from(player.inventory.held_slot())
        && stack.item_type.network_id == network_id
        && stack.count == u16::from(held.count())
}

/// A finite float's bits; non-finite values are 0.
fn bits(v: f32) -> u32 {
    finite(v).to_bits()
}

/// A finite float, or 0.
fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// A click on a block (PMMP `ACTION_CLICK_BLOCK`): the repeat filter, the held press, the held
/// item check, then the interaction; a refused one sends the client the real blocks.
fn click_block(state: &mut State, session: SessionId, data: &UseItemTransactionData) {
    let Some(face) = face_of(data.block_face) else {
        return;
    };
    let pos = block_pos(data.block_position);
    let click = Click {
        pos,
        face: data.block_face,
        at: data.clicked_position.map(bits),
        player: data.position.map(bits),
    };
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase != Phase::Spawned || !survival::is_alive(player) {
        return;
    }
    let repeat = player
        .breaking
        .last_click
        .is_some_and(|(t, c)| c == click && tick.saturating_sub(t) < REPEAT_TICKS);
    if repeat {
        // The first click's time stays, so a repeat changes nothing at all.
        return;
    }
    player.breaking.last_click = Some((tick, click));
    let repeated = TriggerType::try_from(data.trigger_type) == Ok(TriggerType::SimulationTick);
    player.breaking.note_click(tick, repeated);
    let clamp = |v: f32| {
        if v.is_finite() {
            math::clamp(f64::from(v), 0.0, 1.0)
        } else {
            0.0
        }
    };
    let at = Vec3::new(
        clamp(data.clicked_position[0]),
        clamp(data.clicked_position[1]),
        clamp(data.clicked_position[2]),
    );
    let done = held_matches(state, session, data)
        && interact_block(state, session, pos, face, at, Source::Client);
    if !done {
        // PMMP resyncs only when the client predicted a success; any refusal may have been
        // predicted, so Anchor always sends the real blocks.
        sync_nearby(state, session, pos, Some(face));
    }
}

/// PMMP `Player::interactBlock`: drawing stops; within reach, use the held item on the block.
fn interact_block(
    state: &mut State,
    session: SessionId,
    pos: BlockPos,
    face: Face,
    click: Vec3,
    source: Source,
) -> bool {
    let Some(actor) = actor(state, session) else {
        return false;
    };
    stop_using(state, session);
    if !actor.can_interact(center(pos), actor.reach()) {
        debug!(session = session.get(), "click out of reach");
        return false;
    }
    using::use_item_on(state, &actor, pos, face, click, source)
}

/// Stops the drawing of `session` ([`Breaking::stop_using`]).
fn stop_using(state: &mut State, session: SessionId) {
    if let Some(player) = state.players.get_mut(&session) {
        player.breaking.stop_using();
    }
}

/// Opens the sign editor of the sign at `pos` for the player (PMMP `Player::openSignEditor`).
fn open_sign_editor(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    front: bool,
) {
    if let Some(player) = state.players.get_mut(&session) {
        player.breaking.sign_editor = Some((world, pos));
        player.send_packet(Packet::OpenSign(OpenSign {
            position: [pos.x, pos.y, pos.z],
            front_side: front,
        }));
    }
}

/// A point an action gives: each part that is not a finite number counts as 0.
fn finite_vec(v: Vec3) -> Vec3 {
    let part = |c: f64| if c.is_finite() { c } else { 0.0 };
    Vec3::new(part(v.x), part(v.y), part(v.z))
}

impl action::Sealed for action::AttackBlock {
    fn perform(self, cx: &mut Context<'_>) {
        let _attacked = attack_block(cx.state, self.player.session(), self.pos, self.face);
    }
}

impl action::Sealed for action::BreakBlock {
    fn perform(self, cx: &mut Context<'_>) {
        let _broke = break_block(cx.state, self.player.session(), self.pos, Timing::Instant);
    }
}

impl action::Sealed for action::UseItemOn {
    fn perform(self, cx: &mut Context<'_>) {
        let click = finite_vec(self.click);
        let click = Vec3::new(
            math::clamp(click.x, 0.0, 1.0),
            math::clamp(click.y, 0.0, 1.0),
            math::clamp(click.z, 0.0, 1.0),
        );
        let _used = interact_block(
            cx.state,
            self.player.session(),
            self.pos,
            self.face,
            click,
            Source::Action,
        );
    }
}

impl action::Sealed for action::UseItem {
    fn perform(self, cx: &mut Context<'_>) {
        use_in_air(cx.state, self.player.session());
    }
}

impl action::Sealed for action::InteractEntity {
    fn perform(self, cx: &mut Context<'_>) {
        let session = self.player.session();
        if actor(cx.state, session).is_some() {
            let click = finite_vec(self.click);
            let _used = using::interact_entity(cx.state, session, self.entity, click);
        }
    }
}

#[cfg(test)]
mod tests;
