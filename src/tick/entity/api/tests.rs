//! Tests of the public entity API.
//!
//! They cover every getter and setter, spawning, queries through the chunk index, and that
//! spawning and despawning through the API leaves the store as it was.

use super::*;
use crate::effect::Potion;
use crate::entity::Projectile;
use crate::nbt::Value;
use crate::protocol::consts::{EntityDataFlag as F, EntityDataKey};
use crate::protocol::packet::Packet;
use crate::protocol::{EntityMetadata, FlagSet, MetadataValue};
use crate::tick::entity::kind::Projectile as Thrown;
use crate::tick::entity::saved::tests::{kinds, stack};
use crate::tick::entity::tests::{Harness, adds, at, removes, standing};
use crate::tick::entity::{EntityTypes, saved};

/// The world the tests' entities live in.
const WORLD: world::Id = world::Id::DEFAULT;

/// A harness with player 1 standing at block (0, 0), spawned.
fn harness() -> Harness {
    let mut h = Harness::new(1);
    h.spawn(1, standing(0.0, 0.0));
    h
}

fn context(h: &mut Harness) -> Context<'_> {
    Context {
        state: &mut h.state,
    }
}

/// Every kind made at a spot of its own near the player, with its ID.
fn every_kind(h: &mut Harness) -> Vec<(Id, Kind)> {
    let mut out = Vec::new();
    for (i, kind) in (0_u32..).zip(kinds()) {
        let x = f64::from(i % 6) * 2.0 - 5.0;
        let z = f64::from(i / 6) * 2.0 - 5.0;
        let id = create(&mut h.state, 0, at(x, -60.0, z), kind.clone(), |_| {
            (Vec3::ZERO, 0.0, 0.0)
        })
        .unwrap();
        out.push((id, kind));
    }
    out
}

/// What a painting, an end crystal or a firework rocket reads as.
fn object_reads(e: &Entity, kind: &Kind) {
    match kind {
        Kind::Painting(p) => {
            assert_eq!(e.painting_motive(), Some(p.motive));
            assert_eq!(e.painting_facing(), Some(p.facing));
        }
        Kind::EndCrystal(c) => {
            assert_eq!(e.shows_base(), c.show_base);
            assert_eq!(e.beam_target(), c.beam_target);
        }
        Kind::Firework(f) => {
            assert_eq!(e.firework_explosions(), Some(f.explosions.as_slice()));
            assert_eq!(e.max_flight_time_ticks(), Some(f.max_flight));
        }
        _ => {}
    }
}

// api: Entity::getId, Entity::getNetworkTypeId, Entity::getWorld, Position::getWorld,
// api: Entity::getPosition, Entity::getLocation, Location::getYaw, Location::getPitch,
// api: Entity::getMotion, Entity::getBoundingBox, Entity::getEyeHeight, Entity::getEyePos,
// api: Entity::getDirectionVector, Entity::getHealth, Entity::getMaxHealth,
// api: Living::getMaxHealth, Entity::isAlive, Entity::isOnGround, Entity::getFallDistance,
// api: Entity::isFireProof, Entity::canBeCollidedWith, ItemEntity, PrimedTNT, Painting,
// api: entity\object\EndCrystal, entity\object\FireworkRocket, entity\projectile\Trident,
// api: entity\projectile\IceBomb, Painting::getMotive, Painting::getFacing,
// api: entity\object\EndCrystal::showBase, entity\object\EndCrystal::getBeamTarget,
// api: entity\object\FireworkRocket::getExplosions,
// api: entity\object\FireworkRocket::getMaxFlightTimeTicks, entity\projectile\Trident::getItem,
// api: entity\projectile\Arrow, entity\projectile\Snowball, entity\projectile\Egg,
// api: entity\projectile\EnderPearl, entity\projectile\SplashPotion,
// api: entity\projectile\ExperienceBottle, ItemEntity::getItem, ItemEntity::getPickupDelay,
// api: ItemEntity::getDespawnDelay, ExperienceOrb::getDespawnDelay, ItemEntity::getOwner,
// api: ItemEntity::getThrower, ExperienceOrb::getXpValue, FallingBlock::getBlock,
// api: PrimedTNT::getFuse, PrimedTNT::worksUnderwater, Projectile::getBaseDamage,
// api: entity\projectile\Arrow::isCritical, entity\projectile\Arrow::getPickupMode,
// api: entity\projectile\Arrow::getPunchKnockback,
// api: entity\projectile\SplashPotion::getPotionType, entity\projectile\SplashPotion::willLinger,
// api: AreaEffectCloud::getRadius, Villager::getProfession
#[test]
fn every_kind_reads_as_what_it_is() {
    let mut h = harness();
    let made = every_kind(&mut h);
    let cx = context(&mut h);
    for (id, kind) in &made {
        let e = cx.entity(*id).unwrap();
        assert!(e.runtime_id().is_some());
        assert_eq!(e.entity_type(), kind.entity_type());
        assert_eq!(e.network_type(), kind.identifier());
        assert_eq!(e.world(), WORLD);
        assert_eq!(e.position(), e.body.position);
        assert_eq!((e.yaw(), e.pitch()), (0.0, 0.0));
        assert!(e.motion().is_finite());
        let bounds = e.bounding_box();
        if let Kind::Painting(p) = kind {
            // The picture on its wall, not a box around its position.
            let block = crate::world::BlockPos::containing(e.position());
            assert_eq!(bounds, object::painting_box(block, p.facing, p.motive));
        } else {
            assert!(bounds.max.y > bounds.min.y && bounds.min.y == e.position().y);
            assert!(e.eye_height() > 0.0 && e.eye_height() <= bounds.max.y - bounds.min.y);
        }
        assert_eq!(e.eye_position().y, e.position().y + e.eye_height());
        assert!((e.direction().length_squared() - 1.0).abs() < 1e-9);
        assert_eq!(e.health(), e.max_health());
        assert!(e.is_alive());
        assert!(e.fall_distance() >= 0.0);
        assert_eq!(e.is_fire_proof(), kind.is_fire_proof());
        assert_eq!(e.can_be_collided_with(), kind.is_obstacle());
        match kind {
            Kind::Item(item) => {
                assert_eq!(e.item(), Some(&item.stack));
                assert_eq!(e.pickup_delay_ticks(), Some(item.pickup_delay));
                assert_eq!(e.despawn_delay_ticks(), Some(item.despawn_delay));
                assert_eq!(e.owner(), Some(item.owner.as_str()));
                assert_eq!(e.thrower(), Some(item.thrower.as_str()));
                assert_eq!(e.entity_type(), Type::Item);
            }
            Kind::XpOrb(orb) => {
                assert_eq!(e.xp_value(), Some(orb.value));
                assert_eq!(e.despawn_delay_ticks(), Some(orb.despawn_delay));
            }
            Kind::FallingBlock(f) => assert_eq!(e.falling_block(), Some(f.block)),
            Kind::Tnt(t) => {
                assert_eq!(e.fuse_ticks(), Some(t.fuse));
                assert_eq!(e.works_underwater(), t.underwater);
                assert_eq!(e.entity_type(), Type::PrimedTnt);
            }
            Kind::Projectile(p) => {
                assert_eq!(e.base_damage(), Some(p.damage));
                match &p.shot {
                    Shot::Arrow(a) => {
                        assert_eq!(e.is_critical(), a.critical);
                        assert_eq!(e.pickup_mode(), Some(a.pickup));
                        assert_eq!(e.punch_knockback(), Some(a.punch));
                    }
                    Shot::Potion { potion, linger } => {
                        assert_eq!(e.potion(), Some(*potion));
                        assert_eq!(e.will_linger(), *linger);
                    }
                    Shot::Trident(t) => {
                        assert_eq!(e.item(), Some(&t.item));
                        assert_eq!(e.entity_type(), Type::Trident);
                    }
                    _ => assert!(e.potion().is_none() && e.pickup_mode().is_none()),
                }
            }
            Kind::Cloud(c) => assert_eq!(e.cloud_radius(), Some(c.radius)),
            Kind::Npc(_) => {
                let mob = matches!(e.entity_type(), Type::Human | Type::Zombie);
                assert!(e.is_invulnerable() || mob);
                assert_eq!(e.profession().is_some(), e.entity_type() == Type::Villager);
            }
            Kind::Painting(_) | Kind::EndCrystal(_) | Kind::Firework(_) => object_reads(e, kind),
        }
    }
    let types: Vec<Type> = made.iter().map(|(_, k)| k.entity_type()).collect();
    for t in [
        Type::Arrow,
        Type::Snowball,
        Type::Egg,
        Type::EnderPearl,
        Type::SplashPotion,
        Type::XpBottle,
        Type::AreaEffectCloud,
        Type::FallingBlock,
        Type::XpOrb,
        Type::Painting,
        Type::EndCrystal,
        Type::FireworkRocket,
        Type::Trident,
        Type::IceBomb,
    ] {
        assert!(types.contains(&t), "{t:?}");
    }
    // A thing that is not that kind answers `None`.
    let (item, _) = made[0];
    let e = cx.entity(item).unwrap();
    assert!(e.fuse_ticks().is_none() && e.xp_value().is_none() && e.skin().is_none());
    assert!(e.owning_player().is_none() && e.target_player().is_none());
    assert!(e.painting_motive().is_none() && e.painting_facing().is_none());
    assert!(!e.shows_base() && e.beam_target().is_none());
    assert!(e.firework_explosions().is_none() && e.max_flight_time_ticks().is_none());
}

