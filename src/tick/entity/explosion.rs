//! Primed TNT, end crystals' explosions and explosions (PMMP `TNT::ignite`, `PrimedTNT`,
//! `EndCrystal::onDeathUpdate`, `EndCrystal::explode`, `Explosion::explodeA`, `explodeB`,
//! `getExposure`).
//!
//! An explosion casts PMMP's 1,352 rays from its centre; each ray starts with the size × 0.7 to
//! 1.3 (engine randomness) and loses force per 0.3-block step and per block by the block's blast
//! resistance; the blocks it still has force for are destroyed through `interact::destroy`
//! (drops with 1 / size chance, containers spill) or, for TNT, lit with a 10 to 30 tick fuse.
//! Entities and players within twice the size are hurt and pushed by their exposure (the share
//! of rays from the centre to points of their box no block stops). With `fire`, a third of the
//! destroyed blocks become fire where the block under them is a full top.
//!
//! Bounds: the size is at most [`MAX_SIZE`], so one explosion looks at no more than 1,352 × 47
//! cells; primed TNT explodes while the tick has done fewer than [`MAX_EXPLOSIONS_PER_TICK`]
//! explosions and affected fewer than [`MAX_BLOCKS_PER_TICK`] blocks, and waits with an empty fuse
//! otherwise (an end crystal that died waits the same way). Rays stop at chunks that are not loaded
//! (PMMP skips such cells and goes on), so an explosion never reaches past the loaded border.
//! Clouds are not hurt by explosions (their box can be 128 blocks wide; PMMP hurts them).
//!
//! Primed TNT and end crystals raise `EntityPreExplode` before they explode (PMMP
//! `ExplosionPrimeEvent` is this event here): its listeners may cancel it, or change the radius, the fire chance and whether
//! blocks break. The explosion of an entity then raises `EntityExplode` once its blocks are known,
//! the explosion of a block (a respawn anchor, whose `BlockPreExplode` the interaction rules
//! raise) `BlockExplode`.

use super::super::{SessionId, dispatch, interact};
use super::kind::{Kind, MAX_FUSE, Tnt};
use super::{
    Blow, Budget, Cause, EYE_HEIGHT, Hooks, Knock, Raise, State, create, hurt, hurt_in,
    is_valid_position, level_event, level_of, physics, player_box, push_entity, targetable, with,
};
use crate::block::{self, Class, Face, SupportType};
use crate::entity::Id;
use crate::event::{
    Actor, BlockExplode, DEFAULT_FIRE_CHANCE, DamageSource, EntityExplode, EntityPreExplode,
};
use crate::item::Enchantment;
use crate::math::{self, Aabb, Vec3};
use crate::protocol::consts::LevelEvent;
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{self, BlockPos, MAX_Y, MIN_Y, World};

/// The size of a TNT explosion (PMMP `PrimedTNT::explode`).
const TNT_SIZE: f64 = 4.0;
/// The size of an end crystal's explosion (PMMP `EndCrystal::explode`).
const CRYSTAL_SIZE: f64 = 6.0;
/// The largest explosion size taken; larger ones are cut to it (PMMP has no limit).
const MAX_SIZE: f64 = 8.0;
/// Primed TNT explosions per tick; the rest wait for the next tick with an empty fuse.
pub(super) const MAX_EXPLOSIONS_PER_TICK: usize = 64;
/// Blocks the TNT explosions of one tick affect before the rest wait (each explosion runs
/// whole, so a tick may end a little over it).
pub(super) const MAX_BLOCKS_PER_TICK: usize = 8192;
/// Rays per edge of PMMP's cube of rays, and the step along a ray.
const RAYS: u32 = 16;
const STEP: f64 = 0.3;
/// The fuse of lit TNT that an explosion reaches (PMMP `explodeB`: `mt_rand(10, 30)`).
const CHAIN_FUSE: (i32, i32) = (10, 30);
/// How far above its position primed TNT explodes (half its height, PMMP `explode`).
const TNT_CENTRE: f64 = 0.49;
/// How far an entity may move in a tick (the speed bound of `physics::MAX_SPEED` per axis plus
/// one): entities this near an explosion move again from their new state.
const RESTEP_REACH: f64 = physics::MAX_SPEED + 1.0;
/// Random streams.
const RAYS_RANDOM: u64 = random::tag("explosion-rays");

