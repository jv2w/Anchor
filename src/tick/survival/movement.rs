//! Movement and input (PMMP `InGamePacketHandler::handlePlayerAuthInput`, `Player::handleMovement`,
//! `Player::processMostRecentMovements`, `Player::checkGroundState`, `Entity::updateFallState`,
//! `Entity::checkBlockIntersections`, the blocks' `onEntityInside`/`onEntityLand`).
//!
//! The client moves itself; the server accepts each position unless it is more than 15 blocks
//! from the last one (with `movement-check`), outside the loaded chunks, or beyond the rate limit
//! (2 moves a tick, 200 in reserve), and then sends the player back (`MovePlayer` reset). PMMP's
//! checks are safety checks, not an anti-cheat.

use super::attributes::{ABILITIES, MOVEMENT};
use super::damage::{self, Damage};
use super::effects::{self, Active};
use super::{Survival, attributes, f32_of, feet, food, sound};

use crate::block::{self, Class, Face};
use crate::effect::Type as Effect;
use crate::event::{
    Actor, CombustSource, DamageSource, EntityFrostWalker, ExhaustCause, ExtinguishCause,
    MAX_FROST_WALKER_RADIUS, PlayerJump, PlayerMissSwing, PlayerMove, PlayerToggleFlight,
    PlayerToggleGlide, PlayerToggleSneak, PlayerToggleSprint, PlayerToggleSwim,
};
use crate::math::{Aabb, Vec3};
use crate::player::Id;
use crate::protocol::consts::{InputFlag, MoveMode, PlayerAction as Action, SoundEvent};
use crate::protocol::packet::{MovePlayer, Packet, PlayerAction, PlayerAuthInput};
use crate::protocol::{FlagSet, TeleportData};
use crate::tick::entity::{Harm, INSIDE_INSET, MOTION_THRESHOLD};
use crate::tick::{
    Phase, Player, SessionId, State, dispatch, entity, inside_border, interact, update,
};
use crate::world::{BlockPos, Entry, World};

/// Moves the reserve gains each tick (PMMP `Player::MOVES_PER_TICK`).
const MOVES_PER_TICK: i32 = 2;
/// The most moves in reserve: 100 ticks (PMMP `Player::MOVE_BACKLOG_SIZE`).
const MOVE_BACKLOG: i32 = 100 * MOVES_PER_TICK;
/// The reserve of a new player (PMMP `Player::$moveRateLimit`).
const MOVE_START: i32 = 10 * MOVES_PER_TICK;
/// The furthest one move may go, squared: 15 blocks (PMMP `actuallyHandleMovement`).
const MAX_MOVE_SQUARED: f64 = 225.0;
/// How far a move after a teleport may be from the teleport target, squared (PMMP
/// `forceMoveSync`).
const SYNC_TOLERANCE_SQUARED: f64 = 1.0;
/// Fall damage above which landing makes the long fall sound (PMMP `Living::onHitGround`).
const LONG_FALL_DAMAGE: f64 = 4.0;

/// A player's movement and input state.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Motion {
    pub(super) sprinting: bool,
    pub(super) sneaking: bool,
    /// The sneak key is held (PMMP `sneakPressed`).
    pub(super) sneak_pressed: bool,
    pub(super) swimming: bool,
    pub(super) gliding: bool,
    pub(super) flying: bool,
    pub(super) on_ground: bool,
    /// Whether hunger runs for the player (settings and game mode), set each tick.
    pub(super) hunger: bool,
    /// Yaw and pitch in degrees, yaw in [0, 360).
    pub(super) yaw: f64,
    pub(super) pitch: f64,
    /// The last raw rotation and input flags received.
    last_rotation: [u32; 2],
    last_flags: FlagSet<InputFlag>,
    /// The last raw position received.
    last_position: Option<[u32; 3]>,
    /// The feet position the movement was last processed at (PMMP `lastLocation`).
    processed: Option<Vec3>,
    /// Moves left before the rate limit (PMMP `moveRateLimit`).
    budget: i32,
    /// After a teleport or revert, moves far from the position are old and ignored.
    pub(super) force_sync: bool,
    /// The blocks around the player acted this tick, so the base tick leaves them (PMMP
    /// `checkBlockIntersectionsNextTick`).
    intersected: bool,
    /// The player lay down in its bed in this session (a saved sleeper is woken on its first
    /// tick, which frees the bed).
    pub(super) in_bed: bool,
    /// The death screen was shown for the current death (a player that joins dead sees it on
    /// its first tick).
    pub(super) death_shown: bool,
    /// Ticks since the player last stood on something (PMMP `inAirTicks`).
    pub(super) in_air: u32,
    /// The client tick of the latest input this tick that toggled a movement state: the player's
    /// own entity data at the end of the tick carries it, as BDS's does (P6 `sneak`), so the
    /// client applies it at that input's frame and keeps what it predicted since
    /// (bedrock-protocol-docs `PlayerMovementOverview`). Sent without it, the answer to a
    /// sneak the client has already ended turns the sneak on again.
    pub(super) answer_tick: Option<u64>,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            sprinting: false,
            sneaking: false,
            sneak_pressed: false,
            swimming: false,
            gliding: false,
            flying: false,
            on_ground: false,
            hunger: false,
            yaw: 0.0,
            pitch: 0.0,
            last_rotation: [0; 2],
            last_flags: FlagSet::new(),
            last_position: None,
            processed: None,
            budget: MOVE_START,
            force_sync: false,
            intersected: false,
            in_bed: false,
            death_shown: false,
            in_air: 0,
            answer_tick: None,
        }
    }
}

crate::tick::checkpoint::fields!(Motion {
    sprinting,
    sneaking,
    sneak_pressed,
    swimming,
    gliding,
    flying,
    on_ground,
    hunger,
    yaw,
    pitch,
    last_rotation,
    last_flags,
    last_position,
    processed,
    budget,
    force_sync,
    intersected,
    in_bed,
    death_shown,
    in_air,
    answer_tick,
});

impl Motion {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            sprinting,
            sneaking,
            sneak_pressed,
            swimming,
            gliding,
            flying,
            on_ground,
            hunger,
            yaw,
            pitch,
            last_rotation,
            last_flags,
            last_position,
            processed,
            budget,
            force_sync,
            intersected,
            in_bed,
            death_shown,
            in_air,
            answer_tick,
        } = self;
        let bits = [
            sprinting,
            sneaking,
            sneak_pressed,
            swimming,
            gliding,
            flying,
            on_ground,
            hunger,
            force_sync,
            intersected,
            in_bed,
            death_shown,
        ]
        .iter()
        .enumerate()
        .fold(0_u64, |b, (i, &&on)| b | u64::from(on) << i);
        h.u64(bits);
        h.u64(yaw.to_bits());
        h.u64(pitch.to_bits());
        for r in last_rotation {
            h.u64(u64::from(*r));
        }
        for w in last_flags.words() {
            h.u64(w);
        }
        match last_position {
            Some(p) => p.iter().for_each(|c| h.u64(u64::from(*c))),
            None => h.u64(u64::MAX),
        }
        match processed {
            Some(p) => [p.x, p.y, p.z].iter().for_each(|c| h.u64(c.to_bits())),
            None => h.u64(u64::MAX),
        }
        h.u64(u64::from(budget.cast_unsigned()));
        h.u64(u64::from(*in_air));
        h.u64(answer_tick.unwrap_or(u64::MAX));
    }
}

