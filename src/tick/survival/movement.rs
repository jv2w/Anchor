//! Movement and input: the client's moves taken or reset, the sneak, sprint, swim, glide and
//! flight toggles, the ground and fall state, and what the blocks a player is inside do to it.
//!
//! The client moves itself; the server accepts each position unless it is more than 15 blocks
//! from the last one (with `movement-check`), outside the loaded chunks, or beyond the rate limit
//! (2 moves a tick, 200 in reserve), and then sends the player back (`MovePlayer` reset). The
//! checks are safety checks, not an anti-cheat.

use super::attributes::{ABILITIES, MOVEMENT};
use super::damage::{self, Damage, Trigger};
use super::effects::{self, Active};
use super::{Stay, Survival, attributes, feet, feet_at, food, sound};
use crate::block::{self, Class};
use crate::effect::Type as Effect;
use crate::event::{
    CombustSource, DamageSource, ExhaustCause, ExtinguishCause, PlayerJump, PlayerMissSwing,
    PlayerMove, PlayerToggleFlight, PlayerToggleGlide, PlayerToggleSneak, PlayerToggleSprint,
    PlayerToggleSwim,
};
use crate::math::{Aabb, Vec3, to_f32};
use crate::player::Id;
use crate::protocol::consts::{InputFlag, MoveMode, PlayerAction as Action, SoundEvent};
use crate::protocol::packet::{MovePlayer, Packet, PlayerAction, PlayerAuthInput};
use crate::protocol::{FlagSet, TeleportData};
use crate::tick::entity::{INSIDE_INSET, Touch};
use crate::tick::{Phase, Player, SessionId, State, checkpoint, entity, inside_border};
use crate::world::{BlockPos, Entry, World};

/// How far above and below the feet the box that tests for ground reaches.
const GROUND_REACH: f64 = 0.2;
/// Fall damage above which landing makes the long fall sound.
const LONG_FALL_DAMAGE: f64 = 4.0;
/// The most moves in reserve: 100 ticks.
const MAX_MOVE_BACKLOG: i32 = 100 * MOVES_PER_TICK;
/// The furthest one move may go, squared: 15 blocks.
const MAX_MOVE_SQUARED: f64 = 225.0;
/// The shortest tick move that counts as a move, squared.
const MIN_MOVE_SQUARED: f64 = 0.0001;
/// The least a [`PlayerMove`] listener must shift the target for it to be another place, squared.
const MIN_RETARGET_SQUARED: f64 = 0.01;
/// The reserve of a new player.
const MOVE_START: i32 = 10 * MOVES_PER_TICK;
/// Moves the reserve gains each tick.
const MOVES_PER_TICK: i32 = 2;
/// How far a move after a teleport may be from the teleport target, squared.
const SYNC_TOLERANCE_SQUARED: f64 = 1.0;
/// The length of the wither effect a wither rose gives, in ticks.
const WITHER_ROSE_EFFECT_TICKS: u32 = 40;

/// A player's movement and input state.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Motion {
    pub(super) sprinting: bool,
    pub(super) sneaking: bool,
    /// The sneak key is held.
    pub(super) sneak_pressed: bool,
    pub(super) swimming: bool,
    pub(super) gliding: bool,
    pub(super) flying: bool,
    pub(super) on_ground: bool,
    /// Whether food runs for the player (settings and game mode), set each tick.
    pub(super) food: bool,
    /// Yaw and pitch in degrees ([`turn`]): the yaw in [0, 360), the pitch above -360 and below
    /// 360.
    pub(super) yaw: f64,
    pub(super) pitch: f64,
    /// The last raw rotation and input flags received.
    last_rotation: [u32; 2],
    last_flags: FlagSet<InputFlag>,
    /// The last raw position received.
    last_position: Option<[u32; 3]>,
    /// The feet position the movement was last processed at.
    processed: Option<Vec3>,
    /// Moves left before the rate limit.
    budget: i32,
    /// After a teleport or revert, moves far from the position are old and ignored.
    pub(super) force_sync: bool,
    /// The blocks around the player acted this tick, so the base tick leaves them.
    intersected: bool,
    /// The death screen was shown for the current death (a player that joins dead sees it on
    /// its first tick).
    pub(super) death_shown: bool,
    /// Ticks since the player last stood on something.
    pub(super) in_air_ticks: u32,
    /// The client tick of the latest input this tick that toggled a movement state: the player's
    /// own entity data at the end of the tick carries it, as the official server's does (P6
    /// `sneak`), so the client applies it at that input's frame and keeps what it predicted since
    /// (bedrock-protocol-docs `PlayerMovementOverview`). Sent without it, the answer to a
    /// sneak the client has already ended turns the sneak on again.
    pub(super) answer_tick: Option<u64>,
}

