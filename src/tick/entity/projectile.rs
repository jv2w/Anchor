//! Projectiles and potion clouds (PMMP `Projectile`, `Arrow`, `Throwable` and its kinds,
//! `Trident`, `IceBomb`, `ProjectileItem::onClickAir`, `Bow::onReleaseUsing`,
//! `item\Trident::onReleaseUsing`, `AreaEffectCloud`).
//!
//! A projectile flies along a ray each tick ([`super::physics`]); the serial phase then looks for
//! the nearest player, NPC or end crystal on the ray (not the shooter in its first 5 ticks, never
//! for a trident) and resolves the hit: the kind's effect (an arrow's or trident's damage and
//! knockback, an ender pearl's teleport, a potion's splash or cloud, a bottle's experience, an ice
//! bomb's frost), then a block hit sticks an arrow or a trident and ends a thrown item, an entity
//! hit ends any projectile but a trident, which bounces back and hits nothing more until it meets a
//! block. Eggs hatch nothing (mobs are out of scope). A stuck trident is picked up whole by the
//! first player who touches it and has room for it.
//!
//! Fixed PMMP defects: a critical arrow's extra damage is counted before the hit clears the
//! critical flag (PMMP clears it first, so the bonus never applies); arrows shot by players
//! with infinite items (creative) are picked up by creative players only, so no one else gets
//! arrows out of nothing; a lingering potion still lingers after a load; a creative player
//! without room does not pick up a trident (PMMP gives it to a full inventory, which loses it).

use super::SessionId;
use super::kind::{
    Arrow, Cloud, Kind, MAX_CLOUD_EFFECTS, Pickup, Projectile, Shot, TRIDENT_DAMAGE, Trident,
    Victim,
};
use super::physics::{BlockHit, Flight, intercept};
use super::{
    Blow, Cause, EYE_HEIGHT, Entity, Hooks, Hurt, Knock, PLAYER_HEIGHT, Raise, State, combust,
    create, direction, discard, heal, hurt, is_live, level_event, level_of, pickup_event,
    player_actor, push_entity, send_to, targetable, with, worn,
};
use crate::block::{self, Face, TypeTag};
use crate::config::GameMode;
use crate::effect::{self, Potion};
use crate::entity::Id;
use crate::event::{
    Actor, AreaEffectCloudApply, CombustSource, DamageSource, EntityItemPickup, EntityShootBow,
    EntityTeleport, ProjectileHitBlock, ProjectileHitEntity, ProjectileLaunch, RegainReason,
};
use crate::item::{self, Class, Enchantment, Stack};
use crate::math::{Aabb, Vec3};
use crate::protocol::consts::{ActorEvent, LevelEvent};
use crate::protocol::packet::{Packet, TakeItemActor};
use crate::random::{self, Random};
use crate::tick::dispatch;
use crate::tick::{Phase, inventory, survival};
use crate::world::particle_id;

/// An arrow's base damage (PMMP `Arrow::$damage`).
pub(super) const ARROW_DAMAGE: f64 = 2.0;
/// A cloud's first radius, life and reapplication delay (PMMP `AreaEffectCloud::DEFAULT_*`).
pub(super) const CLOUD_RADIUS: f64 = 3.0;
pub(super) const CLOUD_DURATION: i32 = 600;
pub(super) const CLOUD_REAPPLICATION: i32 = 40;
/// A cloud's radius change per entity it affects and per tick.
pub(super) const CLOUD_RADIUS_ON_USE: f64 = -0.5;
pub(super) const CLOUD_RADIUS_PER_TICK: f64 = -(CLOUD_RADIUS / CLOUD_DURATION as f64);
/// Ticks between a cloud's applications (PMMP `UPDATE_DELAY`).
const CLOUD_UPDATE_DELAY: u32 = 10;
/// The smallest radius a cloud has (PMMP `entityBaseTick`).
const CLOUD_MIN_RADIUS: f64 = 0.5;
/// Ticks a projectile flies through its shooter (PMMP `Projectile::move`).
const OWNER_GRACE_TICKS: u32 = 5;
/// How much bigger an entity's box is to a projectile (PMMP `expandedCopy(0.3, …)`).
const HIT_REACH: f64 = 0.3;
/// The box around a splash potion that its effects reach, and the distance from the eyes
/// (PMMP `SplashPotion::onHit`).
const SPLASH_REACH: Vec3 = Vec3::new(4.125, 2.125, 4.125);
const SPLASH_RANGE: f64 = 4.0;
/// The shortest effect a splash gives, in ticks.
const SPLASH_MIN_TICKS: f64 = 20.0;
/// BDS's splash duration share ([`duration_share`]): `1.05 − 0.225 × distance`, at most 1
/// (measured: 2565, 1755 and 945 of 3600 ticks at 1.5, 2.5 and 3.5 blocks).
const SPLASH_DURATION_FULL: f64 = 1.05;
const SPLASH_DURATION_FALLOFF: f64 = 0.225;
/// The shake data of an arrow that hits a block (PMMP `ArrowShakeAnimation`).
const ARROW_SHAKE: i32 = 7;
/// Bow draws (PMMP `Bow::onReleaseUsing`): the weakest force and the fewest ticks that shoot.
const BOW_MIN_FORCE: f64 = 0.1;
const BOW_MIN_TICKS: u32 = 5;
/// The burn a Flame bow gives its arrow (PMMP: `setOnFire(100)`), in ticks.
const FLAME_TICKS: i16 = 2000;
/// The burn a burning projectile gives the player or NPC it hits (PMMP `Projectile::onHitEntity`).
const HIT_FIRE_SECONDS: i16 = 5;
/// The fall damage an ender pearl's thrower takes (PMMP `EnderPearl::onHit`).
const PEARL_DAMAGE: f64 = 5.0;
/// The particles a snowball or an egg leaves where it hits (PMMP `Snowball::onHit`,
/// `Egg::onHit`; BDS sends as many).
const HIT_PARTICLES: usize = 6;
/// The experience a bottle drops (PMMP `ExperienceBottle::onHit`: `mt_rand(3, 11)`).
const BOTTLE_EXPERIENCE: (i32, i32) = (3, 11);
/// The lingering potion item (PMMP's lingering potion is a splash potion that lingers).
const LINGERING_POTION: &str = "minecraft:lingering_potion";
/// The fewest ticks a trident is drawn to throw (PMMP `item\Trident::onReleaseUsing`), and the
/// speed of a full draw.
const TRIDENT_MIN_TICKS: u32 = 14;
const TRIDENT_FORCE: f64 = 2.4;
/// What a trident keeps of its speed when it bounces off what it hit (PMMP
/// `Trident::onHitEntity`).
const TRIDENT_BOUNCE: Vec3 = Vec3::new(-0.01, -0.1, -0.01);
/// How far around the block it hits an ice bomb freezes water (PMMP `IceBomb::onHitBlock`: the
/// 3 × 3 × 3 blocks around it).
const ICE_BOMB_REACH: i32 = 1;
/// Random streams.
const CRITICAL_RANDOM: u64 = random::tag("projectile-critical");
const BOTTLE_RANDOM: u64 = random::tag("projectile-bottle");
const BOW_RANDOM: u64 = random::tag("projectile-bow-wear");
const TRIDENT_RANDOM: u64 = random::tag("projectile-trident-wear");

/// Throws the projectile of `item` ([`super::throw`]) with the given hooks.
pub(super) fn throw_with(
    state: &mut State,
    session: SessionId,
    item: &Stack,
    hooks: Hooks,
) -> bool {
    let Some(t) = item.item() else {
        return false;
    };
    let shot = if t.is(Class::Snowball) {
        Shot::Snowball
    } else if t.is(Class::Egg) {
        Shot::Egg
    } else if t.is(Class::EnderPearl) {
        Shot::EnderPearl
    } else if t.is(Class::ExperienceBottle) {
        // Without orbs (`experience-orbs` or `experience` false) the throw is refused.
        if !state.settings.has_orbs() {
            return false;
        }
        Shot::ExperienceBottle
    } else if t.is(Class::IceBomb) {
        Shot::IceBomb
    } else if t.is(Class::SplashPotion) {
        let Some(potion) = t.potion() else {
            return false;
        };
        Shot::Potion {
            potion,
            linger: t.name() == LINGERING_POTION,
        }
    } else {
        return false;
    };
    let Some(force) = t.throw_force() else {
        return false;
    };
    let Some((world, eye, runtime_id, yaw, pitch)) = shooter(state, session, hooks) else {
        return false;
    };
    let kind = Kind::Projectile(Projectile {
        shot,
        damage: 0.0,
        stuck: None,
        owner: Some(session),
        owner_runtime_id: runtime_id,
    });
    let motion = direction(yaw, pitch) * force;
    let Some(id) = create(state, world, eye, kind, |_| (motion, yaw, pitch)) else {
        return false;
    };
    let still = |state: &State| {
        shooter(state, session, hooks).is_some()
            && state
                .players
                .get(&session)
                .is_some_and(|p| p.inventory.held() == item)
    };
    if !launch(state, id, still) {
        return false;
    }
    let feet = eye - Vec3::new(0.0, EYE_HEIGHT, 0.0);
    super::play(state, world, feet, crate::world::Sound::Throw);
    true
}

/// The world, eye position, runtime ID and look of a player who may shoot: spawned, alive and
/// not a spectator.
fn shooter(
    state: &State,
    session: SessionId,
    hooks: Hooks,
) -> Option<(usize, Vec3, u64, f64, f64)> {
    let player = state.players.get(&session)?;
    if player.phase != Phase::Spawned
        || !(hooks.is_alive)(player)
        || survival::game_mode(state, player) == GameMode::Spectator
    {
        return None;
    }
    let (yaw, pitch) = (hooks.look)(player);
    Some((player.world, player.position, player.runtime_id, yaw, pitch))
}

