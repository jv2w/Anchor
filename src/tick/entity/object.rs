//! Paintings, end crystals and firework rockets (PMMP `entity\object\Painting`, `PaintingItem`,
//! `EndCrystal`, `item\EndCrystal`, `entity\object\FireworkRocket`, `item\FireworkRocket`).
//!
//! A painting hangs in one block on the wall behind its facing and covers its motive's width and
//! height from there (PMMP `getPaintingBB`, `canFit`). A placed painting picks among the largest
//! motives that fit (engine randomness), and falls when a neighbour update finds that it no
//! longer fits; at its death it drops its item (not when a player with infinite items broke it)
//! where it hung, with the planks particle. End crystals stand on obsidian or bedrock, and a hit
//! other than the void's makes their death an explosion of size 6 ([`super::explosion`]). A
//! firework rocket speeds up for its first 60 ticks and bursts when its flight time is up: the
//! burst shows its explosions and hurts the players and NPCs within 5 blocks that a ray reaches
//! (PMMP `FireworkRocket::explode`); rockets are never saved.
//!
//! Fixed PMMP defects: a painting whose blocks are in a chunk that is not loaded stays (PMMP
//! reads such blocks as air and drops it); a firework item's broken explosion compound is left
//! out instead of making the whole item unreadable.

use super::kind::{Crystal, Firework, Kind, MAX_EXPLOSIONS, Painting};
use super::{
    Blow, Cause, Hooks, Knock, Raise, SessionId, State, create, drop_item, hurt, physics,
    push_player, send_to, survival, targetable, with,
};
use crate::block::{self, Face};
use crate::entity::{Animation, Id, PaintingMotive};
use crate::event::{Actor, DamageSource};
use crate::item::{self, Stack};
use crate::math::{Aabb, Vec3};
use crate::nbt::{Compound, List, Value};
use crate::random::Random;
use crate::save::WorldIndex;
use crate::tick::interact::block_vec;
use crate::tick::interact::props::rotate_y;
use crate::world::{BlockPos, Particle, Sound, World};

/// How much of its block a painting leaves free in front: it is 1/16 thick (PMMP
/// `getPaintingBB`: `trim($facing, 15 / 16)`).
const PAINTING_TRIM: f64 = 15.0 / 16.0;
/// How far above the block it is placed on an end crystal stands.
const CRYSTAL_LIFT: f64 = 1.0;
/// Ticks a firework rocket speeds up (PMMP: "vanilla fireworks only live about 52 ticks").
const FIREWORK_BOOST_TICKS: u32 = 60;
/// The horizontal speed-up and the climb per tick of a firework rocket.
const FIREWORK_SPREAD: f64 = 0.15;
const FIREWORK_CLIMB: f64 = 0.04;
/// How far a burst reaches players and NPCs (PMMP: within 5 blocks).
const BURST_REACH: f64 = 5.0;
/// The largest flight multiplier of a firework item (PMMP `setFlightTimeMultiplier`: 1 to 127).
const MAX_FLIGHT_MULTIPLIER: u8 = 127;
/// The flight multiplier of a firework item without one (PMMP
/// `FireworkRocket::$flightTimeMultiplier`).
pub(super) const DEFAULT_FLIGHT_MULTIPLIER: u8 = 1;
/// A firework item's flight random part, 0 to this many ticks (PMMP `mt_rand(0, 12)`).
const FLIGHT_RANDOM_TICKS: i32 = 12;
/// The explosion shapes (PMMP `FireworkRocketTypeIdMap`: small ball 0, large ball 1, star 2,
/// creeper 3, burst 4) and the colours (dye IDs 0 to 15).
const LARGE_BALL: u8 = 1;
const MAX_SHAPE: u8 = 4;
const COLOURS: u8 = 16;
/// The item NBT keys (PMMP `FireworkRocket`, `FireworkRocketExplosion`).
pub(super) const TAG_FIREWORKS: &str = "Fireworks";
pub(super) const TAG_FLIGHT: &str = "Flight";
pub(super) const TAG_EXPLOSIONS: &str = "Explosions";
const TAG_TYPE: &str = "FireworkType";
const TAG_COLOURS: &str = "FireworkColor";
const TAG_FADE: &str = "FireworkFade";
const TAG_TWINKLE: &str = "FireworkFlicker";
const TAG_TRAIL: &str = "FireworkTrail";

