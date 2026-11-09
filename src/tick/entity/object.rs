//! Paintings, end crystals and firework rockets.
//!
//! A painting hangs in one block on the wall behind its facing and covers its motive's width and
//! height from there ([`can_hang`] tells whether one fits). It stays where it hangs when a block
//! around it changes. At its death it drops its item (not when a
//! player with infinite items broke it) where it hung, with the planks particle. An end crystal
//! that is hit other than by the void dies in an explosion of size 6 ([`super::explosion`]). A
//! firework rocket speeds up for its first 60 ticks and bursts when its flight time is up: the
//! burst shows its explosions and hurts the players and NPCs within 5 blocks that a ray reaches;
//! rockets are never saved.
//!
//! A painting whose blocks are in a chunk that is not loaded stays instead of being read as
//! air; a firework item's broken explosion compound is left out instead of making the whole item
//! unreadable.

use super::kind::{Crystal, Firework, Kind, Painting};
use super::{
    Blow, Cause, Hooks, Knock, Raise, SessionId, State, create, hurt, physics, push_player,
    send_to, survival, with,
};
use crate::block::{self, Face};
use crate::entity::{Animation, Id, PaintingMotive};
use crate::event::{Actor, DamageSource};
use crate::item::{self, Stack};
use crate::math::{Aabb, Vec3};
use crate::nbt::{Compound, List, Value};
use crate::random::Random;
use crate::save::WorldIndex;
use crate::tick::interact::props::rotate_clockwise;
use crate::tick::{block_entity, emit};
use crate::world::{BlockPos, Particle, Sound, World};

/// The damage of a burst at its center before its explosions add theirs.
const BURST_DAMAGE_BASE: f64 = 5.0;
/// The damage each explosion adds to a burst.
const BURST_DAMAGE_PER_EXPLOSION: f64 = 2.0;
/// How far a burst reaches players and NPCs, in blocks.
const BURST_REACH: f64 = 5.0;
/// The number of dye colors (dye IDs 0 to 15): a color list holds at most one entry per dye.
const COLORS: u8 = 16;
/// The flight multiplier of a firework item without one.
pub(super) const DEFAULT_FLIGHT_MULTIPLIER: u8 = 1;
/// The keys an explosion compound may hold: a key of another name is refused, so the compound
/// every viewer is sent stays small.
const EXPLOSION_KEYS: [&str; 5] = [TAG_COLORS, TAG_FADE, TAG_TRAIL, TAG_TWINKLE, TAG_TYPE];
/// Ticks a firework rocket speeds up.
const FIREWORK_BOOST_TICKS: u32 = 60;
/// The climb per tick of a firework rocket.
const FIREWORK_CLIMB: f64 = 0.04;
/// The horizontal speed-up per tick of a firework rocket.
const FIREWORK_SPREAD: f64 = 0.15;
/// A firework item's flight random part, 0 to this many ticks.
const FLIGHT_RANDOM_TICKS: i32 = 12;
/// The ticks one step of the flight multiplier adds: a rocket flies (multiplier + 1) steps.
const FLIGHT_STEP_TICKS: u32 = 10;
/// The shape of a large ball in the explosion shapes (small ball 0, large ball 1, star 2, creeper
/// 3, burst 4).
const LARGE_BALL: u8 = 1;
/// The upward speed of a launched rocket at its start.
const LAUNCH_CLIMB: f64 = 0.05;
/// The largest sideways speed a launched rocket starts with.
const LAUNCH_DRIFT: f64 = 0.0023;
/// The pitch of a launched rocket, in degrees: straight up.
const LAUNCH_PITCH_DEGREES: f64 = 90.0;
/// The largest flight multiplier of a firework item (1 to 127).
const MAX_FLIGHT_MULTIPLIER: u8 = 127;
/// The largest explosion shape.
const MAX_SHAPE: u8 = 4;
/// How much of its block a painting leaves free in front: it is 1/16 thick.
const PAINTING_TRIM: f64 = 15.0 / 16.0;
/// The item NBT key of an explosion's colors.
const TAG_COLORS: &str = "FireworkColor";
/// The item NBT key of the explosions.
pub(super) const TAG_EXPLOSIONS: &str = "Explosions";
/// The item NBT key of an explosion's fade colors.
const TAG_FADE: &str = "FireworkFade";
/// The item NBT key of the firework compound.
pub(super) const TAG_FIREWORKS: &str = "Fireworks";
/// The item NBT key of the flight multiplier.
pub(super) const TAG_FLIGHT: &str = "Flight";
/// The item NBT key of an explosion's trail.
const TAG_TRAIL: &str = "FireworkTrail";
/// The item NBT key of an explosion's twinkle.
const TAG_TWINKLE: &str = "FireworkFlicker";
/// The item NBT key of an explosion's shape.
const TAG_TYPE: &str = "FireworkType";