impl Motion {
    /// Hashes the state kept between ticks (state hash).
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
            food,
            yaw,
            pitch,
            last_rotation,
            last_flags,
            last_position,
            processed,
            budget,
            force_sync,
            intersected,
            death_shown,
            in_air_ticks,
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
            food,
            force_sync,
            intersected,
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
            Some(p) => {
                for c in p {
                    h.u64(u64::from(*c));
                }
            }
            None => h.u64(u64::MAX),
        }
        match processed {
            Some(p) => {
                for c in [p.x, p.y, p.z] {
                    h.u64(c.to_bits());
                }
            }
            None => h.u64(u64::MAX),
        }
        h.u64(u64::from(budget.cast_unsigned()));
        h.u64(u64::from(*in_air_ticks));
        h.u64(answer_tick.unwrap_or(u64::MAX));
    }
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
            food: false,
            yaw: 0.0,
            pitch: 0.0,
            last_rotation: [0; 2],
            last_flags: FlagSet::new(),
            last_position: None,
            processed: None,
            budget: MOVE_START,
            force_sync: false,
            intersected: false,
            death_shown: false,
            in_air_ticks: 0,
            answer_tick: None,
        }
    }
}

checkpoint::fields!(Motion {
    sprinting,
    sneaking,
    sneak_pressed,
    swimming,
    gliding,
    flying,
    on_ground,
    food,
    yaw,
    pitch,
    last_rotation,
    last_flags,
    last_position,
    processed,
    budget,
    force_sync,
    intersected,
    death_shown,
    in_air_ticks,
    answer_tick,
});

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

/// Takes a `PlayerAuthInput`: rotation, input flags, then the movement; only from a spawned,
/// living player, so a client cannot place itself anywhere before it spawns. The tick keeps the
/// client's eye position exactly; the feet the rules use are rounded to 4 decimals
/// (`survival::feet`).
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
        turn(m, packet.yaw, packet.pitch);
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

/// Handles changed input flags: sneaking, sprinting, swimming, gliding, flying, a jump, a missed
/// swing. Each toggle that changes something asks its event (`PlayerToggleSneak` and the
/// others); a cancelled one leaves the state as it was and sends the player its entity data
/// again, as a refused flight does. The answer, accepted or refused, carries the input's client
/// tick `input_tick`.
fn input_flags(state: &mut State, session: SessionId, flags: FlagSet<InputFlag>, input_tick: u64) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
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
        mismatch |= !toggle(state, session, Toggle::Fly(v));
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

/// Whether a start/stop flag pair turns something on, off, or neither.
fn on_off(flags: FlagSet<InputFlag>, start: InputFlag, stop: InputFlag) -> Option<bool> {
    let (on, off) = (flags.contains(start), flags.contains(stop));
    (on != off).then_some(on)
}