/// What a use of an item that makes an entity did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Placed {
    /// Nothing: not this item's use here (no room, a wrong block or face); placement comes next.
    Nothing,
    /// The entity is made.
    Made,
    /// Refused: the world is at its entity limit (the player keeps the item).
    Full,
}

/// Whether a painting can face `face`: north, south, west or east.
pub(super) fn is_horizontal(face: Face) -> bool {
    !matches!(face, Face::Up | Face::Down)
}

/// The blocks a motive reaches to the clockwise side and down from the block it hangs in (PMMP
/// `ceil(size / 2) - 1`).
fn starts(motive: PaintingMotive) -> (i32, i32) {
    let start = |n: u8| (i32::from(n) + 1) / 2 - 1;
    (start(motive.width()), start(motive.height()))
}

/// The box of a painting of `motive` facing `facing` that hangs in the block `pos` (PMMP
/// `getPaintingBB` offset to the block).
pub(super) fn painting_box(pos: BlockPos, facing: Face, motive: PaintingMotive) -> Aabb {
    let (horizontal, vertical) = starts(motive);
    let (width, height) = (i32::from(motive.width()), i32::from(motive.height()));
    let mut min = Vec3::ZERO;
    let mut max = Vec3::new(1.0, 1.0, 1.0);
    let mut extend = |face: Face, d: f64| match face {
        Face::Down => min.y -= d,
        Face::Up => max.y += d,
        Face::North => min.z -= d,
        Face::South => max.z += d,
        Face::West => min.x -= d,
        Face::East => max.x += d,
    };
    extend(facing, -PAINTING_TRIM);
    extend(rotate_y(facing, true), f64::from(horizontal));
    extend(
        rotate_y(facing, true).opposite(),
        f64::from(width - 1 - horizontal),
    );
    extend(Face::Down, f64::from(vertical));
    extend(Face::Up, f64::from(height - 1 - vertical));
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    Aabb::new(min + at, max + at)
}

/// Whether a painting of `motive` facing `facing` fits from the block `pos` (PMMP
/// `Painting::canFit` without the overlap check): each block it covers is not solid and has a
/// solid block behind it. `None` when that cannot be told: no block fails, and one of them is not
/// loaded or outside the world.
pub(super) fn fits(
    world: &World,
    pos: BlockPos,
    facing: Face,
    motive: PaintingMotive,
) -> Option<bool> {
    if !is_horizontal(facing) {
        return Some(false);
    }
    let (horizontal, vertical) = starts(motive);
    let (cx, _, cz) = rotate_y(facing, true).offset();
    let (bx, _, bz) = facing.opposite().offset();
    let start = pos.offset(cx * horizontal, -vertical, cz * horizontal)?;
    let mut known = true;
    for w in 0..i32::from(motive.width()) {
        for h in 0..i32::from(motive.height()) {
            // The painting runs counter-clockwise from its start (PMMP `rotateY($facing, false)`).
            let at = start.offset(-cx * w, h, -cz * w);
            let block = at.and_then(|p| world.block(p));
            let behind = at
                .and_then(|p| p.offset(bx, 0, bz))
                .and_then(|p| world.block(p));
            match (block, behind) {
                (Some(block), Some(behind)) => {
                    if block.is_solid() || !behind.is_solid() {
                        return Some(false);
                    }
                }
                _ => known = false,
            }
        }
    }
    known.then_some(true)
}