/// `PlayerAuthInput`: rotation, input flags, then the movement; only from a spawned, living
/// player (PMMP handles it in the in-game handler only, so a client cannot place itself anywhere
/// before it spawns). The tick keeps the client's eye position exactly; the feet the rules use are
/// rounded to 4 decimals as PMMP's (`survival::feet`).
pub(super) fn on_auth_input(state: &mut State, session: SessionId, packet: &PlayerAuthInput) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let finite = packet
        .position
        .iter()
        .chain(&[packet.yaw, packet.pitch, packet.head_yaw])
        .all(|c| c.is_finite());
    if player.phase != Phase::Spawned || !finite || player.survival.is_dead() {
        return;
    }
    let m = &mut player.survival.motion;
    let rotation = [packet.yaw.to_bits(), packet.pitch.to_bits()];
    if rotation != m.last_rotation {
        m.last_rotation = rotation;
        let yaw = f64::from(packet.yaw) % 360.0;
        m.yaw = if yaw < 0.0 { yaw + 360.0 } else { yaw };
        m.pitch = f64::from(packet.pitch) % 360.0;
    }
    if let Ok(flags) = packet.input_data.to_set()
        && flags != player.survival.motion.last_flags
    {
        player.survival.motion.last_flags = flags;
        input_flags(state, session, flags, packet.tick);
    }
    let raw = Vec3::from_f32(packet.position);
    if !inside_border(raw) {
        return;
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let bits = packet.position.map(f32::to_bits);
    if player.survival.motion.last_position == Some(bits) {
        return;
    }
    if player.survival.motion.force_sync {
        if raw.distance_squared(player.position) > SYNC_TOLERANCE_SQUARED {
            return;
        }
        player.survival.motion.force_sync = false;
    }
    player.survival.motion.last_position = Some(bits);
    handle_movement(state, session, raw);
}

/// Whether a start/stop flag pair turns something on, off, or neither (PMMP
/// `resolveOnOffInputFlags`).
fn on_off(flags: FlagSet<InputFlag>, start: InputFlag, stop: InputFlag) -> Option<bool> {
    let (on, off) = (flags.contains(start), flags.contains(stop));
    (on != off).then_some(on)
}

/// The input flags changed: sneaking, sprinting, swimming, gliding, flying, a jump, a missed
/// swing (PMMP `InGamePacketHandler::handlePlayerAuthInput`). Each toggle that changes something
/// asks its event (`PlayerToggleSneak` and the others); a cancelled one leaves the state as it
/// was and sends the player its entity data again, as a refused flight does. The answer,
/// accepted or refused, carries the input's client tick `input_tick`.
fn input_flags(state: &mut State, session: SessionId, flags: FlagSet<InputFlag>, input_tick: u64) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let may_fly = player.survival.may_fly();
    let m = &mut player.survival.motion;
    let pressed = flags.contains(InputFlag::Sneaking);
    let sneak = on_off(flags, InputFlag::StartSneaking, InputFlag::StopSneaking)
        .filter(|&v| v != m.sneaking);
    let mut mismatch = sneak.is_none() && pressed != m.sneak_pressed;
    m.sneak_pressed = pressed;
    let sprint = on_off(flags, InputFlag::StartSprinting, InputFlag::StopSprinting)
        .filter(|&v| v != m.sprinting);
    let swim = on_off(flags, InputFlag::StartSwimming, InputFlag::StopSwimming)
        .filter(|&v| v != m.swimming);
    let glide =
        on_off(flags, InputFlag::StartGliding, InputFlag::StopGliding).filter(|&v| v != m.gliding);
    let fly =
        on_off(flags, InputFlag::StartFlying, InputFlag::StopFlying).filter(|&v| v != m.flying);
    let answered = mismatch
        || sneak.is_some()
        || sprint.is_some()
        || swim.is_some()
        || glide.is_some()
        || fly.is_some();
    if answered {
        m.answer_tick = Some(input_tick);
    }
    if let Some(v) = sneak {
        mismatch |= !toggle(state, session, Toggle::Sneak(v, pressed));
    }
    if let Some(v) = sprint {
        mismatch |= !toggle(state, session, Toggle::Sprint(v));
    }
    if let Some(v) = swim {
        mismatch |= !toggle(state, session, Toggle::Swim(v));
    }
    if let Some(v) = glide {
        mismatch |= !toggle(state, session, Toggle::Glide(v));
    }
    if let Some(v) = fly {
        mismatch |= !((may_fly || !v) && toggle(state, session, Toggle::Fly(v)));
        if let Some(player) = state.players.get_mut(&session) {
            player.survival.mark(ABILITIES);
        }
    }
    if mismatch && let Some(player) = state.players.get_mut(&session) {
        player.survival.shown = attributes::Shown::default();
    }
    if flags.contains(InputFlag::StartJumping) {
        jump(state, session);
    }
    if flags.contains(InputFlag::MissedSwing) {
        miss_swing(state, session);
    }
}

/// A movement state a player turns on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toggle {
    /// Sneaking, and whether the sneak key is down.
    Sneak(bool, bool),
    Sprint(bool),
    Swim(bool),
    Glide(bool),
    Fly(bool),
}

