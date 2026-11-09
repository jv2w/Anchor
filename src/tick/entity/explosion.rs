//! Primed TNT, end crystals' explosions and explosions.
//!
//! An explosion casts 1,352 rays from its center; each ray starts with the size × 0.7 to 1.3
//! (engine randomness) and loses force per 0.3-block step and per block by the block's blast
//! resistance; the blocks it still has force for are destroyed through
//! `interact::destroy_deferring_hooks` (drops with 1 / size chance, containers spill; TNT drops its
//! own item like any block, so explosions never light more TNT). Entities and players within twice the size are hurt and
//! pushed by their exposure (the share of rays from the center to points of their box no block
//! stops). With `fire`, a third of the destroyed blocks become fire where the block under them is
//! a full top; that fire is a plain block, which never spreads or burns out.
//!
//! Bounds: the size is at most [`MAX_EXPLOSION_SIZE`], so the block rays of one explosion look
//! at no more than 1,352 × 47 cells (the exposure rays to each entity and player hit, at most
//! 1,000 each by [`MAX_EXPOSURE_STEPS`], come on top);
//! primed TNT and end crystals that died explode while the tick has done fewer than
//! [`MAX_EXPLOSIONS_PER_TICK`] explosions and affected fewer than [`MAX_BLOCKS_PER_TICK`] blocks,
//! and wait otherwise (TNT with an empty fuse). A ray stops at a chunk that is not loaded and at a
//! state Anchor does not know, so an explosion never reaches past the loaded border. Clouds are
//! not hurt by explosions (their box can be 128 blocks wide).
//!
//! The custom blocks an explosion destroys run their `on_break` hooks as one step after the
//! explosion is over (its sound and monitors included), so a hook sees the whole explosion
//! applied. An explosion a hook starts is nested; past [`MAX_NESTED_EXPLOSIONS`] levels it does
//! nothing, so a chain of such blocks cannot overflow the tick thread's stack.
//!
//! Primed TNT and end crystals raise `EntityPreExplode` before they explode: its listeners may
//! cancel it, or change the size, the fire chance and whether blocks break. The explosion of an
//! entity then raises `EntityExplode` once its blocks are known. The exploding entity stays until
//! its explosion is over, so these listeners can look it up; it goes either way.

use std::ops::RangeInclusive;

use tracing::debug;

use super::kind::Kind;
use super::{
    Blow, Budget, Cause, Hooks, Knock, Raise, State, hurt, hurt_in, is_live, is_valid_explosion,
    level_event, physics, push_entity, survival, with,
};
use crate::block::{self, Face, SupportType};
use crate::entity::Id;
use crate::event::{
    Actor, DEFAULT_FIRE_CHANCE, DamageSource, EntityExplode, EntityPreExplode, MAX_EXPLOSION_SIZE,
};
use crate::item::Enchantment;
use crate::math::{self, Aabb, Vec3};
use crate::protocol::consts::LevelEvent;
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::{SessionId, dispatch, interact};
use crate::world::{self, BlockPos, Entry, MAX_Y, MIN_Y, World};

/// The share of a player's knockback that each level of Blast Protection removes.
const BLAST_PROTECTION_REDUCTION: f64 = 0.15;
/// The cost of a ray's step through any block, before the step length.
const BLOCK_BASE_COST: f64 = 0.3;
/// The size of an end crystal's explosion.
const CRYSTAL_SIZE: f64 = 6.0;
/// The damage per block of reach at full impact, before the base 1.
const DAMAGE_PER_REACH: f64 = 8.0;
/// Blocks the explosions of primed TNT and end crystals of one tick affect before the rest wait
/// (each explosion runs whole, so a tick may end a little over it); also the most blocks, and the
/// most ignitions, an explosion takes from its event's listeners.
pub(super) const MAX_BLOCKS_PER_TICK: usize = 8192;
/// Explosions of primed TNT and end crystals per tick; the rest wait for the next tick.
pub(super) const MAX_EXPLOSIONS_PER_TICK: usize = 64;
/// The most steps an exposure check takes along one side of a box: a side of up to 4 blocks is
/// sampled as densely as the game does, a longer one with wider spacing, so the rays to one
/// entity stay at most 10 per axis (1,000) whatever its size.
const MAX_EXPOSURE_STEPS: f64 = 9.0;
/// How many explosions may run inside each other's custom block hooks; a deeper one does nothing.
const MAX_NESTED_EXPLOSIONS: u8 = 8;
/// The least size of an explosion that looks for blocks to break.
const MIN_BREAKING_SIZE: f64 = 0.1;
/// Rays per edge of the cube of rays.
const RAYS: u32 = 16;
/// The random stream of the rays' strengths and the fires.
const RAYS_RANDOM: u64 = random::tag("explosion-rays");
/// The part of a block's blast resistance that a ray's step through it costs, before the step
/// length.
const RESISTANCE_DIVISOR: f64 = 5.0;
/// How far an entity may move in a tick (the speed bound of `physics::MAX_SPEED` per axis plus
/// one): entities this near an explosion move again from their new state.
const RESTEP_REACH: f64 = physics::MAX_SPEED + 1.0;
/// The step along a ray, in blocks.
const STEP: f64 = 0.3;
/// The share of a step's length that every step of a ray costs.
const STEP_DECAY: f64 = 0.75;
/// The strength of a ray, in thousandths of the size.
const STRENGTH_PER_MILLE_RANGE: RangeInclusive<i32> = 700..=1300;
/// The size of a TNT explosion.
const TNT_SIZE: f64 = 4.0;

/// What an explosion is: its size, its fire chance, and whether it breaks blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::tick) struct Blast {
    /// The size (radius), above 0; cut to [`MAX_EXPLOSION_SIZE`].
    pub(in crate::tick) size: f64,
    /// The chance, 0 to 1, that a destroyed block catches fire.
    pub(in crate::tick) fire_chance: f64,
    /// It breaks blocks.
    pub(in crate::tick) breaks: bool,
}