/// Turns the TNT block at `pos` into primed TNT with `fuse` ticks (PMMP `TNT::ignite`), its
/// fuse sound with [`WorldSound`](crate::event::WorldSound) if `raise` says so. False at the
/// entity limit (the block stays) or if the block is not TNT.
#[doc(alias = "TNT::ignite")]
pub(super) fn light(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    fuse: u32,
    raise: Raise,
) -> bool {
    let Some(block) = state.worlds.get(world).and_then(|w| w.block(pos)) else {
        return false;
    };
    if !block.is(Class::Tnt) || !super::has_room(state, world, 1) {
        return false;
    }
    let tnt = Tnt {
        fuse: u16::try_from(fuse.min(u32::from(MAX_FUSE))).unwrap_or(MAX_FUSE),
        underwater: block.name() == "minecraft:underwater_tnt",
    };
    let Some(w) = state.worlds.get_mut(world) else {
        return false;
    };
    if w.set_block(pos, block::AIR).is_err() {
        return false;
    }
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    let made = create(state, world, at, Kind::Tnt(tnt), |r| {
        let turn = (r.unit() * 2.0 - 1.0) * std::f64::consts::PI * 2.0;
        let motion = Vec3::new(-math::sin(turn) * 0.02, 0.2, -math::cos(turn) * 0.02);
        (motion, 0.0, 0.0)
    });
    if made.is_none() {
        // Runtime IDs ran out: the block comes back.
        if let Some(w) = state.worlds.get_mut(world)
            && let Err(e) = w.set_block(pos, block)
        {
            tracing::warn!(?e, "TNT could not be put back");
        }
        return false;
    }
    let fused = crate::world::Sound::Ignite;
    super::super::emit::sound_as(state, world, at, fused, raise);
    true
}

/// The ticking primed TNT `id` (PMMP `PrimedTNT::entityBaseTick`): the fuse burns down; at 0 it
/// explodes if the tick's budget allows, else it waits.
pub(super) fn tnt_tick(state: &mut State, id: Id, budget: &mut Budget, hooks: Hooks) {
    let Some(due) = with(state, id, |e| {
        let Kind::Tnt(t) = &mut e.kind else {
            return None;
        };
        t.fuse = t.fuse.saturating_sub(1);
        (t.fuse == 0).then_some((e.world, e.body.position))
    })
    .flatten() else {
        return;
    };
    if budget.explosions >= MAX_EXPLOSIONS_PER_TICK || budget.blocks >= MAX_BLOCKS_PER_TICK {
        return;
    }
    let (world, position) = due;
    let center = position + Vec3::new(0.0, TNT_CENTRE, 0.0);
    blow_up(state, id, world, center, TNT_SIZE, budget, hooks);
}

/// The end crystal `id`, at 0 health, ends (PMMP `EndCrystal::onDeathUpdate`): a primed one
/// explodes with size 6 at its position if the tick's budget allows (else it waits), then it
/// goes; one the void killed just goes.
pub(super) fn crystal_died(state: &mut State, id: Id, budget: &mut Budget, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let (world, center) = (e.world, e.body.position);
    if !matches!(e.kind, Kind::EndCrystal(c) if c.primed) {
        state.rules.entities.doom(id);
        return;
    }
    if budget.explosions >= MAX_EXPLOSIONS_PER_TICK || budget.blocks >= MAX_BLOCKS_PER_TICK {
        return;
    }
    blow_up(state, id, world, center, CRYSTAL_SIZE, budget, hooks);
}