// api: Entity::getOwningEntity, Entity::getOwningEntityId, ExperienceOrb::getTargetPlayer,
// api: ExperienceOrb::hasTargetPlayer, Entity::getViewers
#[test]
fn owners_targets_and_viewers_are_players() {
    let mut h = harness();
    let me = crate::tick::SessionId::new(1);
    let arrow = Kind::Projectile(Thrown {
        shot: Shot::Snowball,
        damage: 0.0,
        stuck: None,
        owner: Some(me),
        owner_runtime_id: h.runtime_id(1),
    });
    let orb = Kind::XpOrb(Orb {
        value: 3,
        despawn_delay: 6000,
        look_for_target: 0,
        target: Some(me),
    });
    let a = create(&mut h.state, 0, at(4.5, -55.0, 4.5), arrow, |_| {
        (Vec3::ZERO, 0.0, 0.0)
    })
    .unwrap();
    let o = create(&mut h.state, 0, at(-6.5, -55.0, 4.5), orb, |_| {
        (Vec3::ZERO, 0.0, 0.0)
    })
    .unwrap();
    let player = player::Id::new(me);
    let cx = context(&mut h);
    assert_eq!(cx.entity(a).unwrap().owning_player(), Some(player));
    assert_eq!(cx.entity(o).unwrap().target_player(), Some(player));
    h.entities();
    let cx = context(&mut h);
    assert_eq!(
        cx.entity(a).unwrap().viewers().collect::<Vec<_>>(),
        vec![player]
    );
}

/// The metadata of the `SetActorData` packets for `runtime_id` in `packets`.
fn data_of(packets: &[Packet], runtime_id: u64) -> Vec<EntityMetadata> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::SetActorData(d) if d.entity_runtime_id == runtime_id => {
                Some(d.entity_metadata.clone())
            }
            _ => None,
        })
        .collect()
}

// api: Entity::setNameTag, Entity::getNameTag, Entity::setNameTagVisible,
// api: Entity::isNameTagVisible, Entity::setNameTagAlwaysVisible,
// api: Entity::isNameTagAlwaysVisible, Entity::setScale, Entity::getScale,
// api: Entity::setInvisible, Entity::isInvisible, Entity::setHasGravity, Entity::hasGravity
#[test]
fn looks_change_at_once_reach_the_viewers_and_are_saved() {
    let mut h = harness();
    let id = context(&mut h)
        .spawn_tnt(WORLD, at(2.5, -60.0, 2.5), 1000)
        .unwrap();
    for _ in 0..3 {
        h.entities();
    }
    h.take(1);
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    {
        let mut cx = context(&mut h);
        let mut e = cx.entity_mut(id).unwrap();
        assert!(e.set_name_tag("Boom"));
        assert!(!e.set_name_tag(&"x".repeat(1025)));
        e.set_name_tag_visible(false);
        e.set_name_tag_always_visible(true);
        assert!(e.set_scale(2.0));
        assert!(!e.set_scale(0.0) && !e.set_scale(65.0) && !e.set_scale(f64::NAN));
        e.set_invisible(true);
        e.set_has_gravity(false);
        assert_eq!(e.name_tag(), "Boom");
        assert!(!e.is_name_tag_visible() && e.is_name_tag_always_visible());
        assert_eq!(e.scale(), 2.0);
        assert!(e.is_invisible() && !e.has_gravity());
        let (w, h2) = (e.bounding_box().max.x - e.bounding_box().min.x, 0.98 * 2.0);
        assert!((w - h2).abs() < 1e-9, "the box scales");
    }
    h.entities();
    let packets = h.take(1);
    let data = data_of(&packets, runtime_id);
    assert_eq!(data.len(), 1, "one SetActorData with everything");
    let m = &data[0];
    assert_eq!(
        m.get(&(EntityDataKey::Name as u32)),
        Some(&MetadataValue::String("Boom".to_owned()))
    );
    assert_eq!(
        m.get(&(EntityDataKey::Scale as u32)),
        Some(&MetadataValue::Float32(2.0))
    );
    assert_eq!(
        m.get(&(EntityDataKey::AlwaysShowNameTag as u32)),
        Some(&MetadataValue::Byte(1))
    );
    // The flags lost gravity and the shown name and gained invisibility.
    let Some(MetadataValue::Int64(flags)) = m.get(&(EntityDataKey::Flags as u32)) else {
        panic!("{m:?}");
    };
    let set = FlagSet::<F>::from_words(&[flags.cast_unsigned()]).unwrap();
    assert!(set.contains(F::Invisible) && !set.contains(F::ShowName));
    assert!(!set.contains(F::HasGravity));
    // Nothing is sent again while nothing changes.
    h.entities();
    assert!(data_of(&h.take(1), runtime_id).is_empty());
    // The saved form keeps the looks; the gravity-less TNT stays where it is.
    let e = h.state.rules.entities.get(id).unwrap();
    let saved = saved::save(e);
    let loaded = saved::load(
        &saved,
        e.chunk,
        &EntityTypes::default(),
        &crate::custom::Registry::default(),
    )
    .unwrap();
    assert_eq!(loaded.looks.name_tag, "Boom");
    assert!(!loaded.looks.name_tag_visible && loaded.looks.name_tag_always_visible == Some(true));
    assert_eq!(loaded.looks.scale, 2.0);
    assert!(loaded.looks.invisible && !loaded.looks.gravity);
    assert_eq!(e.position().y, -60.0);
}