impl Blast {
    /// A block-breaking explosion of `size`, with the default fire chance when `fire`.
    pub(in crate::tick) fn new(size: f64, fire: bool) -> Self {
        Self {
            size,
            fire_chance: if fire { DEFAULT_FIRE_CHANCE } else { 0.0 },
            breaks: true,
        }
    }
}

/// What explodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    /// Nothing in particular: a direct call.
    None,
    /// An entity: excluded from the hurt, and the point living things are knocked back from.
    Entity(Id),
}

impl Source {
    /// What deals the hurt.
    fn damage_source(self) -> DamageSource {
        match self {
            Self::None => DamageSource::None,
            Self::Entity(id) => DamageSource::Entity(Actor::Entity(id)),
        }
    }

    /// The exploding entity.
    fn entity(self) -> Option<Id> {
        match self {
            Self::Entity(id) => Some(id),
            Self::None => None,
        }
    }
}

/// The blocks an explosion breaks and sets on fire: as its rays found them, or as the listeners
/// of its event left them.
enum Outcome {
    /// Nobody heard the event, or none was raised.
    Found {
        /// The blocks that break, in the order the rays found them.
        blocks: Vec<BlockPos>,
        /// The blocks that catch fire.
        ignitions: Vec<BlockPos>,
    },
    /// The listeners of [`EntityExplode`] saw it; kept for its monitors.
    Entity(EntityExplode),
}

impl Outcome {
    /// Raises [`EntityExplode`] for an entity's explosion, if heard, with the `blocks` and
    /// `ignitions` found and the default `drop_chance`; `None` when the listeners cancel it or
    /// the world is gone.
    fn ask(
        state: &mut State,
        world: WorldIndex,
        center: Vec3,
        source: Source,
        blocks: Vec<BlockPos>,
        ignitions: Vec<BlockPos>,
        drop_chance: f64,
    ) -> Option<Self> {
        let outcome = match source {
            Source::Entity(entity) if dispatch::heard::<EntityExplode>(state) => {
                let mut e = EntityExplode {
                    entity,
                    world: world::Id::of(world),
                    center,
                    blocks,
                    drop_chance,
                    ignitions,
                    cancelled: false,
                };
                dispatch::call(state, &mut e);
                Self::Entity(e)
            }
            _ => return Some(Self::Found { blocks, ignitions }),
        };
        let cancelled = match &outcome {
            Self::Found { .. } => false,
            Self::Entity(e) => e.cancelled,
        };
        (!cancelled && state.worlds.get(world).is_some()).then_some(outcome)
    }

    /// The blocks that break, at most [`MAX_BLOCKS_PER_TICK`].
    fn blocks(&self) -> &[BlockPos] {
        match self {
            Self::Found { blocks, .. } => capped(blocks),
            Self::Entity(e) => capped(&e.blocks),
        }
    }

    /// The blocks that catch fire, at most [`MAX_BLOCKS_PER_TICK`].
    fn ignitions(&self) -> &[BlockPos] {
        match self {
            Self::Found { ignitions, .. } => capped(ignitions),
            Self::Entity(e) => capped(&e.ignitions),
        }
    }

    /// Hands the event, if one was raised, to its monitors.
    fn watch(self, state: &mut State) {
        match self {
            Self::Found { .. } => {}
            Self::Entity(e) => dispatch::watch(state, &e),
        }
    }

    /// The drop chance the listeners left; `None` when no event was raised.
    fn drop_chance(&self) -> Option<f64> {
        match self {
            Self::Found { .. } => None,
            Self::Entity(e) => Some(e.drop_chance),
        }
    }
}

/// Burns down the fuse of the primed TNT `id`; at 0 it explodes if the tick's budget allows, else
/// it waits.
pub(super) fn tnt_tick(state: &mut State, id: Id, budget: &mut Budget, hooks: Hooks) {
    let burnt_out = with(state, id, |e| {
        let Kind::Tnt(t) = &mut e.kind else {
            return false;
        };
        t.fuse = t.fuse.saturating_sub(1);
        t.fuse == 0
    });
    if burnt_out != Some(true) || !has_budget(budget) {
        return;
    }
    blow_up(state, id, TNT_SIZE, budget, hooks);
}

/// Ends the end crystal `id`, at 0 health: a primed one explodes with size 6 at its position if
/// the tick's budget allows (else it waits), then it goes; one the void killed just goes.
pub(super) fn crystal_died(state: &mut State, id: Id, budget: &mut Budget, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    if !matches!(e.kind, Kind::EndCrystal(c) if c.primed) {
        state.rules.entities.doom(id);
        return;
    }
    if !has_budget(budget) {
        return;
    }
    blow_up(state, id, CRYSTAL_SIZE, budget, hooks);
}

/// Whether the explosions of primed TNT and end crystals this tick leave room for another.
fn has_budget(budget: &Budget) -> bool {
    budget.explosions < MAX_EXPLOSIONS_PER_TICK && budget.blocks < MAX_BLOCKS_PER_TICK
}