/// Whether another painting's box meets the box a painting of `motive` would have there (PMMP
/// `canFit`'s overlap check).
fn overlaps_painting(
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
        && !overlaps_painting(state, world, pos, facing, motive)
}

/// The facing byte of a painting (PMMP `Painting::FACING_TO_DATA`).
pub(super) fn facing_data(facing: Face) -> u8 {
    match facing {
        Face::South => 0,
        Face::West => 1,
        Face::East => 3,
        Face::North | Face::Up | Face::Down => 2,
    }
}

/// The facing of a painting's byte (PMMP `DATA_TO_FACING`; north for an unknown one, as PMMP).
pub(super) fn data_facing(data: u8) -> Face {
    match data {
        0 => Face::South,
        1 => Face::West,
        3 => Face::East,
        _ => Face::North,
    }
}

/// A new painting's kind.
pub(super) fn painting(facing: Face, motive: PaintingMotive) -> Kind {
    Kind::Painting(Painting {
        motive,
        facing,
        drops: true,
    })
}

/// Hangs a painting on the `face` side of the block `clicked` (PMMP
/// `PaintingItem::onInteractBlock`): on a side, one of the largest motives that fit (picked with
/// `random`), with the place sound. The caller uses up the item when it is made.
pub(super) fn place_painting(
    state: &mut State,
    world: WorldIndex,
    clicked: BlockPos,
    face: Face,
    random: &mut Random,
) -> Placed {
    let Some(pos) = clicked.step(face).filter(|_| is_horizontal(face)) else {
        return Placed::Nothing;
    };
    let mut best = 0;
    let mut picks = Vec::new();
    for &motive in PaintingMotive::all() {
        let size = motive.width() + motive.height();
        if size < best || !can_hang(state, world, pos, face, motive) {
            continue;
        }
        if size > best {
            best = size;
            picks.clear();
        }
        picks.push(motive);
    }
    let last = i32::try_from(picks.len())
        .unwrap_or(i32::MAX)
        .saturating_sub(1);
    let Some(&motive) = usize::try_from(random.range(0, last))
        .ok()
        .and_then(|i| picks.get(i))
    else {
        return Placed::Nothing;
    };
    let made = create(state, world, block_vec(pos), painting(face, motive), |_| {
        (Vec3::ZERO, 0.0, 0.0)
    });
    if made.is_none() {
        return Placed::Full;
    }
    let centre = block_vec(pos) + Vec3::new(0.5, 0.5, 0.5);
    super::play(state, world, centre, Sound::PaintingPlace);
    Placed::Made
}

/// A neighbour update reached the painting `id` (PMMP `Painting::onNearbyBlockChange`): it dies
/// when it no longer fits (its item drops at its removal).
pub(super) fn check_painting(state: &mut State, id: Id) {
    let Some(e) = state.rules.entities.get(id).filter(|e| !e.flagged) else {
        return;
    };
    let Kind::Painting(p) = e.kind else {
        return;
    };
    let pos = BlockPos::containing(e.body.position);
    let fallen = state
        .worlds
        .get(e.world)
        .and_then(|w| fits(w, pos, p.facing, p.motive))
        == Some(false);
    if fallen {
        with(state, id, |e| e.health = 0.0);
        state.rules.entities.doom(id);
    }
}

/// A painting that died at `at` is removed (PMMP `Painting::onDeath`): it drops its item unless
/// a player with infinite items broke it, and shows oak planks breaking. At the world's entity
/// limit the item is lost (as an NPC's death drops are).
pub(super) fn painting_broke(state: &mut State, world: WorldIndex, at: Vec3, painting: Painting) {
    if painting.drops {
        let stack = Stack::new(item::Kind::Painting.default_type(), 1);
        if !drop_item(state, world, at, stack) {
            tracing::debug!(world, "a painting's item did not fit the world");
        }
    }
    let centre = at + Vec3::new(0.5, 0.5, 0.5);
    let planks = Particle::BlockBreak(block::Kind::OakPlanks.default_state());
    super::super::emit::particle(state, world, centre, planks);
}