/// The explosive entity `id` goes and explodes with `size` at `center` (PMMP `PrimedTNT::explode`,
/// `EndCrystal::explode`), counted in the tick's `budget`: `EntityPreExplode` first, whose
/// listeners may cancel it or change the radius, the fire chance and whether blocks break.
fn blow_up(
    state: &mut State,
    id: Id,
    world: WorldIndex,
    center: Vec3,
    size: f64,
    budget: &mut Budget,
    hooks: Hooks,
) {
    with(state, id, |e| e.flagged = true);
    state.rules.entities.doomed.insert(id);
    budget.explosions += 1;
    let mut blast = Blast::new(size, false);
    let mut event = None;
    if dispatch::heard::<EntityPreExplode>(state) {
        let mut e = EntityPreExplode {
            entity: id,
            radius: blast.size,
            fire_chance: blast.fire_chance,
            block_breaking: blast.breaks,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return;
        }
        blast = Blast {
            size: e.radius,
            fire_chance: e.fire_chance,
            breaks: e.block_breaking,
        };
        event = Some(e);
    }
    let source = Source::Entity(id);
    let blocks = explode_with(state, world, center, blast, source, hooks, Raise::Events);
    budget.blocks = budget.blocks.saturating_add(blocks);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// What an explosion is (PMMP `Explosion`'s radius and fire chance, and whether `explodeA`
/// runs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::tick) struct Blast {
    /// The size (radius), above 0; cut to [`MAX_SIZE`].
    pub(in crate::tick) size: f64,
    /// The chance, 0 to 1, that a destroyed block catches fire.
    pub(in crate::tick) fire_chance: f64,
    /// It breaks blocks.
    pub(in crate::tick) breaks: bool,
}

impl Blast {
    /// A block-breaking explosion of `size`, with PMMP's default fire chance when `fire`.
    pub(in crate::tick) fn new(size: f64, fire: bool) -> Self {
        Self {
            size,
            fire_chance: if fire { DEFAULT_FIRE_CHANCE } else { 0.0 },
            breaks: true,
        }
    }
}

/// What explodes (PMMP `Explosion::$what`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    /// Nothing in particular: a direct call.
    None,
    /// An entity: excluded from the hurt, and the point living things are knocked back from.
    Entity(Id),
    /// The block that was at `pos`, already gone.
    Block {
        /// Where it was.
        pos: BlockPos,
        /// What it was.
        block: block::State,
    },
}

impl Source {
    /// The exploding entity.
    fn entity(self) -> Option<Id> {
        match self {
            Self::Entity(id) => Some(id),
            Self::None | Self::Block { .. } => None,
        }
    }

    /// What deals the hurt (PMMP `EntityDamageByEntityEvent`, `EntityDamageByBlockEvent`).
    fn damage_source(self) -> DamageSource {
        match self {
            Self::None => DamageSource::None,
            Self::Entity(id) => DamageSource::Entity(Actor::Entity(id)),
            Self::Block { pos, .. } => DamageSource::Block(pos),
        }
    }
}

