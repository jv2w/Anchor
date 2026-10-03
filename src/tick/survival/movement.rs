//! Movement and input (PMMP `InGamePacketHandler::handlePlayerAuthInput`, `Player::handleMovement`,
//! `Player::processMostRecentMovements`, `Player::checkGroundState`, `Entity::updateFallState`,
//! `Entity::checkBlockIntersections`, the blocks' `onEntityInside`/`onEntityLand`).
//!
//! The client moves itself; the server accepts each position unless it is more than 15 blocks
//! from the last one, outside the loaded chunks, or beyond the rate limit (2 moves a tick, 200
//! in reserve), and then sends the player back (`MovePlayer` reset). PMMP's checks are safety
//! checks, not an anti-cheat.

use super::attributes::{ABILITIES, MOVEMENT};
use super::damage::{self, Damage};
use super::effects::{self, Active};
use super::{Survival, attributes, block_at, f32_of, feet, food, game_mode, sound};

use crate::block::{self, Class, PropertyValue};
use crate::config::GameMode;
use crate::effect::Type as Effect;
use crate::item::Enchantment;
use crate::math::{self, Aabb, Vec3};
use crate::protocol::consts::{InputFlag, MoveMode, PlayerAction as Action, SoundEvent};
use crate::protocol::packet::{MovePlayer, Packet, PlayerAction, PlayerAuthInput};
use crate::protocol::{FlagSet, TeleportData};
use crate::tick::{MAX_COORDINATE, Phase, Player, SessionId, State, update};
use crate::world::{BlockPos, World};

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
/// Inset of the box that finds the blocks an entity is inside (PMMP
/// `getBlocksAroundWithEntityInsideActions`).
const INSIDE_INSET: f64 = 0.001;
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
    /// The blocks around the player acted this tick (PMMP `checkBlockIntersectionsNextTick`).
    intersected: bool,
    /// The player lay down in its bed in this session (a saved sleeper is woken on its first
    /// tick, which frees the bed).
    pub(super) in_bed: bool,
    /// The death screen was shown for the current death (a player that joins dead sees it on
    /// its first tick).
    pub(super) death_shown: bool,
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
        }
    }
}

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
    }
}