/// Takes a `PlayerAction`: respawn, and the older toggles a client may still send, which ask
/// their events as the input flags do.
pub(super) fn on_player_action(state: &mut State, session: SessionId, packet: &PlayerAction) {
    let Ok(action) = Action::try_from(packet.action_type) else {
        return;
    };
    if action == Action::Respawn {
        damage::respawn(state, session, Trigger::Client, Stay::Online);
        return;
    }
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

/// Turns `toggle` on or off for a spawned, living player (one that [may fly](Survival::can_fly),
/// to start flying) unless its event's listener cancels it or leaves the player unable to; false
/// if refused.
fn toggle(state: &mut State, session: SessionId, toggle: Toggle) -> bool {
    let id = Id::new(session);
    let may = |state: &State| {
        state.players.get(&session).is_some_and(|p| {
            p.phase == Phase::Spawned
                && !p.survival.is_dead()
                && (toggle != Toggle::Fly(true) || p.survival.can_fly())
        })
    };
    if !may(state) {
        return false;
    }
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
            // A change of flight forgets the fall.
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

/// Makes a jump: the [`PlayerJump`] listeners hear it, then it costs 0.2 exhaustion sprinting,
/// 0.05 walking.
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

/// Handles a swing at nothing: unless a [`PlayerMissSwing`] listener cancels it, the no-damage
/// sound at the feet and the swing for the viewers.
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

/// Handles a movement of the eyes to `target`; the 15-block check only with `movement-check`. A
/// move into a chunk the server has not loaded, or into a block while the client lacks the
/// ground ([`enters_block`]), sends the player back.
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
    let world = state.worlds.get(player.world);
    let loaded = world.is_some_and(|w| w.is_chunk_loaded(BlockPos::containing(target).chunk()));
    let blocked = world.is_some_and(|w| enters_block(w, player, target));
    if (state.settings.movement_check && distance > MAX_MOVE_SQUARED) || !loaded || blocked {
        send_position(player, MoveMode::Reset);
        return;
    }
    if distance != 0.0 {
        move_to(state, session, target);
    }
}

/// Moves the player's eyes to `target`: the blocks it enters act, then the ground and fall state
/// update.
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
    let on_ground = is_on_ground(state, player, delta);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.motion.on_ground = on_ground;
    fall_state(state, session, delta.y, on_ground);
}

/// Whether the player stands on something after moving by `delta`: a box 0.2 above and below the
/// feet, stretched back along the move, touches a block's collision box. A move past the
/// 15-block limit, which only `movement-check = false` lets through, tests the box at its end
/// alone: stretched back along a move of millions of blocks, the box would make one input test
/// millions of blocks. A player without block collision never stands.
fn is_on_ground(state: &State, player: &Player, delta: Vec3) -> bool {
    if !player.survival.has_block_collision() {
        return false;
    }
    let Some(world) = state.worlds.get(player.world) else {
        return false;
    };
    let f = feet(player);
    let half = f64::from(attributes::size(&player.survival)[0]) / 2.0;
    let bb = Aabb::new(
        Vec3::new(f.x - half, f.y - GROUND_REACH, f.z - half),
        Vec3::new(f.x + half, f.y + GROUND_REACH, f.z + half),
    );
    if delta.length_squared() > MAX_MOVE_SQUARED {
        return has_collision(world, bb);
    }
    has_collision(world, bb.stretch(-delta))
}

/// Whether moving the player's eyes to `target` puts its body into a block, or past one on its
/// way down, that it is not in already, while its client lacks the ground after a teleport
/// ([`View::await_ground`](crate::tick::view::View::await_ground)). A client whose terrain has
/// not arrived (a respawn or teleport far away or into another world) falls into the ground
/// under it; Anchor does not simulate a player's movement, as the official server does to
/// correct such a prediction (P6), so it refuses the fall until the ground is sent, and without
/// that the player would stay a block low. Other moves are the client's: a check of every move
/// against the server's blocks would refuse moves the client makes before the server knows of
/// them (a block broken and stepped into in one input). A player without block collision enters
/// anything.
fn enters_block(world: &World, player: &Player, target: Vec3) -> bool {
    if !player.view.awaits_ground() || !player.survival.has_block_collision() {
        return false;
    }
    let inset = Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET);
    let body = |feet: Vec3| {
        let bb = bounding_box(&player.survival, feet);
        Aabb::new(bb.min + inset, bb.max - inset)
    };
    let (from, to) = (feet(player), feet_at(target));
    let at_end = body(to);
    // Stretched up to where it was, so a fall past a thin floor between two inputs counts.
    let top = crate::math::max(body(from).max.y, at_end.max.y);
    let path = Aabb::new(at_end.min, Vec3::new(at_end.max.x, top, at_end.max.z));
    collides(world, player, path) && !collides(world, player, body(from))
}