/// [`explode`] by `source` with the given hooks; returns the number of blocks it affected. An
/// entity's, called by the entity tick, makes the entities near it move again from their new
/// state.
///
/// With [`Raise::Events`], an explosion by an entity raises [`EntityExplode`] and one by a block
/// [`BlockExplode`] once the blocks are known (PMMP `explodeB`): cancelling stops it there (no
/// block breaks, nothing is hurt), and the listeners' block list (at most
/// [`MAX_BLOCKS_PER_TICK`], unloaded blocks left alone), ignitions and yield are used; the hits
/// and pushes raise their own events.
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
    let size = blast.size;
    if !is_valid_position(center) || size.is_nan() || size <= 0.0 || !size.is_finite() {
        return 0;
    }
    let size = math::min(size, MAX_SIZE);
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
    let (mut blocks, mut ignitions) = if size < 0.1 || !blast.breaks {
        (Vec::new(), Vec::new())
    } else {
        affected(w, center, size, blast.fire_chance, &mut random)
    };
    // The size whose 1 / size drop chance the blocks get: the yield as a size.
    let mut drop_size = size;
    let mut event = None;
    if raise == Raise::Events {
        let blast = Told::ask(state, world, center, size, source, (blocks, ignitions));
        let Some(Asked {
            told,
            listed,
            yield_percent,
        }) = blast
        else {
            return 0;
        };
        (blocks, ignitions) = listed;
        if let Some(told) = told {
            if yield_percent != math::min(100.0, 100.0 / size) {
                // A yield of 0 is an infinite size: no drops.
                drop_size = 100.0 / yield_percent;
            }
            event = Some(told);
        }
    }
    hit_entities(state, world, center, size, source, hooks, raise);
    if source.entity().is_some() {
        let reach = size * 2.0 + 1.0 + RESTEP_REACH;
        let area = Aabb::new(center, center).grow(Vec3::new(reach, reach, reach));
        let near = state.rules.entities.nearby(world, area);
        state.rules.entities.restep.extend(near);
    }
    for &pos in &blocks {
        let Some(now) = state.worlds.get(world).and_then(|w| w.block(pos)) else {
            continue;
        };
        if now == block::AIR {
            continue;
        }
        if now.is(Class::Tnt) {
            let fuse = random.range(CHAIN_FUSE.0, CHAIN_FUSE.1);
            light(state, world, pos, fuse.unsigned_abs(), raise);
        } else {
            let cause = interact::Destroy::Explosion { size: drop_size };
            interact::destroy(state, world, pos, cause);
        }
    }
    let flame = block::Kind::Fire.default_state();
    for pos in ignitions {
        set_fire(state, world, pos, flame);
    }
    // BDS's explosion level event (PMMP's `HugeExplodeSeedParticle` is a numbered particle).
    level_event(state, world, center, LevelEvent::ParticlesExplosion, 0);
    let explode = crate::world::Sound::Explode;
    super::super::emit::sound_as(state, world, center, explode, raise);
    match event {
        Some(Told::Entity(e)) => dispatch::watch(state, &e),
        Some(Told::Block(e)) => dispatch::watch(state, &e),
        None => {}
    }
    blocks.len()
}

/// The blocks and ignitions a listener left, at most [`MAX_BLOCKS_PER_TICK`] of each.
fn listed(blocks: &[BlockPos], ignitions: &[BlockPos]) -> (Vec<BlockPos>, Vec<BlockPos>) {
    let first = |list: &[BlockPos]| list.iter().copied().take(MAX_BLOCKS_PER_TICK).collect();
    (first(blocks), first(ignitions))
}

/// An explosion event its listeners have seen, kept for its monitors.
enum Told {
    Entity(EntityExplode),
    Block(BlockExplode),
}

/// What the explosion goes on with after its event.
struct Asked {
    /// The event, if one was raised.
    told: Option<Told>,
    /// The blocks that break and catch fire.
    listed: (Vec<BlockPos>, Vec<BlockPos>),
    /// The drop chance in percent.
    yield_percent: f64,
}

impl Told {
    /// Raises [`EntityExplode`] for an entity's explosion or [`BlockExplode`] for a block's, if
    /// heard, with the `blocks` and ignitions found (PMMP `explodeB`): what the listeners leave,
    /// at most [`MAX_BLOCKS_PER_TICK`] of each; `None` when they cancel it or unload the world.
    fn ask(
        state: &mut State,
        world: WorldIndex,
        center: Vec3,
        size: f64,
        source: Source,
        blocks: (Vec<BlockPos>, Vec<BlockPos>),
    ) -> Option<Asked> {
        let yield_percent = math::min(100.0, 100.0 / size);
        let (blocks, ignitions) = blocks;
        let told = match source {
            Source::Entity(entity) if dispatch::heard::<EntityExplode>(state) => {
                let mut e = EntityExplode {
                    entity,
                    world: world::Id::of(world),
                    position: center,
                    blocks,
                    yield_percent,
                    ignitions,
                    cancelled: false,
                };
                dispatch::call(state, &mut e);
                Self::Entity(e)
            }
            Source::Block { pos, block } if dispatch::heard::<BlockExplode>(state) => {
                let mut e = BlockExplode {
                    world: world::Id::of(world),
                    pos,
                    block,
                    position: center,
                    blocks,
                    yield_percent,
                    ignitions,
                    cancelled: false,
                };
                dispatch::call(state, &mut e);
                Self::Block(e)
            }
            _ => {
                return Some(Asked {
                    told: None,
                    listed: (blocks, ignitions),
                    yield_percent,
                });
            }
        };
        let (cancelled, yield_percent, listed) = match &told {
            Self::Entity(e) => (
                e.cancelled,
                e.yield_percent,
                listed(&e.blocks, &e.ignitions),
            ),
            Self::Block(e) => (
                e.cancelled,
                e.yield_percent,
                listed(&e.blocks, &e.ignitions),
            ),
        };
        if cancelled || state.worlds.get(world).is_none() {
            return None;
        }
        Some(Asked {
            told: Some(told),
            listed,
            yield_percent,
        })
    }
}