/// Turns `toggle` on or off for a spawned, living player unless its event's listener cancels it
/// (PMMP `Player::toggleSneak` and the others); false if refused.
fn toggle(state: &mut State, session: SessionId, toggle: Toggle) -> bool {
    let id = Id::new(session);
    let may = |state: &State| {
        state
            .players
            .get(&session)
            .is_some_and(|p| p.phase == Phase::Spawned && !p.survival.is_dead())
    };
    let set = |state: &mut State| {
        let Some(player) = state.players.get_mut(&session) else {
            return;
        };
        let s = &mut player.survival;
        match toggle {
            Toggle::Sneak(v, _) => s.motion.sneaking = v,
            Toggle::Sprint(v) => {
                s.motion.sprinting = v;
                s.mark(MOVEMENT);
            }
            Toggle::Swim(v) => s.motion.swimming = v,
            Toggle::Glide(v) => s.motion.gliding = v,
            // A change of flight forgets the fall (PMMP `Player::setFlying`).
            Toggle::Fly(v) => {
                s.motion.flying = v;
                player.reset_fall_distance();
            }
        }
    };
    macro_rules! ask {
        ($event:expr, $cancelled:expr) => {{
            let Ok(event) = super::ask(state, || $event, $cancelled, |state, _| may(state)) else {
                return false;
            };
            set(state);
            super::watch(state, event.as_ref());
        }};
    }
    match toggle {
        Toggle::Sneak(sneaking, sneak_pressed) => ask!(
            PlayerToggleSneak {
                player: id,
                sneaking,
                sneak_pressed,
                cancelled: false,
            },
            PlayerToggleSneak::is_cancelled
        ),
        Toggle::Sprint(sprinting) => ask!(
            PlayerToggleSprint {
                player: id,
                sprinting,
                cancelled: false,
            },
            PlayerToggleSprint::is_cancelled
        ),
        Toggle::Swim(swimming) => ask!(
            PlayerToggleSwim {
                player: id,
                swimming,
                cancelled: false,
            },
            PlayerToggleSwim::is_cancelled
        ),
        Toggle::Glide(gliding) => ask!(
            PlayerToggleGlide {
                player: id,
                gliding,
                cancelled: false,
            },
            PlayerToggleGlide::is_cancelled
        ),
        Toggle::Fly(flying) => ask!(
            PlayerToggleFlight {
                player: id,
                flying,
                cancelled: false,
            },
            PlayerToggleFlight::is_cancelled
        ),
    }
    true
}

/// A jump (PMMP `Player::jump`, `Human::jump`): the [`PlayerJump`] listeners hear it, then it
/// costs 0.2 exhaustion sprinting, 0.05 walking.
fn jump(state: &mut State, session: SessionId) {
    let id = Id::new(session);
    super::notify(state, || PlayerJump { player: id });
    let Some(sprinting) = state
        .players
        .get(&session)
        .map(|p| p.survival.motion.sprinting)
    else {
        return;
    };
    let (cost, cause) = if sprinting {
        (0.2, ExhaustCause::SprintJumping)
    } else {
        (0.05, ExhaustCause::Jumping)
    };
    food::exhaust_as(state, session, cost, cause);
}

/// A swing at nothing (PMMP `Player::missSwing`): unless a [`PlayerMissSwing`] listener cancels
/// it, the no-damage sound at the feet and the swing for the viewers.
fn miss_swing(state: &mut State, session: SessionId) {
    let Ok(event) = super::ask(
        state,
        || PlayerMissSwing {
            player: Id::new(session),
            cancelled: false,
        },
        PlayerMissSwing::is_cancelled,
        |state, _| state.players.contains_key(&session),
    ) else {
        return;
    };
    if let Some(at) = state.players.get(&session).map(feet) {
        sound::hit(
            state,
            session,
            at,
            crate::world::Sound::EntityAttackNoDamage,
        );
    }
    damage::swing(state, session);
    super::watch(state, event.as_ref());
}

/// `PlayerAction`: respawn, waking up, and the older toggles a client may still send, which ask
/// their events as the input flags do.
pub(super) fn on_player_action(state: &mut State, session: SessionId, packet: &PlayerAction) {
    let Ok(action) = Action::try_from(packet.action_type) else {
        return;
    };
    match action {
        Action::Respawn => damage::respawn(state, session),
        Action::StopSleeping => super::clock::wake(state, session),
        _ => {
            let Some(player) = state.players.get(&session) else {
                return;
            };
            if player.phase != Phase::Spawned || player.survival.is_dead() {
                return;
            }
            let m = &player.survival.motion;
            let change = match action {
                Action::Jump => {
                    jump(state, session);
                    return;
                }
                Action::StartSprint | Action::StopSprint => {
                    let on = action == Action::StartSprint;
                    (m.sprinting != on).then_some(Toggle::Sprint(on))
                }
                Action::StartSneak | Action::StopSneak => {
                    let on = action == Action::StartSneak;
                    (m.sneaking != on).then_some(Toggle::Sneak(on, m.sneak_pressed))
                }
                Action::StartGlide | Action::StopGlide => {
                    let on = action == Action::StartGlide;
                    (m.gliding != on).then_some(Toggle::Glide(on))
                }
                Action::StartSwimming | Action::StopSwimming => {
                    let on = action == Action::StartSwimming;
                    (m.swimming != on).then_some(Toggle::Swim(on))
                }
                _ => None,
            };
            if let Some(change) = change {
                toggle(state, session, change);
            }
        }
    }
}

/// A movement of the eyes to `target` (PMMP `Player::actuallyHandleMovement`); the 15-block
/// check only with `movement-check`.
fn handle_movement(state: &mut State, session: SessionId, target: Vec3) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let m = &mut player.survival.motion;
    m.budget -= 1;
    if m.budget < 0 {
        return;
    }
    let distance = target.distance_squared(player.position);
    let loaded = state
        .worlds
        .get(player.world)
        .is_some_and(|w| w.is_chunk_loaded(BlockPos::containing(target).chunk()));
    if (state.settings.movement_check && distance > MAX_MOVE_SQUARED) || !loaded {
        send_position(player, MoveMode::Reset);
        return;
    }
    if distance != 0.0 {
        move_to(state, session, target);
    }
}

/// Moves the player's eyes to `target` (PMMP `Entity::move` for players, which keep their
/// movement): the blocks it enters act, then the ground and fall state update.
fn move_to(state: &mut State, session: SessionId, target: Vec3) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let delta = target - player.position;
    player.position = target;
    player.data_dirty = true;
    intersect(state, session);
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let on_ground = ground(state, player, delta);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let m = &mut player.survival.motion;
    if m.on_ground != on_ground {
        m.on_ground = on_ground;
    }
    fall_state(state, session, delta.y, on_ground);
    if delta.x.abs() > MOTION_THRESHOLD || delta.z.abs() > MOTION_THRESHOLD {
        frost_walker(state, session);
    }
}

/// Whether `block` is `liquid`, or the same with the other of the still and flowing names. PMMP
/// compares the exact state, so the still water of a loaded world (`minecraft:water`) never
/// froze; its own water is all `minecraft:flowing_water`.
fn same_liquid(block: block::State, liquid: block::State) -> bool {
    block == liquid
        || (block.kind() == liquid.kind()
            && block.value("liquid_depth") == liquid.value("liquid_depth"))
}