/// Whether a block's collision box overlaps `bb`, as the player's client is shown the blocks (fake
/// blocks included).
fn collides(world: &World, player: &Player, bb: Aabb) -> bool {
    // Boxes leave their cell only upward (fences, walls): the cells under the box count too.
    let lo = BlockPos::containing(bb.min - Vec3::new(0.0, 1.0, 0.0));
    let hi = BlockPos::containing(bb.max);
    (lo.x..=hi.x).any(|x| {
        (lo.z..=hi.z).any(|z| {
            (lo.y..=hi.y).any(|y| {
                let pos = BlockPos::new(x, y, z);
                let block = player.view.shown(pos).or_else(|| world.block(pos));
                block.is_some_and(|b| boxes_collide(b.collision_boxes(), pos, bb))
            })
        })
    })
}

/// Whether any loaded block's collision box overlaps `bb`.
fn has_collision(world: &World, bb: Aabb) -> bool {
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
    let at = pos.to_corner();
    boxes.iter().any(|b| b.offset(at).intersects(bb))
}

/// Updates the fall distance after a vertical move; landing hurts after the block landed on had
/// its say (bed blocks and hay soften, slime cancels). A player without block collision builds no
/// fall: its client flies, and a fall kept from then would hurt at the first landing once blocks
/// stop it again.
fn fall_state(state: &mut State, session: SessionId, dy: f64, on_ground: bool) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    if !s.has_block_collision() {
        s.fall_distance = 0.0;
        return;
    }
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

/// Lands the player: the block landed on has its say first (bed blocks and hay bales soften the fall,
/// slime cancels it), then the fall hurts with the long or short fall sound, or the landing makes
/// the block's land sound (always with `fall-damage = false`).
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
        // The block's network ID as the packet carries it: its hash's bits.
        let id = block.network_id().cast_signed();
        sound::of_player(state, session, SoundEvent::Land, id);
    }
}

/// Lets the blocks the player is inside act on it in the base tick, unless a move this tick had
/// them act.
pub(super) fn block_intersections(state: &mut State, session: SessionId) {
    if state
        .players
        .get(&session)
        .is_some_and(|p| !p.survival.motion.intersected)
    {
        intersect(state, session);
    }
}

/// Lets the blocks the player is inside act on it, on every move and in the base tick: the blocks
/// that hurt (`Harm`, as for entities), water and water cauldrons put out
/// fire, climbable blocks and cobwebs end a fall, wither roses wither.
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

/// Applies what one block does to a player inside it.
fn inside(state: &mut State, session: SessionId, pos: BlockPos, block: block::State) {
    let touch = Touch::of(block);
    if let Some(Touch::Harm(harm)) = touch {
        if harm.is_fall_ending() {
            reset_fall(state, session);
        }
        let spared = harm.is_spared_by_sneaking()
            && state.players.get(&session).is_none_or(|p| {
                p.survival.motion.sneaking
                    || damage::highest_armor_level(p, crate::item::Enchantment::FrostWalker) > 0
            });
        if !spared {
            let source = DamageSource::Block(pos);
            let cause = Damage::of(harm.cause(), source);
            damage::damage_by(state, session, harm.damage(), cause, source);
        }
        if let Some(seconds) = harm.burn_secs() {
            let source = CombustSource::Block(pos);
            damage::combust(state, session, u32::from(seconds), source);
        }
    } else if touch == Some(Touch::Water) {
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
    } else if touch == Some(Touch::WaterCauldron) {
        let Some(player) = state
            .players
            .get_mut(&session)
            .filter(|p| p.survival.fire_ticks > 0)
        else {
            return;
        };
        player.survival.fire_ticks = 0;
        player.data_dirty = true;
        damage::extinguished(state, session, ExtinguishCause::WaterCauldron);
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
    } else if touch == Some(Touch::EndsFall) {
        reset_fall(state, session);
    } else if block.is(Class::WitherRose) {
        let withered = state
            .players
            .get(&session)
            .is_none_or(|p| p.survival.effects.contains_key(&Effect::Wither));
        if !withered {
            effects::add(
                state,
                session,
                Effect::Wither,
                Active::new(WITHER_ROSE_EFFECT_TICKS, 0),
            );
        }
    }
}