/// Releases a bow ([`super::release_bow`]) with the given hooks.
pub(super) fn release_bow_with(
    state: &mut State,
    session: SessionId,
    used_ticks: u32,
    hooks: Hooks,
) -> bool {
    let Some((world, eye, runtime_id, yaw, pitch)) = shooter(state, session, hooks) else {
        return false;
    };
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let bow = player.inventory.held().clone();
    if !bow.item().is_some_and(|t| t.is(Class::Bow)) {
        return false;
    }
    let mode = survival::game_mode(state, player);
    let finite = survival::has_finite_resources(mode);
    if finite && !(hooks.has_arrow)(player) {
        return false;
    }
    let p = f64::from(used_ticks) / 20.0;
    let force = crate::math::min((p * p + p * 2.0) / 3.0, 1.0);
    if force < BOW_MIN_FORCE || used_ticks < BOW_MIN_TICKS {
        return false;
    }
    let infinity = level_of(&bow, Enchantment::Infinity) > 0;
    let power = level_of(&bow, Enchantment::Power);
    let damage = if power > 0 {
        ARROW_DAMAGE + f64::midpoint(f64::from(power), 1.0)
    } else {
        ARROW_DAMAGE
    };
    let arrow = Arrow {
        critical: force >= 1.0,
        pickup: if infinity || !finite {
            Pickup::Creative
        } else {
            Pickup::Any
        },
        punch: level_of(&bow, Enchantment::Punch),
        life: 0,
    };
    let kind = Kind::Projectile(Projectile {
        shot: Shot::Arrow(arrow),
        damage,
        stuck: None,
        owner: Some(session),
        owner_runtime_id: runtime_id,
    });
    let aim = direction(yaw, pitch);
    let motion = aim * (force * 3.0);
    let turned = (if yaw > 180.0 { 360.0 } else { 0.0 }) - yaw;
    let Some(id) = create(state, world, eye, kind, |_| (motion, turned, -pitch)) else {
        return false;
    };
    if level_of(&bow, Enchantment::Flame) > 0 {
        with(state, id, |e| e.fire_ticks = FLAME_TICKS);
    }
    let still = |state: &State| {
        shooter(state, session, hooks).is_some()
            && state
                .players
                .get(&session)
                .is_some_and(|p| p.inventory.held() == &bow && (!finite || (hooks.has_arrow)(p)))
    };
    if !shoot(state, session, &bow, id, force * 3.0, aim, still) || !launch(state, id, still) {
        return false;
    }
    let feet = eye - Vec3::new(0.0, EYE_HEIGHT, 0.0);
    super::play(state, world, feet, crate::world::Sound::BowShoot);
    if finite {
        let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
        let mut r = Random::new(seed, state.tick, random::key(BOW_RANDOM, &[session.get()]));
        if let Some(player) = state.players.get_mut(&session) {
            if !infinity {
                (hooks.take_arrow)(player);
            }
            inventory::set_held(player, worn(&state.settings, &bow, 1, &mut r));
        }
    }
    true
}

/// Throws the trident `session` holds after drawing it for `used_ticks` (PMMP
/// `item\Trident::onReleaseUsing`): drawn 14 ticks or more, one trident leaves the hand worn by
/// one (with finite items; a throw that would break it fails) and flies from the eyes along the
/// look at up to 2.4 blocks per tick. False if nothing was thrown; the caller takes the trident
/// from the hand on true.
pub(super) fn release_trident_with(
    state: &mut State,
    session: SessionId,
    used_ticks: u32,
    hooks: Hooks,
) -> bool {
    let Some((world, eye, runtime_id, yaw, pitch)) = shooter(state, session, hooks) else {
        return false;
    };
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let held = player.inventory.held().clone();
    if !held.item().is_some_and(|t| t.is(Class::Trident)) || used_ticks < TRIDENT_MIN_TICKS {
        return false;
    }
    let mode = survival::game_mode(state, player);
    let mut item = held.with_count(1);
    if survival::has_finite_resources(mode) {
        let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
        let key = random::key(TRIDENT_RANDOM, &[session.get()]);
        item = worn(
            &state.settings,
            &item,
            1,
            &mut Random::new(seed, state.tick, key),
        );
    }
    if item.is_empty() {
        return false;
    }
    let p = f64::from(used_ticks) / 20.0;
    let force = crate::math::min((p * p + p * 2.0) / 3.0, 1.0) * TRIDENT_FORCE;
    let kind = Kind::Projectile(Projectile {
        shot: Shot::Trident(Trident {
            item,
            creative: mode == GameMode::Creative,
            can_collide: true,
        }),
        damage: TRIDENT_DAMAGE,
        stuck: None,
        owner: Some(session),
        owner_runtime_id: runtime_id,
    });
    let motion = direction(yaw, pitch) * force;
    let turned = (if yaw > 180.0 { 360.0 } else { 0.0 }) - yaw;
    let Some(id) = create(state, world, eye, kind, |_| (motion, turned, -pitch)) else {
        return false;
    };
    let still = |state: &State| {
        shooter(state, session, hooks).is_some()
            && state
                .players
                .get(&session)
                .is_some_and(|p| p.inventory.held() == &held)
    };
    if !launch(state, id, still) {
        return false;
    }
    // BDS plays it at the middle of the thrower's body (P6 `itemuse`).
    let middle = eye - Vec3::new(0.0, EYE_HEIGHT - PLAYER_HEIGHT / 2.0, 0.0);
    super::play(state, world, middle, crate::world::Sound::TridentThrow);
    true
}