/// A new end crystal's kind.
pub(super) fn crystal() -> Kind {
    Kind::EndCrystal(Crystal {
        show_base: false,
        beam_target: None,
        primed: false,
    })
}

/// Places an end crystal on the block `clicked` (PMMP `item\EndCrystal::onInteractBlock`): on
/// obsidian or bedrock with air in the two blocks above and no entity or player in them and the
/// block itself. The caller uses up the item when it is made.
pub(super) fn place_end_crystal(state: &mut State, world: WorldIndex, clicked: BlockPos) -> Placed {
    let Some(w) = state.worlds.get(world) else {
        return Placed::Nothing;
    };
    let base = w.block(clicked).map(block::State::kind);
    let air = |dy: i32| clicked.offset(0, dy, 0).and_then(|p| w.block(p)) == Some(block::AIR);
    if !matches!(base, Some(block::Kind::Obsidian | block::Kind::Bedrock)) || !air(1) || !air(2) {
        return Placed::Nothing;
    }
    let low = block_vec(clicked);
    let area = Aabb::new(low, low + Vec3::new(1.0, 2.0, 1.0));
    if super::anything_in(state, world, area) {
        return Placed::Nothing;
    }
    let at = low + Vec3::new(0.5, CRYSTAL_LIFT, 0.5);
    match create(state, world, at, crystal(), |_| (Vec3::ZERO, 0.0, 0.0)) {
        Some(_) => Placed::Made,
        None => Placed::Full,
    }
}

/// Whether `c` is a firework explosion compound PMMP reads (`FireworkRocketExplosion::
/// fromCompoundTag`): a known shape, at least one colour and only known colours.
fn is_explosion(c: &Compound) -> bool {
    let colours = |k: &str, required: bool| match c.get(k) {
        Some(Value::ByteArray(b)) => (!required || !b.is_empty()) && b.iter().all(|&d| d < COLOURS),
        None => !required,
        Some(_) => false,
    };
    let flag = |k: &str| matches!(c.get(k), None | Some(Value::Byte(_)));
    matches!(c.get(TAG_TYPE), Some(Value::Byte(t)) if *t <= MAX_SHAPE)
        && colours(TAG_COLOURS, true)
        && colours(TAG_FADE, false)
        && flag(TAG_TWINKLE)
        && flag(TAG_TRAIL)
}

/// `explosions` if each is a firework explosion and there are at most [`MAX_EXPLOSIONS`].
pub(super) fn checked_explosions(explosions: Vec<Compound>) -> Option<Vec<Compound>> {
    (explosions.len() <= MAX_EXPLOSIONS && explosions.iter().all(is_explosion))
        .then_some(explosions)
}

/// A firework item's flight multiplier (1 to 127) and explosions (PMMP `FireworkRocket::
/// deserializeCompoundTag`); an item without its `Fireworks` compound flies as the default one.
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
            .take(MAX_EXPLOSIONS)
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    (flight, explosions)
}

/// The flight multiplier a rocket without an item shows (an API rocket): the one whose flight
/// time, (multiplier + 1) × 10 ticks, comes nearest below its own (at least 1; the client only
/// draws it, the server decides the burst).
pub(super) fn flight_multiplier(max_flight: u32) -> u8 {
    let tens = (max_flight / 10).saturating_sub(1);
    u8::try_from(tens).map_or(MAX_FLIGHT_MULTIPLIER, |t| t.clamp(1, MAX_FLIGHT_MULTIPLIER))
}

/// A new firework rocket's kind; `flight` is the multiplier its item shows.
pub(super) fn firework(
    max_flight: u32,
    flight: u8,
    explosions: Vec<Compound>,
    owner: Option<(SessionId, u64)>,
) -> Kind {
    Kind::Firework(Firework {
        max_flight,
        explosions,
        owner: owner.map(|(s, _)| s),
        owner_runtime_id: owner.map_or(0, |(_, r)| r),
        flight,
    })
}

