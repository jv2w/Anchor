//! Projectiles and potion clouds (PMMP `Projectile`, `Arrow`, `Throwable` and its kinds,
//! `ProjectileItem::onClickAir`, `Bow::onReleaseUsing`, `AreaEffectCloud`).
//!
//! A projectile flies along a ray each tick ([`super::physics`]); the serial phase then looks for
//! the nearest player or NPC on the ray (not the shooter in its first 5 ticks) and resolves the
//! hit: the kind's effect (an arrow's damage and knockback, an ender pearl's teleport, a
//! potion's splash or cloud, a bottle's experience), then a block hit sticks an arrow and ends a
//! thrown item, an entity hit ends any projectile. Eggs hatch nothing (mobs are out of scope,
//! CLAUDE.md 1).
//!
//! Fixed PMMP defects: a critical arrow's extra damage is counted before the hit clears the
//! critical flag (PMMP clears it first, so the bonus never applies); arrows shot by players
//! with infinite items (creative) are picked up by creative players only, so no one else gets
//! arrows out of nothing; a lingering potion still lingers after a load.

use super::SessionId;
use super::kind::{Arrow, Cloud, Kind, MAX_CLOUD_EFFECTS, Pickup, Projectile, Shot, Victim};
use super::physics::{BlockHit, Flight, intercept};
use super::{
    Hooks, Hurt, Knock, PLAYER_EYE_HEIGHT, State, create, direction, hurt, level_event, level_of,
    npc, player_box, send_to, sound, targetable, with, worn,
};
use crate::block::{self, Face, TypeTag};
use crate::config::GameMode;
use crate::effect::{self, Potion};
use crate::entity::Id;
use crate::item::{self, Class, Enchantment, Stack};
use crate::math::{Aabb, Vec3};
use crate::protocol::consts::{ActorEvent, LevelEvent, SoundEvent};
use crate::protocol::packet::{Packet, TakeItemActor};
use crate::random::{self, Random};
use crate::tick::{Phase, inventory, survival};