/// `PlayerAuthInput`: rotation, input flags, then the movement; only from a spawned, living
/// player (PMMP handles it in the in-game handler only, so a client cannot place itself anywhere
/// before it spawns). The tick keeps the client's eye position exactly (PMMP rounds the feet to
/// 4 decimals; an `f32` position needs no rounding).
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
        input_flags(state, session, flags);
    }
    let border = f64::from(MAX_COORDINATE);
    let raw = Vec3::from_f32(packet.position);
    if [raw.x, raw.y, raw.z].iter().any(|c| c.abs() > border) {
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
/// swing.
fn input_flags(state: &mut State, session: SessionId, flags: FlagSet<InputFlag>) {
    let may_fly = state.players.get(&session).is_some_and(|p| {
        matches!(
            game_mode(state, p),
            GameMode::Creative | GameMode::Spectator
        )
    });
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let m = &mut player.survival.motion;
    let pressed = flags.contains(InputFlag::Sneaking);
    let sneak = on_off(flags, InputFlag::StartSneaking, InputFlag::StopSneaking);
    let mut mismatch = sneak.unwrap_or(m.sneaking) == m.sneaking && pressed != m.sneak_pressed;
    m.sneak_pressed = pressed;
    if let Some(v) = sneak {
        m.sneaking = v;
    }
    if let Some(v) = on_off(flags, InputFlag::StartSprinting, InputFlag::StopSprinting)
        && v != m.sprinting
    {
        m.sprinting = v;
        player.survival.mark(MOVEMENT);
    }
    let m = &mut player.survival.motion;
    if let Some(v) = on_off(flags, InputFlag::StartSwimming, InputFlag::StopSwimming) {
        m.swimming = v;
    }
    if let Some(v) = on_off(flags, InputFlag::StartGliding, InputFlag::StopGliding) {
        m.gliding = v;
    }
    if let Some(v) = on_off(flags, InputFlag::StartFlying, InputFlag::StopFlying)
        && v != m.flying
    {
        if may_fly || !v {
            m.flying = v;
        } else {
            mismatch = true;
        }
        player.survival.mark(ABILITIES);
    }
    if mismatch {
        player.survival.shown = attributes::Shown::default();
    }
    if flags.contains(InputFlag::StartJumping) {
        jump(&mut player.survival);
    }
    if flags.contains(InputFlag::MissedSwing) {
        // PMMP `Player::missSwing`: the no-damage sound at the feet, the swing for the viewers.
        if let Some(at) = state.players.get(&session).map(feet) {
            sound::hit(state, session, at, SoundEvent::AttackNoDamage);
        }
        damage::swing(state, session);
    }
}

/// A jump costs 0.2 exhaustion sprinting, 0.05 walking (PMMP `Human::jump`).
fn jump(s: &mut Survival) {
    let cost = if s.motion.sprinting { 0.2 } else { 0.05 };
    food::exhaust(s, cost);
}

/// `PlayerAction`: respawn, waking up, and the older toggles a client may still send.
pub(super) fn on_player_action(state: &mut State, session: SessionId, packet: &PlayerAction) {
    let Ok(action) = Action::try_from(packet.action_type) else {
        return;
    };
    match action {
        Action::Respawn => damage::respawn(state, session),
        Action::StopSleeping => super::clock::wake(state, session),
        _ => {
            let Some(player) = state.players.get_mut(&session) else {
                return;
            };
            if player.phase != Phase::Spawned || player.survival.is_dead() {
                return;
            }
            let s = &mut player.survival;
            match action {
                Action::Jump => jump(s),
                Action::StartSprint | Action::StopSprint => {
                    let on = action == Action::StartSprint;
                    if s.motion.sprinting != on {
                        s.motion.sprinting = on;
                        s.mark(MOVEMENT);
                    }
                }
                Action::StartSneak | Action::StopSneak => {
                    s.motion.sneaking = action == Action::StartSneak;
                }
                Action::StartGlide | Action::StopGlide => {
                    s.motion.gliding = action == Action::StartGlide;
                }
                Action::StartSwimming | Action::StopSwimming => {
                    s.motion.swimming = action == Action::StartSwimming;
                }
                _ => {}
            }
        }
    }
}

/// A movement of the eyes to `target` (PMMP `Player::actuallyHandleMovement`).
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
        .is_some_and(|w| w.is_chunk_loaded(block_at(target).chunk()));
    if distance > MAX_MOVE_SQUARED || !loaded {
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
    block_intersections(state, session);
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
}

/// Whether the player stands on something after moving by `delta` (PMMP
/// `Player::checkGroundState`): a box 0.2 above and below the feet, stretched back along the
/// move, touches a block's collision box. Spectators never stand.
fn ground(state: &State, player: &Player, delta: Vec3) -> bool {
    if game_mode(state, player) == GameMode::Spectator {
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
    collides(world, stretch(bb, -delta))
}

/// The box extended by `d` (PMMP `AxisAlignedBB::addCoord`).
fn stretch(bb: Aabb, d: Vec3) -> Aabb {
    let mut out = bb;
    if d.x < 0.0 {
        out.min.x += d.x;
    } else {
        out.max.x += d.x;
    }
    if d.y < 0.0 {
        out.min.y += d.y;
    } else {
        out.max.y += d.y;
    }
    if d.z < 0.0 {
        out.min.z += d.z;
    } else {
        out.max.z += d.z;
    }
    out
}

/// Whether any loaded block's collision box overlaps `bb` (PMMP `World::getCollisionBlocks`).
pub(super) fn collides(world: &World, bb: Aabb) -> bool {
    let lo = block_at(bb.min - Vec3::new(1.0, 1.0, 1.0));
    let hi = block_at(bb.max + Vec3::new(1.0, 1.0, 1.0));
    for x in lo.x..=hi.x {
        for z in lo.z..=hi.z {
            for y in lo.y..=hi.y {
                let pos = BlockPos::new(x, y, z);
                if world.block(pos).is_some_and(|b| block_collides(b, pos, bb)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether a block's collision boxes overlap `bb` (PMMP `Block::collidesWithBB`).
fn block_collides(block: block::State, pos: BlockPos, bb: Aabb) -> bool {
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    block
        .collision_boxes()
        .iter()
        .any(|b| b.offset(at).intersects(bb))
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
        player.survival.fall_distance = 0.0;
    }
}

/// The player lands (PMMP `Living::onHitGround`): the block landed on has its say first (beds
/// and hay bales soften the fall, slime cancels it, farmland may be trampled), then the fall
/// hurts with the long or short fall sound, or the landing makes the block's land sound.
fn land(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let world = player.world;
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let mut pos = block_at(feet(player));
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
            update::trample(state, world, pos, distance);
        }
    }
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let amount = damage::fall_damage(&player.survival, player.survival.fall_distance);
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

/// The blocks the player is inside act on it once a tick (PMMP `checkBlockIntersections` and the
/// blocks' `onEntityInside`): fire and lava burn, cactus and berry bushes prick, water puts out
/// fire, climbable blocks and cobwebs end a fall, wither roses wither.
pub(super) fn block_intersections(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.survival.motion.intersected || player.survival.is_dead() {
        return;
    }
    player.survival.motion.intersected = true;
    let bb = bounding_box(&player.survival, feet(player));
    let lo = block_at(bb.min + Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET));
    let hi = block_at(bb.max - Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET));
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

/// Clears the per-tick intersection mark (the next tick's movement or base tick acts again).
pub(super) fn end_tick(player: &mut Player) {
    player.survival.motion.intersected = false;
}

/// What one block does to a player inside it.
fn inside(state: &mut State, session: SessionId, pos: BlockPos, block: block::State) {
    let int = |name: &str| match block.value(name) {
        Some(PropertyValue::Int(v)) => v,
        Some(PropertyValue::Byte(v)) => i32::from(v),
        _ => 0,
    };
    if block.is(Class::BaseFire) {
        let amount = if block.is(Class::SoulFire) { 2.0 } else { 1.0 };
        damage::damage(state, session, amount, Damage::Fire);
        burn(state, session, 8);
    } else if block.is(Class::Lava) || block.is(Class::LavaCauldron) {
        damage::damage(state, session, 4.0, Damage::Lava);
        burn(state, session, 8);
        if block.is(Class::Lava) {
            reset_fall(state, session);
        }
    } else if block.is(Class::Water) {
        reset_fall(state, session);
        if let Some(player) = state.players.get_mut(&session) {
            if player.survival.fire_ticks > 0 {
                player.data_dirty = true;
            }
            player.survival.fire_ticks = 0;
        }
    } else if block.is(Class::Cactus) {
        damage::damage(state, session, 1.0, Damage::Contact);
    } else if block.is(Class::Magma) {
        let hurts = state.players.get(&session).is_some_and(|p| {
            !p.survival.motion.sneaking
                && p.inventory
                    .armor()
                    .get(3)
                    .is_none_or(|boots| damage::level_of(boots, Enchantment::FrostWalker) == 0)
        });
        if hurts {
            damage::damage(state, session, 1.0, Damage::Fire);
        }
    } else if block.is(Class::Campfire) {
        if int("extinguished") == 0 {
            let amount = if block.is(Class::SoulCampfire) {
                2.0
            } else {
                1.0
            };
            damage::damage(state, session, amount, Damage::Fire);
        }
    } else if block.is(Class::SweetBerryBush) {
        if int("growth") >= 1 {
            reset_fall(state, session);
            damage::damage(state, session, 1.0, Damage::Contact);
        }
    } else if block.is(Class::Ladder) {
        let inside_block = state
            .players
            .get(&session)
            .is_some_and(|p| block_at(feet(p)) == pos);
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

/// Sets the player on fire for `seconds`.
fn burn(state: &mut State, session: SessionId, seconds: u32) {
    if let Some(player) = state.players.get_mut(&session) {
        damage::set_on_fire(player, seconds);
    }
}

/// Ends the player's fall.
fn reset_fall(state: &mut State, session: SessionId) {
    if let Some(player) = state.players.get_mut(&session) {
        player.survival.fall_distance = 0.0;
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
/// and a horizontal move while sprinting costs 0.01 exhaustion per block. A player beyond the rate
/// limit is sent back to its accepted position.
pub(super) fn process(player: &mut Player) {
    let now = feet(player);
    let m = &mut player.survival.motion;
    let exceeded = m.budget < 0;
    m.budget = (m.budget.max(0) + MOVES_PER_TICK).min(MOVE_BACKLOG);
    let from = m.processed.unwrap_or(now);
    let (dx, dz) = (now.x - from.x, now.z - from.z);
    if now.distance_squared(from) > 0.0001 {
        m.processed = Some(now);
        let horizontal = (dx * dx + dz * dz).sqrt();
        if horizontal > 0.0 && m.sprinting {
            food::exhaust(&mut player.survival, 0.01 * horizontal);
        }
    } else if m.processed.is_none() {
        m.processed = Some(now);
    }
    if exceeded {
        send_position(player, MoveMode::Reset);
    }
}

/// Sends the player its own position (PMMP `Player::sendPosition`); moves far from it are
/// ignored until one arrives near it.
fn send_position(player: &mut Player, mode: MoveMode) {
    player.survival.motion.force_sync = true;
    let packet = position_packet(player, mode);
    player.send(packet);
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
    player.survival.fall_distance = 0.0;
    player.survival.motion.processed = Some(feet(player));
    send_position(player, MoveMode::Teleport);
}

/// The look direction (PMMP `Entity::getDirectionVector`).
pub(super) fn direction(s: &Survival) -> Vec3 {
    let pitch = math::radians(s.motion.pitch);
    let yaw = math::radians(s.motion.yaw);
    let xz = math::cos(pitch);
    let v = Vec3::new(-xz * math::sin(yaw), -math::sin(pitch), xz * math::cos(yaw));
    let length = v.length();
    if length > 0.0 { v * (1.0 / length) } else { v }
}

/// Whether the player's eyes are under water (PMMP `Entity::isUnderwater`): in a water block,
/// below its surface.
pub(super) fn is_underwater(state: &State, player: &Player) -> bool {
    let eye = player.position;
    let pos = block_at(eye);
    let Some(block) = state.worlds.get(player.world).and_then(|w| w.block(pos)) else {
        return false;
    };
    if !block.is(Class::Water) {
        return false;
    }
    let depth = match block.value("liquid_depth") {
        Some(PropertyValue::Int(d)) => d,
        _ => 0,
    };
    let decay = if depth >= 8 { 0 } else { depth & 7 };
    let height = f64::from(decay + 1) / 9.0;
    eye.y < f64::from(pos.y) + 1.0 - (height - 0.111_111_1)
}

/// Whether the player's eyes are inside an opaque solid block (PMMP `Entity::isInsideOfSolid`).
pub(super) fn is_inside_solid(state: &State, player: &Player) -> bool {
    let pos = block_at(player.position);
    let Some(block) = state.worlds.get(player.world).and_then(|w| w.block(pos)) else {
        return false;
    };
    block.is_solid()
        && !block.is_transparent()
        && block_collides(block, pos, bounding_box(&player.survival, feet(player)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let d = direction(&s);
        assert!(d.z > 0.99 && d.x.abs() < 1e-9, "yaw 0 looks south: {d:?}");
        s.motion.yaw = 90.0;
        let d = direction(&s);
        assert!(d.x < -0.99, "yaw 90 looks west: {d:?}");
        s.motion.pitch = 90.0;
        let d = direction(&s);
        assert!(d.y < -0.99, "pitch 90 looks down: {d:?}");
    }

    #[test]
    fn boxes_stretch_along_the_move() {
        let bb = Aabb::new(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0));
        let s = stretch(bb, Vec3::new(-2.0, 3.0, 0.0));
        assert_eq!(s.min, Vec3::new(-2.0, 0.0, 0.0));
        assert_eq!(s.max, Vec3::new(1.0, 4.0, 1.0));
    }
}