/// Sets fire at `pos` if it was destroyed and the block under it has a full top (PMMP
/// `explodeB`).
pub(super) fn set_fire(state: &mut State, world: WorldIndex, pos: BlockPos, flame: block::State) {
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    let below = pos.step(Face::Down).and_then(|p| w.block(p));
    if w.block(pos) == Some(block::AIR)
        && below.is_some_and(|b| b.support(Face::Up) == SupportType::Full)
        && let Err(e) = w.set_block(pos, flame)
    {
        // The air stays; nothing depends on the fire.
        tracing::debug!(?e, "an explosion could not set a fire");
    }
}

/// The blocks the explosion destroys, in the order the rays find them, and those set on fire
/// (PMMP `explodeA`). Pure: reads the world only.
fn affected(
    w: &World,
    center: Vec3,
    size: f64,
    fire_chance: f64,
    random: &mut Random,
) -> (Vec<BlockPos>, Vec<BlockPos>) {
    let mut seen = std::collections::BTreeSet::new();
    let mut blocks = Vec::new();
    let mut ignitions = Vec::new();
    let last = f64::from(RAYS - 1);
    for i in 0..RAYS {
        for j in 0..RAYS {
            for k in 0..RAYS {
                let edge = |n: u32| n == 0 || n == RAYS - 1;
                if !(edge(i) || edge(j) || edge(k)) {
                    continue;
                }
                let shell = |n: u32| f64::from(n) / last * 2.0 - 1.0;
                let (x, y, z) = (shell(i), shell(j), shell(k));
                let length = (x * x + y * y + z * z).sqrt();
                let shift = Vec3::new(x / length * STEP, y / length * STEP, z / length * STEP);
                let mut pointer = center;
                let mut force = size * (f64::from(random.range(700, 1300)) / 1000.0);
                // A step is shorter than a block, so a ray stays in a cell for several steps. The
                // cell is read, and tried for the list, only at the first: a straight ray never
                // comes back to a cell it left, and it goes past that first step only with force
                // left, so the cell was already tried there.
                let mut cell: Option<(BlockPos, f64)> = None;
                while force > 0.0 {
                    let pos = BlockPos::containing(pointer);
                    pointer += shift;
                    if (MIN_Y..=MAX_Y).contains(&pos.y) {
                        let repeat = cell.filter(|&(at, _)| at == pos);
                        let resistance = match repeat {
                            Some((_, resistance)) => resistance,
                            None => {
                                let Some(state) = w.block(pos) else {
                                    // A chunk that is not loaded: the ray stops at the border.
                                    break;
                                };
                                let resistance = state.break_info().blast_resistance();
                                cell = Some((pos, resistance));
                                resistance
                            }
                        };
                        if resistance >= 0.0 {
                            force -= (resistance / 5.0 + 0.3) * STEP;
                            if force > 0.0 && repeat.is_none() && seen.insert(pos) {
                                blocks.push(pos);
                                if fire_chance > 0.0 && random.unit() <= fire_chance {
                                    ignitions.push(pos);
                                }
                            }
                        }
                    }
                    force -= STEP * 0.75;
                }
            }
        }
    }
    (blocks, ignitions)
}