/// An arrow's base damage (PMMP `Arrow::$damage`).
pub(super) const ARROW_DAMAGE: f64 = 2.0;
/// A cloud's first radius, life and reapplication delay (PMMP `AreaEffectCloud::DEFAULT_*`).
pub(super) const CLOUD_RADIUS: f64 = 3.0;
pub(super) const CLOUD_DURATION: i32 = 600;
pub(super) const CLOUD_REAPPLICATION: i32 = 40;
/// A cloud's radius change per entity it affects and per tick.
pub(super) const CLOUD_RADIUS_ON_USE: f64 = -0.5;
pub(super) const CLOUD_RADIUS_PER_TICK: f64 = -(CLOUD_RADIUS / 600.0);
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
/// The experience a bottle drops (PMMP `ExperienceBottle::onHit`: `mt_rand(3, 11)`).
const BOTTLE_EXPERIENCE: (i32, i32) = (3, 11);
/// The lingering potion item (PMMP's lingering potion is a splash potion that lingers).
const LINGERING_POTION: &str = "minecraft:lingering_potion";
/// The fuse of TNT lit by a burning projectile (PMMP `TNT::onProjectileHit`: `ignite()`).
const PROJECTILE_FUSE: u32 = 80;
/// Random streams.
const CRITICAL_RANDOM: u64 = random::tag("projectile-critical");
const BOTTLE_RANDOM: u64 = random::tag("projectile-bottle");
const BOW_RANDOM: u64 = random::tag("projectile-bow-wear");

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
        Shot::ExperienceBottle
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
    if create(state, world, eye, kind, |_| (motion, yaw, pitch)).is_none() {
        return false;
    }
    let feet = eye - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    sound(state, world, feet, SoundEvent::Throw);
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
    let finite = matches!(mode, GameMode::Survival | GameMode::Adventure);
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
    let motion = direction(yaw, pitch) * (force * 3.0);
    let turned = (if yaw > 180.0 { 360.0 } else { 0.0 }) - yaw;
    let Some(id) = create(state, world, eye, kind, |_| (motion, turned, -pitch)) else {
        return false;
    };
    if level_of(&bow, Enchantment::Flame) > 0 {
        with(state, id, |e| e.fire_ticks = FLAME_TICKS);
    }
    let feet = eye - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    sound(state, world, feet, SoundEvent::Bow);
    if finite {
        let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
        let mut r = Random::new(seed, state.tick, random::key(BOW_RANDOM, &[session.get()]));
        if let Some(player) = state.players.get_mut(&session) {
            if !infinity {
                (hooks.take_arrow)(player);
            }
            inventory::set_held(player, worn(&bow, 1, &mut r));
        }
    }
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
    let (width, height) = this.kind.size();
    let half = width / 2.0;
    let start = flight.start;
    let start_box = Aabb::new(
        Vec3::new(start.x - half, start.y, start.z - half),
        Vec3::new(start.x + half, start.y + height, start.z + half),
    );
    let mut end = flight.end;
    let mut struck = None;
    if !was_on_ground {
        let area = start_box
            .stretch(end - start)
            .grow(Vec3::new(1.0, 1.0, 1.0));
        let mut best = f64::MAX;
        let entities = &state.rules.entities;
        for other in entities.nearby(world, area) {
            let Some(other_entity) = entities.get(other) else {
                continue;
            };
            if other == id || !other_entity.kind.is_living() {
                continue;
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
        }
        for (&session, player) in &state.players {
            if !targetable(state, player, world, hooks)
                || !player_box(player).overlaps(area, super::EPSILON)
                || (owner == Some(session) && age < OWNER_GRACE_TICKS)
            {
                continue;
            }
            let reach = player_box(player).grow(Vec3::new(HIT_REACH, HIT_REACH, HIT_REACH));
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
        Shot::Snowball | Shot::Egg | Shot::EnderPearl => Some(p.damage.ceil()),
        Shot::Potion { .. } | Shot::ExperienceBottle => None,
    }
}

/// A hit (PMMP `onHit`, then `onHitEntity` or `onHitBlock`).
fn on_hit(state: &mut State, id: Id, at: Vec3, motion: Vec3, struck: Struck, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let Kind::Projectile(p) = &e.kind else {
        return;
    };
    let p = p.clone();
    let (world, burning, runtime_id) = (e.world, e.fire_ticks > 0, e.runtime_id);
    let damage = result_damage(state, id, &p, motion);
    match p.shot {
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
            sound(state, world, at, SoundEvent::BowHit);
        }
        Shot::Snowball | Shot::Egg => {}
        Shot::EnderPearl => pearl(state, world, at, p.owner, hooks),
        Shot::ExperienceBottle => {
            splash_effect(state, world, at, super::track::DEFAULT_POTION_COLOUR);
            let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
            let mut r = Random::new(seed, state.tick, random::key(BOTTLE_RANDOM, &[runtime_id]));
            let amount = r.range(BOTTLE_EXPERIENCE.0, BOTTLE_EXPERIENCE.1);
            super::drop_experience(state, world, at, amount.unsigned_abs());
        }
        Shot::Potion { potion, linger } => {
            potion_hit(state, id, world, at, potion, linger, struck, hooks);
        }
    }
    match struck {
        Struck::Block(b) => {
            with(state, id, |e| {
                if let Kind::Projectile(p) = &mut e.kind {
                    p.stuck = Some(b.pos);
                }
            });
            if burning
                && state
                    .worlds
                    .get(world)
                    .and_then(|w| w.block(b.pos))
                    .is_some_and(|s| s.is(block::Class::Tnt))
            {
                super::explosion::light(state, world, b.pos, PROJECTILE_FUSE);
            }
            if matches!(p.shot, Shot::Arrow(_)) {
                if let Some(e) = state.rules.entities.get(id) {
                    let (viewers, runtime_id) = (e.viewers.clone(), e.runtime_id);
                    let packet = super::actor_event(runtime_id, ActorEvent::Shake, ARROW_SHAKE);
                    send_to(&mut state.players, &viewers, &packet);
                }
            } else {
                state.rules.entities.doom(id);
            }
        }
        Struck::Player(session) => {
            if let Some(d) = damage {
                hit_player(state, session, d, motion, &p, burning, hooks);
            }
            state.rules.entities.doom(id);
        }
        Struck::Entity(target) => {
            if let Some(d) = damage {
                let knock = Knock::new(motion.x, motion.z);
                let landed = hurt(state, target, super::Cause::Projectile, d, Some(knock));
                if landed != Hurt::Ignored {
                    if burning {
                        with(state, target, |e| e.set_on_fire(HIT_FIRE_SECONDS));
                    }
                    if let Some(push) = punch(&p, motion) {
                        with(state, target, |e| {
                            e.body.motion = super::physics::bounded(e.body.motion + push);
                        });
                    }
                }
            }
            state.rules.entities.doom(id);
        }
    }
}