/// Raises [`ProjectileLaunch`] for the new projectile `id` (PMMP `ProjectileItem::onClickAir`,
/// `Bow::onReleaseUsing`): false, with the projectile taken away unseen, when a listener
/// cancels it or `still` (the throw's check) fails after the listeners.
fn launch(state: &mut State, id: Id, still: impl Fn(&State) -> bool) -> bool {
    if !dispatch::heard::<ProjectileLaunch>(state) {
        return true;
    }
    let mut e = ProjectileLaunch {
        entity: id,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled
        || (dispatch::listened::<ProjectileLaunch>(state) && !(is_live(state, id) && still(state)))
    {
        discard(state, id);
        return false;
    }
    dispatch::watch(state, &e);
    true
}

/// Raises [`EntityShootBow`] for `session` shooting the new arrow `id` from `bow` at `force`
/// blocks per tick along `aim` (PMMP `Bow::onReleaseUsing`): false, with the arrow taken away
/// unseen, when a listener cancels it or `still` (the shot's check) fails after the listeners.
/// A force a listener sets becomes the arrow's speed.
fn shoot(
    state: &mut State,
    session: SessionId,
    bow: &Stack,
    id: Id,
    force: f64,
    aim: Vec3,
    still: impl Fn(&State) -> bool,
) -> bool {
    if !dispatch::heard::<EntityShootBow>(state) {
        return true;
    }
    let mut e = EntityShootBow {
        entity: player_actor(session),
        bow: bow.clone(),
        projectile: id,
        force,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled
        || (dispatch::listened::<EntityShootBow>(state) && !(is_live(state, id) && still(state)))
    {
        discard(state, id);
        return false;
    }
    if e.force != force {
        with(state, id, |arrow| {
            arrow.body.motion = super::physics::bounded(aim * e.force);
        });
    }
    dispatch::watch(state, &e);
    true
}

/// What a projectile hit.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Struck {
    /// A block.
    Block(BlockHit),
    /// A player.
    Player(SessionId),
    /// An NPC.
    Entity(Id),
}

/// Resolves the hits of a projectile's flight (PMMP `Projectile::move` after the ray): the
/// nearest player or NPC on the way, else the block the ray met. Projectiles on the ground
/// (just let go by their block) hit no one, as in PMMP `canCollideWith`.
pub(super) fn hit(
    state: &mut State,
    id: Id,
    flight: Flight,
    block: Option<BlockHit>,
    was_on_ground: bool,
    hooks: Hooks,
) {
    let Some(this) = state.rules.entities.get(id) else {
        return;
    };
    let Kind::Projectile(shot_data) = &this.kind else {
        return;
    };
    let (world, age, owner) = (this.world, this.age, shot_data.owner);
    // PMMP `Trident::canCollideWith`: not between an entity hit and a block, never its owner.
    let (collides, trident) = match &shot_data.shot {
        Shot::Trident(t) => (t.can_collide, true),
        _ => (true, false),
    };
    let (width, height) = this.size();
    let half = width / 2.0;
    let start = flight.start;
    let start_box = Aabb::new(
        Vec3::new(start.x - half, start.y, start.z - half),
        Vec3::new(start.x + half, start.y + height, start.z + half),
    );
    let mut end = flight.end;
    let mut struck = None;
    if !was_on_ground && collides {
        let area = start_box
            .stretch(end - start)
            .grow(Vec3::new(1.0, 1.0, 1.0));
        let mut best = f64::MAX;
        let entities = &state.rules.entities;
        entities.each_nearby(world, area, |other| {
            let Some(other_entity) = entities.get(other) else {
                return;
            };
            if other == id || !super::is_projectile_target(other_entity) {
                return;
            }
            let reach = other_entity
                .bounding_box()
                .grow(Vec3::new(HIT_REACH, HIT_REACH, HIT_REACH));
            if let Some((point, _)) = intercept(reach, start, end) {
                let d = start.distance_squared(point);
                if d < best {
                    best = d;
                    end = point;
                    struck = Some(Struck::Entity(other));
                }
            }
        });
        for (&session, player) in &state.players {
            if !targetable(state, player, world, hooks)
                || !survival::bounding_box(player).overlaps(area, super::EPSILON)
                || (owner == Some(session) && (trident || age < OWNER_GRACE_TICKS))
            {
                continue;
            }
            let reach =
                survival::bounding_box(player).grow(Vec3::new(HIT_REACH, HIT_REACH, HIT_REACH));
            if let Some((point, _)) = intercept(reach, start, end) {
                let d = start.distance_squared(point);
                if d < best {
                    best = d;
                    end = point;
                    struck = Some(Struck::Player(session));
                }
            }
        }
    }
    let struck = match (struck, block) {
        (Some(target), _) => {
            with(state, id, |this| {
                this.body.position = end;
                this.body.on_ground = true;
                this.body.motion = Vec3::ZERO;
            });
            state.rules.entities.reindex(id);
            target
        }
        (None, Some(b)) => Struck::Block(b),
        (None, None) => return,
    };
    on_hit(state, id, end, flight.motion, struck, hooks);
}

/// The damage a projectile deals to what it hits (PMMP `getResultDamage`): an arrow's base
/// damage times its speed, rounded up, plus a random bonus up to half when critical; none for
/// potions and bottles.
fn result_damage(state: &State, id: Id, p: &Projectile, motion: Vec3) -> Option<f64> {
    match p.shot {
        Shot::Arrow(a) => {
            let base = (motion.length() * p.damage).ceil();
            if !a.critical || !base.is_finite() {
                return Some(base);
            }
            let world_seed = state
                .rules
                .entities
                .get(id)
                .and_then(|e| state.worlds.get(e.world))
                .map_or(0, crate::world::World::seed);
            let runtime_id = state.rules.entities.get(id).map_or(0, |e| e.runtime_id);
            let mut r = Random::new(
                world_seed,
                state.tick,
                random::key(CRITICAL_RANDOM, &[runtime_id]),
            );
            let half = super::physics_block(base / 2.0).saturating_add(1);
            Some(base + f64::from(r.range(0, half)))
        }
        Shot::Snowball | Shot::Egg | Shot::EnderPearl | Shot::Trident(_) => Some(p.damage.ceil()),
        Shot::Potion { .. } | Shot::ExperienceBottle | Shot::IceBomb => None,
    }
}

/// A hit (PMMP `onHit`, then `onHitEntity` or `onHitBlock`).
fn on_hit(state: &mut State, id: Id, at: Vec3, motion: Vec3, struck: Struck, hooks: Hooks) {
    let event = hit_event(state, id, at, struck);
    if event.is_some() && !is_live(state, id) {
        // A listener took the projectile away: nothing more happens.
        return;
    }
    hit_effects(state, id, at, motion, struck, hooks);
    match event {
        Some(Hit::Block(e)) => dispatch::watch(state, &e),
        Some(Hit::Entity(e)) => dispatch::watch(state, &e),
        None => {}
    }
}

/// The event of a hit, raised before it does anything.
enum Hit {
    /// [`ProjectileHitBlock`].
    Block(ProjectileHitBlock),
    /// [`ProjectileHitEntity`].
    Entity(ProjectileHitEntity),
}

/// Raises [`ProjectileHitBlock`] or [`ProjectileHitEntity`] for the projectile `id` hitting
/// `struck` at `at` (PMMP `Projectile::move`, before `onHit`); `None` when nobody listens.
fn hit_event(state: &mut State, id: Id, at: Vec3, struck: Struck) -> Option<Hit> {
    let world = state.rules.entities.get(id)?.world;
    match struck {
        Struck::Block(b) => {
            if !dispatch::heard::<ProjectileHitBlock>(state) {
                return None;
            }
            let block = state
                .worlds
                .get(world)
                .and_then(|w| w.block(b.pos))
                .unwrap_or(block::AIR);
            let mut e = ProjectileHitBlock {
                entity: id,
                world: crate::world::Id::of(world),
                pos: b.pos,
                block,
                hit_position: at,
            };
            dispatch::call(state, &mut e);
            Some(Hit::Block(e))
        }
        Struck::Player(_) | Struck::Entity(_) => {
            if !dispatch::heard::<ProjectileHitEntity>(state) {
                return None;
            }
            let mut e = ProjectileHitEntity {
                entity: id,
                entity_hit: actor_of(struck)?,
                hit_position: at,
            };
            dispatch::call(state, &mut e);
            Some(Hit::Entity(e))
        }
    }
}

/// What deals a projectile's damage: the projectile, and its shooter while that one is online.
fn child_source(state: &State, id: Id) -> DamageSource {
    let owner = match state.rules.entities.get(id).map(|e| &e.kind) {
        Some(Kind::Projectile(p)) => p.owner,
        _ => None,
    };
    DamageSource::Child {
        child: id,
        owner: owner
            .filter(|s| state.players.contains_key(s))
            .map(player_actor),
    }
}

/// The actor of a player or NPC target.
fn actor_of(target: Struck) -> Option<Actor> {
    match target {
        Struck::Player(s) => Some(player_actor(s)),
        Struck::Entity(id) => Some(Actor::Entity(id)),
        Struck::Block(_) => None,
    }
}

/// What a hit does ([`on_hit`]).
fn hit_effects(state: &mut State, id: Id, at: Vec3, motion: Vec3, struck: Struck, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let Kind::Projectile(p) = &e.kind else {
        return;
    };
    let p = p.clone();
    let (world, burning, runtime_id) = (e.world, e.fire_ticks > 0, e.runtime_id);
    let damage = result_damage(state, id, &p, motion);
    match &p.shot {
        Shot::Arrow(_) => {
            with(state, id, |e| {
                if let Kind::Projectile(Projectile {
                    shot: Shot::Arrow(a),
                    ..
                }) = &mut e.kind
                {
                    a.critical = false;
                }
            });
            super::play(state, world, at, crate::world::Sound::ArrowHit);
        }
        Shot::Snowball => hit_particles(state, world, at, particle_id::SNOWBALL_POOF),
        Shot::Egg => {
            let egg = i32::from(item::Kind::Egg.default_type().runtime_id());
            hit_particles(state, world, at, (egg << 16) | particle_id::ITEM_BREAK);
        }
        Shot::EnderPearl => pearl(state, world, at, p.owner, hooks),
        Shot::ExperienceBottle => {
            splash_effect(state, world, at, super::track::DEFAULT_POTION_COLOUR);
            let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
            let mut r = Random::new(seed, state.tick, random::key(BOTTLE_RANDOM, &[runtime_id]));
            let amount = r.range(BOTTLE_EXPERIENCE.0, BOTTLE_EXPERIENCE.1);
            super::drop_experience(state, world, at, amount.unsigned_abs());
        }
        Shot::Potion { potion, linger } => {
            potion_hit(state, id, world, at, *potion, *linger, struck, hooks);
        }
        Shot::Trident(_) => {}
        Shot::IceBomb => {
            super::play(state, world, at, crate::world::Sound::IceBombHit);
            let bomb = i32::from(item::Kind::IceBomb.default_type().runtime_id());
            hit_particles(state, world, at, (bomb << 16) | particle_id::ITEM_BREAK);
        }
    }
    match struck {
        Struck::Block(b) => hit_block(state, id, &p.shot, at, b, burning),
        Struck::Player(session) => {
            if let Some(d) = damage {
                let blow = Blow::of(Cause::Projectile, d).by(child_source(state, id));
                let fire = burning.then_some(CombustSource::Entity(Actor::Entity(id)));
                hit_player(state, session, blow, fire, motion, &p, hooks);
            }
            ended(state, id, world, at, motion);
        }
        Struck::Entity(target) => {
            if let Some(d) = damage {
                let knock = Knock::new(motion.x, motion.z);
                let blow = Blow::of(Cause::Projectile, d).by(child_source(state, id));
                let landed = hurt(state, target, blow, Some(knock));
                if landed != Hurt::Ignored {
                    if burning {
                        let by = CombustSource::Entity(Actor::Entity(id));
                        combust(state, target, HIT_FIRE_SECONDS, by);
                    }
                    if let Some(push) = punch(&p, motion)
                        && let Some(now) = state.rules.entities.get(target).map(|e| e.body.motion)
                    {
                        push_entity(state, target, now + push);
                    }
                }
            }
            ended(state, id, world, at, motion);
        }
    }
}

/// A projectile of `shot` hits the block `b` at `at` (PMMP `onHitBlock`): it is stuck there, a
/// burning one lights TNT; an arrow shakes and stays, a trident stays and may hit entities
/// again, an ice bomb freezes the water around and ends, any other thrown item ends.
fn hit_block(state: &mut State, id: Id, shot: &Shot, at: Vec3, b: BlockHit, burning: bool) {
    let Some(world) = with(state, id, |e| {
        if let Kind::Projectile(p) = &mut e.kind {
            p.stuck = Some(b.pos);
        }
        e.world
    }) else {
        return;
    };
    if burning
        && state
            .worlds
            .get(world)
            .and_then(|w| w.block(b.pos))
            .is_some_and(|s| s.is(block::Class::Tnt))
    {
        let fuse = u32::from(super::TNT_FUSE);
        super::explosion::light(state, world, b.pos, fuse, Raise::Events);
    }
    match shot {
        Shot::Arrow(_) => {
            if let Some(e) = state.rules.entities.get(id) {
                let (viewers, runtime_id) = (e.viewers.clone(), e.runtime_id);
                let packet = super::actor_event(runtime_id, ActorEvent::Shake, ARROW_SHAKE);
                send_to(&mut state.players, &viewers, &packet);
            }
        }
        Shot::Trident(_) => {
            with(state, id, |e| set_collides(e, true));
            super::play(state, world, at, crate::world::Sound::TridentHitBlock);
        }
        Shot::IceBomb => {
            state.rules.entities.doom(id);
            freeze(state, world, b.pos);
        }
        Shot::Snowball
        | Shot::Egg
        | Shot::EnderPearl
        | Shot::Potion { .. }
        | Shot::ExperienceBottle => {
            state.rules.entities.doom(id);
        }
    }
}

/// Whether the entity `id` is a thrown trident.
fn is_trident(state: &State, id: Id) -> bool {
    state.rules.entities.get(id).is_some_and(|e| {
        matches!(
            &e.kind,
            Kind::Projectile(Projectile {
                shot: Shot::Trident(_),
                ..
            })
        )
    })
}

/// A projectile that hit a player or an entity at `at`, flying at `motion`, ends (PMMP
/// `despawnsOnEntityHit`), except a trident: it bounces back and hits nothing more until it meets
/// a block (PMMP `Trident::onHitEntity`).
fn ended(state: &mut State, id: Id, world: usize, at: Vec3, motion: Vec3) {
    let bounced = with(state, id, |e| {
        let trident = matches!(
            &e.kind,
            Kind::Projectile(Projectile {
                shot: Shot::Trident(_),
                ..
            })
        );
        if trident {
            set_collides(e, false);
            e.body.motion = Vec3::new(
                motion.x * TRIDENT_BOUNCE.x,
                motion.y * TRIDENT_BOUNCE.y,
                motion.z * TRIDENT_BOUNCE.z,
            );
        }
        trident
    });
    if bounced == Some(true) {
        super::play(state, world, at, crate::world::Sound::TridentHitEntity);
    } else {
        state.rules.entities.doom(id);
    }
}

/// Sets whether a trident may hit players and entities (PMMP `canCollide`).
fn set_collides(e: &mut super::Entity, collides: bool) {
    if let Kind::Projectile(Projectile {
        shot: Shot::Trident(t),
        ..
    }) = &mut e.kind
    {
        t.can_collide = collides;
    }
}

/// An ice bomb that hit the block `pos` freezes the water in the 3 × 3 × 3 blocks around it into
/// ice (PMMP `IceBomb::onHitBlock`, which raises no block event).
fn freeze(state: &mut State, world: usize, pos: crate::world::BlockPos) {
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    let ice = block::Kind::Ice.default_state();
    for dx in -ICE_BOMB_REACH..=ICE_BOMB_REACH {
        for dy in -ICE_BOMB_REACH..=ICE_BOMB_REACH {
            for dz in -ICE_BOMB_REACH..=ICE_BOMB_REACH {
                let Some(at) = pos.offset(dx, dy, dz) else {
                    continue;
                };
                if w.block(at).is_some_and(|s| s.is(block::Class::Water))
                    && let Err(e) = w.set_block(at, ice)
                {
                    // The water stays; nothing else depends on it.
                    tracing::debug!(?e, "an ice bomb could not freeze water");
                }
            }
        }
    }
}

/// The extra push of a Punch arrow (PMMP `Arrow::onHitEntity`).
fn punch(p: &Projectile, motion: Vec3) -> Option<Vec3> {
    let Shot::Arrow(a) = &p.shot else {
        return None;
    };
    let horizontal = (motion.x * motion.x + motion.z * motion.z).sqrt();
    if a.punch == 0 || horizontal.is_nan() || horizontal <= 0.0 {
        return None;
    }
    let m = f64::from(a.punch) * 0.6 / horizontal;
    Some(Vec3::new(motion.x * m, 0.1, motion.z * m))
}

/// A projectile hits a player (PMMP `Projectile::onHitEntity`): the damage of `blow` from its
/// source (PMMP `EntityDamageByChildEntityEvent`); the knockback along the flight only on a hit
/// outside the player's hit cooldown (PMMP `Living::attack` with the child entity's motion); a
/// burning projectile (`fire`, its source) sets the player on fire for 5 seconds; a Punch
/// arrow's push. The pushes are sent as the player's motion. A hit that does not land (creative,
/// spectator, a weaker hit within the cooldown) does nothing else, as for NPCs (PMMP still burns
/// and pushes then).
fn hit_player(
    state: &mut State,
    session: SessionId,
    blow: Blow,
    fire: Option<CombustSource>,
    motion: Vec3,
    p: &Projectile,
    hooks: Hooks,
) {
    let cold = state
        .players
        .get(&session)
        .is_some_and(|player| !(hooks.has_attack_cooldown)(player));
    let damage = survival::Damage::Entity;
    if !(hooks.damage)(state, session, blow.amount, damage, blow.source) {
        return;
    }
    if let Some(source) = fire {
        let seconds = u32::from(HIT_FIRE_SECONDS.unsigned_abs());
        (hooks.set_on_fire)(state, session, seconds, source);
    }
    if !state.players.get(&session).is_some_and(hooks.is_alive) {
        return;
    }
    let mut push = if cold {
        Knock::new(motion.x, motion.z)
            .applied_to(Vec3::ZERO)
            .unwrap_or(Vec3::ZERO)
    } else {
        Vec3::ZERO
    };
    if let Some(extra) = punch(p, motion) {
        push += extra;
    }
    if push != Vec3::ZERO {
        super::push_player(state, session, push, Raise::Events);
    }
}

/// An ender pearl lands at `at` (PMMP `EnderPearl::onHit`): its thrower, if still in the world
/// and alive, is moved there and takes 5 fall damage. [`EntityTeleport`] comes first: cancelling
/// keeps the thrower where it is (and unhurt), and a listener may send it elsewhere (a world the
/// server has, a position inside the world border).
fn pearl(state: &mut State, world: usize, at: Vec3, owner: Option<SessionId>, hooks: Hooks) {
    let Some(owner) = owner else {
        return;
    };
    let origin = |state: &State| {
        let player = state.players.get(&owner)?;
        (player.world == world && player.phase == Phase::Spawned && (hooks.is_alive)(player))
            .then(|| player.position - Vec3::new(0.0, EYE_HEIGHT, 0.0))
    };
    let Some(from) = origin(state) else {
        return;
    };
    let (mut to_world, mut to) = (world, at);
    let mut event = None;
    if dispatch::heard::<EntityTeleport>(state) {
        let mut e = EntityTeleport {
            entity: player_actor(owner),
            origin_world: crate::world::Id::of(world),
            origin: from,
            target_world: crate::world::Id::of(world),
            target: at,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled || (dispatch::listened::<EntityTeleport>(state) && origin(state).is_none()) {
            return;
        }
        to_world = e.target_world.index();
        to = e.target;
        if to_world >= state.worlds.len() || !super::is_valid_position(to) {
            return;
        }
        event = Some(e);
    }
    let Some(from) = origin(state) else {
        return;
    };
    let teleported = crate::world::Particle::EndermanTeleport;
    super::super::interact::particle(state, world, from, teleported);
    super::play(state, world, from, crate::world::Sound::EndermanTeleport);
    (hooks.teleport)(state, owner, to_world, to + Vec3::new(0.0, EYE_HEIGHT, 0.0));
    super::play(state, to_world, to, crate::world::Sound::EndermanTeleport);
    (hooks.damage)(
        state,
        owner,
        PEARL_DAMAGE,
        survival::Damage::Fall,
        DamageSource::None,
    );
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// [`HIT_PARTICLES`] numbered particles `data` at `at`, in BDS's form: `LevelEvent` 2009
/// (`ParticleGenericSpawn`) with the particle number and its extra value in the data
/// (P6).
fn hit_particles(state: &mut State, world: usize, at: Vec3, data: i32) {
    for _ in 0..HIT_PARTICLES {
        level_event(state, world, at, LevelEvent::ParticleGenericSpawn, data);
    }
}

/// The splash particle in a colour and the glass sound (PMMP `PotionSplashParticle`,
/// `PotionSplashSound`).
fn splash_effect(state: &mut State, world: usize, at: Vec3, colour: [u8; 4]) {
    let [r, g, b, a] = colour;
    let splash = crate::world::Particle::PotionSplash(crate::protocol::Rgba { r, g, b, a });
    super::super::interact::particle(state, world, at, splash);
    super::play(state, world, at, crate::world::Sound::PotionSplash);
}

/// A new cloud giving `effects` (at most [`MAX_CLOUD_EFFECTS`] of them) with PMMP's defaults
/// for a lingering potion's (`AreaEffectCloud::__construct`).
pub(super) fn new_cloud(effects: &[crate::effect::Instance]) -> Cloud {
    Cloud {
        effects: effects.iter().copied().take(MAX_CLOUD_EFFECTS).collect(),
        age: 0,
        max_age: CLOUD_DURATION,
        max_age_on_use: 0,
        reapplication_delay: CLOUD_REAPPLICATION,
        pickup_count: 0,
        initial_radius: CLOUD_RADIUS,
        radius: CLOUD_RADIUS,
        radius_on_pickup: CLOUD_RADIUS_ON_USE,
        radius_on_use: CLOUD_RADIUS_ON_USE,
        radius_per_tick: CLOUD_RADIUS_PER_TICK,
        victims: std::collections::BTreeMap::new(),
    }
}

/// A potion hits (PMMP `SplashPotion::onHit`): a splash gives its effects to the players and
/// NPCs within 4 blocks of their eyes, weaker with the distance (full to what it hit: durations
/// by [`duration_share`], instant effects by PMMP's distance from the eyes); a lingering potion
/// leaves a cloud; a water bottle puts out the fire it lands by.
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: the hit's parts, as PMMP's `onHit` reads them from its event"
)]
fn potion_hit(
    state: &mut State,
    id: Id,
    world: usize,
    at: Vec3,
    potion: Potion,
    linger: bool,
    struck: Struck,
    hooks: Hooks,
) {
    let effects = potion.effects();
    splash_effect(state, world, at, super::track::effect_colour(effects));
    if linger {
        let cloud = new_cloud(effects);
        let place = at.floor() + Vec3::new(0.5, 0.5, 0.5);
        // At the entity limit the cloud is not made (a potion cannot be given back).
        create(state, world, place, Kind::Cloud(cloud), |_| {
            (Vec3::ZERO, 0.0, 0.0)
        });
    } else if !effects.is_empty() {
        let Some(e) = state.rules.entities.get(id) else {
            return;
        };
        let area = e.bounding_box().grow(SPLASH_REACH);
        let by = child_source(state, id);
        for target in living_in(state, world, area, id, hooks) {
            let Some(eye) = eye_of(state, target) else {
                continue;
            };
            let d2 = eye.distance_squared(at);
            if d2 > SPLASH_RANGE * SPLASH_RANGE {
                continue;
            }
            let potency = if same(target, struck) {
                1.0
            } else {
                1.0 - d2.sqrt() / SPLASH_RANGE
            };
            let share = if same(target, struck) {
                1.0
            } else {
                duration_share(state, target, at)
            };
            for &effect in effects {
                if effect.effect().is_instant() {
                    instant(state, target, effect, potency, by, hooks);
                } else {
                    let ticks = (f64::from(effect.duration_ticks()) * share).round();
                    if ticks < SPLASH_MIN_TICKS {
                        continue;
                    }
                    give(state, target, with_duration(effect, ticks), hooks);
                }
            }
        }
    }
    if effects.is_empty()
        && potion.id() == 0
        && let Struck::Block(b) = struck
    {
        extinguish(state, world, b);
    }
}

/// Puts out the fire in the block next to the hit face and beside it (PMMP: a water splash).
fn extinguish(state: &mut State, world: usize, b: BlockHit) {
    let Some(inside) = b.pos.step(b.face) else {
        return;
    };
    let around = [Face::North, Face::South, Face::West, Face::East].map(|f| inside.step(f));
    for pos in std::iter::once(Some(inside)).chain(around).flatten() {
        let Some(w) = state.worlds.get_mut(world) else {
            return;
        };
        if w.block(pos).is_some_and(|s| s.has_type_tag(TypeTag::Fire))
            && let Err(e) = w.set_block(pos, block::AIR)
        {
            // The fire stays; nothing else depends on it.
            tracing::debug!(?e, "a splash could not put out a fire");
        }
    }
}

/// Who a potion or cloud can affect in `area`: living NPCs (not `except`) in `Id` order, then
/// players that can be hit, by session.
fn living_in(state: &State, world: usize, area: Aabb, except: Id, hooks: Hooks) -> Vec<Struck> {
    let entities = &state.rules.entities;
    let mut out: Vec<Struck> = entities
        .nearby(world, area)
        .into_iter()
        .filter(|&o| o != except && entities.get(o).is_some_and(|e| e.kind.is_living()))
        .map(Struck::Entity)
        .collect();
    out.extend(
        state
            .players
            .iter()
            .filter(|(_, p)| {
                targetable(state, p, world, hooks)
                    && survival::bounding_box(p).overlaps(area, super::EPSILON)
            })
            .map(|(&s, _)| Struck::Player(s)),
    );
    out
}

/// The eye position of a player or NPC.
fn eye_of(state: &State, target: Struck) -> Option<Vec3> {
    match target {
        Struck::Player(s) => state.players.get(&s).map(|p| p.position),
        Struck::Entity(id) => state.rules.entities.get(id).map(Entity::eye_position),
        Struck::Block(_) => None,
    }
}

/// The share of a splash potion's effect durations `target` gets from a splash at `at`, as BDS
/// gives it (P6 `itemuse`): full within 2/9 of a block of the target's upright middle line (feet to
/// the top of its box), then 0.225 less per block. PMMP's `0.75 × (1 − distance from the eyes / 4)`
/// gives a third less at the feet and differs with the height of the splash, which BDS ignores
/// beside the body.
fn duration_share(state: &State, target: Struck, at: Vec3) -> f64 {
    let b = match target {
        Struck::Player(s) => state.players.get(&s).map(survival::bounding_box),
        Struck::Entity(id) => state
            .rules
            .entities
            .get(id)
            .map(super::Entity::bounding_box),
        Struck::Block(_) => None,
    };
    let Some(b) = b else {
        return 0.0;
    };
    let line = Vec3::new(
        f64::midpoint(b.min.x, b.max.x),
        crate::math::clamp(at.y, b.min.y, b.max.y),
        f64::midpoint(b.min.z, b.max.z),
    );
    crate::math::min(
        1.0,
        SPLASH_DURATION_FULL - SPLASH_DURATION_FALLOFF * line.distance_squared(at).sqrt(),
    )
}

/// The feet position of a player or NPC.
fn feet_of(state: &State, target: Struck) -> Option<Vec3> {
    match target {
        Struck::Player(s) => state
            .players
            .get(&s)
            .map(|p| p.position - Vec3::new(0.0, EYE_HEIGHT, 0.0)),
        Struck::Entity(id) => state.rules.entities.get(id).map(|e| e.body.position),
        Struck::Block(_) => None,
    }
}

/// Whether `target` is what the projectile hit.
fn same(target: Struck, struck: Struck) -> bool {
    match (target, struck) {
        (Struck::Player(a), Struck::Player(b)) => a == b,
        (Struck::Entity(a), Struck::Entity(b)) => a == b,
        _ => false,
    }
}

/// `effect` lasting `ticks`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "invariant: ticks is a rounded duration between 0 and the effect's own u32 duration"
)]
fn with_duration(effect: effect::Instance, ticks: f64) -> effect::Instance {
    effect::Instance::new(
        effect.effect(),
        crate::math::max(ticks, 0.0) as u32,
        effect.amplifier(),
        effect.is_visible(),
        effect.is_ambient(),
    )
}