/// What a use of an item that makes an entity did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Placed {
    /// Nothing: not this item's use here.
    Nothing,
    /// The entity is made.
    Made,
    /// Refused: the world is at its entity limit (the player keeps the item).
    Full,
}

/// Ends a painting that died at `at`: it drops its item unless a player with infinite items broke
/// it, and shows oak planks breaking. At the world's entity limit the item waits as a spill.
pub(super) fn painting_broke(state: &mut State, world: WorldIndex, at: Vec3, painting: Painting) {
    if painting.drops {
        let stack = Stack::new(item::Kind::Painting.default_type(), 1);
        block_entity::drop_all(state, world, at, vec![stack]);
    }
    let center = at + Vec3::new(0.5, 0.5, 0.5);
    let planks = Particle::BlockBreak(block::Kind::OakPlanks.default_state());
    emit::particle(state, world, center, planks);
}

/// A new painting's kind.
pub(super) fn painting(facing: Face, motive: PaintingMotive) -> Kind {
    Kind::Painting(Painting {
        motive,
        facing,
        drops: true,
    })
}

/// Whether a painting of `motive` facing `facing` can be hung from the block `pos` now: it fits
/// and meets no other painting.
pub(super) fn can_hang(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    facing: Face,
    motive: PaintingMotive,
) -> bool {
    state
        .worlds
        .get(world)
        .and_then(|w| fits(w, pos, facing, motive))
        == Some(true)
        && !is_overlapping_painting(state, world, pos, facing, motive)
}

/// Whether another painting's box meets the box a painting of `motive` would have there.
fn is_overlapping_painting(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    facing: Face,
    motive: PaintingMotive,
) -> bool {
    let entities = &state.rules.entities;
    entities
        .nearby(world, painting_box(pos, facing, motive))
        .into_iter()
        .any(|id| {
            entities
                .get(id)
                .is_some_and(|e| matches!(e.kind, Kind::Painting(_)))
        })
}

/// Whether a painting of `motive` hung at `pos` facing `facing` has a free block in front of a
/// solid wall block for every block it covers (without the overlap check of [`can_hang`]), or
/// `None` when no block fails but some block cannot be read.
fn fits(world: &World, pos: BlockPos, facing: Face, motive: PaintingMotive) -> Option<bool> {
    if !is_horizontal(facing) {
        return Some(false);
    }
    let (clockwise_blocks, down_blocks) = starts(motive);
    let (clockwise_x, _, clockwise_z) = rotate_clockwise(facing).offset();
    let first = pos.offset(
        clockwise_x * clockwise_blocks,
        -down_blocks,
        clockwise_z * clockwise_blocks,
    )?;
    let wall = facing.opposite();
    let mut is_any_unread = false;
    for across in 0..i32::from(motive.width()) {
        for up in 0..i32::from(motive.height()) {
            // The cells run counterclockwise from the most clockwise one.
            let Some(cell) = first.offset(-clockwise_x * across, up, -clockwise_z * across) else {
                is_any_unread = true;
                continue;
            };
            let behind = cell.step(wall).and_then(|behind| world.block(behind));
            match (world.block(cell), behind) {
                (Some(front), Some(back)) => {
                    if front.is_solid() || !back.is_solid() {
                        return Some(false);
                    }
                }
                _ => is_any_unread = true,
            }
        }
    }
    if is_any_unread { None } else { Some(true) }
}