/// PMMP `Living::applyFrostWalker` after a move: with Frost Walker boots of level `n`, the water
/// sources within `n + 2` blocks (a square) of the block under the feet that have air above
/// and no entity or player in them become frosted ice, after [`EntityFrostWalker`].
fn frost_walker(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let level = player.frost_walker_level();
    if level == 0 {
        return;
    }
    let world = player.world;
    let under = BlockPos::containing(feet(player)).step(Face::Down);
    let Some(under) = under else {
        return;
    };
    let mut radius = level.saturating_add(2).min(MAX_FROST_WALKER_RADIUS);
    let mut liquid = block::Kind::Water.default_state();
    let mut target = block::Kind::FrostedIce.default_state();
    let mut event = None;
    if dispatch::heard::<EntityFrostWalker>(state) {
        let mut e = EntityFrostWalker {
            entity: Actor::Player(Id::new(session)),
            world: crate::world::Id::of(world),
            radius,
            liquid,
            target_block: target,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let moved = dispatch::listened::<EntityFrostWalker>(state)
            && !state.players.get(&session).is_some_and(|p| {
                p.world == world && BlockPos::containing(feet(p)).step(Face::Down) == Some(under)
            });
        if e.cancelled || moved {
            return;
        }
        (radius, liquid, target) = (e.radius, e.liquid, e.target_block);
        event = Some(e);
    }
    let reach = i32::try_from(radius).unwrap_or(0);
    let (x0, x1) = (under.x.saturating_sub(reach), under.x.saturating_add(reach));
    let (z0, z1) = (under.z.saturating_sub(reach), under.z.saturating_add(reach));
    let square = Aabb::new(
        Vec3::new(f64::from(x0), f64::from(under.y), f64::from(z0)),
        Vec3::new(
            f64::from(x1) + 1.0,
            f64::from(under.y) + 1.0,
            f64::from(z1) + 1.0,
        ),
    );
    // Gathered once for the move: a cell is tested against what overlaps the square alone.
    let taken = entity::boxes_in(state, world, square);
    for x in x0..=x1 {
        for z in z0..=z1 {
            let pos = BlockPos::new(x, under.y, z);
            let Some(w) = state.worlds.get(world) else {
                return;
            };
            let above = pos.step(Face::Up).and_then(|p| w.block(p));
            let cell = Aabb::new(
                Vec3::new(f64::from(x), f64::from(under.y), f64::from(z)),
                Vec3::new(
                    f64::from(x) + 1.0,
                    f64::from(under.y) + 1.0,
                    f64::from(z) + 1.0,
                ),
            );
            if !w.block(pos).is_some_and(|b| same_liquid(b, liquid))
                || !above.is_some_and(|b| b.is(Class::Air))
                || taken.iter().any(|b| b.overlaps(cell, entity::EPSILON))
            {
                continue;
            }
            if let Some(w) = state.worlds.get_mut(world)
                && let Err(e) = w.set_block(pos, target)
            {
                tracing::debug!(?pos, %e, "frost walker could not freeze water");
            }
        }
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Whether the player stands on something after moving by `delta` (PMMP
/// `Player::checkGroundState`): a box 0.2 above and below the feet, stretched back along the
/// move, touches a block's collision box. A move past the 15-block limit, which only
/// `movement-check = false` lets through, tests the box at its end alone: stretched back along
/// a move of millions of blocks, the box would make one input test millions of blocks.
/// A player without block collision never stands.
fn ground(state: &State, player: &Player, delta: Vec3) -> bool {
    if !player.survival.collides() {
        return false;
    }
    let Some(world) = state.worlds.get(player.world) else {
        return false;
    };
    let f = feet(player);
    let half = f64::from(attributes::size(&player.survival)[0]) / 2.0;
    let bb = Aabb::new(
        Vec3::new(f.x - half, f.y - 0.2, f.z - half),
        Vec3::new(f.x + half, f.y + 0.2, f.z + half),
    );
    if delta.length_squared() > MAX_MOVE_SQUARED {
        return collides(world, bb);
    }
    collides(world, bb.stretch(-delta))
}

/// Whether any loaded block's collision box overlaps `bb` (PMMP `World::getCollisionBlocks`);
/// inert blocks have theirs.
pub(super) fn collides(world: &World, bb: Aabb) -> bool {
    let lo = BlockPos::containing(bb.min - Vec3::new(1.0, 1.0, 1.0));
    let hi = BlockPos::containing(bb.max + Vec3::new(1.0, 1.0, 1.0));
    for x in lo.x..=hi.x {
        for z in lo.z..=hi.z {
            for y in lo.y..=hi.y {
                let pos = BlockPos::new(x, y, z);
                let boxes = world.entry(pos).and_then(Entry::collision_boxes);
                if boxes.is_some_and(|b| boxes_collide(b, pos, bb)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether block-local collision boxes at `pos` overlap `bb`.
fn boxes_collide(boxes: &[Aabb], pos: BlockPos, bb: Aabb) -> bool {
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    boxes.iter().any(|b| b.offset(at).intersects(bb))
}

/// The fall distance after a vertical move (PMMP `Entity::updateFallState`); landing hurts
/// (PMMP `Living::onHitGround`) after the block landed on had its say (beds and hay soften,
/// slime cancels, farmland may be trampled).
fn fall_state(state: &mut State, session: SessionId, dy: f64, on_ground: bool) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    if dy < s.fall_distance {
        s.fall_distance -= dy;
    } else {
        s.fall_distance = 0.0;
    }
    if !(on_ground && s.fall_distance > 0.0) {
        return;
    }
    land(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        player.reset_fall_distance();
    }
}

/// The player lands (PMMP `Living::onHitGround`): the block landed on has its say first (beds
/// and hay bales soften the fall, slime cancels it, farmland may be trampled), then the fall
/// hurts with the long or short fall sound, or the landing makes the block's land sound (always
/// with `fall-damage = false`).
fn land(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let world = player.world;
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let mut pos = BlockPos::containing(feet(player));
    let mut block = w.block(pos);
    if block.is_none_or(|b| b.collision_boxes().is_empty()) {
        pos = BlockPos::new(pos.x, pos.y - 1, pos.z);
        block = w.block(pos);
    }
    let s = &mut player.survival;
    if let Some(block) = block {
        if (block.is(Class::Bed) && !s.motion.sneaking) || block.is(Class::HayBale) {
            s.fall_distance *= if block.is(Class::Bed) { 0.5 } else { 0.2 };
        } else if block.is(Class::Slime) && !s.motion.sneaking {
            s.fall_distance = 0.0;
        } else if block.is(Class::Farmland) {
            let distance = s.fall_distance;
            update::trample(
                state,
                world,
                pos,
                distance,
                crate::event::Actor::Player(crate::player::Id::new(session)),
            );
        }
    }
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let amount = if state.settings.fall_damage {
        damage::fall_damage(&player.survival, player.survival.fall_distance)
    } else {
        0.0
    };
    if amount > 0.0 {
        damage::damage(state, session, amount, Damage::Fall);
        let sound = if amount > LONG_FALL_DAMAGE {
            SoundEvent::FallBig
        } else {
            SoundEvent::FallSmall
        };
        sound::of_player(state, session, sound, -1);
    } else if let Some(block) = block.filter(|b| !b.is(Class::Air)) {
        let id = i32::try_from(block.network_id()).unwrap_or(-1);
        sound::of_player(state, session, SoundEvent::Land, id);
    }
}

/// The blocks the player is inside act on it in the base tick, unless a move this tick had them
/// act (PMMP `checkBlockIntersectionsNextTick`).
pub(super) fn block_intersections(state: &mut State, session: SessionId) {
    if state
        .players
        .get(&session)
        .is_some_and(|p| !p.survival.motion.intersected)
    {
        intersect(state, session);
    }
}

/// The blocks the player is inside act on it, on every move and in the base tick (PMMP
/// `checkBlockIntersections` and the blocks' `onEntityInside`): the blocks that hurt ([`Harm`],
/// as for entities), a burning player lights a campfire, water and water cauldrons put out fire,
/// climbable blocks and cobwebs end a fall, wither roses wither.
fn intersect(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.survival.is_dead() {
        return;
    }
    player.survival.motion.intersected = true;
    let bb = bounding_box(&player.survival, feet(player));
    let lo = BlockPos::containing(bb.min + Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET));
    let hi = BlockPos::containing(bb.max - Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET));
    let world = player.world;
    for z in lo.z..=hi.z {
        for x in lo.x..=hi.x {
            for y in lo.y..=hi.y {
                let pos = BlockPos::new(x, y, z);
                let Some(block) = state.worlds.get(world).and_then(|w| w.block(pos)) else {
                    continue;
                };
                if block.has_entity_collision() {
                    inside(state, session, pos, block);
                }
            }
        }
    }
}

/// Clears the per-tick intersection mark (the next tick's base tick acts again).
pub(super) fn end_tick(player: &mut Player) {
    player.survival.motion.intersected = false;
}

/// What one block does to a player inside it.
fn inside(state: &mut State, session: SessionId, pos: BlockPos, block: block::State) {
    if let Some(harm) = Harm::of(block) {
        if harm.ends_fall() {
            reset_fall(state, session);
        }
        let spared = harm.spared_by_sneaking()
            && state
                .players
                .get(&session)
                .is_none_or(|p| p.survival.motion.sneaking || p.frost_walker_level() > 0);
        if !spared {
            let source = DamageSource::Block(pos);
            let cause = Damage::of(harm.cause(), source);
            damage::damage_by(state, session, harm.damage(), cause, source);
        }
        if let Some(seconds) = harm.burn_secs() {
            let source = CombustSource::Block(pos);
            damage::combust(state, session, u32::from(seconds), source);
        }
    } else if block.is(Class::Campfire) {
        // An unlit one: a lit one is a `Harm`.
        if let Some(player) = state
            .players
            .get(&session)
            .filter(|p| p.survival.fire_ticks > 0)
        {
            let world = player.world;
            interact::light_campfire(state, world, pos, block);
        }
    } else if block.is(Class::Water) {
        reset_fall(state, session);
        let mut out = false;
        if let Some(player) = state.players.get_mut(&session) {
            if player.survival.fire_ticks > 0 {
                player.data_dirty = true;
                out = true;
            }
            player.survival.fire_ticks = 0;
        }
        if out {
            damage::extinguished(state, session, ExtinguishCause::Water);
        }
    } else if block.is(Class::WaterCauldron) {
        let Some(player) = state
            .players
            .get_mut(&session)
            .filter(|p| p.survival.fire_ticks > 0)
        else {
            return;
        };
        player.survival.fire_ticks = 0;
        player.data_dirty = true;
        let world = player.world;
        damage::extinguished(state, session, ExtinguishCause::WaterCauldron);
        interact::lower_water_cauldron(state, world, pos, block);
    } else if block.is(Class::Ladder) {
        let inside_block = state
            .players
            .get(&session)
            .is_some_and(|p| BlockPos::containing(feet(p)) == pos);
        if inside_block {
            reset_fall(state, session);
            if let Some(player) = state.players.get_mut(&session) {
                player.survival.motion.on_ground = true;
            }
        }
    } else if block.is(Class::Cobweb)
        || block.is(Class::Vine)
        || block.is(Class::CaveVines)
        || block.is(Class::NetherVines)
    {
        reset_fall(state, session);
    } else if block.is(Class::WitherRose) {
        let withered = state
            .players
            .get(&session)
            .is_none_or(|p| p.survival.effects.contains_key(&Effect::Wither));
        if !withered {
            effects::add(state, session, Effect::Wither, Active::new(40, 0));
        }
    }
}

/// Ends the player's fall.
fn reset_fall(state: &mut State, session: SessionId) {
    if let Some(player) = state.players.get_mut(&session) {
        player.reset_fall_distance();
    }
}

/// The player's bounding box with its feet at `feet` (PMMP `Entity::recalculateBoundingBox`).
pub(super) fn bounding_box(s: &Survival, feet: Vec3) -> Aabb {
    let [width, height] = attributes::size(s);
    let half = f64::from(width) / 2.0;
    Aabb::new(
        Vec3::new(feet.x - half, feet.y, feet.z - half),
        Vec3::new(feet.x + half, feet.y + f64::from(height), feet.z + half),
    )
}

/// The movement of a tick (PMMP `Player::processMostRecentMovements`): the rate limit refills,
/// the tick's move asks [`PlayerMove`] (cancelled, the player goes back to where the tick
/// started; sent elsewhere, it is teleported there), and a horizontal move costs 0.01 exhaustion
/// per block while sprinting (PMMP announces walking with 0). A player beyond the rate limit is
/// sent back to its accepted position.
pub(super) fn process(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let now = feet(player);
    let m = &mut player.survival.motion;
    let exceeded = m.budget < 0;
    m.budget = (m.budget.max(0) + MOVES_PER_TICK).min(MOVE_BACKLOG);
    let from = m.processed.unwrap_or(now);
    let (dx, dz) = (now.x - from.x, now.z - from.z);
    if now.distance_squared(from) > 0.0001 {
        let world = player.world;
        let Ok(event) = super::ask(
            state,
            || PlayerMove {
                player: Id::new(session),
                origin: from,
                target: now,
                cancelled: false,
            },
            PlayerMove::is_cancelled,
            |state, _| {
                state.players.get(&session).is_some_and(|p| {
                    p.phase == Phase::Spawned && !p.survival.is_dead() && feet(p) == now
                })
            },
        ) else {
            if let Some(player) = state.players.get_mut(&session) {
                player.position = from + Vec3::new(0.0, super::EYE_HEIGHT, 0.0);
                player.data_dirty = true;
                send_position(player, MoveMode::Reset);
            }
            return;
        };
        if let Some(e) = &event
            && e.target.distance_squared(now) > 0.01
        {
            let eye = e.target + Vec3::new(0.0, super::EYE_HEIGHT, 0.0);
            super::teleport(state, session, world, eye);
            super::watch(state, event.as_ref());
            return;
        }
        let Some(player) = state.players.get_mut(&session) else {
            return;
        };
        let m = &mut player.survival.motion;
        m.processed = Some(now);
        let horizontal = (dx * dx + dz * dz).sqrt();
        if horizontal > 0.0 {
            if m.sprinting {
                food::exhaust_as(state, session, 0.01 * horizontal, ExhaustCause::Sprinting);
            } else {
                food::exhaust_as(state, session, 0.0, ExhaustCause::Walking);
            }
        }
        super::watch(state, event.as_ref());
    } else if m.processed.is_none() {
        m.processed = Some(now);
    }
    if exceeded && let Some(player) = state.players.get_mut(&session) {
        send_position(player, MoveMode::Reset);
    }
}

/// Sends the player its own position (PMMP `Player::sendPosition`); moves far from it are
/// ignored until one arrives near it. The last raw position is forgotten: the client's next
/// input may report the position it had before, now a move back from where it was sent.
fn send_position(player: &mut Player, mode: MoveMode) {
    player.survival.motion.force_sync = true;
    player.survival.motion.last_position = None;
    let packet = position_packet(player, mode);
    player.send_packet(packet);
}

/// `MovePlayer` with the player's eye position and rotation in `mode`.
pub(super) fn position_packet(player: &Player, mode: MoveMode) -> Packet {
    let m = &player.survival.motion;
    let yaw = f32_of(m.yaw);
    Packet::MovePlayer(MovePlayer {
        entity_runtime_id: player.runtime_id,
        position: player.position.to_f32(),
        pitch: f32_of(m.pitch),
        yaw,
        head_yaw: yaw,
        mode: u8::from(mode),
        on_ground: m.on_ground,
        ridden_entity_runtime_id: 0,
        teleport_data: (mode == MoveMode::Teleport).then(TeleportData::default),
        tick: 0,
    })
}

/// Teleports the player's eyes to `eye` (PMMP `Player::teleport`): the fall ends and the client
/// is told.
pub(super) fn teleport(state: &mut State, session: SessionId, eye: Vec3) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.position = eye;
    player.data_dirty = true;
    player.reset_fall_distance();
    player.survival.motion.processed = Some(feet(player));
    send_position(player, MoveMode::Teleport);
}

/// Whether the player's eyes are under water (PMMP `Entity::isUnderwater`): in a water block,
/// below its surface.
pub(super) fn is_underwater(state: &State, player: &Player) -> bool {
    let eye = player.position;
    let pos = BlockPos::containing(eye);
    let Some(block) = state.worlds.get(player.world).and_then(|w| w.block(pos)) else {
        return false;
    };
    if !block.is(Class::Water) {
        return false;
    }
    let decay = match entity::liquid_depth(block) {
        Some((decay, false)) => decay,
        _ => 0,
    };
    let height = f64::from(decay + 1) / 9.0;
    eye.y < f64::from(pos.y) + 1.0 - (height - 0.111_111_1)
}

/// Whether the player's eyes are inside an opaque solid block (PMMP `Entity::isInsideOfSolid`).
pub(super) fn is_inside_solid(state: &State, player: &Player) -> bool {
    state.worlds.get(player.world).is_some_and(|w| {
        let bounds = bounding_box(&player.survival, feet(player));
        entity::is_inside_solid(w, player.position, bounds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::PropertyValue;

    thread_local! {
        /// What listeners and monitors wrote down; they run on the test's thread.
        static NOTES: std::cell::Cell<Vec<String>> = const { std::cell::Cell::new(Vec::new()) };
    }

    /// Writes down `note`.
    fn note(note: String) {
        NOTES.with(|n| {
            let mut list = n.take();
            list.push(note);
            n.set(list);
        });
    }

    /// What was written down since the last call.
    fn notes() -> Vec<String> {
        NOTES.with(std::cell::Cell::take)
    }

    #[test]
    fn on_off_flags_need_exactly_one() {
        let set = |f: &[InputFlag]| f.iter().copied().collect::<FlagSet<InputFlag>>();
        let (start, stop) = (InputFlag::StartSprinting, InputFlag::StopSprinting);
        assert_eq!(on_off(set(&[start]), start, stop), Some(true));
        assert_eq!(on_off(set(&[stop]), start, stop), Some(false));
        assert_eq!(on_off(set(&[start, stop]), start, stop), None);
        assert_eq!(on_off(set(&[]), start, stop), None);
    }

    #[test]
    fn the_look_direction_follows_pmmp() {
        let mut s = Survival::default();
        let d = entity::direction(s.motion.yaw, s.motion.pitch);
        assert!(d.z > 0.99 && d.x.abs() < 1e-9, "yaw 0 looks south: {d:?}");
        s.motion.yaw = 90.0;
        let d = entity::direction(s.motion.yaw, s.motion.pitch);
        assert!(d.x < -0.99, "yaw 90 looks west: {d:?}");
        s.motion.pitch = 90.0;
        let d = entity::direction(s.motion.yaw, s.motion.pitch);
        assert!(d.y < -0.99, "pitch 90 looks down: {d:?}");
    }

    /// A world with one spawned player (session 1) whose next move counts, and its outbound
    /// queue (dropping it ends the session).
    fn walker(
        listeners: crate::event::Listeners,
    ) -> (
        State,
        crate::tick::Output,
        tokio::sync::mpsc::Receiver<crate::tick::queue::Outbound>,
    ) {
        use crate::tick::tests::{initialised, join, new_state, radius, settings, tick};
        let listeners = listeners;
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = crate::tick::Output::default();
        let (j, rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        tick(&mut state, &mut vec![initialised(1, 2, 1)], &mut output);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.survival.motion.force_sync = false;
        (state, output, rx)
    }

    /// Water sources over the ground from x 0 to 12 and z -6 to 6.
    fn lake(state: &mut State) {
        let water = block::Kind::Water.default_state();
        for x in 0..=12 {
            for z in -6..=6 {
                state.worlds[0]
                    .set_block(BlockPos::new(x, -61, z), water)
                    .unwrap();
            }
        }
    }

    /// Frost Walker boots of `level` on session 1.
    fn boots(state: &mut State, level: u8) {
        let extra = crate::item::Extra {
            enchantments: vec![(crate::item::Enchantment::FrostWalker, level)],
            ..crate::item::Extra::default()
        };
        let boots = crate::item::Stack::new(crate::item::Kind::DiamondBoots.default_type(), 1)
            .with_extra(Some(extra))
            .unwrap();
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        crate::tick::inventory::set_armor(player, 3, boots);
    }

    /// Session 1 walks to the feet position `x, -60, z`.
    fn walk(state: &mut State, output: &mut crate::tick::Output, x: f64, z: f64) {
        let eye = Vec3::new(x, -60.0 + super::super::EYE_HEIGHT, z).to_f32();
        let input = crate::tick::tests::moved_to(1, 10, eye);
        crate::tick::tests::tick(state, &mut vec![input], output);
    }

    /// The frosted ice cells of the lake.
    fn frozen(state: &State) -> Vec<(i32, i32)> {
        let ice = block::Kind::FrostedIce.default_state();
        let mut out = Vec::new();
        for x in 0..=12 {
            for z in -6..=6 {
                if state.worlds[0].block(BlockPos::new(x, -61, z)) == Some(ice) {
                    out.push((x, z));
                }
            }
        }
        out
    }

    /// Every move lets the blocks it enters act before the landing (PMMP `Entity::move`): a fall
    /// into water counts from the water on, even when it lands on the tick's second move.
    #[test]
    fn a_fall_into_water_on_the_second_move_of_a_tick_does_not_hurt() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let water = block::Kind::Water.default_state();
        state.worlds[0]
            .set_block(BlockPos::new(0, -60, 0), water)
            .unwrap();
        let eye = |feet_y: f64| Vec3::new(0.5, feet_y + super::super::EYE_HEIGHT, 0.5);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.position = eye(-45.0);
        player.survival.motion.processed = Some(Vec3::new(0.5, -45.0, 0.5));
        let mid = crate::tick::tests::moved_to(1, 10, eye(-58.0).to_f32());
        let down = crate::tick::tests::moved_to(1, 11, eye(-60.0).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![mid, down], &mut output);
        let player = &state.players[&SessionId::new(1)];
        assert_eq!(feet(player).y, -60.0);
        assert_eq!(player.survival.health, 20.0);
    }

    /// A burning player in a water cauldron is put out and the cauldron loses a level (PMMP
    /// `WaterCauldron::onEntityInside`).
    #[test]
    fn a_water_cauldron_puts_out_a_burning_player_and_drops_a_level() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let level = |n| {
            block::Kind::Cauldron
                .default_state()
                .with("cauldron_liquid", PropertyValue::String("water"))
                .and_then(|s| s.with("fill_level", PropertyValue::Int(n)))
                .unwrap()
        };
        let pos = BlockPos::new(0, -60, 0);
        state.worlds[0].set_block(pos, level(6)).unwrap();
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.position = Vec3::new(0.5, -60.0 + super::super::EYE_HEIGHT, 0.5);
        player.survival.fire_ticks = 100;
        crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.players[&session].survival.fire_ticks, 0);
        assert_eq!(state.worlds[0].block(pos), Some(level(5)));
        // Not burning: the water stays.
        crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.worlds[0].block(pos), Some(level(5)));
    }

    /// A burning player lights an unlit campfire it is in; one not burning leaves it out (PMMP
    /// `Campfire::onEntityInside`).
    #[test]
    fn a_burning_player_lights_an_unlit_campfire() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let lit = |on: bool| {
            block::Kind::Campfire
                .default_state()
                .with("extinguished", PropertyValue::Byte(u8::from(!on)))
                .unwrap()
        };
        let pos = BlockPos::new(0, -60, 0);
        state.worlds[0].set_block(pos, lit(false)).unwrap();
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.position = Vec3::new(0.5, -60.0 + super::super::EYE_HEIGHT, 0.5);
        crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.worlds[0].block(pos), Some(lit(false)));
        let player = state.players.get_mut(&session).unwrap();
        player.survival.fire_ticks = 100;
        crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.worlds[0].block(pos), Some(lit(true)));
    }

    /// A player without block collision never stands (PMMP `Player::checkGroundState`), so a
    /// fall through the floor does not land.
    #[test]
    fn a_player_without_block_collision_never_lands() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let eye = |feet_y: f64| Vec3::new(0.5, feet_y + super::super::EYE_HEIGHT, 0.5);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.set_block_collision(false);
        player.position = eye(-50.0);
        player.survival.motion.processed = Some(Vec3::new(0.5, -50.0, 0.5));
        let mid = crate::tick::tests::moved_to(1, 10, eye(-55.0).to_f32());
        let down = crate::tick::tests::moved_to(1, 11, eye(-60.0).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![mid, down], &mut output);
        let player = &state.players[&SessionId::new(1)];
        assert!(!player.survival.motion.on_ground);
        assert_eq!(player.survival.health, 20.0);
    }

    /// Whatever ends a fall restarts the ticks in the air (PMMP `Player::resetFallDistance`): a
    /// player floating in water stays at none, and a teleport starts it again.
    #[test]
    fn what_ends_a_fall_restarts_the_ticks_in_the_air() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let water = block::Kind::Water.default_state();
        state.worlds[0]
            .set_block(BlockPos::new(0, -60, 0), water)
            .unwrap();
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.position = Vec3::new(0.5, -60.0 + super::super::EYE_HEIGHT, 0.5);
        player.survival.motion.on_ground = false;
        for _ in 0..3 {
            crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        }
        assert_eq!(state.players[&session].in_air_ticks(), 0);
        let player = state.players.get_mut(&session).unwrap();
        player.survival.motion.in_air = 5;
        teleport(&mut state, session, Vec3::new(0.5, -40.0, 0.5));
        assert_eq!(state.players[&session].in_air_ticks(), 0);
    }

    // pmmp: Living::getFrostWalkerLevel
    #[test]
    fn frost_walker_freezes_the_water_sources_around_the_feet_with_air_above_and_nobody_in() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        lake(&mut state);
        // Still water freezes as well (PMMP compares the exact state: only the flowing name).
        let still = block::State::find(
            "minecraft:water",
            &[("liquid_depth", PropertyValue::Int(0))],
        )
        .unwrap();
        state.worlds[0]
            .set_block(BlockPos::new(7, -61, -2), still)
            .unwrap();
        // A flowing cell, a covered cell and an item in a cell stay water.
        let flowing = block::Kind::Water
            .default_state()
            .with("liquid_depth", PropertyValue::Int(1))
            .unwrap();
        state.worlds[0]
            .set_block(BlockPos::new(6, -61, 1), flowing)
            .unwrap();
        state.worlds[0]
            .set_block(
                BlockPos::new(4, -60, -1),
                block::Kind::Stone.default_state(),
            )
            .unwrap();
        let stick = crate::item::Stack::new(crate::item::Kind::Stick.default_type(), 1);
        assert!(entity::drop_item(
            &mut state,
            0,
            Vec3::new(3.5, -60.9, 2.5),
            stick
        ));
        // Without the enchantment nothing freezes.
        walk(&mut state, &mut output, 5.5, 0.5);
        assert!(frozen(&state).is_empty());
        boots(&mut state, 2);
        assert_eq!(state.players[&SessionId::new(1)].frost_walker_level(), 2);
        walk(&mut state, &mut output, 5.6, 0.5);
        let mut want = Vec::new();
        for x in 1..=9 {
            for z in -4..=4 {
                if ![(6, 1), (4, -1), (3, 2)].contains(&(x, z)) {
                    want.push((x, z));
                }
            }
        }
        assert_eq!(
            frozen(&state),
            want,
            "radius 2 + 2 around the block under the feet"
        );
        // Standing still freezes no more.
        lake(&mut state);
        walk(&mut state, &mut output, 5.6, 0.5);
        assert!(frozen(&state).is_empty());
    }

    #[test]
    fn frost_walker_leaves_water_where_another_spawned_player_wades() {
        use crate::tick::tests::{join, radius, tick};
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let mut receivers = Vec::new();
        for session in 2..=9 {
            let (j, rx) = join(session, 0);
            receivers.push(rx);
            tick(&mut state, &mut vec![j, radius(session, 1, 4)], &mut output);
        }
        // One wades in the cell (8, -3); one more there is not spawned; the rest stand away.
        for session in 2..=9 {
            let p = state.players.get_mut(&SessionId::new(session)).unwrap();
            p.phase = if session == 3 {
                Phase::Joined
            } else {
                Phase::Spawned
            };
            p.position = match session {
                2 => Vec3::new(8.5, -60.5 + super::super::EYE_HEIGHT, -2.5),
                3 => Vec3::new(2.5, -60.5 + super::super::EYE_HEIGHT, 3.5),
                _ => Vec3::new(
                    f64::from(u32::try_from(session).unwrap()) * 3.0,
                    -40.0,
                    30.0,
                ),
            };
        }
        lake(&mut state);
        boots(&mut state, 2);
        walk(&mut state, &mut output, 5.6, 0.5);
        let mut want = Vec::new();
        for x in 1..=9 {
            for z in -4..=4 {
                if (x, z) != (8, -3) {
                    want.push((x, z));
                }
            }
        }
        assert_eq!(frozen(&state), want);
    }

    // pmmp: EntityFrostWalkerEvent, EntityFrostWalkerEvent::getRadius, EntityFrostWalkerEvent::getLiquid, EntityFrostWalkerEvent::getTargetBlock, EntityFrostWalkerEvent::setRadius, EntityFrostWalkerEvent::setTargetBlock, EntityFrostWalkerEvent::cancel
    #[test]
    fn a_frost_walker_listener_sees_the_freeze_and_may_change_or_cancel_it() {
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            move |e: &mut EntityFrostWalker, _: &mut crate::tick::Context<'_>| {
                note(format!(
                    "{:?}",
                    (e.entity(), e.radius(), e.liquid(), e.target_block())
                ));
                e.set_radius(0);
                e.set_target_block(block::Kind::Stone.default_state());
            },
        );
        l.push_monitor(
            move |_: &EntityFrostWalker, cx: &crate::tick::Context<'_>| {
                let under = cx.state.worlds[0].block(BlockPos::new(5, -61, 0));
                note(format!("{under:?}"));
            },
        );
        let (mut state, mut output, _rx) = walker(l);
        lake(&mut state);
        boots(&mut state, 1);
        walk(&mut state, &mut output, 5.5, 0.5);
        let stone = block::Kind::Stone.default_state();
        let player = Actor::Player(Id::new(SessionId::new(1)));
        let water = block::Kind::Water.default_state();
        let ice = block::Kind::FrostedIce.default_state();
        assert_eq!(
            notes(),
            [
                format!("{:?}", (player, 3u32, water, ice)),
                format!("{:?}", Some(stone)),
            ]
        );
        assert!(frozen(&state).is_empty());
        let under = state.worlds[0].block(BlockPos::new(5, -61, 0));
        assert_eq!(
            under,
            Some(stone),
            "radius 0: only the block under the feet"
        );
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            |e: &mut EntityFrostWalker, _: &mut crate::tick::Context<'_>| e.cancel(),
        );
        let (mut state, mut output, _rx) = walker(l);
        lake(&mut state);
        boots(&mut state, 2);
        walk(&mut state, &mut output, 5.5, 0.5);
        assert!(frozen(&state).is_empty());
    }

    // pmmp: EntityFrostWalkerEvent::setLiquid, EntityFrostWalkerEvent::uncancel, EntityFrostWalkerEvent::isCancelled
    #[test]
    fn frost_walker_values_stay_in_range() {
        let mut e = EntityFrostWalker {
            entity: Actor::Player(Id::new(SessionId::new(1))),
            world: crate::world::Id::DEFAULT,
            radius: 3,
            liquid: block::Kind::Water.default_state(),
            target_block: block::Kind::FrostedIce.default_state(),
            cancelled: false,
        };
        e.set_radius(MAX_FROST_WALKER_RADIUS + 1);
        assert_eq!(e.radius(), 3);
        e.set_liquid(block::Kind::Stone.default_state());
        assert_eq!(e.liquid(), block::Kind::Water.default_state());
        e.set_liquid(block::Kind::Lava.default_state());
        assert_eq!(e.liquid(), block::Kind::Lava.default_state());
        assert_eq!(e.world(), crate::world::Id::DEFAULT);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    /// A teleport forgets the last position the client reported: an input that reports it again
    /// after the teleport, near the target, moves the player back there.
    #[test]
    fn an_input_at_the_position_before_a_teleport_is_applied_after_it() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let session = SessionId::new(1);
        // Positions a client can report: f32.
        let eye = |x: f64| Vec3::from_f32(Vec3::new(x, -58.0, 0.5).to_f32());
        let before = crate::tick::tests::moved_to(1, 10, eye(0.5).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![before], &mut output);
        assert_eq!(state.players[&session].position, eye(0.5));
        teleport(&mut state, session, eye(1.0));
        let again = crate::tick::tests::moved_to(1, 11, eye(0.5).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![again], &mut output);
        let player = &state.players[&session];
        assert_eq!(player.position, eye(0.5));
        assert!(!player.survival.motion.force_sync);
    }

    #[test]
    fn a_move_past_fifteen_blocks_stands_only_on_what_is_under_its_end() {
        use crate::tick::tests::{initialised, join, moved_to, new_state, radius, settings, tick};
        let config = settings(|c| c.general.movement_check = false);
        let mut state = new_state(config, crate::event::Listeners::default());
        let mut output = crate::tick::Output::default();
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        tick(&mut state, &mut vec![initialised(1, 2, 1)], &mut output);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.survival.motion.force_sync = false;
        // Straight up from the ground: the box stretched back along the move reaches the ground.
        let eye = player.position + Vec3::new(0.0, 30.0, 0.0);
        tick(
            &mut state,
            &mut vec![moved_to(1, 10, eye.to_f32())],
            &mut output,
        );
        let player = &state.players[&SessionId::new(1)];
        assert!(player.position.y > -40.0, "the move is taken");
        assert!(!player.survival.motion.on_ground);
    }
}