/// Hurts and pushes the entities and players within twice the size of the centre (PMMP
/// `explodeB`): damage by the impact (closeness × exposure), a push along the line from the
/// centre by the impact; living things also take the knockback of a hit from an exploding
/// entity.
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
    for (&s, p) in &state.players {
        if !targetable(state, p, world, hooks) || !player_box(p).overlaps(area, super::EPSILON) {
            continue;
        }
        let feet = p.position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
        if let Some(impact) = impact(w, center, reach, feet, player_box(p)) {
            players.push((s, impact.0, impact.1));
        }
    }
    let by = source.damage_source();
    // PMMP: `CAUSE_ENTITY_EXPLOSION` for an entity's, `CAUSE_BLOCK_EXPLOSION` otherwise.
    let (entity_cause, player_cause) = match source {
        Source::Entity(_) => (Cause::EntityExplosion, super::survival::Damage::Explosion),
        Source::None | Source::Block { .. } => (
            Cause::BlockExplosion,
            super::survival::Damage::BlockExplosion,
        ),
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
        let cause = player_cause;
        let landed = match raise {
            Raise::Events => (hooks.damage)(state, session, damage, cause, by),
            Raise::Quiet => super::survival::damage_quiet(state, session, damage, cause),
        };
        let mut push = direction * impact;
        if landed
            && let Some(f) = from
            && let Some(p) = state.players.get(&session)
        {
            let feet = p.position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
            let blast = p
                .inventory
                .armor()
                .iter()
                .map(|s| level_of(s, Enchantment::BlastProtection))
                .max()
                .unwrap_or(0);
            let base = super::KNOCKBACK;
            let force = base - math::min(base, base * f64::from(blast) * 0.15);
            let knock = Knock {
                x: feet.x - f.x,
                z: feet.z - f.z,
                force,
                vertical: super::KNOCKBACK_VERTICAL_LIMIT,
            };
            if let Some(m) = knock.applied_to(Vec3::ZERO) {
                push += m;
            }
        }
        super::push_player(state, session, push, raise);
    }
}

/// An entity takes an explosion's `blow` with `knock`, then its `push` (PMMP `explodeB`).
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
/// `None` beyond `reach` (PMMP `explodeB`).
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

/// The damage of an impact (PMMP: `(int) (((impact² + impact) / 2) × 8 × size × 2 + 1)`).
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: impact is 0 to 1 and reach at most 16, so the value fits"
)]
fn damage_of(impact: f64, reach: f64) -> f64 {
    f64::from((f64::midpoint(impact * impact, impact) * 8.0 * reach + 1.0) as i32)
}