/// The box a painting of `motive` hung at `pos` facing `facing` takes up: 1/16 thick against the
/// wall and covering every block it hangs over.
pub(super) fn painting_box(pos: BlockPos, facing: Face, motive: PaintingMotive) -> Aabb {
    let (clockwise_blocks, down_blocks) = starts(motive);
    let (facing_x, facing_y, facing_z) = facing.offset();
    let (low_x, high_x) = wall_slab(facing_x);
    let (low_y, high_y) = wall_slab(facing_y);
    let (low_z, high_z) = wall_slab(facing_z);
    let corner = pos.to_corner();
    let clockwise = rotate_clockwise(facing);
    Aabb::new(
        corner + Vec3::new(low_x, low_y, low_z),
        corner + Vec3::new(high_x, high_y, high_z),
    )
    .stretch(toward(clockwise, clockwise_blocks))
    .stretch(toward(
        clockwise.opposite(),
        i32::from(motive.width()) - 1 - clockwise_blocks,
    ))
    .stretch(toward(Face::Down, down_blocks))
    .stretch(toward(
        Face::Up,
        i32::from(motive.height()) - 1 - down_blocks,
    ))
}

/// The span of one block cell along an axis whose facing component is `component`, trimmed to the
/// 1/16 against the wall on the facing axis.
fn wall_slab(component: i32) -> (f64, f64) {
    match component {
        ..0 => (PAINTING_TRIM, 1.0),
        0 => (0.0, 1.0),
        1.. => (0.0, 1.0 - PAINTING_TRIM),
    }
}

/// The vector of `blocks` blocks toward `face`.
fn toward(face: Face, blocks: i32) -> Vec3 {
    let (x, y, z) = face.offset();
    Vec3::new(
        f64::from(x * blocks),
        f64::from(y * blocks),
        f64::from(z * blocks),
    )
}

/// The blocks a painting of `motive` reaches past its hanging block toward the clockwise side and
/// downward, as (clockwise, down).
fn starts(motive: PaintingMotive) -> (i32, i32) {
    // Half the size rounded up, less the hanging block itself.
    let reach = |size: u8| (i32::from(size) + 1) / 2 - 1;
    (reach(motive.width()), reach(motive.height()))
}

/// Whether a painting can face `face`: north, south, west or east.
pub(super) fn is_horizontal(face: Face) -> bool {
    !matches!(face, Face::Down | Face::Up)
}

/// The facing byte of a painting.
pub(super) fn facing_data(facing: Face) -> u8 {
    match facing {
        Face::Down | Face::Up | Face::North => 2,
        Face::South => 0,
        Face::West => 1,
        Face::East => 3,
    }
}

/// The facing of a painting's byte; north for an unknown one.
pub(super) fn data_facing(data: u8) -> Face {
    match data {
        0 => Face::South,
        1 => Face::West,
        3 => Face::East,
        _ => Face::North,
    }
}

/// A new end crystal's kind.
pub(super) fn crystal() -> Kind {
    Kind::EndCrystal(Crystal {
        show_base: false,
        beam_target: None,
        primed: false,
    })
}

/// Launches the firework `item` that `session` used at `at` as [`launch`] does. False at the world's entity limit (the caller uses up
/// the item on true).
pub(super) fn launch_firework(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    at: Vec3,
    item: &Stack,
    random: &mut Random,
) -> bool {
    let owner = state.players.contains_key(&session).then_some(session);
    launch(state, world, at, firework_item(item), owner, random).is_some()
}

/// Launches a firework rocket of flight `multiplier` and `explosions` at `at` as a used rocket
/// item does: a flight of (multiplier + 1) × [`FLIGHT_STEP_TICKS`] ticks and up to
/// [`FLIGHT_RANDOM_TICKS`] more, a slow start up with a little random drift, looking straight up
/// with a random yaw (all from `random`).
pub(super) fn launch(
    state: &mut State,
    world: WorldIndex,
    at: Vec3,
    (multiplier, explosions): (u8, Vec<Compound>),
    owner: Option<SessionId>,
    random: &mut Random,
) -> Option<Id> {
    let flight = (u32::from(multiplier) + 1) * FLIGHT_STEP_TICKS
        + random.range(0, FLIGHT_RANDOM_TICKS).unsigned_abs();
    let yaw = super::random_yaw(random);
    let mut drift = || (random.unit() - random.unit()) * LAUNCH_DRIFT;
    let motion = Vec3::new(drift(), LAUNCH_CLIMB, drift());
    let kind = firework(flight, multiplier, explosions, owner);
    create(state, world, at, kind, |_| {
        (motion, yaw, LAUNCH_PITCH_DEGREES)
    })
}