/// The extra push of a Punch arrow (PMMP `Arrow::onHitEntity`).
fn punch(p: &Projectile, motion: Vec3) -> Option<Vec3> {
    let Shot::Arrow(a) = p.shot else {
        return None;
    };
    let horizontal = (motion.x * motion.x + motion.z * motion.z).sqrt();
    if a.punch == 0 || horizontal.is_nan() || horizontal <= 0.0 {
        return None;
    }
    let m = f64::from(a.punch) * 0.6 / horizontal;
    Some(Vec3::new(motion.x * m, 0.1, motion.z * m))
}

/// A projectile hits a player (PMMP `Projectile::onHitEntity`): the damage; the knockback along
/// the flight only on a hit outside the player's hit cooldown (PMMP `Living::attack` with the
/// child entity's motion); a burning projectile sets the player on fire for 5 seconds; a Punch
/// arrow's push. The pushes are sent as the player's motion. A hit that does not land (creative,
/// spectator, a weaker hit within the cooldown) does nothing else, as for NPCs (PMMP still burns
/// and pushes then).
fn hit_player(
    state: &mut State,
    session: SessionId,
    damage: f64,
    motion: Vec3,
    p: &Projectile,
    burning: bool,
    hooks: Hooks,
) {
    let cold = state
        .players
        .get(&session)
        .is_some_and(|player| !(hooks.has_attack_cooldown)(player));
    if !(hooks.damage)(state, session, damage, survival::Damage::Entity) {
        return;
    }
    if burning {
        (hooks.set_on_fire)(state, session, u32::from(HIT_FIRE_SECONDS.unsigned_abs()));
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
        super::push_player(state, session, push);
    }
}

/// An ender pearl lands at `at` (PMMP `EnderPearl::onHit`): its thrower, if still in the world
/// and alive, is moved there and takes 5 fall damage.
fn pearl(state: &mut State, world: usize, at: Vec3, owner: Option<SessionId>, hooks: Hooks) {
    let Some(owner) = owner else {
        return;
    };
    let Some(player) = state.players.get(&owner) else {
        return;
    };
    if player.world != world || player.phase != Phase::Spawned || !(hooks.is_alive)(player) {
        return;
    }
    let origin = player.position - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    level_event(state, world, origin, LevelEvent::ParticlesTeleport, 0);
    level_event(state, world, origin, LevelEvent::SoundEndermanTeleport, 0);
    (hooks.teleport)(
        state,
        owner,
        world,
        at + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0),
    );
    level_event(state, world, at, LevelEvent::SoundEndermanTeleport, 0);
    (hooks.damage)(state, owner, PEARL_DAMAGE, survival::Damage::Fall);
}

/// The splash particle in a colour and the glass sound (PMMP `PotionSplashParticle`,
/// `PotionSplashSound`).
fn splash_effect(state: &mut State, world: usize, at: Vec3, colour: [u8; 4]) {
    let data = super::track::argb(colour);
    level_event(state, world, at, LevelEvent::ParticlesPotionSplash, data);
    sound(state, world, at, SoundEvent::Glass);
}

/// A potion hits (PMMP `SplashPotion::onHit`): a splash gives its effects to the players and
/// NPCs within 4 blocks of their eyes, weaker with the distance (full to what it hit); a
/// lingering potion leaves a cloud; a water bottle puts out the fire it lands by.
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
        let cloud = Cloud {
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
        };
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
            for &effect in effects {
                if effect.effect().is_instant() {
                    instant(state, target, effect, potency, hooks);
                } else {
                    let ticks = (f64::from(effect.duration()) * 0.75 * potency).round();
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
                targetable(state, p, world, hooks) && player_box(p).overlaps(area, super::EPSILON)
            })
            .map(|(&s, _)| Struck::Player(s)),
    );
    out
}

/// The eye position of a player or NPC.
fn eye_of(state: &State, target: Struck) -> Option<Vec3> {
    match target {
        Struck::Player(s) => state.players.get(&s).map(|p| p.position),
        Struck::Entity(id) => {
            let e = state.rules.entities.get(id)?;
            Some(e.body.position + Vec3::new(0.0, npc::eye_height(&e.kind), 0.0))
        }
        Struck::Block(_) => None,
    }
}