/// Gives a lasting effect: players get it; NPCs have no lasting effects (they have no AI the
/// effects could change).
fn give(state: &mut State, target: Struck, effect: effect::Instance, hooks: Hooks) {
    if let Struck::Player(s) = target {
        (hooks.add_effect)(state, s, effect);
    }
}

/// An instant effect at `potency` (PMMP `InstantHealthEffect`: heals the drunk amount ×
/// potency; `InstantDamageEffect`: the drunk amount × potency magic damage from `source`).
fn instant(
    state: &mut State,
    target: Struck,
    effect: effect::Instance,
    potency: f64,
    source: DamageSource,
    hooks: Hooks,
) {
    let amount = survival::instant_amount(effect.effect(), effect.amplifier()) * potency;
    match effect.effect() {
        effect::Type::InstantHealth => match target {
            Struck::Player(s) => (hooks.heal)(state, s, amount),
            Struck::Entity(id) => heal(state, id, amount, RegainReason::Magic),
            Struck::Block(_) => {}
        },
        effect::Type::InstantDamage => match target {
            Struck::Player(s) => {
                let none = DamageSource::None;
                (hooks.damage)(state, s, amount, survival::Damage::Magic, none);
            }
            Struck::Entity(id) => {
                hurt(state, id, Blow::of(Cause::Magic, amount).by(source), None);
            }
            Struck::Block(_) => {}
        },
        _ => {}
    }
}