/// Launches the firework `item` that `session` used at `at` (PMMP `item\FireworkRocket::
/// onInteractBlock`) as [`launch`] does. False at the world's entity limit (the caller uses up
/// the item on true).
pub(super) fn launch_firework(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    at: Vec3,
    item: &Stack,
    random: &mut Random,
) -> bool {
    let (multiplier, explosions) = firework_item(item);
    let owner = state.players.get(&session).map(|p| (session, p.runtime_id));
    launch(state, world, at, (multiplier, explosions), owner, random).is_some()
}

/// Launches a firework rocket of flight multiplier and explosions `item` at `at` as a used
/// rocket item does: a flight of (multiplier + 1) × 10 ticks and up to 12 more, a slow start up
/// with a little random drift, looking straight up with a random yaw (all from `random`).
pub(super) fn launch(
    state: &mut State,
    world: WorldIndex,
    at: Vec3,
    (multiplier, explosions): (u8, Vec<Compound>),
    owner: Option<(SessionId, u64)>,
    random: &mut Random,
) -> Option<Id> {
    let flight =
        (u32::from(multiplier) + 1) * 10 + random.range(0, FLIGHT_RANDOM_TICKS).unsigned_abs();
    let yaw = super::random_yaw(random);
    let mut drift = || (random.unit() - random.unit()) * 0.0023;
    let motion = Vec3::new(drift(), 0.05, drift());
    let kind = firework(flight, multiplier, explosions, owner);
    create(state, world, at, kind, |_| (motion, yaw, 90.0))
}

/// A firework rocket's tick (PMMP `FireworkRocket::onFirstUpdate`, `entityBaseTick`): the launch
/// sound on its first tick, a speed-up for its first 60, and at its flight time it bursts and
/// goes.
pub(super) fn firework_tick(state: &mut State, id: Id, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id).filter(|e| !e.flagged) else {
        return;
    };
    let Kind::Firework(f) = &e.kind else {
        return;
    };
    let (world, at, age, max_flight) = (e.world, e.body.position, e.age, f.max_flight);
    if age == 1 {
        super::play(state, world, at, Sound::FireworkLaunch);
    }
    if age < FIREWORK_BOOST_TICKS {
        with(state, id, |e| {
            let m = e.body.motion;
            let push = Vec3::new(m.x * FIREWORK_SPREAD, FIREWORK_CLIMB, m.z * FIREWORK_SPREAD);
            e.body.motion = physics::bounded(m + push);
        });
    }
    if age >= max_flight {
        state.rules.entities.doom(id);
        burst(state, id, hooks);
    }
}

/// A firework rocket bursts (PMMP `FireworkRocket::explode`): the viewers see the burst event (as
/// BDS sends it, explosions or not: P6 `entities`); with explosions they hear each one (a large
/// ball louder, a twinkle crackling), and the players and NPCs within 5 blocks of it that a ray to
/// their feet or their middle reaches without a block take (explosions × 2 + 5) × √((5 − distance)
/// / 5) damage from it.
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
    let force = f64::from(u32::try_from(sounds.len()).unwrap_or(u32::MAX)) * 2.0 + 5.0;
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
                physics::clear(w, at, target)
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
    for (&s, p) in &state.players {
        let (feet, bounds) = (survival::feet(p), survival::bounding_box(p));
        if targetable(state, p, world, hooks)
            && bounds.overlaps(area, super::EPSILON)
            && reached(feet, bounds.max.y - bounds.min.y)
        {
            players.push((s, feet, damage_at(feet)));
        }
    }
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
        if (hooks.damage)(state, session, damage, survival::Damage::Explosion, by)
            && let Some(push) = Knock::new(feet.x - at.x, feet.z - at.z).applied_to(Vec3::ZERO)
        {
            push_player(state, session, push, Raise::Events);
        }
    }
}

#[cfg(test)]
mod tests;