/// Ticks a firework rocket: the launch sound on its first tick, a speed-up for its first 60, and
/// at its flight time it bursts and goes.
pub(super) fn firework_tick(state: &mut State, id: Id, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id).filter(|e| !e.flagged) else {
        return;
    };
    let Kind::Firework(f) = &e.kind else {
        return;
    };
    let (world, at, age_ticks, max_flight_ticks) =
        (e.world, e.body.position, e.age_ticks, f.max_flight);
    if age_ticks == 1 {
        super::play(state, world, at, Sound::FireworkLaunch);
    }
    if age_ticks < FIREWORK_BOOST_TICKS {
        with(state, id, |e| {
            let m = e.body.motion;
            let push = Vec3::new(m.x * FIREWORK_SPREAD, FIREWORK_CLIMB, m.z * FIREWORK_SPREAD);
            e.body.motion = physics::bounded(m + push);
        });
    }
    if age_ticks >= max_flight_ticks {
        state.rules.entities.doom(id);
        burst(state, id, hooks);
    }
}

/// Bursts the firework rocket `id`: the viewers see the burst event (the official server sends it,
/// explosions or not); with explosions they hear each one
/// (a large ball louder, a twinkle crackling), and the players and NPCs within 5 blocks of it
/// that a ray to their feet or their middle reaches without a block take (explosions × 2 + 5) ×
/// √((5 − distance) / 5) damage from it.
fn burst(state: &mut State, id: Id, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let Kind::Firework(f) = &e.kind else {
        return;
    };
    let (world, at, bounds, runtime_id) =
        (e.world, e.body.position, e.bounding_box(), e.runtime_id);
    let viewers = e.viewers.clone();
    let quiet = f.explosions.is_empty();
    let sounds: Vec<(Sound, bool)> = f
        .explosions
        .iter()
        .map(|c| {
            let large = matches!(c.get(TAG_TYPE), Some(Value::Byte(LARGE_BALL)));
            let twinkle = matches!(c.get(TAG_TWINKLE), Some(Value::Byte(b)) if *b != 0);
            let sound = if large {
                Sound::FireworkLargeExplosion
            } else {
                Sound::FireworkExplosion
            };
            (sound, twinkle)
        })
        .collect();
    let force = sounds.iter().fold(BURST_DAMAGE_BASE, |force, _| {
        force + BURST_DAMAGE_PER_EXPLOSION
    });
    send_to(
        &mut state.players,
        &viewers,
        &Animation::FireworkParticles.packet(runtime_id),
    );
    if quiet {
        return;
    }
    for (sound, twinkle) in sounds {
        super::play(state, world, at, sound);
        if twinkle {
            super::play(state, world, at, Sound::FireworkCrackle);
        }
    }
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let area = bounds.grow(Vec3::new(BURST_REACH, BURST_REACH, BURST_REACH));
    let reached = |feet: Vec3, height: f64| {
        feet.distance_squared(at) <= BURST_REACH * BURST_REACH
            && (0..2).any(|i| {
                let target = feet + Vec3::new(0.0, 0.5 * f64::from(i) * height, 0.0);
                physics::is_clear(w, at, target)
            })
    };
    let damage_at = |feet: Vec3| {
        force * ((BURST_REACH - feet.distance_squared(at).sqrt()) / BURST_REACH).sqrt()
    };
    let mut npcs = Vec::new();
    for other in state.rules.entities.nearby(world, area) {
        let Some(o) = state.rules.entities.get(other) else {
            continue;
        };
        let (_, height) = o.size();
        if other != id && o.kind.is_living() && reached(o.body.position, height) {
            npcs.push((other, o.body.position, damage_at(o.body.position)));
        }
    }
    let mut players = Vec::new();
    super::each_player_in(state, world, area, hooks, |s, p| {
        let (feet, bounds) = (survival::feet(p), survival::bounding_box(p));
        if reached(feet, bounds.max.y - bounds.min.y) {
            players.push((s, feet, damage_at(feet)));
        }
    });
    let by = DamageSource::Entity(Actor::Entity(id));
    for (other, feet, damage) in npcs {
        let knock = Knock::new(feet.x - at.x, feet.z - at.z);
        hurt(
            state,
            other,
            Blow::of(Cause::EntityExplosion, damage).by(by),
            Some(knock),
        );
    }
    for (session, feet, damage) in players {
        if let Some((force, vertical)) =
            (hooks.damage)(state, session, damage, survival::Damage::Explosion, by)
            && let Some(push) = (Knock {
                x: feet.x - at.x,
                z: feet.z - at.z,
                force,
                vertical,
            })
            .applied_to(Vec3::ZERO)
        {
            push_player(state, session, push, Raise::Events);
        }
    }
}