// api: Entity::setMotion, Entity::addMotion, Entity::setRotation, Living::lookAt, Entity::teleport
#[test]
fn motion_rotation_and_teleports_change_the_entity_and_its_index() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let id = cx
        .spawn_human(WORLD, at(0.5, -60.0, 3.5), "Guide", None)
        .unwrap();
    let mut e = cx.entity_mut(id).unwrap();
    assert!(!e.set_motion(Vec3::new(f64::NAN, 0.0, 0.0)));
    assert!(e.set_motion(Vec3::new(0.0, 0.5, 0.0)));
    assert_eq!(e.motion(), Vec3::new(0.0, 0.5, 0.0));
    assert!(e.set_rotation(90.25, -10.0));
    assert_eq!((e.yaw(), e.pitch()), (90.25, -10.0));
    assert!(!e.set_rotation(f64::INFINITY, 0.0));
    // Finite as a double but past a float's range: refused, as no tick state holds an infinity.
    assert!(!e.set_rotation(1e39, 0.0));
    assert!(!e.set_rotation(0.0, -1e39));
    assert_eq!((e.yaw(), e.pitch()), (90.25, -10.0));
    // A target straight south of the eyes: yaw 0, level.
    assert!(e.look_at(at(0.5, -60.0 + e.eye_height(), 13.5)));
    assert!(e.yaw().abs() < 1e-4 && e.pitch().abs() < 1e-4);
    assert!(!e.look_at(at(f64::NAN, 0.0, 0.0)));
    // Within the loaded chunks; not into unloaded ones, other worlds or out of the world.
    assert!(e.teleport(WORLD, at(40.5, -60.0, 40.5)));
    assert_eq!(e.position(), at(40.5, -60.0, 40.5));
    assert_eq!(e.motion(), Vec3::ZERO);
    assert!(!e.teleport(WORLD, at(4000.5, -60.0, 0.5)));
    assert!(!e.teleport(world::Id::of(1), at(0.5, -60.0, 0.5)));
    assert!(!e.teleport(WORLD, at(0.5, 30_000_001.0, 0.5)));
    assert!(!e.teleport(WORLD, at(0.5, f64::NAN, 0.5)));
    let found = cx.entities_in(
        WORLD,
        Aabb::new(at(39.0, -61.0, 39.0), at(42.0, -58.0, 42.0)),
    );
    assert_eq!(found, vec![id]);
    assert!(
        cx.entities_in(WORLD, Aabb::new(at(-2.0, -61.0, 2.0), at(2.0, -58.0, 5.0)))
            .is_empty()
    );
    assert_eq!(cx.chunk_entities(WORLD, ChunkPos::new(2, 2)).count(), 1);
    assert_eq!(cx.chunk_entities(WORLD, ChunkPos::new(0, 0)).count(), 0);
}

// api: Entity::setHealth, Living::setHealth, Entity::heal, Entity::attack, Living::attack,
// api: Entity::kill, Entity::setOnFire, Living::setOnFire, Entity::setFireTicks,
// api: Entity::getFireTicks, Entity::isOnFire, Entity::extinguish, Living::knockBack
#[test]
fn health_fire_damage_and_death_follow_pmmp_without_events() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let human = cx
        .spawn_human(WORLD, at(2.5, -60.0, 2.5), "Target", None)
        .unwrap();
    let item = cx
        .drop_item(
            WORLD,
            at(-2.5, -60.0, 2.5),
            stack("minecraft:stick", 3),
            None,
        )
        .unwrap();
    let mut e = cx.entity_mut(human).unwrap();
    // Invulnerable NPCs take no API damage.
    assert!(!e.attack(5.0));
    assert!(e.set_invulnerable(false));
    assert!(e.attack(5.0));
    assert_eq!(e.health(), 15.0);
    // Within the cooldown only a stronger hit counts its difference.
    assert!(!e.attack(4.0));
    assert!(e.attack(7.0));
    assert_eq!(e.health(), 13.0);
    assert!(e.set_health(30.0));
    assert_eq!(e.health(), 20.0, "at most the maximum");
    assert!(!e.set_health(f64::NAN));
    assert!(e.set_health(4.0));
    assert!(e.heal(2.5));
    assert!(!e.heal(-1.0));
    assert_eq!(e.health(), 6.5);
    e.set_on_fire(3);
    assert!(e.is_on_fire() && e.fire_ticks() == 60);
    e.set_on_fire(1);
    assert_eq!(e.fire_ticks(), 60, "a shorter fire does not shorten it");
    e.set_fire_ticks(5);
    assert_eq!(e.fire_ticks(), 5);
    e.set_fire_ticks(0);
    assert!(!e.is_on_fire());
    assert!(e.knock_back(1.0, 0.0, 0.4, 0.4));
    assert!(e.motion().x > 0.0 && e.motion().y > 0.0);
    assert!(!e.knock_back(0.0, 0.0, 0.4, 0.4));
    e.kill();
    assert!(!e.is_alive());
    assert!(!e.set_health(10.0), "a dying NPC stays dying");
    e.kill();
    let mut i = cx.entity_mut(item).unwrap();
    assert!(!i.knock_back(1.0, 0.0, 0.4, 0.4), "only living NPCs");
    assert!(i.attack(2.0));
    assert!(i.set_health(0.0));
    assert!(cx.entity(item).is_none(), "a dead item is gone");
    for _ in 0..30 {
        h.entities();
    }
    assert!(h.state.rules.entities.get(human).is_none(), "the NPC died");
}