/// Explodes the entity `id` with `size` and removes it, counted in the tick's `budget`:
/// `EntityPreExplode` first, whose listeners may cancel it or change the size, the fire chance
/// and whether blocks break. It explodes where it is after the listeners: primed TNT half its
/// height above its position, an end crystal at its position. A listener that takes it away
/// cancels the explosion.
fn blow_up(state: &mut State, id: Id, size: f64, budget: &mut Budget, hooks: Hooks) {
    budget.explosions += 1;
    let mut blast = Blast::new(size, false);
    let mut event = None;
    if dispatch::heard::<EntityPreExplode>(state) {
        let mut e = EntityPreExplode {
            entity: id,
            size: blast.size,
            fire_chance: blast.fire_chance,
            block_breaking: blast.breaks,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled || (dispatch::listened::<EntityPreExplode>(state) && !is_live(state, id)) {
            state.rules.entities.doom(id);
            return;
        }
        blast = Blast {
            size: e.size,
            fire_chance: e.fire_chance,
            breaks: e.block_breaking,
        };
        event = Some(e);
    }
    let Some((world, center)) = state.rules.entities.get(id).map(|e| {
        let rise = if matches!(e.kind, Kind::Tnt(_)) {
            e.size().1 / 2.0
        } else {
            0.0
        };
        (e.world, e.body.position + Vec3::new(0.0, rise, 0.0))
    }) else {
        return;
    };
    let source = Source::Entity(id);
    let blocks = explode_with(state, world, center, blast, source, hooks, Raise::Events);
    budget.blocks = budget.blocks.saturating_add(blocks);
    state.rules.entities.doom(id);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Explodes as [`explode`] does, by `source` with the given hooks, and returns the number of
/// blocks it affected. An entity's explosion, called by the entity tick, makes the entities near
/// it move again from their new state.
///
/// With [`Raise::Events`], an explosion by an entity raises [`EntityExplode`] once the blocks are
/// known: cancelling stops it there (no block breaks, nothing is hurt), and the listeners' block
/// list (at most [`MAX_BLOCKS_PER_TICK`], unloaded blocks left alone), ignitions and drop chance
/// are used; the hits and pushes raise their own events.
///
/// The `on_break` hooks of the custom blocks it destroyed run last, as one step. An explosion
/// inside such a hook that is already [`MAX_NESTED_EXPLOSIONS`] levels deep does nothing and
/// affects 0 blocks.
///
/// [`explode`]: super::explode
pub(super) fn explode_with(
    state: &mut State,
    world: WorldIndex,
    center: Vec3,
    blast: Blast,
    source: Source,
    hooks: Hooks,
    raise: Raise,
) -> usize {
    let Some(w) = state.worlds.get(world) else {
        return 0;
    };
    if !is_valid_explosion(center, blast.size)
        || state.rules.entities.blast_depth >= MAX_NESTED_EXPLOSIONS
    {
        return 0;
    }
    let size = math::min(blast.size, MAX_EXPLOSION_SIZE);
    let mut random = Random::new(
        w.seed(),
        state.tick,
        random::key(
            RAYS_RANDOM,
            &[
                center.x.to_bits(),
                center.y.to_bits(),
                center.z.to_bits(),
                size.to_bits(),
            ],
        ),
    );
    let (blocks, ignitions) = if size < MIN_BREAKING_SIZE || !blast.breaks {
        (Vec::new(), Vec::new())
    } else {
        affected(w, center, size, blast.fire_chance, &mut random)
    };
    let drop_chance = math::min(1.0, 1.0 / size);
    let outcome = if raise == Raise::Events {
        let asked = Outcome::ask(state, world, center, source, blocks, ignitions, drop_chance);
        let Some(outcome) = asked else {
            return 0;
        };
        outcome
    } else {
        Outcome::Found { blocks, ignitions }
    };
    // The size whose 1 / size drop chance the blocks get: the chance as a size.
    let drop_size = match outcome.drop_chance() {
        // A chance of 0 is an infinite size: no drops.
        Some(chosen) if chosen != drop_chance => 1.0 / chosen,
        _ => size,
    };
    hit_entities(state, world, center, size, source, hooks, raise);
    if source.entity().is_some() {
        let reach = size * 2.0 + 1.0 + RESTEP_REACH;
        let area = Aabb::new(center, center).grow(Vec3::new(reach, reach, reach));
        let near = state.rules.entities.nearby(world, area);
        state.rules.entities.restep.extend(near);
    }
    let mut destroyed = Vec::new();
    let cause = interact::Destroy::Explosion {
        size: drop_size,
        raise,
    };
    for &pos in outcome.blocks() {
        if let Some(removed) = interact::destroy_deferring_hooks(state, world, pos, &cause) {
            destroyed.push(removed);
        }
    }
    let flame = block::Kind::Fire.default_state();
    for &pos in outcome.ignitions() {
        set_fire(state, world, pos, flame);
    }
    // The official server's explosion level event.
    level_event(state, world, center, LevelEvent::ParticlesExplosion, 0);
    let explode = crate::world::Sound::Explode;
    crate::tick::emit::sound_as(state, world, center, explode, raise);
    let broken = outcome.blocks().len();
    outcome.watch(state);
    state.rules.entities.blast_depth = state.rules.entities.blast_depth.saturating_add(1);
    for removed in destroyed {
        interact::run_break_hooks(state, world, removed);
    }
    state.rules.entities.blast_depth = state.rules.entities.blast_depth.saturating_sub(1);
    broken
}

/// The first [`MAX_BLOCKS_PER_TICK`] positions of `list`.
fn capped(list: &[BlockPos]) -> &[BlockPos] {
    list.get(..MAX_BLOCKS_PER_TICK).unwrap_or(list)
}

/// Sets a plain fire block at `pos` if it was destroyed and the block under it has a full top.
fn set_fire(state: &mut State, world: WorldIndex, pos: BlockPos, flame: block::State) {
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    let below = pos.step(Face::Down).and_then(|p| w.block(p));
    if w.block(pos) == Some(block::AIR)
        && below.is_some_and(|b| b.support(Face::Up) == SupportType::Full)
        && let Err(e) = w.set_block(pos, flame)
    {
        // The air stays; nothing depends on the fire.
        debug!(
            error = &e as &(dyn std::error::Error + 'static),
            "an explosion could not set a fire"
        );
    }
}

/// The blocks the explosion's rays from `center` break and, of those, the blocks to try to set on
/// fire.
///
/// Rays leave through the 1352 cells on the surface of a 16 by 16 by 16 grid, in x, then y, then
/// z order. Each ray starts with `size` times a random 0.7 to 1.3 and walks in steps of [`STEP`]
/// blocks, losing strength per step and per block it crosses. A ray ends at once in a chunk that
/// is not loaded or in a state Anchor does not know, so a blast never reaches past the loaded
/// edge. Steps outside the world height check no block. States with negative blast resistance,
/// such as air, are never listed.
///
/// The broken list holds each position once, in the order rays first reached it. The fire list is
/// a subset in the same order: each newly broken position draws one unit float when `fire_chance`
/// is above 0 and is listed when the draw is at most `fire_chance`. The function reads only `w`
/// and draws from `random` in a fixed order (one range per ray, then one unit per newly broken
/// position of that ray), so its result is deterministic.
fn affected(
    w: &World,
    center: Vec3,
    size: f64,
    fire_chance: f64,
    random: &mut Random,
) -> (Vec<BlockPos>, Vec<BlockPos>) {
    let last = RAYS - 1;
    let component = |n: u32| f64::from(n) / f64::from(last) * 2.0 - 1.0;
    let mut broken = Vec::new();
    let mut ignitions = Vec::new();
    let mut listed = std::collections::BTreeSet::new();
    for i in 0..RAYS {
        for j in 0..RAYS {
            for k in 0..RAYS {
                let on_surface = [i, j, k].iter().any(|&n| n == 0 || n == last);
                if !on_surface {
                    continue;
                }
                let (dx, dy, dz) = (component(i), component(j), component(k));
                // Summed and divided by hand to keep the exact float order the result depends on.
                let length = (dx * dx + dy * dy + dz * dz).sqrt();
                let step = Vec3::new(dx / length * STEP, dy / length * STEP, dz / length * STEP);
                let per_mille = random.range(
                    *STRENGTH_PER_MILLE_RANGE.start(),
                    *STRENGTH_PER_MILLE_RANGE.end(),
                );
                let mut strength = size * (f64::from(per_mille) / 1000.0);
                let mut point = center;
                let mut previous_cell = None;
                while strength > 0.0 {
                    let cell = BlockPos::containing(point);
                    point += step;
                    let entered = previous_cell != Some(cell);
                    previous_cell = Some(cell);
                    if (MIN_Y..=MAX_Y).contains(&cell.y) {
                        let Some(state) = w.entry(cell).and_then(Entry::state) else {
                            break;
                        };
                        let resistance = state.break_info().blast_resistance();
                        if resistance >= 0.0 {
                            strength -= (resistance / RESISTANCE_DIVISOR + BLOCK_BASE_COST) * STEP;
                            // A ray stays in one cell for several steps; only its first step
                            // there may list it.
                            if entered && strength > 0.0 && listed.insert(cell) {
                                broken.push(cell);
                                if fire_chance > 0.0 && random.unit() <= fire_chance {
                                    ignitions.push(cell);
                                }
                            }
                        }
                    }
                    strength -= STEP * STEP_DECAY;
                }
            }
        }
    }
    (broken, ignitions)
}

/// Hurts and pushes the entities and players within twice the size of the center: damage by the
/// impact (closeness × exposure), a push along the line from the center by the impact; living
/// things also take the knockback of a hit from an exploding entity.
fn hit_entities(
    state: &mut State,
    world: WorldIndex,
    center: Vec3,
    size: f64,
    source: Source,
    hooks: Hooks,
    raise: Raise,
) {
    let reach = size * 2.0;
    let area = Aabb::new(
        Vec3::new(
            (center.x - reach - 1.0).floor(),
            (center.y - reach - 1.0).floor(),
            (center.z - reach - 1.0).floor(),
        ),
        Vec3::new(
            (center.x + reach + 1.0).ceil(),
            (center.y + reach + 1.0).ceil(),
            (center.z + reach + 1.0).ceil(),
        ),
    );
    let from = source
        .entity()
        .and_then(|s| state.rules.entities.get(s))
        .map(|e| e.body.position);
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let mut hits: Vec<(Id, f64, Vec3)> = Vec::new();
    for id in state.rules.entities.nearby(world, area) {
        let Some(e) = state.rules.entities.get(id) else {
            continue;
        };
        if Some(id) == source.entity() || matches!(e.kind, Kind::Cloud(_)) {
            continue;
        }
        if let Some(impact) = impact(w, center, reach, e.body.position, e.bounding_box()) {
            hits.push((id, impact.0, impact.1));
        }
    }
    let mut players: Vec<(SessionId, f64, Vec3)> = Vec::new();
    super::each_player_in(state, world, area, hooks, |s, p| {
        let bounds = survival::bounding_box(p);
        if let Some(impact) = impact(w, center, reach, survival::feet(p), bounds) {
            players.push((s, impact.0, impact.1));
        }
    });
    let by = source.damage_source();
    // An entity's explosion and one of no entity are different causes.
    let (entity_cause, player_cause) = match source {
        Source::Entity(_) => (Cause::EntityExplosion, survival::Damage::Explosion),
        Source::None => (Cause::BlockExplosion, survival::Damage::BlockExplosion),
    };
    for (id, impact, direction) in hits {
        let damage = damage_of(impact, reach);
        let knock = from.and_then(|f| {
            let e = state.rules.entities.get(id)?;
            Some(Knock::new(e.body.position.x - f.x, e.body.position.z - f.z))
        });
        let blow = Blow::of(entity_cause, damage).by(by);
        hit_entity(state, id, blow, knock, direction * impact, raise);
    }
    for (session, impact, direction) in players {
        let damage = damage_of(impact, reach);
        let landed = match raise {
            Raise::Events => (hooks.damage)(state, session, damage, player_cause, by),
            Raise::Quiet => survival::damage_quiet(state, session, damage, player_cause)
                .then_some((super::KNOCKBACK, super::MAX_KNOCKBACK_VERTICAL)),
        };
        let mut push = direction * impact;
        if let Some((base, vertical)) = landed
            && let Some(f) = from
            && let Some(p) = state.players.get(&session)
        {
            let feet = survival::feet(p);
            let blast = survival::protection_level(p, Enchantment::BlastProtection);
            let force =
                base - math::min(base, base * f64::from(blast) * BLAST_PROTECTION_REDUCTION);
            let knock = Knock {
                x: feet.x - f.x,
                z: feet.z - f.z,
                force,
                vertical,
            };
            if let Some(m) = knock.applied_to(Vec3::ZERO) {
                push += m;
            }
        }
        super::push_player(state, session, push, raise);
    }
}

/// Applies an explosion's `blow` with `knock` to the entity `id`, then its `push`.
fn hit_entity(
    state: &mut State,
    id: Id,
    blow: Blow,
    knock: Option<Knock>,
    push: Vec3,
    raise: Raise,
) {
    match raise {
        Raise::Events => {
            hurt(state, id, blow, knock);
            if let Some(motion) = state.rules.entities.get(id).map(|e| e.body.motion + push) {
                push_entity(state, id, motion);
            }
        }
        Raise::Quiet => {
            let State { rules, players, .. } = &mut *state;
            hurt_in(
                &mut rules.entities,
                players,
                id,
                blow.cause,
                blow.amount,
                knock,
            );
            with(state, id, |e| {
                e.body.motion = physics::bounded(e.body.motion + push);
            });
        }
    }
}

/// The impact on something at `position` with the box `bounds` and the direction it is pushed:
/// `None` beyond `reach`.
fn impact(
    w: &World,
    center: Vec3,
    reach: f64,
    position: Vec3,
    bounds: Aabb,
) -> Option<(f64, Vec3)> {
    let distance = position.distance_squared(center).sqrt() / reach;
    if distance.is_nan() || distance > 1.0 {
        return None;
    }
    let direction = (position - center).normalize();
    Some(((1.0 - distance) * exposure(w, center, bounds), direction))
}

/// The damage of an impact (0 to 1) at a reach (twice the explosion size).
///
/// The damage is ⌊((i² + i) / 2) · 8 · reach + 1⌋ for impact `i`, so it is always at least 1.
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: impact is 0 to 1 and reach at most 16, so the value fits"
)]
fn damage_of(impact: f64, reach: f64) -> f64 {
    let raw = f64::midpoint(impact * impact, impact) * DAMAGE_PER_REACH * reach + 1.0;
    // The value is at least 1, so truncation toward zero is the floor.
    f64::from(raw as i32)
}

/// The share of points spread over `bounds` that a ray from `origin` reaches without a block in
/// the way; blocks of chunks that are not loaded stop rays.
fn exposure(w: &World, origin: Vec3, bounds: Aabb) -> f64 {
    let size = bounds.max - bounds.min;
    let diff = Vec3::new(
        exposure_steps(size.x),
        exposure_steps(size.y),
        exposure_steps(size.z),
    );
    let step = Vec3::new(1.0 / diff.x, 1.0 / diff.y, 1.0 / diff.z);
    let x_offset = (1.0 - diff.x.floor() / diff.x) / 2.0;
    let z_offset = (1.0 - diff.z.floor() / diff.z) / 2.0;
    let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
    let (mut checks, mut hits) = (0_u32, 0_u32);
    let mut x = 0.0;
    while x <= 1.0 {
        let mut y = 0.0;
        while y <= 1.0 {
            let mut z = 0.0;
            while z <= 1.0 {
                let point = Vec3::new(
                    lerp(x, bounds.min.x, bounds.max.x) + x_offset,
                    lerp(y, bounds.min.y, bounds.max.y),
                    lerp(z, bounds.min.z, bounds.max.z) + z_offset,
                );
                if physics::is_clear(w, origin, point) {
                    hits += 1;
                }
                checks += 1;
                z += step.z;
            }
            y += step.y;
        }
        x += step.x;
    }
    // Every loop starts at 0, so at least one point was checked.
    f64::from(hits) / f64::from(checks)
}

/// The steps an exposure check takes along a box side of `side` blocks: 2 × side + 1, at most
/// [`MAX_EXPOSURE_STEPS`].
fn exposure_steps(side: f64) -> f64 {
    math::min(side * 2.0 + 1.0, MAX_EXPOSURE_STEPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::Class;
    use crate::protocol::packet::Packet;
    use crate::tick::entity::create;
    use crate::tick::entity::kind::{Character, Form, Tnt};
    use crate::tick::entity::physics::tests::named;
    use crate::tick::entity::tests::{Harness, TEST_HOOKS, calls, standing};

    thread_local! {
        /// Whether the explosion listeners of a test found the exploding entity.
        static FOUND: std::cell::Cell<Vec<bool>> = const { std::cell::Cell::new(Vec::new()) };
    }

    /// Blocks destroyed by `tnt_in_stone_is_a_fixed_scene_…` (engine randomness: a change of
    /// the rays or their random stream changes it).
    const GOLDEN_STONE: usize = 26;

    /// Sets every block of the box `from..=to` to `name`.
    fn fill(h: &mut Harness, from: BlockPos, to: BlockPos, name: &str) {
        let state = named(name);
        for x in from.x..=to.x {
            for y in from.y..=to.y {
                for z in from.z..=to.z {
                    h.state.worlds[0]
                        .set_block(BlockPos::new(x, y, z), state)
                        .unwrap();
                }
            }
        }
    }

    /// The positions in the box `from..=to` that hold air.
    fn air(h: &Harness, from: BlockPos, to: BlockPos) -> std::collections::BTreeSet<BlockPos> {
        let mut out = std::collections::BTreeSet::new();
        for x in from.x..=to.x {
            for y in from.y..=to.y {
                for z in from.z..=to.z {
                    let p = BlockPos::new(x, y, z);
                    if h.state.worlds[0].block(p) == Some(block::AIR) {
                        out.insert(p);
                    }
                }
            }
        }
        out
    }

    /// Explodes with size 4 at `center` without a source, with the test hooks.
    fn size_four(h: &mut Harness, center: Vec3) {
        let blast = Blast::new(4.0, false);
        explode_with(
            &mut h.state,
            0,
            center,
            blast,
            Source::None,
            TEST_HOOKS,
            Raise::Events,
        );
    }

    fn tnt_count(h: &Harness) -> usize {
        h.live()
            .iter()
            .filter(|e| matches!(e.kind, Kind::Tnt(_)) && !e.flagged)
            .count()
    }

    /// Runs entity ticks until no primed TNT is left (at most `limit`); the ticks it took.
    fn burn_out(h: &mut Harness, limit: u32) -> u32 {
        for tick in 1..=limit {
            h.entities();
            if tnt_count(h) == 0 {
                return tick;
            }
        }
        panic!("TNT still primed after {limit} ticks");
    }

    /// Primes TNT with a `fuse` of ticks in the cell of the block `pos`, which it clears.
    fn prime(h: &mut Harness, pos: BlockPos, fuse: u16) {
        h.state.worlds[0].set_block(pos, block::AIR).unwrap();
        let at = Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y),
            f64::from(pos.z) + 0.5,
        );
        let tnt = Kind::Tnt(Tnt {
            fuse,
            underwater: false,
        });
        let made = create(&mut h.state, 0, at, tnt, |r| {
            let turn = (r.unit() * 2.0 - 1.0) * std::f64::consts::PI * 2.0;
            let motion = Vec3::new(-math::sin(turn) * 0.02, 0.2, -math::cos(turn) * 0.02);
            (motion, 0.0, 0.0)
        });
        assert!(made.is_some());
    }

    /// A stone cube around one primed TNT; what the explosion destroyed, the items it dropped
    /// and the state hash.
    fn stone_scene(threads: usize) -> (std::collections::BTreeSet<BlockPos>, u64, u64) {
        let mut h = Harness::new(threads);
        h.spawn(1, standing(20.0, 20.0));
        let (from, to) = (BlockPos::new(-5, -55, -5), BlockPos::new(5, -45, 5));
        fill(&mut h, from, to, "minecraft:stone");
        let center = BlockPos::new(0, -50, 0);
        prime(&mut h, center, 80);
        assert_eq!(burn_out(&mut h, 200), 80, "the fuse: 80 ticks");
        let mut destroyed = air(&h, from, to);
        destroyed.remove(&center);
        (destroyed, h.ground(), h.state.hash())
    }

    #[test]
    fn tnt_in_stone_is_a_fixed_scene_on_one_or_many_threads() {
        let (destroyed, items, hash) = stone_scene(1);
        assert_eq!(stone_scene(4), (destroyed.clone(), items, hash));
        let center = Vec3::new(0.5, -49.51, 0.5);
        for side in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 0, 1),
            (0, 0, -1),
            (0, 1, 0),
            (0, -1, 0),
        ] {
            let p = BlockPos::new(side.0, -50 + side.1, side.2);
            assert!(
                destroyed.contains(&p),
                "the faces of the TNT's cell go: {p:?}"
            );
        }
        for p in &destroyed {
            assert!(
                p.to_center().distance_squared(center) < 3.5 * 3.5,
                "stone stops the rays: {p:?}"
            );
        }
        assert!(
            items <= destroyed.len() as u64,
            "at most one item per block"
        );
        assert_eq!(destroyed.len(), GOLDEN_STONE);
    }

    #[test]
    fn a_box_of_mixed_blast_resistance_keeps_its_strong_walls() {
        let mut h = Harness::new(2);
        h.spawn(1, standing(20.0, 20.0));
        // TNT at (0, -50, 0) with walls two blocks away: obsidian east, water west, glass north,
        // planks south, dirt above, stone below.
        let walls = [
            (
                BlockPos::new(2, -52, -2),
                BlockPos::new(2, -48, 2),
                "minecraft:obsidian",
            ),
            (
                BlockPos::new(-2, -52, -2),
                BlockPos::new(-2, -48, 2),
                "minecraft:water",
            ),
            (
                BlockPos::new(-1, -52, -2),
                BlockPos::new(1, -48, -2),
                "minecraft:glass",
            ),
            (
                BlockPos::new(-1, -52, 2),
                BlockPos::new(1, -48, 2),
                "minecraft:oak_planks",
            ),
            (
                BlockPos::new(-1, -48, -1),
                BlockPos::new(1, -48, 1),
                "minecraft:dirt",
            ),
            (
                BlockPos::new(-1, -52, -1),
                BlockPos::new(1, -52, 1),
                "minecraft:stone",
            ),
        ];
        for (from, to, name) in walls {
            fill(&mut h, from, to, name);
        }
        prime(&mut h, BlockPos::new(0, -50, 0), 1);
        burn_out(&mut h, 10);
        let gone = |h: &Harness, (from, to, _): (BlockPos, BlockPos, &str)| air(h, from, to).len();
        assert_eq!(gone(&h, walls[0]), 0, "obsidian (1,200 × 5) holds");
        for wall in &walls[2..] {
            assert!(gone(&h, *wall) > 0, "{} breaks", wall.2);
        }
        let water = named("minecraft:water");
        let (from, to, _) = walls[1];
        assert!(
            (from.y..=to.y).all(|y| h.state.worlds[0]
                .block(BlockPos::new(-2, y, 0))
                .is_some_and(|s| s.is(Class::Water) || s == water)),
            "water (500) is not destroyed"
        );
        // Glass (1.5) breaks deeper than stone would: the whole middle of its wall goes.
        assert!(air(&h, walls[2].0, walls[2].1).contains(&BlockPos::new(0, -50, -2)));
    }

    #[test]
    fn a_blast_breaks_the_tnt_it_reaches_without_lighting_it() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        // On the ground, three blocks apart.
        let line: Vec<BlockPos> = (0..6).map(|i| BlockPos::new(i * 3 - 8, -60, 0)).collect();
        for &p in &line {
            h.state.worlds[0]
                .set_block(p, named("minecraft:tnt"))
                .unwrap();
        }
        prime(&mut h, line[0], 1);
        let mut explosions = 0;
        for _ in 0..400_u32 {
            h.entities();
            explosions += h
                .take(1)
                .iter()
                .filter(|p| matches!(p, Packet::LevelEvent(e) if e.event_type == i32::from(LevelEvent::ParticlesExplosion)))
                .count();
            assert!(tnt_count(&h) <= 1, "no more TNT is lit");
        }
        assert_eq!(explosions, 1, "only the lit TNT went");
        let tnt_at = |p: BlockPos| h.state.worlds[0].block(p).is_some_and(|s| s.is(Class::Tnt));
        assert!(!tnt_at(line[1]), "the next TNT is broken like any block");
        assert!(tnt_at(line[5]), "TNT out of reach stays");
    }

    #[test]
    fn exposure_counts_the_rays_no_block_stops() {
        let mut h = Harness::new(1);
        let w = &h.state.worlds[0];
        let origin = Vec3::new(0.5, -40.0, 0.5);
        let open = Aabb::new(Vec3::new(4.2, -41.0, 0.2), Vec3::new(4.8, -39.1, 0.8));
        assert_eq!(exposure(w, origin, open), 1.0);
        fill(
            &mut h,
            BlockPos::new(2, -45, -3),
            BlockPos::new(2, -35, 3),
            "minecraft:obsidian",
        );
        let w = &h.state.worlds[0];
        assert_eq!(exposure(w, origin, open), 0.0);
        let mut h = Harness::new(1);
        fill(
            &mut h,
            BlockPos::new(2, -45, -3),
            BlockPos::new(2, -41, 3),
            "minecraft:obsidian",
        );
        let half = exposure(&h.state.worlds[0], origin, open);
        assert!(half > 0.0 && half < 1.0, "{half}");
        // The damage formula.
        assert_eq!(damage_of(1.0, 8.0), 65.0);
        assert_eq!(damage_of(0.25, 8.0), 11.0);
        assert_eq!(damage_of(0.0, 8.0), 1.0);
    }

    /// A box side of up to 4 blocks is sampled as densely as the game does; a longer one takes
    /// no more steps, so a 16-block box costs no more rays than a 4-block one.
    #[test]
    fn exposure_steps_stop_growing_past_a_four_block_side() {
        assert_eq!(exposure_steps(0.6), 0.6 * 2.0 + 1.0);
        assert_eq!(exposure_steps(4.0), 9.0);
        assert_eq!(exposure_steps(16.0), 9.0);
        let mut h = Harness::new(1);
        let origin = Vec3::new(0.5, -40.0, 0.5);
        let huge = Aabb::new(Vec3::new(4.0, -48.0, -8.0), Vec3::new(20.0, -32.0, 8.0));
        assert_eq!(exposure(&h.state.worlds[0], origin, huge), 1.0);
        fill(
            &mut h,
            BlockPos::new(2, -45, -5),
            BlockPos::new(2, -35, 5),
            "minecraft:obsidian",
        );
        assert_eq!(exposure(&h.state.worlds[0], origin, huge), 0.0);
    }

    #[test]
    fn explosions_hurt_and_push_npcs_and_players_by_their_exposure() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(5.0, 0.0));
        let npc = crate::tick::entity::npc::spawn(
            &mut h.state,
            0,
            Vec3::new(-5.5, -60.0, 0.5),
            crate::tick::entity::Npc::Human {
                name: "target".to_owned(),
                skin: None,
            },
        )
        .unwrap();
        let hidden = crate::tick::entity::npc::spawn(
            &mut h.state,
            0,
            Vec3::new(0.5, -60.0, -5.5),
            crate::tick::entity::Npc::Human {
                name: "hidden".to_owned(),
                skin: None,
            },
        )
        .unwrap();
        for id in [npc, hidden] {
            with(&mut h.state, id, |e| {
                if let Kind::Npc(c) = &mut e.kind {
                    c.invulnerable = false;
                }
            });
        }
        fill(
            &mut h,
            BlockPos::new(-2, -60, -3),
            BlockPos::new(2, -57, -3),
            "minecraft:obsidian",
        );
        h.entities();
        let center = Vec3::new(0.5, -59.0, 0.5);
        let expected = |h: &Harness, feet: Vec3, bounds: Aabb| {
            let d = feet.distance_squared(center).sqrt() / 8.0;
            damage_of(
                (1.0 - d) * exposure(&h.state.worlds[0], center, bounds),
                8.0,
            )
        };
        let e = h.state.rules.entities.get(npc).unwrap();
        let npc_damage = expected(&h, e.body.position, e.bounding_box());
        let e = h.state.rules.entities.get(hidden).unwrap();
        let hidden_damage = expected(&h, e.body.position, e.bounding_box());
        let p = &h.state.players[&SessionId::new(1)];
        let player_damage = expected(&h, survival::feet(p), survival::bounding_box(p));
        size_four(&mut h, center);
        assert_eq!(h.health(npc), 20.0 - npc_damage);
        assert_eq!(hidden_damage, 1.0, "behind obsidian: no ray reaches it");
        assert_eq!(h.health(hidden), 20.0 - hidden_damage);
        assert!(
            h.state.rules.entities.get(npc).unwrap().body.motion.x < 0.0,
            "pushed away"
        );
        // An explosion of no entity is a block explosion.
        let cause = crate::tick::survival::Damage::BlockExplosion;
        assert_eq!(calls(|c| c.damage.clone()), vec![(1, player_damage, cause)]);
        let me = h.runtime_id(1);
        let pushed = h.state.players[&SessionId::new(1)].outbox.iter().any(|p| {
            matches!(p,
            Packet::SetActorMotion(m) if m.entity_runtime_id == me && m.velocity[0] > 0.0)
        });
        assert!(pushed);
        // Invulnerable NPCs and items far away are not hurt; an item near is destroyed.
        assert!(crate::tick::entity::drop_item(
            &mut h.state,
            0,
            Vec3::new(1.5, -60.0, 0.5),
            crate::tick::entity::saved::tests::stack("minecraft:stick", 1)
        ));
        with(&mut h.state, hidden, |e| {
            if let Kind::Npc(c) = &mut e.kind {
                c.invulnerable = true;
            }
        });
        let before = h.health(hidden);
        size_four(&mut h, center);
        assert_eq!(h.health(hidden), before);
        let sticks = h
            .live()
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Item(i) if !e.flagged && i.stack.item_type().unwrap().name() == "minecraft:stick"))
            .count();
        assert_eq!(sticks, 0, "the item burst");
        assert!(matches!(
            &h.state.rules.entities.get(npc).map(|e| &e.kind),
            Some(Kind::Npc(Character {
                form: Form::Human { .. },
                ..
            }))
        ));
    }

    #[test]
    fn explosion_drops_come_only_from_destroyed_blocks() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        let (from, to) = (BlockPos::new(-4, -54, -4), BlockPos::new(4, -46, 4));
        fill(&mut h, from, to, "minecraft:dirt");
        let center = BlockPos::new(0, -50, 0);
        prime(&mut h, center, 1);
        burn_out(&mut h, 10);
        let mut destroyed = air(&h, from, to);
        destroyed.remove(&center);
        let items = h.ground();
        assert!(!destroyed.is_empty());
        assert!(items > 0, "a quarter drops (drop chance 1 / 4)");
        assert!(
            items <= destroyed.len() as u64,
            "{items} items from {} blocks",
            destroyed.len()
        );
        assert!(h.live().iter().all(|e| match &e.kind {
            Kind::Item(i) => i.stack.item_type().unwrap().name() == "minecraft:dirt",
            _ => true,
        }));
    }

    #[test]
    fn a_tick_explodes_at_most_its_budget_and_the_rest_wait() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        for i in 0..100_i32 {
            let at = Vec3::new(
                f64::from(i % 10) * 3.0 - 15.0,
                -40.0,
                f64::from(i / 10) * 3.0 - 15.0,
            );
            let tnt = Kind::Tnt(Tnt {
                fuse: 1,
                underwater: false,
            });
            assert!(create(&mut h.state, 0, at, tnt, |_| (Vec3::ZERO, 0.0, 0.0)).is_some());
        }
        h.entities();
        assert_eq!(tnt_count(&h), 100 - MAX_EXPLOSIONS_PER_TICK);
        h.entities();
        assert_eq!(tnt_count(&h), 0, "the rest go next tick");
    }

    /// An incendiary blast of the rules takes the fire chance the events' `set_incendiary` sets.
    #[test]
    fn incendiary_blasts_take_the_event_fire_chance() {
        let chance = |fire| Blast::new(4.0, fire).fire_chance.to_bits();
        assert_eq!(chance(true), DEFAULT_FIRE_CHANCE.to_bits());
        assert_eq!(chance(false), 0.0_f64.to_bits());
    }

    #[test]
    fn explosions_stop_at_chunks_that_are_not_loaded_and_can_set_fire() {
        let h = Harness::new(1);
        let w = &h.state.worlds[0];
        // Chunk 5 (x 80..) is not loaded around a center at the loaded edge (x 79).
        let edge = Vec3::new(79.5, -60.0, 0.5);
        assert!(w.block(BlockPos::new(80, -61, 0)).is_none());
        let mut r = Random::new(0, 0, 1);
        let (blocks, _) = affected(w, edge, 4.0, 0.0, &mut r);
        assert!(!blocks.is_empty());
        assert!(blocks.iter().all(|p| p.x < 80), "nothing past the border");
        // Fire: a third of the destroyed blocks with a full top under them burn.
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        explode_with(
            &mut h.state,
            0,
            Vec3::new(0.5, -60.0, 0.5),
            Blast::new(4.0, true),
            Source::None,
            TEST_HOOKS,
            Raise::Events,
        );
        let fire = named("minecraft:fire");
        let burning = air(&h, BlockPos::new(-5, -64, -5), BlockPos::new(5, -55, 5));
        let lit = (-5..=5)
            .flat_map(|x| {
                (-64..=-55).flat_map(move |y| (-5..=5).map(move |z| BlockPos::new(x, y, z)))
            })
            .filter(|&p| {
                h.state.worlds[0]
                    .block(p)
                    .is_some_and(|s| s.name() == fire.name())
            })
            .count();
        assert!(lit > 0, "some fire ({} air)", burning.len());
    }

    /// An explosion breaks a block without a dump type (moss) into air like any block, and it
    /// drops nothing, as the block has no item.
    #[test]
    fn explosions_break_blocks_without_a_dump_type_and_drop_nothing() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        let (from, to) = (BlockPos::new(-2, -55, -2), BlockPos::new(2, -50, 2));
        fill(&mut h, from, to, "minecraft:moss_block");
        let center = Vec3::new(0.5, -52.5, 0.5);
        size_four(&mut h, center);
        let gone = air(&h, from, to);
        assert!(
            gone.contains(&BlockPos::new(0, -53, 0)),
            "the cell of the blast"
        );
        assert!(gone.contains(&BlockPos::new(1, -53, 0)), "a neighbour");
        assert_eq!(h.ground(), 0, "no drops");
    }

    /// Records whether a listener found the exploding entity.
    fn found(cx: &crate::tick::Context<'_>, id: Id) {
        FOUND.with(|f| {
            let mut list = f.take();
            list.push(cx.entity(id).is_some());
            f.set(list);
        });
    }

    /// The listeners of `EntityPreExplode` and `EntityExplode` can look up the primed TNT that
    /// explodes, which is gone afterwards.
    #[test]
    fn explosion_listeners_can_look_up_the_exploding_entity() {
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            |e: &mut EntityPreExplode, cx: &mut crate::tick::Context<'_>| found(cx, e.entity),
        );
        l.push(
            crate::event::Priority::Normal,
            |e: &mut EntityExplode, cx: &mut crate::tick::Context<'_>| found(cx, e.entity),
        );
        let mut h = Harness::with_listeners(1, l);
        h.spawn(1, standing(20.0, 20.0));
        FOUND.with(std::cell::Cell::take);
        prime(&mut h, BlockPos::new(0, -50, 0), 1);
        burn_out(&mut h, 10);
        assert_eq!(FOUND.with(std::cell::Cell::take), [true, true]);
    }

    /// An explosion inside the hooks of a destroyed custom block does nothing past the nesting
    /// limit, and an explosion leaves the nesting depth as it found it.
    #[test]
    fn explosions_nested_past_the_limit_do_nothing_and_leave_the_depth_unchanged() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        let (from, to) = (BlockPos::new(-5, -55, -5), BlockPos::new(5, -45, 5));
        fill(&mut h, from, to, "minecraft:stone");
        let center = Vec3::new(0.5, -49.5, 0.5);
        h.state.rules.entities.blast_depth = MAX_NESTED_EXPLOSIONS;
        size_four(&mut h, center);
        assert!(air(&h, from, to).is_empty(), "too deep: nothing breaks");
        h.state.rules.entities.blast_depth = MAX_NESTED_EXPLOSIONS - 1;
        size_four(&mut h, center);
        assert!(!air(&h, from, to).is_empty());
        assert_eq!(
            h.state.rules.entities.blast_depth,
            MAX_NESTED_EXPLOSIONS - 1
        );
    }
}