/// The feet position of a player or NPC.
fn feet_of(state: &State, target: Struck) -> Option<Vec3> {
    match target {
        Struck::Player(s) => state
            .players
            .get(&s)
            .map(|p| p.position - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0)),
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
/// effects could change; PLAN.md 10.7 decision).
fn give(state: &mut State, target: Struck, effect: effect::Instance, hooks: Hooks) {
    if let Struck::Player(s) = target {
        (hooks.add_effect)(state, s, effect);
    }
}

/// An instant effect at `potency` (PMMP `InstantHealthEffect`: heals (4 << amplifier) ×
/// potency; `InstantDamageEffect`: (6 << amplifier) × potency magic damage).
fn instant(
    state: &mut State,
    target: Struck,
    effect: effect::Instance,
    potency: f64,
    hooks: Hooks,
) {
    let level = u32::from(effect.amplifier()).min(16);
    match effect.effect() {
        effect::Type::InstantHealth => {
            let amount = f64::from(4_u32 << level) * potency;
            match target {
                Struck::Player(s) => (hooks.heal)(state, s, amount),
                Struck::Entity(id) => {
                    with(state, id, |e| {
                        let max = e.kind.max_health();
                        e.health = crate::math::min(e.health + amount, max);
                    });
                }
                Struck::Block(_) => {}
            }
        }
        effect::Type::InstantDamage => {
            let amount = f64::from(6_u32 << level) * potency;
            match target {
                Struck::Player(s) => {
                    (hooks.damage)(state, s, amount, survival::Damage::Magic);
                }
                Struck::Entity(id) => {
                    hurt(state, id, super::Cause::Magic, amount, None);
                }
                Struck::Block(_) => {}
            }
        }
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
pub(super) fn pick_up(state: &mut State, session: SessionId, id: Id, hooks: Hooks) -> bool {
    let Some(e) = state.rules.entities.get(id).filter(|e| !e.flagged) else {
        return false;
    };
    let Kind::Projectile(Projectile {
        shot: Shot::Arrow(a),
        stuck: Some(_),
        ..
    }) = &e.kind
    else {
        return false;
    };
    let pickup = a.pickup;
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let finite = matches!(
        survival::game_mode(state, player),
        GameMode::Survival | GameMode::Adventure
    );
    if pickup == Pickup::None || (pickup == Pickup::Creative && finite) {
        return false;
    }
    if finite {
        let Some(arrow) =
            item::Type::from_name("minecraft:arrow", 0, None, &crate::nbt::Compound::new())
        else {
            return false;
        };
        let Some(player) = state.players.get_mut(&session) else {
            return false;
        };
        if !(hooks.give)(player, Stack::new(arrow, 1)).is_empty() {
            return false;
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
    let mut until = Vec::with_capacity(affected.len());
    for &(target, victim) in &affected {
        for &effect in &effects {
            if effect.effect().is_instant() {
                instant(state, target, effect, 0.5, hooks);
            } else {
                let ticks = (f64::from(effect.duration()) / 4.0).round();
                give(state, target, with_duration(effect, ticks), hooks);
            }
        }
        until.push(victim);
    }
    with(state, id, |e| {
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
    });
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Harness, TEST_HOOKS, at, calls, room, standing, taken};
    use super::*;
    use crate::block::Class as BlockClass;
    use crate::nbt::Compound;
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
            let small = |v: f64| if v.abs() <= 0.000_01 { 0.0 } else { v };
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
        let bow = stack("minecraft:bow", 1).with_extra(Some(extra));
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
        let infinity = stack("minecraft:bow", 1).with_extra(Some(extra));
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
        assert_eq!(eye.y, -60.0 + PLAYER_EYE_HEIGHT, "on the ground it hit");
        assert!(eye.z > 2.0, "forward: {eye:?}");
        assert_eq!(
            calls(|c| c.damage.clone()),
            vec![(1, PEARL_DAMAGE, survival::Damage::Fall)]
        );
    }

    #[test]
    fn splash_potions_give_effects_weaker_with_the_distance_from_the_eyes() {
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
        let expected = |eye: Vec3| {
            let potency = 1.0 - eye.distance_squared(hit).sqrt() / SPLASH_RANGE;
            (f64::from(regeneration.duration()) * 0.75 * potency).round()
        };
        let eyes = [1, 2].map(|n| h.state.players[&session(n)].position);
        assert_eq!(effects.len(), 2, "player 3 is beyond 4 blocks: {effects:?}");
        for ((who, e), (n, eye)) in effects.iter().zip([1u64, 2].into_iter().zip(eyes)) {
            assert_eq!(*who, n);
            assert_eq!(e.effect(), regeneration.effect());
            assert_eq!(f64::from(e.duration()), expected(eye));
        }
        assert!(effects[0].1.duration() > effects[1].1.duration());
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
        let quarter = (f64::from(regeneration.duration()) / 4.0).round();
        assert_eq!(f64::from(effects[0].1.duration()), quarter);
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
        let flame = stack("minecraft:bow", 1).with_extra(Some(item::Extra {
            enchantments: vec![(Enchantment::Flame, 1)],
            ..item::Extra::default()
        }));
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