// api: ItemEntity::setPickupDelay, ItemEntity::setDespawnDelay, ExperienceOrb::setDespawnDelay,
// api: ItemEntity::setOwner, ItemEntity::setThrower, ExperienceOrb::setXpValue,
// api: PrimedTNT::setFuse, PrimedTNT::setWorksUnderwater, Projectile::setBaseDamage,
// api: entity\projectile\Arrow::setCritical, entity\projectile\Arrow::setPickupMode,
// api: entity\projectile\Arrow::setPunchKnockback, entity\projectile\SplashPotion::setLinger,
// api: Villager::setProfession
#[test]
fn kind_setters_change_their_kind_only() {
    let mut h = harness();
    let made = every_kind(&mut h);
    let mut cx = context(&mut h);
    for (id, kind) in made {
        let mut e = cx.entity_mut(id).unwrap();
        let is = |t: Type| e.entity_type() == t;
        let (item, orb, tnt, arrow) = (
            is(Type::Item),
            is(Type::XpOrb),
            is(Type::PrimedTnt),
            is(Type::Arrow),
        );
        let (potion, villager, painting) = (
            is(Type::SplashPotion),
            is(Type::Villager),
            is(Type::Painting),
        );
        let projectile = matches!(kind, Kind::Projectile(_));
        assert_eq!(e.set_pickup_delay_ticks(-5), item);
        assert_eq!(e.set_despawn_delay_ticks(100), item || orb);
        assert!(!e.set_despawn_delay_ticks(-2) && !e.set_despawn_delay_ticks(40_000));
        assert_eq!(e.set_owner("me"), item);
        assert_eq!(e.set_thrower("you"), item);
        assert_eq!(e.set_xp_value(9), orb);
        assert!(!e.set_xp_value(0) && !e.set_xp_value(40_000));
        assert_eq!(e.set_fuse_ticks(12), tnt);
        assert!(!e.set_fuse_ticks(40_000));
        assert_eq!(e.set_works_underwater(true), tnt);
        assert_eq!(e.set_base_damage(6.0), projectile);
        assert!(!e.set_base_damage(-1.0) && !e.set_base_damage(f64::NAN));
        assert_eq!(e.set_critical(true), arrow);
        assert_eq!(e.set_pickup_mode(ArrowPickup::None), arrow);
        assert_eq!(e.set_punch_knockback(3), arrow);
        assert_eq!(e.set_linger(true), potion);
        assert_eq!(e.set_profession(4), villager);
        assert_eq!(
            e.set_scale(1.0),
            !painting,
            "a painting's box is its picture"
        );
        if item {
            assert_eq!(e.pickup_delay_ticks(), Some(-1));
            assert_eq!(e.despawn_delay_ticks(), Some(100));
            assert_eq!((e.owner(), e.thrower()), (Some("me"), Some("you")));
        }
        if orb {
            assert_eq!(e.xp_value(), Some(9));
        }
        if tnt {
            assert!(e.fuse_ticks() == Some(12) && e.works_underwater());
        }
        if arrow {
            assert!(e.is_critical());
            assert_eq!(e.pickup_mode(), Some(ArrowPickup::None));
            assert_eq!(e.punch_knockback(), Some(3));
        }
        if potion {
            assert!(e.will_linger());
        }
        if villager {
            assert_eq!(e.profession(), Some(4));
        }
        if projectile {
            assert_eq!(e.base_damage(), Some(6.0));
        }
    }
}

/// A dropped item takes the despawn delay `item-despawn-ticks` gave it, and no more.
#[test]
fn a_dropped_item_takes_the_despawn_delay_its_getter_returns() {
    let mut h = harness();
    h.state.settings.item_despawn_ticks = 50_000;
    let mut cx = context(&mut h);
    let stick = stack("minecraft:stick", 1);
    let id = cx
        .drop_item(WORLD, at(2.5, -60.0, 2.5), stick, None)
        .unwrap();
    let mut e = cx.entity_mut(id).unwrap();
    assert_eq!(e.despawn_delay_ticks(), Some(50_000));
    assert!(e.set_despawn_delay_ticks(50_000));
    assert!(!e.set_despawn_delay_ticks(50_001));
}

// api: Entity::setCanSaveWithChunk, Entity::canSaveWithChunk
#[test]
fn entities_not_saved_with_their_chunk_are_left_out_and_go_with_it() {
    let mut h = harness();
    let key = (0, ChunkPos::new(1, 1));
    // The chunk holds sample saved compounds of its own.
    let before = h.state.rules.entities.list(key).len();
    let mut cx = context(&mut h);
    let pos = at(20.5, -60.0, 20.5);
    let kept = cx.spawn_tnt(WORLD, pos, 1000).unwrap();
    let temporary = cx.spawn_tnt(WORLD, pos, 1000).unwrap();
    let mut e = cx.entity_mut(temporary).unwrap();
    assert!(e.can_save_with_chunk());
    assert!(e.set_can_save_with_chunk(false));
    assert!(!e.can_save_with_chunk());
    let list = h.state.rules.entities.list(key);
    assert_eq!(list.len(), before + 1, "only the saved one");
    let unloaded = h.state.rules.entities.unload(key).unwrap();
    assert_eq!(unloaded.len(), before + 1);
    assert!(h.state.rules.entities.get(kept).is_none());
    assert!(h.state.rules.entities.get(temporary).is_none());
}

/// A firework rocket is never saved with its chunk, whatever the API asks.
#[test]
fn a_firework_rocket_is_never_saved_with_its_chunk() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let rocket = cx
        .spawn_firework_rocket(WORLD, at(2.5, -55.0, 2.5), 30, Vec::new(), Vec3::ZERO)
        .unwrap();
    let mut e = cx.entity_mut(rocket).unwrap();
    assert!(!e.can_save_with_chunk());
    assert!(!e.set_can_save_with_chunk(true));
    assert!(!e.can_save_with_chunk());
    assert!(e.set_can_save_with_chunk(false));
}