/// Ends the player's fall.
fn reset_fall(state: &mut State, session: SessionId) {
    if let Some(player) = state.players.get_mut(&session) {
        player.reset_fall_distance();
    }
}

/// Processes the movement of a tick: the rate limit refills, the tick's move asks [`PlayerMove`]
/// (cancelled, or sent where no teleport may go, the player goes back to where the tick started;
/// sent elsewhere, it is teleported there), and a horizontal move costs 0.01 exhaustion per block
/// while sprinting (walking announces 0). A player beyond the rate limit is sent back to its
/// accepted position.
pub(super) fn process(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let now = feet(player);
    let m = &mut player.survival.motion;
    let exceeded = m.budget < 0;
    m.budget = (m.budget.max(0) + MOVES_PER_TICK).min(MAX_MOVE_BACKLOG);
    let from = m.processed.unwrap_or(now);
    let (dx, dz) = (now.x - from.x, now.z - from.z);
    if now.distance_squared(from) > MIN_MOVE_SQUARED {
        let world = player.world;
        let send_back = move |state: &mut State| {
            if let Some(player) = state.players.get_mut(&session) {
                player.position = super::eye_at(from);
                player.data_dirty = true;
                send_position(player, MoveMode::Reset);
            }
        };
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
            send_back(state);
            return;
        };
        if let Some(e) = &event
            && e.target.distance_squared(now) > MIN_RETARGET_SQUARED
        {
            let eye = super::eye_at(e.target);
            if super::check_destination(state, world, eye).is_err() {
                send_back(state);
                return;
            }
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

/// Sets the rotation to the finite `yaw` and `pitch` in degrees, each wrapped by the remainder of
/// 360 (above -360, below 360), then a negative yaw turned once more: [0, 360).
pub(super) fn turn(m: &mut Motion, yaw: f32, pitch: f32) {
    let yaw = f64::from(yaw) % 360.0;
    let turned = if yaw < 0.0 { yaw + 360.0 } else { yaw };
    // A negative yaw too small to change 360 rounds up to a full turn, which is 0.
    m.yaw = if turned < 360.0 { turned } else { 0.0 };
    m.pitch = f64::from(pitch) % 360.0;
}

/// Teleports the player's eyes to `eye`: the fall ends and the client is told.
pub(super) fn teleport(state: &mut State, session: SessionId, eye: Vec3) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.position = eye;
    player.data_dirty = true;
    player.reset_fall_distance();
    player.survival.motion.processed = Some(feet(player));
    player.view.await_ground(BlockPos::containing(feet(player)));
    send_position(player, MoveMode::Teleport);
}

/// Sends the player its own position; moves far from it are ignored until one arrives near it.
/// The last raw position is forgotten: the client's next input may report the position it had
/// before, now a move back from where it was sent.
pub(super) fn send_position(player: &mut Player, mode: MoveMode) {
    player.survival.motion.force_sync = true;
    player.survival.motion.last_position = None;
    let packet = position_packet(player, mode);
    player.send_packet(packet);
}

/// `MovePlayer` with the player's eye position and rotation in `mode`.
pub(super) fn position_packet(player: &Player, mode: MoveMode) -> Packet {
    let m = &player.survival.motion;
    let yaw = to_f32(m.yaw);
    Packet::MovePlayer(MovePlayer {
        entity_runtime_id: player.runtime_id,
        position: player.position.to_f32(),
        pitch: to_f32(m.pitch),
        yaw,
        head_yaw: yaw,
        mode: u8::from(mode),
        on_ground: m.on_ground,
        ridden_entity_runtime_id: 0,
        teleport_data: (mode == MoveMode::Teleport).then(TeleportData::default),
        tick: 0,
    })
}

/// Whether the player's eyes are under water: in a water block, below its surface.
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

/// Whether the player's eyes are inside an opaque solid block.
pub(super) fn is_inside_solid(state: &State, player: &Player) -> bool {
    state.worlds.get(player.world).is_some_and(|w| {
        let bounds = bounding_box(&player.survival, feet(player));
        entity::is_inside_solid(w, player.position, bounds)
    })
}

/// The player's bounding box with its feet at `feet`.
pub(super) fn bounding_box(s: &Survival, feet: Vec3) -> Aabb {
    let [width, height] = attributes::size(s);
    let half = f64::from(width) / 2.0;
    Aabb::new(
        Vec3::new(feet.x - half, feet.y, feet.z - half),
        Vec3::new(feet.x + half, feet.y + f64::from(height), feet.z + half),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::PropertyValue;
    use crate::tick::tests::{
        initialised, join, moved_to, new_state, radius, received, settings, tick,
    };

    thread_local! {
        /// What listeners and monitors wrote down; they run on the test's thread.
        static NOTES: std::cell::Cell<Vec<String>> = const { std::cell::Cell::new(Vec::new()) };
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
    fn the_look_direction_follows_the_yaw_and_the_pitch() {
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

    #[test]
    fn a_yaw_is_turned_into_zero_to_below_a_full_turn() {
        let mut motion = Motion::default();
        for (sent, turned) in [(-90.0, 270.0), (360.0, 0.0), (450.0, 90.0), (-1e-30, 0.0)] {
            turn(&mut motion, sent, 0.0);
            assert_eq!(motion.yaw, turned, "yaw {sent}");
        }
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
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = crate::tick::Output::default();
        let (j, rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        tick(&mut state, &mut vec![initialised(1, 2, 1)], &mut output);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.survival.motion.force_sync = false;
        (state, output, rx)
    }

    /// Every move lets the blocks it enters act before the landing: a fall into water counts from
    /// the water on, even when it lands on the tick's second move.
    #[test]
    fn a_fall_into_water_on_the_second_move_of_a_tick_does_not_hurt() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let water = block::Kind::Water.default_state();
        state.worlds[0]
            .set_block(BlockPos::new(0, -60, 0), water)
            .unwrap();
        let eye = |feet_y: f64| crate::tick::survival::eye_at(Vec3::new(0.5, feet_y, 0.5));
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

    /// A burning player in a water cauldron is put out and the cauldron keeps its level.
    #[test]
    fn a_water_cauldron_puts_out_a_burning_player_and_keeps_its_level() {
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
        player.position = crate::tick::survival::eye_at(Vec3::new(0.5, -60.0, 0.5));
        player.survival.fire_ticks = 100;
        crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.players[&session].survival.fire_ticks, 0);
        assert_eq!(state.worlds[0].block(pos), Some(level(6)));
    }

    /// A player without block collision never stands, so a fall through the floor does not land.
    #[test]
    fn a_player_without_block_collision_never_lands() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let eye = |feet_y: f64| crate::tick::survival::eye_at(Vec3::new(0.5, feet_y, 0.5));
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

    /// A player without block collision builds no fall while its client flies, so its first
    /// landing once blocks stop it again does not hurt.
    #[test]
    fn a_fall_without_block_collision_does_not_hurt_once_collision_is_back() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let eye = |feet_y: f64| crate::tick::survival::eye_at(Vec3::new(0.5, feet_y, 0.5));
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.set_block_collision(false);
        player.position = eye(-30.0);
        player.survival.motion.processed = Some(Vec3::new(0.5, -30.0, 0.5));
        let high = crate::tick::tests::moved_to(1, 10, eye(-40.0).to_f32());
        let mid = crate::tick::tests::moved_to(1, 11, eye(-50.0).to_f32());
        let low = crate::tick::tests::moved_to(1, 12, eye(-59.0).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![high, mid, low], &mut output);
        let player = state.players.get_mut(&session).unwrap();
        player.set_block_collision(true);
        let down = crate::tick::tests::moved_to(1, 13, eye(-60.0).to_f32());
        crate::tick::tests::tick(&mut state, &mut vec![down], &mut output);
        let player = &state.players[&session];
        assert!(player.survival.motion.on_ground);
        assert_eq!(player.survival.health, 20.0);
    }

    /// The land sound names the block landed on by its network ID's bits, as the packet carries
    /// every block ID, a hash above `i32::MAX` included.
    #[test]
    fn the_land_sound_names_a_block_hashed_above_i32_max_by_its_bits() {
        let (mut state, mut output, mut rx) = walker(crate::event::Listeners::default());
        let plain = [
            block::Kind::Stone,
            block::Kind::Dirt,
            block::Kind::Cobblestone,
            block::Kind::OakPlanks,
            block::Kind::Bricks,
            block::Kind::Bedrock,
            block::Kind::Obsidian,
            block::Kind::Netherrack,
            block::Kind::EndStone,
            block::Kind::Sandstone,
            block::Kind::Clay,
            block::Kind::Glowstone,
            block::Kind::MossyCobblestone,
            block::Kind::StoneBricks,
            block::Kind::Deepslate,
        ];
        let ground = plain
            .into_iter()
            .map(block::Kind::default_state)
            .find(|b| b.network_id() > i32::MAX.cast_unsigned())
            .expect("a plain block hashes above i32::MAX");
        state.worlds[0]
            .set_block(BlockPos::new(0, -61, 0), ground)
            .unwrap();
        let eye = |feet_y: f64| crate::tick::survival::eye_at(Vec3::new(0.5, feet_y, 0.5));
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        player.position = eye(-58.0);
        player.survival.motion.processed = Some(Vec3::new(0.5, -58.0, 0.5));
        received(&mut rx);
        tick(
            &mut state,
            &mut vec![moved_to(1, 10, eye(-60.0).to_f32())],
            &mut output,
        );
        let lands: Vec<i32> = received(&mut rx)
            .0
            .iter()
            .filter_map(|p| match p {
                Packet::LevelSoundEvent(s) if s.sound_type == SoundEvent::Land.as_str() => {
                    Some(s.extra_data)
                }
                _ => None,
            })
            .collect();
        assert_eq!(lands, [ground.network_id().cast_signed()]);
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

    /// A flight listener that takes the right to fly away refuses the flight as a cancel does:
    /// the second check asks whether the player may fly, as the first does.
    #[test]
    fn a_flight_whose_listener_takes_the_right_to_fly_does_not_start() {
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            |e: &mut PlayerToggleFlight, cx: &mut crate::tick::Context<'_>| {
                if let Some(p) = cx.player_mut(e.player()) {
                    p.set_can_fly(false);
                }
            },
        );
        l.push_monitor(|_: &PlayerToggleFlight, _: &crate::tick::Context<'_>| {
            note("flew".to_owned());
        });
        let (mut state, _output, _rx) = walker(l);
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.set_can_fly(true);
        let flags = [InputFlag::StartFlying].into_iter().collect();
        input_flags(&mut state, session, flags, 0);
        assert!(!state.players[&session].survival.motion.flying);
        assert!(notes().is_empty(), "no monitor sees a refused flight");
    }

    /// Whatever ends a fall restarts the ticks in the air: a player floating in water stays at
    /// none, and a teleport starts it again.
    #[test]
    fn what_ends_a_fall_restarts_the_ticks_in_the_air() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        let water = block::Kind::Water.default_state();
        state.worlds[0]
            .set_block(BlockPos::new(0, -60, 0), water)
            .unwrap();
        let session = SessionId::new(1);
        let player = state.players.get_mut(&session).unwrap();
        player.position = crate::tick::survival::eye_at(Vec3::new(0.5, -60.0, 0.5));
        player.survival.motion.on_ground = false;
        for _ in 0..3 {
            crate::tick::tests::tick(&mut state, &mut Vec::new(), &mut output);
        }
        assert_eq!(state.players[&session].in_air_ticks(), 0);
        let player = state.players.get_mut(&session).unwrap();
        player.survival.motion.in_air_ticks = 5;
        teleport(&mut state, session, Vec3::new(0.5, -40.0, 0.5));
        assert_eq!(state.players[&session].in_air_ticks(), 0);
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
    fn boots(state: &mut State, level: u16) {
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
        let eye = crate::tick::survival::eye_at(Vec3::new(x, -60.0, z)).to_f32();
        let input = crate::tick::tests::moved_to(1, 10, eye);
        crate::tick::tests::tick(state, &mut vec![input], output);
    }

    /// Frost Walker boots freeze no water: walking over a lake changes no block.
    #[test]
    fn frost_walker_boots_leave_the_water_they_walk_on() {
        let (mut state, mut output, _rx) = walker(crate::event::Listeners::default());
        lake(&mut state);
        boots(&mut state, 2);
        assert_eq!(
            damage::highest_armor_level(
                &state.players[&SessionId::new(1)],
                crate::item::Enchantment::FrostWalker
            ),
            2
        );
        walk(&mut state, &mut output, 5.5, 0.5);
        walk(&mut state, &mut output, 5.6, 0.5);
        let water = block::Kind::Water.default_state();
        for x in 0..=12 {
            for z in -6..=6 {
                assert_eq!(state.worlds[0].block(BlockPos::new(x, -61, z)), Some(water));
            }
        }
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

    /// The eye positions of the `MovePlayer` resets among `packets`.
    fn resets(packets: Vec<Packet>) -> Vec<[f32; 3]> {
        packets
            .into_iter()
            .filter_map(|p| match p {
                Packet::MovePlayer(m) if m.mode == u8::from(MoveMode::Reset) => Some(m.position),
                _ => None,
            })
            .collect()
    }

    /// A client teleported where its terrain has not arrived (a respawn far away or in another
    /// world) falls into the ground under it, or past a thin floor: the move is refused and the
    /// client is sent back on top, so it does not stay a block low.
    #[test]
    fn a_client_without_its_ground_is_sent_back_from_a_fall_into_it() {
        let (mut state, mut output, mut rx) = walker(crate::event::Listeners::default());
        let session = SessionId::new(1);
        for y in [-63, -62] {
            state.worlds[0]
                .set_block(BlockPos::new(0, y, 0), block::AIR)
                .unwrap();
        }
        let standing = crate::tick::survival::eye_at(Vec3::new(0.5, -60.0, 0.5));
        state.players.get_mut(&session).unwrap().position = standing;
        received(&mut rx);
        for (tick_number, sunk) in [(10, -60.08), (11, -60.31), (12, -63.0)] {
            let player = state.players.get_mut(&session).unwrap();
            player.survival.motion.force_sync = false;
            // A chunk the client was not sent, as after a teleport into another world; the view
            // phase drops it again, being out of the view.
            player.view.await_ground(BlockPos::new(1000, -60, 0));
            let eye = crate::tick::survival::eye_at(Vec3::new(0.5, sunk, 0.5)).to_f32();
            tick(
                &mut state,
                &mut vec![moved_to(1, tick_number, eye)],
                &mut output,
            );
            assert_eq!(state.players[&session].position, standing, "{sunk}");
            assert_eq!(resets(received(&mut rx).0), [standing.to_f32()], "{sunk}");
        }
    }

    /// A client that has its ground moves as it reports: into a block (one it broke in the same
    /// input, as the server learns after the move) and out of one.
    #[test]
    fn a_client_with_its_ground_moves_as_it_reports() {
        let (mut state, mut output, mut rx) = walker(crate::event::Listeners::default());
        let session = SessionId::new(1);
        received(&mut rx);
        for (tick_number, feet) in [
            (10, Vec3::new(0.5, -60.2, 0.5)),
            (11, Vec3::new(0.5, -60.0, 0.5)),
            (12, Vec3::new(1.5, -60.0, 0.5)),
        ] {
            let eye = crate::tick::survival::eye_at(feet).to_f32();
            tick(
                &mut state,
                &mut vec![moved_to(1, tick_number, eye)],
                &mut output,
            );
            assert_eq!(state.players[&session].position, Vec3::from_f32(eye));
        }
        assert!(resets(received(&mut rx).0).is_empty());
    }
}