/// The share of points spread over `bounds` that a ray from `origin` reaches without a block in
/// the way (PMMP `getExposure`); blocks of chunks that are not loaded stop rays.
fn exposure(w: &World, origin: Vec3, bounds: Aabb) -> f64 {
    let size = bounds.max - bounds.min;
    let diff = Vec3::new(size.x * 2.0 + 1.0, size.y * 2.0 + 1.0, size.z * 2.0 + 1.0);
    let step = Vec3::new(1.0 / diff.x, 1.0 / diff.y, 1.0 / diff.z);
    let x_offset = (1.0 - diff.x.floor() / diff.x) / 2.0;
    let z_offset = (1.0 - diff.z.floor() / diff.z) / 2.0;
    let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
    let (mut checks, mut hits) = (0u32, 0u32);
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
                if physics::clear(w, origin, point) {
                    hits += 1;
                }
                checks += 1;
                z += step.z;
            }
            y += step.y;
        }
        x += step.x;
    }
    if checks > 0 {
        f64::from(hits) / f64::from(checks)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::kind::{Character, Form};
    use super::super::tests::{Harness, TEST_HOOKS, calls, standing};
    use super::*;
    use crate::protocol::packet::Packet;
    use crate::tick::entity::physics::tests::named;

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
    fn air(h: &Harness, from: BlockPos, to: BlockPos) -> BTreeSet<BlockPos> {
        let mut out = BTreeSet::new();
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

    /// A size 4 explosion at `center` without a source, with the test hooks.
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

    /// The items on the ground, by count.
    fn ground(h: &Harness) -> u64 {
        h.ground()
    }

    /// A stone cube around one TNT block, lit; what the explosion destroyed, the items it
    /// dropped and the state hash.
    fn stone_scene(threads: usize) -> (BTreeSet<BlockPos>, u64, u64) {
        let mut h = Harness::new(threads);
        h.spawn(1, standing(20.0, 20.0));
        let (from, to) = (BlockPos::new(-5, -55, -5), BlockPos::new(5, -45, 5));
        fill(&mut h, from, to, "minecraft:stone");
        let center = BlockPos::new(0, -50, 0);
        h.state.worlds[0]
            .set_block(center, named("minecraft:tnt"))
            .unwrap();
        assert!(light(&mut h.state, 0, center, 80, Raise::Events));
        assert_eq!(burn_out(&mut h, 200), 80, "PMMP's fuse: 80 ticks");
        let mut destroyed = air(&h, from, to);
        destroyed.remove(&center);
        (destroyed, ground(&h), h.state.hash())
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
            let c = Vec3::new(
                f64::from(p.x) + 0.5,
                f64::from(p.y) + 0.5,
                f64::from(p.z) + 0.5,
            );
            assert!(
                c.distance_squared(center) < 3.5 * 3.5,
                "stone stops the rays: {p:?}"
            );
        }
        assert!(
            items <= destroyed.len() as u64,
            "at most one cobblestone per block"
        );
        assert_eq!(destroyed.len(), GOLDEN_STONE);
    }

    /// Blocks destroyed by `tnt_in_stone_is_a_fixed_scene_…` (engine randomness: a change of
    /// the rays or their random stream changes it).
    const GOLDEN_STONE: usize = 26;

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
        let center = BlockPos::new(0, -50, 0);
        h.state.worlds[0]
            .set_block(center, named("minecraft:tnt"))
            .unwrap();
        assert!(light(&mut h.state, 0, center, 1, Raise::Events));
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

    // pmmp: TNT::ignite
    #[test]
    fn tnt_lights_the_tnt_it_reaches_with_ten_to_thirty_ticks() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(20.0, 20.0));
        // On the ground, three blocks apart.
        let line: Vec<BlockPos> = (0..6).map(|i| BlockPos::new(i * 3 - 8, -60, 0)).collect();
        for &p in &line {
            h.state.worlds[0]
                .set_block(p, named("minecraft:tnt"))
                .unwrap();
        }
        assert!(light(&mut h.state, 0, line[0], 1, Raise::Events));
        let mut explosions = Vec::new();
        for tick in 0..400u32 {
            h.entities();
            let n = h
                .take(1)
                .iter()
                .filter(|p| matches!(p, Packet::LevelEvent(e) if e.event_type == i32::from(LevelEvent::ParticlesExplosion)))
                .count();
            for _ in 0..n {
                explosions.push(tick);
            }
        }
        assert_eq!(explosions.len(), 6, "every TNT went: {explosions:?}");
        for later in &explosions[1..] {
            assert!(later - explosions[0] >= 10, "chained fuses: {explosions:?}");
        }
        assert!(
            line.iter()
                .all(|&p| h.state.worlds[0].block(p) == Some(block::AIR))
        );
        assert_eq!(tnt_count(&h), 0);
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
        // PMMP's damage formula.
        assert_eq!(damage_of(1.0, 8.0), 65.0);
        assert_eq!(damage_of(0.25, 8.0), 11.0);
        assert_eq!(damage_of(0.0, 8.0), 1.0);
    }

    #[test]
    fn explosions_hurt_and_push_npcs_and_players_by_their_exposure() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(5.0, 0.0));
        let npc = super::super::npc::spawn(
            &mut h.state,
            0,
            Vec3::new(-5.5, -60.0, 0.5),
            super::super::Npc::Human {
                name: "target".to_owned(),
                skin: None,
            },
        )
        .unwrap();
        let hidden = super::super::npc::spawn(
            &mut h.state,
            0,
            Vec3::new(0.5, -60.0, -5.5),
            super::super::Npc::Human {
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
        let feet = p.position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
        let player_damage = expected(&h, feet, player_box(p));
        size_four(&mut h, center);
        let health = |h: &Harness, id: Id| h.state.rules.entities.get(id).unwrap().health;
        assert_eq!(health(&h, npc), 20.0 - npc_damage);
        assert_eq!(hidden_damage, 1.0, "behind obsidian: no ray reaches it");
        assert_eq!(health(&h, hidden), 20.0 - hidden_damage);
        assert!(
            h.state.rules.entities.get(npc).unwrap().body.motion.x < 0.0,
            "pushed away"
        );
        // PMMP: an explosion of no entity is a block explosion.
        let cause = crate::tick::survival::Damage::BlockExplosion;
        assert_eq!(calls(|c| c.damage.clone()), vec![(1, player_damage, cause)]);
        let me = h.runtime_id(1);
        let pushed = h.state.players[&SessionId::new(1)].outbox.iter().any(|p| {
            matches!(p,
            Packet::SetActorMotion(m) if m.entity_runtime_id == me && m.velocity[0] > 0.0)
        });
        assert!(pushed);
        // Invulnerable NPCs and items far away are not hurt; an item near is destroyed.
        assert!(super::super::drop_item(
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
        let before = health(&h, hidden);
        size_four(&mut h, center);
        assert_eq!(health(&h, hidden), before);
        let sticks = h
            .live()
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Item(i) if !e.flagged && i.stack.item().unwrap().name() == "minecraft:stick"))
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
        h.state.worlds[0]
            .set_block(center, named("minecraft:tnt"))
            .unwrap();
        assert!(light(&mut h.state, 0, center, 1, Raise::Events));
        burn_out(&mut h, 10);
        let mut destroyed = air(&h, from, to);
        destroyed.remove(&center);
        let items = h.ground();
        assert!(!destroyed.is_empty());
        assert!(items > 0, "a quarter drops (yield 1 / 4)");
        assert!(
            items <= destroyed.len() as u64,
            "{items} items from {} blocks",
            destroyed.len()
        );
        assert!(h.live().iter().all(|e| match &e.kind {
            Kind::Item(i) => i.stack.item().unwrap().name() == "minecraft:dirt",
            _ => true,
        }));
    }

    #[test]
    fn a_tick_explodes_at_most_its_budget_and_the_rest_wait() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        for i in 0..100i32 {
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
        assert_eq!(chance(false), 0.0f64.to_bits());
    }

    #[test]
    fn explosions_stop_at_chunks_that_are_not_loaded_and_can_set_fire() {
        let h = Harness::new(1);
        let w = &h.state.worlds[0];
        // Chunk 5 (x 80..) is not loaded around a centre at the loaded edge (x 79).
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

    /// Measures a 100-TNT chain: the worst entity tick while it goes off. `cargo test --release
    /// --lib tick::entity::explosion::tests::chain_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn chain_cost() {
        for threads in [1, 4] {
            let mut h = Harness::new(threads);
            h.spawn(1, standing(30.0, 30.0));
            fill(
                &mut h,
                BlockPos::new(-8, -61, -8),
                BlockPos::new(8, -52, 8),
                "minecraft:stone",
            );
            for i in 0..100i32 {
                let p = BlockPos::new(i % 5 * 2 - 4, -56 + i / 25, i / 5 % 5 * 2 - 4);
                h.state.worlds[0]
                    .set_block(p, named("minecraft:tnt"))
                    .unwrap();
            }
            assert!(light(
                &mut h.state,
                0,
                BlockPos::new(-4, -56, -4),
                1,
                Raise::Events
            ));
            let (mut worst, mut total, mut ticks, mut explosions) =
                (std::time::Duration::ZERO, std::time::Duration::ZERO, 0, 0);
            for _ in 0..600 {
                let start = std::time::Instant::now();
                h.entities();
                let took = start.elapsed();
                h.take(1);
                worst = worst.max(took);
                total += took;
                ticks += 1;
                if tnt_count(&h) == 0
                    && h.state.worlds[0].block(BlockPos::new(-4, -56, -4)) == Some(block::AIR)
                {
                    explosions += 1;
                    if explosions > 3 {
                        break;
                    }
                }
            }
            let left = (0..100i32)
                .filter(|i| {
                    let p = BlockPos::new(i % 5 * 2 - 4, -56 + i / 25, i / 5 % 5 * 2 - 4);
                    h.state.worlds[0].block(p).is_some_and(|s| s.is(Class::Tnt))
                })
                .count();
            assert_eq!(left, 0, "every TNT went off");
            eprintln!(
                "100-TNT chain, {threads} threads: worst tick {worst:?}, {ticks} ticks, mean {:?}",
                total / ticks
            );
        }
    }
}