// api: Entity::flagForDespawn, Entity::close, World::dropItem, World::dropExperience,
// api: ExperienceOrb::__construct, FallingBlock::__construct, Projectile::__construct,
// api: Human::__construct
#[test]
fn spawning_and_despawning_through_the_api_leaves_every_table_at_its_start() {
    let mut h = harness();
    for _ in 0..4 {
        h.entities();
    }
    h.take(1);
    h.state.rules.entities.take_changed();
    let sand = crate::block::State::find("minecraft:sand", &[]).unwrap();
    let skin = Skin::new("npc", vec![3; 64 * 64 * 4]).unwrap();
    let mut spawned = 0;
    for round in 0..200_i32 {
        let x = f64::from(round % 20) - 10.0;
        let p = at(x + 0.5, -55.0, 6.5);
        let mut cx = context(&mut h);
        let mut ids = vec![
            cx.drop_item(WORLD, p, stack("minecraft:stick", 1), Some(Vec3::ZERO))
                .unwrap(),
            cx.spawn_xp_orb(WORLD, p, 5).unwrap(),
            cx.spawn_falling_block(WORLD, p, sand).unwrap(),
            cx.spawn_tnt(WORLD, p, 500).unwrap(),
            cx.spawn_projectile(WORLD, p, Projectile::Snowball, Vec3::ZERO)
                .unwrap(),
            cx.spawn_projectile(
                WORLD,
                p,
                Projectile::LingeringPotion(Potion::Healing),
                Vec3::ZERO,
            )
            .unwrap(),
            cx.spawn_human(WORLD, p, "npc", Some(skin.clone())).unwrap(),
        ];
        assert_eq!(cx.drop_xp(WORLD, p, 10), 10);
        ids.extend(cx.entities(WORLD).map(|(id, _)| id));
        ids.sort_unstable();
        ids.dedup();
        spawned += ids.len();
        for id in ids {
            cx.entity_mut(id).unwrap().flag_for_despawn();
            assert!(cx.entity(id).is_none(), "gone at once for the API");
        }
        h.entities();
    }
    assert!(spawned >= 200 * 9);
    for _ in 0..5 {
        h.entities();
    }
    h.state.rules.entities.take_changed();
    let Entities {
        slots,
        free,
        counts,
        columns,
        runtime_ids,
        unassigned,
        changed,
        departed,
        doomed,
        restep,
        ..
    } = &h.state.rules.entities;
    assert!(slots.is_empty() && free.is_empty() && counts.is_empty());
    assert!(columns.is_empty() && runtime_ids.is_empty() && unassigned.is_empty());
    assert!(changed.is_empty() && departed.is_empty() && doomed.is_empty());
    assert!(restep.is_empty());
    let packets = h.take(1);
    assert_eq!(adds(&packets).len(), removes(&packets).len());
}

// api: World::getNearbyEntities, World::getCollidingEntities, World::getNearestEntity,
// api: World::getEntities, World::getChunkEntities, WorldManager::findEntity, World::getEntity
#[test]
fn queries_go_through_the_chunk_index_whatever_the_area() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let mut ids = Vec::new();
    for i in 0..40_i32 {
        let p = at(
            f64::from(i * 3) - 60.0,
            -60.0,
            f64::from(i % 7) * 5.0 - 15.0,
        );
        ids.push(cx.spawn_tnt(WORLD, p, 1000).unwrap());
    }
    // A small box, a box over many chunks (the index walk) and a huge one agree.
    let small = Aabb::new(at(-61.0, -61.0, -16.0), at(-55.0, -50.0, 20.0));
    let found = cx.entities_in(WORLD, small);
    assert_eq!(found, ids[..2].to_vec());
    let big = Aabb::new(at(-61.0, -61.0, -16.0), at(60.0, -50.0, 20.0));
    let all = cx.entities_in(WORLD, big);
    assert_eq!(all.len(), 40);
    let mut sorted = all.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, ids);
    let huge = Aabb::new(at(-3e7, -1e9, -3e7), at(3e7, 1e9, 3e7));
    assert_eq!(cx.entities_in(WORLD, huge), all, "chunk then Id order");
    assert!(
        cx.entities_in(WORLD, Aabb::new(at(f64::NAN, 0.0, 0.0), at(1.0, 1.0, 1.0)))
            .is_empty()
    );
    assert!(cx.entities_in(world::Id::of(3), big).is_empty());
    // The nearest, with a filter and a reach.
    let origin = at(-60.0, -60.0, -15.0);
    assert_eq!(
        cx.nearest_entity(WORLD, origin, 2.0, |_| true),
        Some(ids[0])
    );
    // ids[1] is √34 away.
    assert_eq!(
        cx.nearest_entity(WORLD, origin, 6.0, |e| e.position().x > -60.0),
        Some(ids[1])
    );
    assert_eq!(
        cx.nearest_entity(WORLD, origin, 5.5, |e| e.position().x > -60.0),
        None
    );
    assert_eq!(
        cx.nearest_entity(WORLD, origin, f64::INFINITY, |_| true),
        None
    );
    assert_eq!(
        cx.nearest_entity(WORLD, origin, 1e7, |_| true),
        Some(ids[0])
    );
    // Every entity of the world, in Id order, and the runtime ID index.
    let listed: Vec<Id> = cx.entities(WORLD).map(|(id, _)| id).collect();
    assert_eq!(listed, ids);
    assert_eq!(cx.entities(world::Id::of(2)).count(), 0);
    // x -60, -57 and -54 at z -15, -10 and -5.
    let chunk = cx.chunk_entities(WORLD, ChunkPos::new(-4, -1)).count();
    assert_eq!(chunk, 3);
    h.entities();
    let cx = context(&mut h);
    let runtime_id = cx.entity(ids[5]).unwrap().runtime_id().unwrap();
    assert_eq!(cx.find_entity(runtime_id), Some(ids[5]));
    assert_eq!(cx.find_entity(u64::MAX), None);
}

#[test]
fn spawns_are_refused_where_the_world_cannot_take_them_and_items_come_back() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let stick = stack("minecraft:stick", 7);
    let ok = at(0.5, -60.0, 0.5);
    let far = at(5000.5, -60.0, 0.5);
    assert_eq!(
        cx.drop_item(world::Id::of(1), ok, stick.clone(), None),
        Err(stick.clone())
    );
    assert_eq!(
        cx.drop_item(WORLD, far, stick.clone(), None),
        Err(stick.clone())
    );
    assert_eq!(
        cx.drop_item(WORLD, ok, Stack::EMPTY, None),
        Err(Stack::EMPTY)
    );
    assert_eq!(
        cx.drop_item(
            WORLD,
            ok,
            stick.clone(),
            Some(Vec3::new(f64::NAN, 0.0, 0.0))
        ),
        Err(stick.clone())
    );
    assert_eq!(cx.drop_xp(WORLD, far, 50), 0);
    assert_eq!(cx.drop_xp(world::Id::of(9), ok, 50), 0);
    assert!(cx.spawn_xp_orb(WORLD, ok, 0).is_none());
    assert!(cx.spawn_tnt(WORLD, ok, 40_000).is_none());
    let high = at(0.5, 30_000_001.0, 0.5);
    assert!(cx.spawn_tnt(WORLD, high, 10).is_none());
    assert!(cx.spawn_human(WORLD, high, "up", None).is_none());
    assert_eq!(cx.drop_xp(WORLD, high, 5), 0);
    assert_eq!(
        cx.drop_item(WORLD, high, stick.clone(), None),
        Err(stick.clone())
    );
    assert!(
        cx.spawn_projectile(WORLD, ok, Projectile::Egg, Vec3::new(0.0, f64::NAN, 0.0))
            .is_none()
    );
    assert!(cx.spawn_human(WORLD, ok, &"n".repeat(1025), None).is_none());
    assert!(cx.entity_mut(Id::new(77, 0)).is_none());
    // At the cap nothing more is made and the stack comes back.
    let n = MAX_ENTITIES_PER_WORLD - cx.entities(WORLD).count();
    for _ in 0..n {
        cx.spawn_tnt(WORLD, ok, 1000).unwrap();
    }
    assert_eq!(cx.drop_item(WORLD, ok, stick.clone(), None), Err(stick));
    assert!(cx.spawn_human(WORLD, ok, "late", None).is_none());
}