/// A firework item's flight multiplier (1 to 127) and explosions; an item without its `Fireworks` compound flies as the default one.
fn firework_item(stack: &Stack) -> (u8, Vec<Compound>) {
    let Some(Value::Compound(fireworks)) = stack.extra().and_then(|x| x.nbt.get(TAG_FIREWORKS))
    else {
        return (DEFAULT_FLIGHT_MULTIPLIER, Vec::new());
    };
    let flight = match fireworks.get(TAG_FLIGHT) {
        Some(Value::Byte(f)) => (*f).clamp(1, MAX_FLIGHT_MULTIPLIER),
        _ => DEFAULT_FLIGHT_MULTIPLIER,
    };
    let explosions = match fireworks.get(TAG_EXPLOSIONS) {
        Some(Value::List(List::Compound(list))) => list
            .iter()
            .filter(|c| is_explosion(c))
            .take(Firework::MAX_EXPLOSIONS)
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    (flight, explosions)
}

/// `explosions` if each is a firework explosion and there are at most [`Firework::MAX_EXPLOSIONS`].
pub(super) fn checked_explosions(explosions: Vec<Compound>) -> Option<Vec<Compound>> {
    (explosions.len() <= Firework::MAX_EXPLOSIONS && explosions.iter().all(is_explosion))
        .then_some(explosions)
}

/// Whether `c` is a firework explosion compound that keeps to Anchor's bounds: a known shape, at least one color, only known
/// colors (at most [`COLORS`] of each list) and no key but [`EXPLOSION_KEYS`].
fn is_explosion(c: &Compound) -> bool {
    let colors = |k: &str, required: bool| match c.get(k) {
        Some(Value::ByteArray(b)) => {
            b.len() <= usize::from(COLORS)
                && (!required || !b.is_empty())
                && b.iter().all(|&d| d < COLORS)
        }
        Some(_) => false,
        None => !required,
    };
    let flag = |k: &str| matches!(c.get(k), None | Some(Value::Byte(_)));
    c.keys().all(|k| EXPLOSION_KEYS.contains(&k.as_str()))
        && colors(TAG_COLORS, true)
        && colors(TAG_FADE, false)
        && flag(TAG_TRAIL)
        && flag(TAG_TWINKLE)
        && matches!(c.get(TAG_TYPE), Some(Value::Byte(t)) if *t <= MAX_SHAPE)
}

/// The flight multiplier a rocket without an item shows (an API rocket): the one whose flight
/// time, (multiplier + 1) × [`FLIGHT_STEP_TICKS`] ticks, comes nearest below its own (at least 1;
/// the client only draws it, the server decides the burst).
pub(super) fn flight_multiplier(max_flight_ticks: u32) -> u8 {
    let steps = (max_flight_ticks / FLIGHT_STEP_TICKS).saturating_sub(1);
    u8::try_from(steps).map_or(MAX_FLIGHT_MULTIPLIER, |s| s.clamp(1, MAX_FLIGHT_MULTIPLIER))
}

/// A new firework rocket's kind; `flight` is the multiplier its item shows.
pub(super) fn firework(
    max_flight_ticks: u32,
    flight: u8,
    explosions: Vec<Compound>,
    owner: Option<SessionId>,
) -> Kind {
    Kind::Firework(Firework {
        max_flight: max_flight_ticks,
        explosions,
        owner,
        flight,
    })
}

#[cfg(test)]
mod tests;