/// A projectile's own tick (PMMP `Arrow::entityBaseTick`): a stuck arrow goes after a minute.
pub(super) fn tick(state: &mut State, id: Id) {
    with(state, id, |e| {
        let Kind::Projectile(p) = &mut e.kind else {
            return;
        };
        let stuck = p.stuck.is_some();
        if let Shot::Arrow(a) = &mut p.shot {
            if stuck {
                a.life = a.life.saturating_add(1);
                if a.life > super::kind::ARROW_LIFE {
                    e.flagged = true;
                }
            } else {
                a.life = 0;
            }
        }
    });
}

/// A player touches a stuck arrow (PMMP `Arrow::onCollideWithPlayer`): a player with finite
/// items gets the arrow if it fits and the arrow allows; a creative player takes it for nothing.
/// True if the arrow was taken.
///
/// While someone listens to [`EntityItemPickup`] the event comes first, offering one arrow:
/// cancelling leaves the arrow; a listener that changes the arrow or the player so that the
/// pickup no longer holds cancels it; an item a listener sets is given instead (what of it does
/// not fit drops where the arrow was).
pub(super) fn pick_up(state: &mut State, session: SessionId, id: Id, hooks: Hooks) -> bool {
    if is_trident(state, id) {
        return pick_up_trident(state, session, id, hooks);
    }
    let check = |state: &State| -> Option<bool> {
        let e = state.rules.entities.get(id).filter(|e| !e.flagged)?;
        let Kind::Projectile(Projectile {
            shot: Shot::Arrow(a),
            stuck: Some(_),
            ..
        }) = &e.kind
        else {
            return None;
        };
        let player = state.players.get(&session)?;
        let finite = matches!(
            survival::game_mode(state, player),
            GameMode::Survival | GameMode::Adventure
        );
        let refused = a.pickup == Pickup::None || (a.pickup == Pickup::Creative && finite);
        (!refused).then_some(finite)
    };
    let Some(finite) = check(state) else {
        return false;
    };
    let heard = dispatch::heard::<EntityItemPickup>(state);
    let arrow = if finite || heard {
        Stack::new(item::Kind::Arrow.default_type(), 1)
    } else {
        Stack::EMPTY
    };
    let fits = |state: &State| {
        !finite
            || state
                .players
                .get(&session)
                .is_some_and(|p| (hooks.room)(p, &arrow) > 0)
    };
    let mut event = None;
    if heard {
        if !fits(state) {
            return false;
        }
        let still = |state: &State| check(state) == Some(finite) && fits(state);
        let Some(e) = pickup_event(state, session, id, &arrow, still) else {
            return false;
        };
        event = Some(e);
    }
    let given = event.as_ref().map_or(&arrow, |e| &e.item).clone();
    let replaced = given != arrow;
    if finite || replaced {
        let Some(player) = state.players.get_mut(&session) else {
            return false;
        };
        let rest = (hooks.give)(player, given);
        if !rest.is_empty() {
            if !replaced {
                return false;
            }
            let (world, at) = state
                .rules
                .entities
                .get(id)
                .map_or((0, Vec3::ZERO), |e| (e.world, e.body.position));
            if !super::drop_item(state, world, at, rest) {
                tracing::debug!(
                    world,
                    "what of a listener's pickup item did not fit was lost"
                );
            }
        }
    }
    let taker = state.players.get(&session).map_or(0, |p| p.runtime_id);
    let Some(e) = state.rules.entities.get_mut(id) else {
        return false;
    };
    e.flagged = true;
    let (viewers, runtime_id, key) = (e.viewers.clone(), e.runtime_id, (e.world, e.chunk));
    let entities = &mut state.rules.entities;
    entities.doomed.insert(id);
    entities.changed.insert(key);
    let packet = Packet::TakeItemActor(TakeItemActor {
        item_entity_runtime_id: runtime_id,
        taker_entity_runtime_id: taker,
    });
    send_to(&mut state.players, &viewers, &packet);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// A player touches a stuck trident (PMMP `Trident::onCollideWithPlayer`, `pickup`): the player
/// gets its item if all of it fits, and it goes; one thrown in creative goes and gives nothing.
/// True if the trident was taken.
///
/// While someone listens to [`EntityItemPickup`] the event comes first (not for one thrown in
/// creative, which PMMP offers cancelled): cancelling leaves the trident; a listener that changes
/// the trident or the player so that the pickup no longer holds cancels it; an item a listener
/// sets is given instead (what of it does not fit drops where the trident was).
fn pick_up_trident(state: &mut State, session: SessionId, id: Id, hooks: Hooks) -> bool {
    let check = |state: &State| -> Option<(Stack, bool)> {
        let e = state.rules.entities.get(id).filter(|e| !e.flagged)?;
        let Kind::Projectile(Projectile {
            shot: Shot::Trident(t),
            stuck: Some(_),
            ..
        }) = &e.kind
        else {
            return None;
        };
        state.players.get(&session)?;
        Some((t.item.clone(), t.creative))
    };
    let Some((item, creative)) = check(state) else {
        return false;
    };
    let fits = |state: &State| {
        creative
            || state
                .players
                .get(&session)
                .is_some_and(|p| (hooks.room)(p, &item) >= item.count())
    };
    if !fits(state) {
        return false;
    }
    let mut event = None;
    if !creative && dispatch::heard::<EntityItemPickup>(state) {
        let offer = (item.clone(), creative);
        let still = |state: &State| check(state).as_ref() == Some(&offer) && fits(state);
        let Some(e) = pickup_event(state, session, id, &item, still) else {
            return false;
        };
        event = Some(e);
    }
    if !creative {
        let given = event.as_ref().map_or(&item, |e| &e.item).clone();
        let replaced = given != item;
        let Some(player) = state.players.get_mut(&session) else {
            return false;
        };
        let rest = (hooks.give)(player, given);
        if !rest.is_empty() {
            let (world, at) = state
                .rules
                .entities
                .get(id)
                .map_or((0, Vec3::ZERO), |e| (e.world, e.body.position));
            if !replaced {
                // What the room promised did not all go in: the rest stays as the trident.
                with(state, id, |e| {
                    if let Kind::Projectile(Projectile {
                        shot: Shot::Trident(t),
                        ..
                    }) = &mut e.kind
                    {
                        t.item = rest;
                    }
                });
                return true;
            }
            if !super::drop_item(state, world, at, rest) {
                tracing::debug!(
                    world,
                    "what of a listener's pickup item did not fit was lost"
                );
            }
        }
    }
    let taker = state.players.get(&session).map_or(0, |p| p.runtime_id);
    let Some(e) = state.rules.entities.get_mut(id) else {
        return false;
    };
    e.flagged = true;
    let (viewers, runtime_id, key) = (e.viewers.clone(), e.runtime_id, (e.world, e.chunk));
    let entities = &mut state.rules.entities;
    entities.doomed.insert(id);
    entities.changed.insert(key);
    let packet = Packet::TakeItemActor(TakeItemActor {
        item_entity_runtime_id: runtime_id,
        taker_entity_runtime_id: taker,
    });
    send_to(&mut state.players, &viewers, &packet);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Raises [`AreaEffectCloudApply`] for the cloud `id` and the players and NPCs it is about to
/// affect (PMMP `AreaEffectCloud::entityBaseTick`), into `event` for the monitors: false when a
/// listener cancelled it or took the cloud away.
fn cloud_applies(
    state: &mut State,
    id: Id,
    affected: &[(Struck, Victim)],
    event: &mut Option<AreaEffectCloudApply>,
) -> bool {
    if !dispatch::heard::<AreaEffectCloudApply>(state) {
        return true;
    }
    let mut e = AreaEffectCloudApply {
        entity: id,
        affected: affected.iter().filter_map(|&(t, _)| actor_of(t)).collect(),
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled || (dispatch::listened::<AreaEffectCloudApply>(state) && !is_live(state, id)) {
        return false;
    }
    *event = Some(e);
    true
}

/// A cloud's tick (PMMP `AreaEffectCloud::entityBaseTick`): it shrinks each tick and goes below
/// half a block; every 10 ticks it gives its effects to the players and NPCs standing in it that
/// it has not affected within the reapplication delay (instant effects at half strength, lasting
/// effects a quarter as long), shrinking and shortening per entity affected.
pub(super) fn cloud_tick(state: &mut State, id: Id, hooks: Hooks) {
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    let world = e.world;
    let position = e.body.position;
    let Kind::Cloud(c) = &mut e.kind else {
        return;
    };
    c.age = c.age.saturating_add(1);
    let radius = c.radius + c.radius_per_tick;
    if radius < CLOUD_MIN_RADIUS {
        e.flagged = true;
        return;
    }
    c.radius = radius;
    if c.age < CLOUD_UPDATE_DELAY || c.age % CLOUD_UPDATE_DELAY != 0 {
        return;
    }
    if i64::from(c.age) > i64::from(c.max_age) {
        e.flagged = true;
        return;
    }
    let age = c.age;
    c.victims.retain(|_, until| age < *until);
    let victims = c.victims.clone();
    let (radius_on_use, max_age_on_use) = (c.radius_on_use, c.max_age_on_use);
    let (max_age, effects) = (c.max_age, c.effects.clone());
    let area = e.bounding_box();
    let mut affected = Vec::new();
    let mut radius_change = 0.0;
    let mut age_change = 0i32;
    for target in living_in(state, world, area, id, hooks) {
        let victim = match target {
            Struck::Player(s) => Victim::Player(s),
            Struck::Entity(o) => Victim::Entity(o),
            Struck::Block(_) => continue,
        };
        if victims.contains_key(&victim) {
            continue;
        }
        let Some(feet) = feet_of(state, target) else {
            continue;
        };
        let (dx, dz) = (feet.x - position.x, feet.z - position.z);
        if dx * dx + dz * dz > radius * radius {
            continue;
        }
        affected.push((target, victim));
        if radius_on_use != 0.0 {
            radius_change += radius_on_use;
            if radius + radius_change <= 0.0 {
                break;
            }
        }
        if max_age_on_use != 0 {
            age_change = age_change.saturating_add(max_age_on_use);
            if max_age.saturating_add(age_change) <= 0 {
                break;
            }
        }
    }
    if affected.is_empty() {
        return;
    }
    let mut event = None;
    if !cloud_applies(state, id, &affected, &mut event) {
        return;
    }
    let by = DamageSource::Entity(Actor::Entity(id));
    let mut until = Vec::with_capacity(affected.len());
    for &(target, victim) in &affected {
        for &effect in &effects {
            if effect.effect().is_instant() {
                instant(state, target, effect, 0.5, by, hooks);
            } else {
                let ticks = (f64::from(effect.duration_ticks()) / 4.0).round();
                give(state, target, with_duration(effect, ticks), hooks);
            }
        }
        until.push(victim);
    }
    with(state, id, |e| {
        used(e, age, until, radius_change, age_change);
    });
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// A cloud gave its effects at `age` to `until`: they wait out its reapplication delay, and it
/// shrinks and shortens by the changes; it goes when it has no radius or time left.
fn used(e: &mut super::Entity, age: u32, until: Vec<Victim>, radius_change: f64, age_change: i32) {
    let Kind::Cloud(c) = &mut e.kind else {
        return;
    };
    if c.reapplication_delay != 0 {
        let at = age.saturating_add(c.reapplication_delay.unsigned_abs());
        for victim in until {
            c.victims.insert(victim, at);
        }
    }
    let radius = c.radius + radius_change;
    let max_age = c.max_age.saturating_add(age_change);
    if radius <= 0.0 || max_age <= 0 {
        e.flagged = true;
    } else {
        c.radius = radius;
        c.max_age = max_age;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Harness, TEST_HOOKS, at, calls, room, standing, taken};
    use super::*;
    use crate::block::Class as BlockClass;
    use crate::nbt::Compound;
    use crate::tick::entity::physics::MOTION_THRESHOLD;
    use crate::tick::entity::physics::tests::named;
    use crate::tick::entity::saved::tests::stack;
    use crate::world::BlockPos;

    /// A potion item of `potion`, splash or lingering.
    fn potion_item(potion: &str, lingering: bool) -> Stack {
        let p = *Potion::all()
            .iter()
            .find(|p| format!("{p:?}") == potion)
            .unwrap();
        let name = if lingering {
            LINGERING_POTION
        } else {
            "minecraft:splash_potion"
        };
        let t = item::Type::from_name(name, u16::from(p.id()), None, &Compound::new()).unwrap();
        Stack::new(t, 1)
    }

    fn session(n: u64) -> SessionId {
        SessionId::new(n)
    }

    /// PMMP `InstantHealthEffect` and `InstantDamageEffect`: a splash's amount is the drunk
    /// potion's (4 or 6 × 2^amplifier) at every amplifier, without the integer overflow.
    #[test]
    fn a_splash_at_amplifier_17_heals_and_hurts_what_the_drink_does() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        for effect in [effect::Type::InstantHealth, effect::Type::InstantDamage] {
            let instance = effect::Instance::new(effect, 1, 17, true, false);
            let none = DamageSource::None;
            instant(
                &mut h.state,
                Struck::Player(session(1)),
                instance,
                1.0,
                none,
                TEST_HOOKS,
            );
        }
        let doubled = f64::from(1_u32 << 17);
        assert_eq!(calls(|c| c.heals.clone()), [(1, 4.0 * doubled)]);
        let hurt: Vec<f64> = calls(|c| c.damage.iter().map(|d| d.1).collect());
        assert_eq!(hurt, [6.0 * doubled]);
    }

    /// Moves a player's eyes to `eye` without a tick.
    fn place(h: &mut Harness, n: u64, eye: Vec3) {
        h.state.players.get_mut(&session(n)).unwrap().position = eye;
    }

    /// The only projectile.
    fn projectile(h: &Harness) -> Option<(Vec3, Vec3, Projectile)> {
        h.live().into_iter().find_map(|e| match &e.kind {
            Kind::Projectile(p) if !e.flagged => Some((e.body.position, e.body.motion, p.clone())),
            _ => None,
        })
    }

    // pmmp: ProjectileItem::onClickAir
    #[test]
    fn a_snowball_flies_with_pmmps_constants() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        calls(|c| c.look = (90.0, -30.0));
        let eye = h.state.players[&session(1)].position;
        assert!(throw_with(
            &mut h.state,
            session(1),
            &stack("minecraft:snowball", 1),
            TEST_HOOKS
        ));
        // PMMP: motion = direction × 1.5; each tick drag 0.01 before gravity 0.03, then move.
        let mut position = eye;
        let mut motion = direction(90.0, -30.0) * 1.5;
        for tick in 0..6 {
            h.entities();
            let small = |v: f64| if v.abs() <= MOTION_THRESHOLD { 0.0 } else { v };
            motion = Vec3::new(
                small(motion.x * 0.99),
                small(motion.y * 0.99 - 0.03),
                small(motion.z * 0.99),
            );
            position += motion;
            let (p, m, _) = projectile(&h).expect("still flying");
            assert_eq!((p, m), (position, motion), "tick {tick}");
        }
        let e = h.live().into_iter().next().unwrap();
        let (yaw, pitch) = super::super::physics::heading(motion);
        assert_eq!((e.yaw, e.pitch), (yaw, pitch), "it turns along its flight");
        assert!((yaw + 90.0).abs() < 1e-6, "flies west: {yaw}");
    }

    /// A snowball and an egg leave six numbered particles where they land, as BDS sends them:
    /// `LevelEvent` 2009 with the snowball poof (15), or the egg item breaking (its runtime ID
    /// 393 (0x189) above the item break particle 14).
    #[test]
    fn snowballs_and_eggs_leave_six_particles_where_they_hit() {
        for (projectile, data) in [
            (crate::entity::Projectile::Snowball, 15),
            (crate::entity::Projectile::Egg, 0x0189_000E),
        ] {
            let mut h = Harness::new(1);
            h.spawn(1, standing(0.0, 0.0));
            h.take(1);
            let id = crate::Context {
                state: &mut h.state,
            }
            .spawn_projectile(
                crate::world::Id::of(0),
                Vec3::new(3.5, -59.0, 0.5),
                projectile,
                Vec3::ZERO,
            )
            .unwrap();
            for _ in 0..20 {
                h.entities();
            }
            assert!(!is_live(&h.state, id), "it hit the ground");
            let particles: Vec<([f32; 3], i32)> = h
                .take(1)
                .into_iter()
                .filter_map(|p| match p {
                    Packet::LevelEvent(e)
                        if e.event_type == i32::from(LevelEvent::ParticleGenericSpawn) =>
                    {
                        Some((e.position, e.event_data))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(particles.len(), HIT_PARTICLES, "{projectile:?}");
            assert!(
                particles
                    .iter()
                    .all(|(at, d)| *d == data && at[1] == -60.0 && *at == particles[0].0)
            );
        }
    }

    /// Throws one of every projectile from two players and shoots arrows, then runs ticks.
    fn volley(threads: usize) -> Vec<u64> {
        let mut h = Harness::new(threads);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(6.0, 0.0));
        room(1000);
        calls(|c| c.arrows = 100);
        let items = [
            stack("minecraft:snowball", 1),
            stack("minecraft:egg", 1),
            stack("minecraft:ender_pearl", 1),
            stack("minecraft:experience_bottle", 1),
            potion_item("Harming", false),
            potion_item("Regeneration", true),
        ];
        let mut trace = Vec::new();
        for tick in 0..120u32 {
            let yaw = f64::from(tick * 37 % 360);
            let pitch = f64::from(tick % 7) * 10.0 - 30.0;
            calls(|c| c.look = (yaw, pitch));
            let item = &items[usize::try_from(tick).unwrap() % items.len()];
            throw_with(
                &mut h.state,
                session(1 + u64::from(tick % 2)),
                item,
                TEST_HOOKS,
            );
            if tick % 5 == 0 {
                let bow = stack("minecraft:bow", 1);
                inventory::set_held(h.state.players.get_mut(&session(2)).unwrap(), bow);
                release_bow_with(&mut h.state, session(2), 5 + tick % 30, TEST_HOOKS);
            }
            h.entities();
            trace.push(h.state.hash());
        }
        trace
    }

    #[test]
    fn projectiles_give_the_same_state_on_one_or_many_threads_and_again() {
        let one = volley(1);
        assert_eq!(one, volley(4));
        assert_eq!(one, volley(1), "a second run (a replay) is the same");
    }

    // pmmp: Bow::onReleaseUsing
    #[test]
    fn a_full_bow_shot_is_critical_hits_a_player_and_knocks_it_back() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(0.0, 8.0));
        calls(|c| {
            c.look = (0.0, 0.0);
            c.arrows = 1;
        });
        let bow = stack("minecraft:bow", 1);
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), bow);
        assert!(
            !release_bow_with(&mut h.state, session(1), 4, TEST_HOOKS),
            "too short"
        );
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        assert_eq!(calls(|c| c.arrows), 0, "the arrow is used");
        let held = h.state.players[&session(1)].inventory.held().clone();
        assert_eq!(held.damage(), 1, "the bow wears");
        let (_, motion, p) = projectile(&h).unwrap();
        assert!(matches!(p.shot, Shot::Arrow(a) if a.critical && a.pickup == Pickup::Any));
        assert!((motion.length() - 3.0).abs() < 1e-9, "full draw: speed 3");
        assert!(
            !release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS),
            "no arrow left"
        );
        for _ in 0..5 {
            h.entities();
        }
        let hits = calls(|c| c.damage.clone());
        assert_eq!(hits.len(), 1, "{hits:?}");
        let (who, amount, cause) = hits[0];
        assert_eq!((who, cause), (2, survival::Damage::Entity));
        // Speed just under 3 after drag: ceil(2.97 × 2) = 6, and up to 6 / 2 + 1 more.
        assert!((6.0..=10.0).contains(&amount), "{amount}");
        assert!(projectile(&h).is_none(), "the arrow is gone");
        let pushed = h.take(2).into_iter().any(|p| matches!(p,
            Packet::SetActorMotion(m) if m.entity_runtime_id == h.runtime_id(2) && m.velocity[2] > 0.3));
        assert!(pushed, "knocked back along the flight");
    }

    /// A burning arrow sets the player it hits on fire for 5 seconds; a hit within the player's
    /// hit cooldown does not knock it back (PMMP `Living::attack` only knocks back cold hits).
    #[test]
    fn burning_arrows_set_players_on_fire_and_cooldown_hits_do_not_knock_back() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(0.0, 8.0));
        calls(|c| {
            c.look = (0.0, 0.0);
            c.arrows = 1;
            c.hit_cooldown.insert(2);
        });
        let extra = item::Extra {
            enchantments: vec![(Enchantment::Flame, 1)],
            ..item::Extra::default()
        };
        let bow = stack("minecraft:bow", 1).with_extra(Some(extra)).unwrap();
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), bow);
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        h.take(2);
        for _ in 0..5 {
            h.entities();
        }
        assert_eq!(calls(|c| c.damage.len()), 1, "the arrow hit");
        assert_eq!(calls(|c| c.fires.clone()), vec![(2, 5)]);
        let target = h.runtime_id(2);
        let pushed = h.take(2).into_iter().any(|p| {
            matches!(p,
            Packet::SetActorMotion(m) if m.entity_runtime_id == target)
        });
        assert!(!pushed, "no knockback within the hit cooldown");
    }

    /// A bow worn to its durability shoots without breaking further (PMMP
    /// `Durable::applyDamage` leaves a broken item as it is).
    #[test]
    fn an_already_broken_bow_is_not_destroyed_by_wear() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        calls(|c| {
            c.look = (0.0, 0.0);
            c.arrows = 1;
        });
        let bow = stack("minecraft:bow", 1);
        let max = bow.item().and_then(item::Type::max_durability).unwrap();
        let broken = bow.with_damage(max).unwrap();
        inventory::set_held(
            h.state.players.get_mut(&session(1)).unwrap(),
            broken.clone(),
        );
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        assert_eq!(h.state.players[&session(1)].inventory.held(), &broken);
    }

    thread_local! {
        /// The sources of the damage events a test heard.
        static SOURCES: std::cell::Cell<Vec<crate::event::DamageSource>> =
            const { std::cell::Cell::new(Vec::new()) };
    }

    /// An arrow that hits a player names the arrow and its shooter in `EntityDamage` (PMMP
    /// `EntityDamageByChildEntityEvent`).
    #[test]
    fn an_arrow_hit_names_its_shooter_in_entity_damage() {
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            |e: &mut crate::event::EntityDamage, _: &mut crate::tick::Context<'_>| {
                SOURCES.with(|s| {
                    let mut list = s.take();
                    list.push(e.source());
                    s.set(list);
                });
            },
        );
        let mut h = Harness::with_listeners(1, l);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(0.0, 8.0));
        h.look(1, 0.0, 0.0);
        let player = h.state.players.get_mut(&session(1)).unwrap();
        inventory::set_held(player, stack("minecraft:bow", 1));
        assert!(inventory::give(player, stack("minecraft:arrow", 1)).is_empty());
        SOURCES.with(std::cell::Cell::take);
        assert!(super::super::release_bow(&mut h.state, session(1), 20));
        for _ in 0..5 {
            h.step(Vec::new());
        }
        let shooter = Some(Actor::Player(crate::player::Id::new(session(1))));
        let sources = SOURCES.with(std::cell::Cell::take);
        assert!(
            matches!(sources.as_slice(), [DamageSource::Child { owner, .. }] if *owner == shooter),
            "{sources:?}"
        );
    }

    /// The running server's hooks: throws fly along the look the survival rules keep, and a bow
    /// in survival takes its arrow from the off hand first (PMMP `Bow::onReleaseUsing`); Infinity
    /// needs an arrow but keeps it.
    #[test]
    fn the_running_server_aims_with_the_look_and_takes_arrows_off_hand_first() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.look(1, 90.0, 0.0);
        let snowball = stack("minecraft:snowball", 1);
        assert!(super::super::throw(&mut h.state, session(1), &snowball));
        let (_, motion, _) = projectile(&h).unwrap();
        assert!(motion.x < -1.0 && motion.z.abs() < 1e-6, "west: {motion:?}");
        let player = h.state.players.get_mut(&session(1)).unwrap();
        inventory::set_held(player, stack("minecraft:bow", 1));
        assert!(
            !super::super::release_bow(&mut h.state, session(1), 20),
            "no arrow to shoot"
        );
        let player = h.state.players.get_mut(&session(1)).unwrap();
        inventory::set_offhand(player, stack("minecraft:arrow", 2));
        assert!(inventory::give(player, stack("minecraft:arrow", 5)).is_empty());
        assert!(super::super::release_bow(&mut h.state, session(1), 20));
        let arrows = |h: &Harness| {
            let inv = &h.state.players[&session(1)].inventory;
            let main: u8 = (0..36)
                .filter_map(|i| inv.main_stack(i))
                .filter(|s| s.item().and_then(item::Type::kind) == Some(item::Kind::Arrow))
                .map(Stack::count)
                .sum();
            (inv.offhand().count(), main)
        };
        assert_eq!(arrows(&h), (1, 5), "the off hand pays first");
        let extra = item::Extra {
            enchantments: vec![(Enchantment::Infinity, 1)],
            ..item::Extra::default()
        };
        let infinity = stack("minecraft:bow", 1).with_extra(Some(extra)).unwrap();
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), infinity);
        assert!(super::super::release_bow(&mut h.state, session(1), 20));
        assert_eq!(arrows(&h), (1, 5), "Infinity keeps the arrow");
    }

    #[test]
    fn arrows_stick_in_blocks_are_picked_up_and_go_after_a_minute() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let wall = BlockPos::new(0, -59, 6);
        h.state.worlds[0]
            .set_block(wall, named("minecraft:stone"))
            .unwrap();
        calls(|c| {
            c.look = (0.0, 0.0);
            c.arrows = 2;
        });
        let bow = stack("minecraft:bow", 1);
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), bow.clone());
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        for _ in 0..4 {
            h.entities();
        }
        let (position, motion, p) = projectile(&h).unwrap();
        assert_eq!(p.stuck, Some(wall));
        assert_eq!(position.z, 6.0, "on the wall's face");
        assert_eq!(motion, Vec3::ZERO);
        // Breaking the wall lets it fall.
        h.state.worlds[0].set_block(wall, block::AIR).unwrap();
        super::super::block_changed(&mut h.state, 0, wall);
        h.entities();
        h.entities();
        assert_eq!(projectile(&h).unwrap().2.stuck, None);
        for _ in 0..40 {
            h.entities();
        }
        let (position, _, p) = projectile(&h).unwrap();
        assert!(p.stuck.is_some(), "stuck in the ground");
        // The player walks to it: it fits, so it is picked up.
        room(64);
        place(&mut h, 1, Vec3::new(position.x, -58.38, position.z));
        h.entities();
        assert!(projectile(&h).is_none());
        assert_eq!(taken(), 1);
        // A second arrow stuck far away goes after 1,200 ticks stuck.
        h.teleport(1, standing(0.0, 0.0));
        calls(|c| c.look = (180.0, 0.0));
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), bow);
        h.state.worlds[0]
            .set_block(BlockPos::new(0, -59, -6), named("minecraft:stone"))
            .unwrap();
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        place(&mut h, 1, Vec3::new(20.5, -58.38, 20.5));
        let mut stuck_at = None;
        for tick in 0..1300 {
            h.entities();
            match projectile(&h) {
                Some((_, _, p)) if p.stuck.is_some() && stuck_at.is_none() => {
                    stuck_at = Some(tick);
                }
                None => {
                    assert_eq!(tick - stuck_at.unwrap(), 1200, "a minute stuck");
                    return;
                }
                Some(_) => {}
            }
        }
        panic!("the arrow stays");
    }

    #[test]
    fn an_ender_pearl_teleports_its_thrower_and_hurts_it_by_five() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        calls(|c| c.look = (0.0, 20.0));
        assert!(throw_with(
            &mut h.state,
            session(1),
            &stack("minecraft:ender_pearl", 1),
            TEST_HOOKS
        ));
        let mut landing = None;
        for _ in 0..60 {
            let before = projectile(&h);
            h.entities();
            if projectile(&h).is_none() {
                landing = before;
                break;
            }
        }
        assert!(landing.is_some(), "it lands");
        let teleports = calls(|c| c.teleports.clone());
        assert_eq!(teleports.len(), 1);
        let (who, world, eye) = teleports[0];
        assert_eq!((who, world), (1, 0));
        assert_eq!(eye.y, -60.0 + EYE_HEIGHT, "on the ground it hit");
        assert!(eye.z > 2.0, "forward: {eye:?}");
        assert_eq!(
            calls(|c| c.damage.clone()),
            vec![(1, PEARL_DAMAGE, survival::Damage::Fall)]
        );
    }

    #[test]
    fn splash_potions_give_bds_durations_and_instant_effects_weaker_from_the_eyes() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(2.0, 0.0));
        h.spawn(3, standing(7.0, 0.0));
        calls(|c| c.look = (0.0, 90.0));
        assert!(throw_with(
            &mut h.state,
            session(1),
            &potion_item("Regeneration", false),
            TEST_HOOKS
        ));
        for _ in 0..10 {
            h.entities();
        }
        assert!(projectile(&h).is_none(), "it splashed");
        // It falls straight down onto the ground under player 1's eyes.
        let hit = Vec3::new(0.5, -60.0, 0.5);
        let effects = calls(|c| c.effects.clone());
        let regeneration = Potion::all()
            .iter()
            .find(|p| format!("{p:?}") == "Regeneration")
            .unwrap()
            .effects()[0];
        // BDS (P6 `itemuse`): the whole duration at the feet, 1.05 − 0.225 × 2 of it two blocks
        // from the body's middle line.
        let full = f64::from(regeneration.duration_ticks());
        let expected = [full, (full * 0.6).round()];
        let eyes = [1, 2].map(|n| h.state.players[&session(n)].position);
        assert_eq!(effects.len(), 2, "player 3 is beyond 4 blocks: {effects:?}");
        for ((who, e), (n, ticks)) in effects.iter().zip([1u64, 2].into_iter().zip(expected)) {
            assert_eq!(*who, n);
            assert_eq!(e.effect(), regeneration.effect());
            assert_eq!(f64::from(e.duration_ticks()), ticks);
        }
        // Harming: instant damage scaled the same way.
        calls(|c| c.damage.clear());
        assert!(throw_with(
            &mut h.state,
            session(1),
            &potion_item("Harming", false),
            TEST_HOOKS
        ));
        for _ in 0..10 {
            h.entities();
        }
        let damage = calls(|c| c.damage.clone());
        let near = 6.0 * (1.0 - eyes[0].distance_squared(hit).sqrt() / SPLASH_RANGE);
        assert_eq!(damage.first(), Some(&(1, near, survival::Damage::Magic)));
        assert_eq!(damage.len(), 2);
    }

    #[test]
    fn a_lingering_potion_leaves_a_cloud_that_reapplies_after_its_delay_and_shrinks() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        calls(|c| c.look = (0.0, 90.0));
        assert!(throw_with(
            &mut h.state,
            session(1),
            &potion_item("Regeneration", true),
            TEST_HOOKS
        ));
        let mut cloud = None;
        for _ in 0..10 {
            h.entities();
            cloud = h.live().into_iter().find_map(|e| match &e.kind {
                Kind::Cloud(c) => Some((e.body.position, c.clone())),
                _ => None,
            });
            if cloud.is_some() {
                break;
            }
        }
        let (position, c) = cloud.expect("a cloud");
        assert_eq!(position, Vec3::new(0.5, -59.5, 0.5));
        assert_eq!(c.radius, CLOUD_RADIUS);
        for _ in 0..60 {
            h.entities();
        }
        let effects = calls(|c| c.effects.clone());
        assert_eq!(effects.len(), 2, "at age 10 and again 40 ticks later");
        let regeneration = Potion::all()
            .iter()
            .find(|p| format!("{p:?}") == "Regeneration")
            .unwrap()
            .effects()[0];
        let quarter = (f64::from(regeneration.duration_ticks()) / 4.0).round();
        assert_eq!(f64::from(effects[0].1.duration_ticks()), quarter);
        let shrunk = h.live().into_iter().find_map(|e| match &e.kind {
            Kind::Cloud(c) => Some(c.radius),
            _ => None,
        });
        let expected = CLOUD_RADIUS + 60.0 * CLOUD_RADIUS_PER_TICK + 2.0 * CLOUD_RADIUS_ON_USE;
        assert!((shrunk.unwrap() - expected).abs() < 1e-9);
        for _ in 0..600 {
            h.entities();
        }
        assert!(
            h.live().iter().all(|e| !matches!(e.kind, Kind::Cloud(_))),
            "gone"
        );
    }

    /// `experience-orbs = false` or `experience = false`: a bottle o' enchanting is not thrown
    /// (the caller keeps it); other projectiles still are.
    #[test]
    fn experience_orbs_off_refuses_the_bottle_throw() {
        for experience in [true, false] {
            let mut h = Harness::new(1);
            h.spawn(1, standing(0.0, 0.0));
            h.state.settings.experience = experience;
            h.state.settings.experience_orbs = !experience;
            let bottle = stack("minecraft:experience_bottle", 1);
            assert!(!throw_with(&mut h.state, session(1), &bottle, TEST_HOOKS));
            assert!(h.live().is_empty());
            let snowball = stack("minecraft:snowball", 1);
            assert!(throw_with(&mut h.state, session(1), &snowball, TEST_HOOKS));
            assert_eq!(h.live().len(), 1);
        }
    }

    #[test]
    fn an_experience_bottle_drops_three_to_eleven_points() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.teleport(1, standing(20.0, 0.0));
        calls(|c| c.look = (0.0, 0.0));
        let at = h.state.players[&session(1)].position;
        let kind = Kind::Projectile(Projectile {
            shot: Shot::ExperienceBottle,
            damage: 0.0,
            stuck: None,
            owner: None,
            owner_runtime_id: 0,
        });
        let start = at + Vec3::new(-20.0, 0.0, 0.0);
        assert!(
            create(&mut h.state, 0, start, kind, |_| (
                Vec3::new(0.0, -1.0, 0.0),
                0.0,
                0.0
            ))
            .is_some()
        );
        for _ in 0..10 {
            h.entities();
        }
        let points: u32 = h
            .live()
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::ExperienceOrb(o) => Some(u32::from(o.value)),
                _ => None,
            })
            .sum();
        assert!((3..=11).contains(&points), "{points}");
    }

    #[test]
    fn a_burning_arrow_lights_tnt_and_players_pick_nothing_up_when_dead() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let tnt = BlockPos::new(0, -59, 5);
        h.state.worlds[0]
            .set_block(tnt, named("minecraft:tnt"))
            .unwrap();
        calls(|c| {
            c.look = (0.0, 0.0);
            c.arrows = 1;
        });
        let flame = stack("minecraft:bow", 1)
            .with_extra(Some(item::Extra {
                enchantments: vec![(Enchantment::Flame, 1)],
                ..item::Extra::default()
            }))
            .unwrap();
        inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), flame);
        assert!(release_bow_with(&mut h.state, session(1), 20, TEST_HOOKS));
        for _ in 0..4 {
            h.entities();
        }
        assert!(
            h.live().iter().any(|e| matches!(e.kind, Kind::Tnt(_))),
            "lit"
        );
        assert!(!h.state.worlds[0].block(tnt).unwrap().is(BlockClass::Tnt));
        // A dead player does not pick up items.
        calls(|c| c.dead.insert(1));
        room(64);
        assert!(super::super::drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", 3)
        ));
        for _ in 0..20 {
            h.entities();
        }
        assert_eq!(taken(), 0);
    }
}