#[test]
fn human_npcs_show_their_skin_and_keep_it_across_a_save() {
    let mut h = harness();
    let skin = Skin::new("custom", vec![9; 64 * 64 * 4]).unwrap();
    let id = context(&mut h)
        .spawn_human(WORLD, at(1.5, -60.0, 3.5), "Shop", Some(skin.clone()))
        .unwrap();
    for _ in 0..3 {
        h.entities();
    }
    let packets = h.take(1);
    let list = packets
        .iter()
        .find_map(|p| match p {
            Packet::PlayerList(l) => l.entries.iter().find(|e| e.username == "Shop").cloned(),
            _ => None,
        })
        .unwrap();
    assert_eq!(list.username, "Shop");
    assert_eq!(list.skin.skin_data, skin.data());
    assert_eq!(list.skin.skin_image_height, 64);
    let cx = context(&mut h);
    let e = cx.entity(id).unwrap();
    assert_eq!(e.skin(), Some(&skin));
    assert!(e.is_name_tag_always_visible());
    let saved = saved::save(e);
    let loaded = saved::load(
        &saved,
        e.chunk,
        &EntityTypes::default(),
        &crate::custom::Registry::default(),
    )
    .unwrap();
    match loaded.kind {
        Kind::Npc(Character {
            form: Form::Human {
                skin: Some(ref s), ..
            },
            ..
        }) => assert_eq!(**s, skin),
        other => panic!("{other:?}"),
    }
    assert_eq!(loaded.looks.name_tag, "Shop");
    assert_eq!(saved::save(e), {
        let mut again = Entity::new(0, loaded.body, loaded.yaw, loaded.pitch, loaded.kind);
        again.health = loaded.health;
        again.name_tag = loaded.looks.name_tag;
        saved::save(&again)
    });
}

/// The `ActorEvent` and `Animate` packets for `runtime_id` in `packets`: (event or action, data).
fn animations_of(packets: &[Packet], runtime_id: u64) -> Vec<(u8, f64)> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id => {
                Some((a.event_type, f64::from(a.event_data)))
            }
            Packet::Animate(a) if a.entity_runtime_id == runtime_id => {
                Some((a.action_type, f64::from(a.data)))
            }
            _ => None,
        })
        .collect()
}

/// The player of session `s`.
fn player_of(s: u64) -> player::Id {
    player::Id::new(crate::tick::SessionId::new(s))
}

// api: Entity::broadcastAnimation, Player::broadcastAnimation, Animation, ArmSwingAnimation,
// api: ArmSwingAnimation::__construct, ArrowShakeAnimation, ArrowShakeAnimation::__construct,
// api: ConsumingItemAnimation, ConsumingItemAnimation::__construct, CriticalHitAnimation,
// api: CriticalHitAnimation::__construct, DeathAnimation, DeathAnimation::__construct,
// api: FireworkParticlesAnimation, FireworkParticlesAnimation::__construct, HurtAnimation,
// api: HurtAnimation::__construct, ItemEntityStackSizeChangeAnimation,
// api: ItemEntityStackSizeChangeAnimation::__construct, MagicHitAnimation,
// api: MagicHitAnimation::__construct, RespawnAnimation, RespawnAnimation::__construct,
// api: TotemUseAnimation, TotemUseAnimation::__construct
#[test]
fn animations_reach_the_players_who_see_the_actor_with_pmmp_values() {
    let mut h = harness();
    let id = context(&mut h)
        .spawn_tnt(WORLD, at(2.5, -60.0, 2.5), 1000)
        .unwrap();
    h.entities();
    h.take(1);
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    let apple = stack("minecraft:apple", 1).item().unwrap();
    let all = [
        (Animation::ArmSwing, (4, 0.0)),
        (Animation::ArrowShake { ticks: 7 }, (39, 7.0)),
        (
            Animation::ConsumingItem(apple),
            (57, f64::from(i32::from(apple.runtime_id()) << 16)),
        ),
        (Animation::CriticalHit { particles: 55 }, (4, 55.0)),
        (Animation::Death, (3, 0.0)),
        (Animation::FireworkParticles, (25, 0.0)),
        (Animation::Hurt, (2, 0.0)),
        (Animation::ItemStackSize { count: 12 }, (69, 12.0)),
        (Animation::MagicHit { particles: 15 }, (5, 15.0)),
        (Animation::Respawn, (18, 0.0)),
        (Animation::TotemUse, (65, 0.0)),
    ];
    let mut cx = context(&mut h);
    for (animation, _) in all {
        assert!(cx.play_animation(Actor::Entity(id), animation));
    }
    let want: Vec<(u8, f64)> = all.iter().map(|(_, w)| *w).collect();
    h.entities();
    assert_eq!(animations_of(&h.take(1), runtime_id), want);
    // A player sees its own animation; a gone actor plays nothing.
    let mut cx = context(&mut h);
    assert!(cx.play_animation(Actor::Player(player_of(1)), Animation::Hurt));
    assert!(!cx.play_animation(Actor::Player(player_of(9)), Animation::Hurt));
    cx.entity_mut(id).unwrap().flag_for_despawn();
    assert!(!cx.play_animation(Actor::Entity(id), Animation::Hurt));
    let own = h.runtime_id(1);
    h.entities();
    assert_eq!(animations_of(&h.take(1), own), vec![(2, 0.0)]);
}

// api: Player::hidePlayer, Player::showPlayer, Player::canSee
#[test]
fn hidden_players_are_removed_for_the_viewer_only_until_shown_or_gone() {
    let mut h = harness();
    h.spawn(2, standing(3.0, 3.0));
    let (one, two) = (h.runtime_id(1), h.runtime_id(2));
    h.step(Vec::new());
    h.take(1);
    h.take(2);
    let (p1, p2) = (player_of(1), player_of(2));
    let mut cx = context(&mut h);
    assert!(cx.can_see(p1, p2));
    assert!(cx.hide_player(p1, p2));
    assert!(cx.hide_player(p1, p2), "hiding twice is hiding once");
    assert!(!cx.hide_player(p1, p1));
    assert!(!cx.hide_player(p1, player_of(9)) && !cx.hide_player(player_of(9), p1));
    assert!(!cx.can_see(p1, p2) && cx.can_see(p2, p1));
    h.step(Vec::new());
    assert_eq!(removes(&h.take(1)), vec![two]);
    assert!(removes(&h.take(2)).is_empty(), "the other way stays");
    h.go(2, standing(4.0, 3.0));
    h.step(Vec::new());
    assert!(adds(&h.take(1)).is_empty(), "not sent again while hidden");
    let mut cx = context(&mut h);
    assert!(cx.show_player(p1, p2));
    assert!(!cx.show_player(p1, p2));
    h.step(Vec::new());
    assert_eq!(adds(&h.take(1)), vec![two]);
    // A pair goes when either player leaves.
    assert!(context(&mut h).hide_player(p2, p1));
    h.step(vec![crate::tick::tests::leave(1, 99)]);
    h.step(Vec::new());
    assert!(h.state.rules.entities.hidden.is_empty());
    assert!(!adds(&h.take(2)).contains(&one));
}

/// The pairs of a player who leaves go as it leaves, both ways, before any entity tick.
#[test]
fn hidden_pairs_go_when_a_player_leaves() {
    let mut h = harness();
    h.spawn(2, standing(3.0, 3.0));
    h.spawn(3, standing(5.0, 3.0));
    let mut cx = context(&mut h);
    let (p1, p2, p3) = (player_of(1), player_of(2), player_of(3));
    assert!(cx.hide_player(p1, p2) && cx.hide_player(p2, p1) && cx.hide_player(p1, p3));
    assert!(h.state.remove(crate::tick::SessionId::new(2)).is_some());
    let left: Vec<_> = h.state.rules.entities.hidden.iter().copied().collect();
    assert_eq!(left, vec![(p1.session(), p3.session())]);
}

/// A hit that is not finite or above the damage limit is refused.
/// So no health becomes infinite.
#[test]
fn damage_not_finite_or_above_the_limit_is_refused() {
    let mut h = harness();
    let mut cx = context(&mut h);
    let stick = stack("minecraft:stick", 1);
    let item = cx
        .drop_item(WORLD, at(2.5, -60.0, 2.5), stick, None)
        .unwrap();
    let mut e = cx.entity_mut(item).unwrap();
    let health = e.health();
    for amount in [f64::INFINITY, f64::MAX, 1e18] {
        assert!(!e.attack(amount), "{amount}");
    }
    assert_eq!(e.health(), health);
}

#[test]
fn a_player_hides_at_most_its_bound() {
    let mut h = harness();
    h.spawn(2, standing(3.0, 3.0));
    let viewer = crate::tick::SessionId::new(1);
    let hidden = &mut h.state.rules.entities.hidden;
    for s in 0..u64::try_from(MAX_HIDDEN_PER_PLAYER).unwrap() {
        hidden.insert((viewer, crate::tick::SessionId::new(1000 + s)));
    }
    let mut cx = context(&mut h);
    assert!(!cx.hide_player(player_of(1), player_of(2)));
    assert!(cx.hide_player(player_of(2), player_of(1)));
}

// api: AreaEffectCloud::__construct, AreaEffectCloud::getEffects, AreaEffectCloud::getAge,
// api: AreaEffectCloud::getMaxAge, AreaEffectCloud::getMaxAgeChangeOnUse,
// api: AreaEffectCloud::getReapplicationDelay, AreaEffectCloud::getInitialRadius,
// api: AreaEffectCloud::getRadiusChangeOnPickup, AreaEffectCloud::getRadiusChangeOnUse,
// api: AreaEffectCloud::getRadiusChangePerTick, AreaEffectCloud::setMaxAge,
// api: AreaEffectCloud::setMaxAgeChangeOnUse, AreaEffectCloud::setReapplicationDelay,
// api: AreaEffectCloud::setRadiusChangeOnPickup, AreaEffectCloud::setRadiusChangeOnUse,
// api: AreaEffectCloud::setRadiusChangePerTick
#[test]
fn clouds_spawn_with_pmmp_defaults_and_take_bounded_values() {
    let mut h = harness();
    let effects = Potion::Poison.effects();
    let mut cx = context(&mut h);
    let too_many = vec![effects[0]; 33];
    let place = at(1.5, -60.0, 1.5);
    assert!(
        cx.spawn_area_effect_cloud(WORLD, place, &too_many)
            .is_none()
    );
    let id = cx.spawn_area_effect_cloud(WORLD, place, effects).unwrap();
    let e = cx.entity(id).unwrap();
    assert_eq!(e.entity_type(), Type::AreaEffectCloud);
    assert_eq!(e.cloud_effects(), Some(effects));
    assert_eq!(e.cloud_age_ticks(), Some(0));
    assert_eq!(e.cloud_max_age_ticks(), Some(600));
    assert_eq!(e.cloud_max_age_change_on_use_ticks(), Some(0));
    assert_eq!(e.cloud_reapplication_delay_ticks(), Some(40));
    assert_eq!(e.cloud_initial_radius(), Some(3.0));
    assert_eq!(e.cloud_radius(), Some(3.0));
    assert_eq!(e.cloud_radius_change_on_pickup(), Some(-0.5));
    assert_eq!(e.cloud_radius_change_on_use(), Some(-0.5));
    assert_eq!(e.cloud_radius_change_per_tick(), Some(-0.005));
    let mut e = cx.entity_mut(id).unwrap();
    assert!(e.set_cloud_max_age_ticks(1200));
    assert!(!e.set_cloud_max_age_ticks(-1) && !e.set_cloud_max_age_ticks(24_001));
    assert!(e.set_cloud_max_age_change_on_use_ticks(-20));
    assert!(!e.set_cloud_max_age_change_on_use_ticks(30_000));
    assert!(
        e.set_cloud_reapplication_delay_ticks(80) && !e.set_cloud_reapplication_delay_ticks(-1)
    );
    assert!(e.set_cloud_radius_change_on_pickup(-1.0));
    assert!(e.set_cloud_radius_change_on_use(0.25) && !e.set_cloud_radius_change_on_use(65.0));
    assert!(e.set_cloud_radius_change_per_tick(0.0));
    assert!(!e.set_cloud_radius_change_per_tick(f64::NAN));
    assert_eq!(e.cloud_max_age_ticks(), Some(1200));
    assert_eq!(e.cloud_max_age_change_on_use_ticks(), Some(-20));
    assert_eq!(e.cloud_reapplication_delay_ticks(), Some(80));
    assert_eq!(e.cloud_radius_change_on_pickup(), Some(-1.0));
    assert_eq!(e.cloud_radius_change_on_use(), Some(0.25));
    assert_eq!(e.cloud_radius_change_per_tick(), Some(0.0));
    // The cloud keeps its radius now and ages.
    for _ in 0..5 {
        h.entities();
    }
    let e = context(&mut h)
        .entity(id)
        .map(|e| (e.cloud_age_ticks(), e.cloud_radius()));
    assert_eq!(e, Some((Some(5), Some(3.0))));
    // Other kinds take none of it.
    let tnt = context(&mut h)
        .spawn_tnt(WORLD, at(-2.5, -60.0, 2.5), 100)
        .unwrap();
    let mut cx = context(&mut h);
    let mut e = cx.entity_mut(tnt).unwrap();
    assert!(e.cloud_effects().is_none() && e.cloud_max_age_ticks().is_none());
    assert!(!e.set_cloud_max_age_ticks(10) && !e.set_cloud_radius_change_on_use(0.0));
}

/// A cloud's changed duration and pickup radius change reach its viewers, which draw them.
#[test]
fn a_cloud_shows_a_changed_duration_and_pickup_radius_change() {
    let mut h = harness();
    let effects = Potion::Poison.effects();
    let id = context(&mut h)
        .spawn_area_effect_cloud(WORLD, at(1.5, -60.0, 1.5), effects)
        .unwrap();
    for _ in 0..3 {
        h.entities();
    }
    h.take(1);
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    {
        let mut cx = context(&mut h);
        let mut e = cx.entity_mut(id).unwrap();
        assert!(e.set_cloud_max_age_ticks(1200));
        assert!(e.set_cloud_radius_change_on_pickup(-1.0));
    }
    h.entities();
    let shown = data_of(&h.take(1), runtime_id);
    let duration = shown
        .iter()
        .find_map(|m| m.get(&(EntityDataKey::DataDuration as u32)));
    let pickup = shown
        .iter()
        .find_map(|m| m.get(&(EntityDataKey::DataChangeOnPickup as u32)));
    assert_eq!(duration, Some(&MetadataValue::Int32(1200)));
    assert_eq!(pickup, Some(&MetadataValue::Float32(-1.0)));
}

/// A rocket's changed flight time shows in the flight multiplier its item carries.
#[test]
fn a_rocket_shows_the_flight_multiplier_of_a_changed_flight_time() {
    let mut h = harness();
    let id = context(&mut h)
        .spawn_firework_rocket(WORLD, at(1.5, -55.0, 1.5), 20, Vec::new(), Vec3::ZERO)
        .unwrap();
    h.entities();
    h.take(1);
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    let mut cx = context(&mut h);
    assert!(cx.entity_mut(id).unwrap().set_max_flight_time_ticks(50));
    h.entities();
    let flight = data_of(&h.take(1), runtime_id).iter().find_map(|m| {
        let Some(MetadataValue::CompoundTag(item)) =
            m.get(&(EntityDataKey::DISPLAY_FIREWORK as u32))
        else {
            return None;
        };
        let Some(Value::Compound(fireworks)) = item.get(object::TAG_FIREWORKS) else {
            return None;
        };
        fireworks.get(object::TAG_FLIGHT).cloned()
    });
    assert_eq!(flight, Some(Value::Byte(4)));
}

// api: Entity::setOwningEntity, ExperienceOrb::setTargetPlayer, ItemEntity::setStackSize,
// api: entity\projectile\SplashPotion::setPotionType,
// api: entity\projectile\SplashPotion::getPotionEffects, Entity::getGravity,
// api: Entity::getStepHeight, Villager::getName
#[test]
fn owners_targets_counts_potions_and_names_change_their_kind_only() {
    let mut h = harness();
    let made = every_kind(&mut h);
    let (me, gone) = (player_of(1), player_of(9));
    let mut cx = context(&mut h);
    for (id, kind) in made {
        let mut e = cx.entity_mut(id).unwrap();
        let t = e.entity_type();
        let projectile = matches!(kind, Kind::Projectile(_) | Kind::Firework(_));
        assert_eq!(e.gravity(), kind.physics().gravity);
        assert_eq!(e.step_height(), 0.0);
        assert_eq!(e.set_owning_player(Some(me)), projectile);
        assert!(!e.set_owning_player(Some(gone)));
        assert_eq!(e.owning_player(), projectile.then_some(me));
        assert_eq!(e.set_owning_player(None), projectile);
        assert!(e.owning_player().is_none());
        let orb = t == Type::XpOrb;
        assert_eq!(e.set_target_player(Some(me)), orb);
        assert!(!e.set_target_player(Some(gone)));
        assert_eq!(e.target_player(), orb.then_some(me));
        let potion = t == Type::SplashPotion;
        assert_eq!(e.set_potion(Potion::Healing), potion);
        if potion {
            assert_eq!(e.potion(), Some(Potion::Healing));
            assert_eq!(e.potion_effects(), Some(Potion::Healing.effects()));
        } else {
            assert!(e.potion_effects().is_none());
        }
        assert_eq!(e.set_stack_size(1), t == Type::Item);
        let name = match t {
            Type::Villager => Some("Villager".to_owned()),
            Type::Zombie => Some("Zombie".to_owned()),
            Type::Human => Some(e.name_tag().to_owned()),
            _ => None,
        };
        assert_eq!(e.name().map(str::to_owned), name);
    }
    // A dropped item takes counts up to its stack size and shows the new one.
    let stick = stack("minecraft:stick", 3);
    // Away from the kinds above: the TNT among them sets off the end crystals.
    let id = context(&mut h)
        .drop_item(WORLD, at(0.5, -60.0, 24.5), stick, Some(Vec3::ZERO))
        .unwrap();
    h.entities();
    h.take(1);
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    let mut cx = context(&mut h);
    let mut e = cx.entity_mut(id).unwrap();
    assert!(!e.set_stack_size(0) && !e.set_stack_size(65));
    assert!(e.set_stack_size(64));
    assert_eq!(e.item().map(Stack::count), Some(64));
    assert_eq!(e.gravity(), 0.04);
    h.entities();
    assert_eq!(animations_of(&h.take(1), runtime_id), vec![(69, 64.0)]);
}

/// A damaged anvil that falls far enough breaks on landing (PMMP `FallingBlock::onHitGround`).
/// It leaves the store and its viewers, as every entity that is used up does.
#[test]
fn a_damaged_anvil_that_breaks_on_landing_is_removed() {
    let mut h = harness();
    let anvil = crate::tick::entity::physics::tests::named("minecraft:damaged_anvil");
    // 20 blocks above the ground: the chance to break is then certain.
    let id = context(&mut h)
        .spawn_falling_block(WORLD, at(4.5, -40.0, 0.5), anvil)
        .unwrap();
    h.entities();
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    for _ in 0..80 {
        h.entities();
    }
    assert!(h.live().is_empty(), "the anvil is gone");
    assert_eq!(h.state.rules.entities.count(0), 0);
    assert!(removes(&h.take(1)).contains(&runtime_id));
}
