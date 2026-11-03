//! Tests of NPCs.
//!
//! They cover saving, trading, melee hits, deaths, zombies, babies, held items, explosions,
//! lightning and fall damage.

use std::cell::Cell;

use super::*;
use crate::item::Enchantment;
use crate::nbt::Value;
use crate::protocol::MetadataValue::Int32;
use crate::protocol::consts::EntityDataKey as K;
use crate::protocol::consts::{ActorEvent, AnimateAction, SoundEvent};
use crate::protocol::packet::Packet;
use crate::tick::entity::explosion::{Source, explode_with};
use crate::tick::entity::physics::tests::named;
use crate::tick::entity::saved::tests::{bytes, stack};
use crate::tick::entity::tests::{Harness, TEST_HOOKS, adds, at, calls, standing};
use crate::tick::trade::Offers;
use crate::world::{BlockPos, ChunkPos};

thread_local! {
    /// Trade windows the stand-in opener opened.
    static OPENED: Cell<u32> = const { Cell::new(0) };
}

fn offers() -> Offers {
    let offer = |n: i32| {
        Compound::from([
            (
                "buyA".to_owned(),
                Value::Compound(stack("minecraft:emerald", 2).to_nbt()),
            ),
            (
                "sell".to_owned(),
                Value::Compound(stack("minecraft:apple", 4).to_nbt()),
            ),
            ("netId".to_owned(), Value::Int(n)),
            ("uses".to_owned(), Value::Int(0)),
        ])
    };
    Offers::from_nbt(
        &crate::nbt::Compound::from([(
            "Recipes".to_owned(),
            Value::List(crate::nbt::List::Compound(vec![
                offer(1),
                offer(2),
                offer(3),
            ])),
        )]),
        &crate::custom::Registry::default(),
    )
}

fn villager(h: &mut Harness, pos: Vec3) -> Id {
    let npc = Npc::Villager {
        profession: 4,
        offers: offers(),
    };
    spawn(&mut h.state, 0, pos, npc).unwrap()
}

fn human(h: &mut Harness, pos: Vec3, name: &str) -> Id {
    let npc = Npc::Human {
        name: name.to_owned(),
        skin: None,
    };
    spawn(&mut h.state, 0, pos, npc).unwrap()
}

#[test]
fn npcs_save_and_load_with_their_offers_byte_identically() {
    let mut h = Harness::new(1);
    let v = villager(&mut h, at(1.5, -60.0, 1.5));
    human(&mut h, at(3.5, -60.0, 1.5), "Guide");
    assert!(crate::tick::entity::villager(&h.state, v).is_some());
    let key = (0, ChunkPos::new(0, 0));
    let ours = |list: &[Compound]| -> Vec<Compound> {
        list.iter()
            .filter(|c| c.contains_key("identifier"))
            .cloned()
            .collect()
    };
    let all = h.state.rules.entities.unload(key).expect("changed");
    let first = ours(&all);
    assert_eq!(first.len(), 2);
    h.state
        .rules
        .entities
        .load_list(key, &all, &crate::custom::Registry::default());
    h.state.rules.entities.changed.insert(key);
    let again = h.state.rules.entities.take_changed();
    let second = ours(&again[0].1);
    assert_eq!(second.len(), 2);
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(bytes(a), bytes(b));
    }
    assert_eq!(
        first[0].get("identifier"),
        Some(&Value::String("minecraft:villager_v2".to_owned()))
    );
    assert_eq!(
        first[0].get("Offers"),
        Some(&Value::Compound(offers().to_nbt()))
    );
    assert_eq!(
        first[1].get("CustomName"),
        Some(&Value::String("Guide".to_owned()))
    );
}

fn zombie(h: &mut Harness, pos: Vec3) -> Id {
    h.context()
        .spawn_zombie(crate::world::Id::DEFAULT, pos)
        .unwrap()
}

#[test]
fn villager_human_and_zombie_npcs_keep_their_custom_data_through_a_save() {
    let mut h = Harness::new(1);
    let npcs = [
        villager(&mut h, at(1.5, -60.0, 1.5)),
        human(&mut h, at(3.5, -60.0, 1.5), "Guide"),
        zombie(&mut h, at(5.5, -60.0, 1.5)),
    ];
    let mark = Compound::from([("npc".to_owned(), Value::String("enforce".to_owned()))]);
    let mut cx = h.context();
    for id in npcs {
        assert_eq!(cx.entity(id).unwrap().custom_data(), Some(&Compound::new()));
        cx.entity_mut(id)
            .unwrap()
            .set_custom_data(mark.clone())
            .unwrap();
        assert_eq!(cx.entity(id).unwrap().custom_data(), Some(&mark));
    }
    let key = (0, ChunkPos::new(0, 0));
    let all = h.state.rules.entities.unload(key).expect("changed");
    h.state
        .rules
        .entities
        .load_list(key, &all, &crate::custom::Registry::default());
    let entities = &h.state.rules.entities;
    let loaded: Vec<_> = entities
        .ids()
        .filter_map(|id| entities.get(id))
        .filter(|e| matches!(e.kind, Kind::Npc(_)))
        .map(|e| e.custom_data().cloned())
        .collect();
    assert_eq!(loaded, vec![Some(mark); 3]);
}

fn open(_: &mut State, _: SessionId, _: Id) -> bool {
    OPENED.with(|o| o.set(o.get() + 1));
    true
}

// api: Player::interactEntity
#[test]
fn interacting_with_a_villager_in_reach_opens_trading() {
    let mut h = Harness::with_player();
    let near = villager(&mut h, at(2.5, -60.0, 0.5));
    let far = villager(&mut h, at(12.5, -60.0, 0.5));
    let guide = human(&mut h, at(0.5, -60.0, 2.5), "Guide");
    let s = SessionId::new(1);
    calls(|c| c.look = (270.0, 0.0));
    assert!(interact_with(&mut h.state, s, near, TEST_HOOKS, open));
    assert!(
        !interact_with(&mut h.state, s, far, TEST_HOOKS, open),
        "beyond 8 blocks"
    );
    assert!(
        !interact_with(&mut h.state, s, guide, TEST_HOOKS, open),
        "no trades"
    );
    calls(|c| c.look = (90.0, 0.0));
    assert!(
        !interact_with(&mut h.state, s, near, TEST_HOOKS, open),
        "behind the player"
    );
    calls(|c| c.dead.insert(1));
    calls(|c| c.look = (270.0, 0.0));
    assert!(
        !interact_with(&mut h.state, s, near, TEST_HOOKS, open),
        "dead"
    );
    assert_eq!(OPENED.with(Cell::get), 1);
}

fn health(h: &Harness, id: Id) -> f64 {
    h.state.rules.entities.get(id).unwrap().health
}

fn vulnerable(h: &mut Harness, id: Id) {
    with(&mut h.state, id, |e| {
        if let Kind::Npc(c) = &mut e.kind {
            c.invulnerable = false;
        }
    });
}

fn e_runtime(h: &Harness, id: Id) -> u64 {
    h.state.rules.entities.get(id).unwrap().runtime_id
}

// api: Player::attackEntity
#[test]
fn a_hit_on_an_npc_hurts_knocks_back_wears_the_weapon_and_kills_it() {
    let mut h = Harness::with_player();
    let s = SessionId::new(1);
    calls(|c| c.look = (270.0, 0.0));
    let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
    h.entities();
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    assert_eq!(health(&h, guide), 20.0, "spawned NPCs are invulnerable");
    vulnerable(&mut h, guide);
    let sword = stack("minecraft:diamond_sword", 1);
    let points = f64::from(sword.item().unwrap().attack_points());
    crate::tick::inventory::set_held(h.state.players.get_mut(&s).unwrap(), sword);
    h.take(1);
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    assert_eq!(health(&h, guide), 20.0 - points);
    let e = h.state.rules.entities.get(guide).unwrap();
    assert!(
        e.body.motion.x > 0.0 && e.body.motion.y > 0.0,
        "knocked back"
    );
    let held = h.state.players[&s].inventory.held().clone();
    assert_eq!(held.damage(), 1, "the sword wears");
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    assert_eq!(
        health(&h, guide),
        20.0 - points,
        "within the cooldown: ignored"
    );
    h.entities();
    let runtime_id = e_runtime(&h, guide);
    // The hurt event carries the official server's cause number, 2 for an attack (P6).
    let hurt = h.take(1).iter().any(|p| {
        matches!(p,
        Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id
            && a.event_type == ActorEvent::Hurt as u8 && a.event_data == 2)
    });
    assert!(hurt, "the hit shows");
    // Hits after the cooldown until it dies; it shows its death and goes 25 ticks later.
    for _ in 0..10 {
        for _ in 0..10 {
            h.entities();
        }
        attack_with(&mut h.state, s, guide, TEST_HOOKS);
        if is_dying(&h.state, guide) {
            break;
        }
    }
    assert!(is_dying(&h.state, guide));
    assert_eq!(health(&h, guide), 0.0);
    let died = h.state.players[&s].outbox.iter().any(|p| {
        matches!(p,
        Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id
            && a.event_type == ActorEvent::Death as u8)
    });
    assert!(died);
    for _ in 0..24 {
        h.entities();
    }
    assert!(h.state.rules.entities.get(guide).is_some());
    h.entities();
    assert!(
        h.state.rules.entities.get(guide).is_none(),
        "gone after 25 ticks"
    );
    // Items take no melee hits.
    assert!(crate::tick::entity::drop_item(
        &mut h.state,
        0,
        at(2.5, -60.0, 0.5),
        stack("minecraft:stick", 1)
    ));
    let item = h.live()[0].runtime_id;
    let id = crate::tick::entity::find(&h.state, item).unwrap();
    attack_with(&mut h.state, s, id, TEST_HOOKS);
    assert_eq!(health(&h, id), 5.0);
}

// api: EntityDamageEvent
/// A player's hit on an invulnerable NPC raises [`crate::event::EntityDamage`] already cancelled.
/// The event names the attacker; the NPC takes nothing, whatever the listeners do. A hit by
/// nothing, such as a fall, raises nothing.
#[test]
fn a_hit_on_an_invulnerable_npc_raises_a_cancelled_damage_event() {
    use crate::event::{Actor, DamageCause, EntityDamage};
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut l = crate::event::Listeners::default();
    l.push(
        crate::event::Priority::Normal,
        move |e: &mut EntityDamage, _: &mut crate::tick::Context<'_>| {
            tx.try_send((e.entity(), e.source(), e.cause(), e.is_cancelled()))
                .unwrap();
            e.uncancel();
        },
    );
    let mut h = Harness::with_listeners(1, l);
    h.spawn(1, standing(0.0, 0.0));
    let s = SessionId::new(1);
    calls(|c| c.look = (270.0, 0.0));
    let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
    h.entities();
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    let by = DamageSource::Entity(player_actor(s));
    assert_eq!(
        rx.try_recv(),
        Ok((Actor::Entity(guide), by, DamageCause::EntityAttack, true))
    );
    assert_eq!(health(&h, guide), 20.0, "uncancelling hurts nothing");
    landed(&mut h.state, guide, 10.0);
    assert!(rx.try_recv().is_err(), "a fall raises nothing");
    vulnerable(&mut h, guide);
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    assert_eq!(
        rx.try_recv().map(|(.., cancelled)| cancelled),
        Ok(false),
        "a hit that lands"
    );
}

/// A hit on an NPC counts the attacker's Strength and the weapon's Sharpness.
/// A hit on a player does too. The viewers see the swing, hear the hit and see the enchanted hit
/// on the NPC; a hit out of reach swings and sounds empty.
#[test]
fn npc_hits_count_the_attackers_effects_and_show_the_swing() {
    let mut h = Harness::with_player();
    h.spawn(2, standing(0.0, 3.0));
    let s = SessionId::new(1);
    calls(|c| c.look = (270.0, 0.0));
    let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
    let far = human(&mut h, at(12.5, -60.0, 0.5), "Far");
    vulnerable(&mut h, guide);
    h.entities();
    let strength = crate::effect::Instance::new(crate::effect::Type::Strength, 600, 0, true, false);
    assert!(survival::add_effect(&mut h.state, s, strength));
    let extra = crate::item::Extra {
        enchantments: vec![(Enchantment::Sharpness, 1)],
        ..crate::item::Extra::default()
    };
    let sword = stack("minecraft:diamond_sword", 1)
        .with_extra(Some(extra))
        .unwrap();
    let points = f64::from(sword.item().unwrap().attack_points());
    crate::tick::inventory::set_held(h.state.players.get_mut(&s).unwrap(), sword);
    h.state
        .players
        .get_mut(&SessionId::new(2))
        .unwrap()
        .outbox
        .clear();
    attack_with(&mut h.state, s, guide, TEST_HOOKS);
    let want = 20.0 - (points + points * 0.3 + 1.0);
    assert!(
        (health(&h, guide) - want).abs() < 1e-9,
        "{}",
        health(&h, guide)
    );
    let (attacker, npc) = (h.runtime_id(1), e_runtime(&h, guide));
    let seen = h.state.players[&SessionId::new(2)].outbox.clone();
    let swing = seen.iter().any(|p| {
        matches!(p,
        Packet::ActorEvent(a) if a.entity_runtime_id == attacker
            && a.event_type == u8::from(ActorEvent::StartAttacking))
    });
    assert!(swing, "the swing shows");
    let sound = |seen: &[Packet], sound: SoundEvent| {
        seen.iter().any(|p| {
            matches!(p,
            Packet::LevelSoundEvent(e) if e.sound_type == sound.as_str())
        })
    };
    assert!(sound(&seen, SoundEvent::AttackStrong));
    let magic = seen.iter().any(|p| {
        matches!(p,
        Packet::Animate(a) if a.entity_runtime_id == npc
            && a.action_type == u8::from(AnimateAction::MagicCriticalHit))
    });
    assert!(magic, "the enchanted hit shows on the NPC");
    h.state
        .players
        .get_mut(&SessionId::new(2))
        .unwrap()
        .outbox
        .clear();
    attack_with(&mut h.state, s, far, TEST_HOOKS);
    assert_eq!(health(&h, far), 20.0);
    let seen = h.state.players[&SessionId::new(2)].outbox.clone();
    assert!(sound(&seen, SoundEvent::AttackNoDamage), "out of reach");
}

/// A hit on an entity in another world than the player's is not swung at or heard.
#[test]
fn a_hit_on_an_entity_in_another_world_is_not_swung_at_or_heard() {
    let mut h = Harness::with_player();
    h.spawn(2, standing(0.0, 3.0));
    calls(|c| c.look = (270.0, 0.0));
    let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
    vulnerable(&mut h, guide);
    h.entities();
    with(&mut h.state, guide, |e| e.world = 1);
    let observer = SessionId::new(2);
    h.state.players.get_mut(&observer).unwrap().outbox.clear();
    attack_with(&mut h.state, SessionId::new(1), guide, TEST_HOOKS);
    assert_eq!(health(&h, guide), 20.0);
    assert!(
        h.state.players[&observer].outbox.is_empty(),
        "nothing shown"
    );
}

/// A listener's attack cooldown past 255 ticks is kept whole, as for a player.
#[test]
fn a_listener_sets_an_npcs_hit_cooldown_beyond_255_ticks() {
    let mut l = crate::event::Listeners::default();
    l.push(
        crate::event::Priority::Normal,
        |e: &mut crate::event::EntityDamage, _: &mut crate::tick::Context<'_>| {
            e.set_attack_cooldown_ticks(300);
        },
    );
    let mut h = Harness::with_listeners(1, l);
    h.spawn(1, standing(0.0, 0.0));
    calls(|c| c.look = (270.0, 0.0));
    let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
    vulnerable(&mut h, guide);
    h.entities();
    attack_with(&mut h.state, SessionId::new(1), guide, TEST_HOOKS);
    let cooldown = match &h.state.rules.entities.get(guide).unwrap().kind {
        Kind::Npc(c) => c.attack_time,
        other => panic!("{other:?}"),
    };
    assert_eq!(cooldown, 300);
}

// api: Zombie, Zombie::getName, Zombie::getNetworkTypeId
#[test]
fn a_zombie_is_an_ai_less_living_mob_that_takes_damage() {
    let mut h = Harness::with_player();
    h.take(1);
    let z = zombie(&mut h, at(2.5, -57.0, 0.5));
    let e = h.state.rules.entities.get(z).unwrap();
    assert_eq!(e.entity_type(), crate::entity::Type::Zombie);
    assert_eq!(e.name(), Some("Zombie"));
    assert_eq!(e.network_type(), "minecraft:zombie");
    assert_eq!(e.size(), (0.6, 1.9));
    assert!(!e.is_invulnerable(), "a zombie is a mob");
    assert_eq!(e.max_health(), 20.0);
    for _ in 0..40 {
        h.entities();
    }
    let e = h.state.rules.entities.get(z).unwrap();
    assert_eq!(
        e.body.position,
        at(2.5, -60.0, 0.5),
        "it falls 3 blocks unhurt and stands: no AI"
    );
    let packets = h.take(1);
    let added = packets
        .iter()
        .any(|p| matches!(p, Packet::AddActor(a) if a.entity_type == "minecraft:zombie"));
    assert!(added);
    h.context().entity_mut(z).unwrap().attack(5.0);
    assert_eq!(health(&h, z), 15.0);
}

/// The rotten flesh, rare items and xp on the ground.
fn loot(h: &Harness) -> (u32, u32, u32) {
    let (mut flesh, mut rare, mut xp) = (0, 0, 0);
    for e in h.live() {
        match &e.kind {
            Kind::Item(i) => {
                let count = u32::from(i.stack.count());
                match i.stack.item().and_then(crate::item::Type::kind) {
                    Some(crate::item::Kind::RottenFlesh) => flesh += count,
                    Some(
                        crate::item::Kind::IronIngot
                        | crate::item::Kind::Carrot
                        | crate::item::Kind::Potato,
                    ) => rare += count,
                    other => panic!("{other:?}"),
                }
            }
            Kind::XpOrb(o) => xp += u32::from(o.value),
            _ => {}
        }
    }
    (flesh, rare, xp)
}

#[test]
fn dead_zombies_drop_rotten_flesh_sometimes_a_rare_item_and_five_xp() {
    let mut h = Harness::with_player();
    let count = 400;
    let mut zombies = Vec::new();
    for i in 0..count {
        let (x, z) = (f64::from(i % 20) - 10.0, f64::from(i / 20) - 10.0);
        zombies.push(zombie(&mut h, at(x + 0.5, -60.0, z + 0.5)));
    }
    let mut cx = h.context();
    for &z in &zombies {
        cx.entity_mut(z).unwrap().kill();
    }
    assert_eq!(
        loot(&h),
        (0, 0, 0),
        "the drops come at the zombie's next tick"
    );
    h.entities();
    let (flesh, rare, xp) = loot(&h);
    // 0 to 2 flesh each (1 on average), a rare item with a 5 in 200 chance.
    assert!((300..=500).contains(&flesh), "{flesh}");
    assert!((2..=25).contains(&rare), "{rare}");
    assert_eq!(xp, 5 * count);
    h.entities();
    assert_eq!(loot(&h).2, 5 * count, "once");
}

// api: EntityDeathEvent::getDrops, EntityDeathEvent::setDrops, EntityDeathEvent::getXpDropAmount
#[test]
fn a_death_listener_sees_and_changes_a_zombies_drops() {
    let mut l = crate::event::Listeners::default();
    l.push(
        crate::event::Priority::Normal,
        |e: &mut crate::event::EntityDeath, _: &mut crate::tick::Context<'_>| {
            assert_eq!(e.xp_drop_amount(), 5);
            assert!(e.drops().iter().all(|s| s.count() <= 2));
            e.set_drops(Vec::new());
            e.set_xp_drop_amount(0);
        },
    );
    let mut h = Harness::with_listeners(1, l);
    h.spawn(1, standing(0.0, 0.0));
    let z = zombie(&mut h, at(2.5, -60.0, 0.5));
    h.context().entity_mut(z).unwrap().kill();
    h.entities();
    assert_eq!(loot(&h), (0, 0, 0));
}

/// `xp = false`: a zombie's death names no xp and drops none.
#[test]
fn without_xp_a_zombie_death_names_and_drops_no_xp() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut l = crate::event::Listeners::default();
    l.push(
        crate::event::Priority::Normal,
        move |e: &mut crate::event::EntityDeath, _: &mut crate::tick::Context<'_>| {
            tx.try_send(e.xp_drop_amount()).unwrap();
        },
    );
    let mut h = Harness::with_listeners(1, l);
    h.state.settings.xp = false;
    h.spawn(1, standing(0.0, 0.0));
    let z = zombie(&mut h, at(2.5, -60.0, 0.5));
    h.context().entity_mut(z).unwrap().kill();
    h.entities();
    assert_eq!(rx.try_recv(), Ok(0));
    assert_eq!(loot(&h).2, 0);
}

/// The `Baby` flag in the flags the viewers of `id` last got.
fn shown_baby(h: &Harness, id: Id) -> bool {
    let flags = h.state.rules.entities.get(id).unwrap().sent.flags;
    flags & (1 << crate::protocol::consts::EntityDataFlag::Baby as u32) != 0
}

// api: entity\Ageable, entity\Ageable::isBaby, Villager::isBaby
#[test]
fn villagers_and_zombies_can_be_babies_and_viewers_see_it() {
    let mut h = Harness::with_player();
    let trader = villager(&mut h, at(2.5, -60.0, 0.5));
    let walker = zombie(&mut h, at(3.5, -60.0, 0.5));
    let guide = human(&mut h, at(4.5, -60.0, 0.5), "Guide");
    h.entities();
    let mut cx = h.context();
    for id in [trader, walker] {
        assert!(!cx.entity(id).unwrap().is_baby());
        cx.entity_mut(id).unwrap().set_baby(true).unwrap();
        assert!(cx.entity(id).unwrap().is_baby());
    }
    assert!(
        cx.entity_mut(guide).unwrap().set_baby(true).is_err(),
        "humans grow up"
    );
    assert!(!cx.entity(guide).unwrap().is_baby());
    h.take(1);
    h.entities();
    assert!(shown_baby(&h, trader) && shown_baby(&h, walker) && !shown_baby(&h, guide));
    let baby = 1_i64 << crate::protocol::consts::EntityDataFlag::Baby as u32;
    let sent = h
        .take(1)
        .iter()
        .filter(|p| {
            matches!(p, Packet::SetActorData(d)
            if matches!(d.entity_metadata.get(&(K::Flags as u32)),
                Some(crate::protocol::MetadataValue::Int64(f)) if f & baby != 0))
        })
        .count();
    assert_eq!(sent, 2);
    let mut cx = h.context();
    cx.entity_mut(walker).unwrap().set_baby(false).unwrap();
    assert!(!cx.entity(walker).unwrap().is_baby());
}

#[test]
fn baby_zombies_and_villagers_survive_a_checkpoint() {
    let mut h = Harness::new(1);
    let v = villager(&mut h, at(2.5, -60.0, 0.5));
    let z = zombie(&mut h, at(3.5, -60.0, 0.5));
    let mut cx = h.context();
    for id in [v, z] {
        cx.entity_mut(id).unwrap().set_baby(true).unwrap();
    }
    h.entities();
    let mut fresh = crate::tick::tests::new_state(
        crate::tick::tests::settings(|_| {}),
        crate::event::Listeners::default(),
    );
    let loaded: Vec<ChunkPos> = fresh.worlds[0].loaded_chunks().collect();
    for pos in loaded {
        if let Some(extras) = fresh.chunks.extras.get((0, pos)).cloned() {
            let read = crate::tick::rules::read_kept(
                (0, pos),
                &extras,
                fresh.worlds[0].chunk(pos).unwrap(),
                &fresh.rules.custom,
                fresh.rules.entities.types(),
            );
            fresh.rules.load((0, pos), read, &extras);
        }
    }
    assert!(crate::tick::checkpoint::tests::assert_round_trip(
        &h.state, fresh, &h.pool
    ));
}

#[test]
fn npcs_leave_farmland_as_it_is_and_take_fall_damage_when_they_land() {
    let mut h = Harness::with_player();
    let soil = BlockPos::new(5, -61, 5);
    let farmland = named("minecraft:farmland");
    h.state.worlds[0].set_block(soil, farmland).unwrap();
    let v = villager(&mut h, at(5.5, -50.0, 5.5));
    vulnerable(&mut h, v);
    for _ in 0..40 {
        h.entities();
    }
    let y = h.state.rules.entities.get(v).unwrap().body.position.y;
    assert_eq!(y, -60.0625, "on the farmland (15/16 high)");
    assert_eq!(h.state.worlds[0].block(soil), Some(farmland));
    let lost = 20.0 - health(&h, v);
    assert!(
        (6.0..=8.0).contains(&lost),
        "about 10 blocks fallen: {lost}"
    );
}

fn offers_mut(h: &mut Harness, v: Id) -> &mut Offers {
    crate::tick::entity::offers_mut(&mut h.state, v).unwrap()
}

#[test]
fn a_trade_sends_the_villagers_viewers_the_keys_it_changed() {
    let mut h = Harness::with_player();
    let v = villager(&mut h, at(2.5, -60.0, 0.5));
    h.entities();
    h.take(1);
    let data = |h: &mut Harness| -> Vec<Vec<(u32, crate::protocol::MetadataValue)>> {
        h.entities();
        h.take(1)
            .into_iter()
            .filter_map(|p| match p {
                Packet::SetActorData(d) => Some(d.entity_metadata.into_iter().collect()),
                _ => None,
            })
            .collect()
    };
    let mut offer = crate::tick::trade::StoredOffer::new(
        stack("minecraft:emerald", 2),
        Stack::EMPTY,
        stack("minecraft:apple", 4),
        16,
    );
    offer.trader_xp = 5;
    offers_mut(&mut h, v).set(vec![offer]).unwrap();
    crate::tick::entity::villager_traded(&mut h.state, v, (0, 0));
    assert!(data(&mut h).is_empty(), "nothing changed, nothing sent");
    offers_mut(&mut h, v).apply(0, 1).unwrap();
    crate::tick::entity::villager_traded(&mut h.state, v, (0, 0));
    assert_eq!(
        data(&mut h),
        vec![vec![(K::TradeExperience as u32, Int32(5))]]
    );
    offers_mut(&mut h, v).apply(0, 1).unwrap();
    crate::tick::entity::villager_traded(&mut h.state, v, (0, 5));
    assert_eq!(
        data(&mut h),
        vec![vec![
            (K::TradeTier as u32, Int32(1)),
            (K::TradeExperience as u32, Int32(10)),
        ]]
    );
}

#[test]
fn npcs_are_shown_as_their_actor_and_humans_through_the_player_list() {
    let mut h = Harness::with_player();
    h.take(1);
    let v = villager(&mut h, at(2.5, -60.0, 0.5));
    let g = human(&mut h, at(4.5, -60.0, 0.5), "Guide");
    h.entities();
    let packets = h.take(1);
    let (vr, gr) = (e_runtime(&h, v), e_runtime(&h, g));
    assert_eq!(adds(&packets), vec![vr, gr]);
    let actor = packets.iter().find_map(|p| match p {
        Packet::AddActor(a) if a.entity_runtime_id == vr => Some(a),
        _ => None,
    });
    let actor = actor.unwrap();
    assert_eq!(actor.entity_type, "minecraft:villager_v2");
    // The official server's `villager_v2` (P6 `trade`): the variant, tier 0 of 4, no xp, skin 0.
    for (key, value) in [
        (K::Variant, 4),
        (K::TradeTier, 0),
        (K::MaxTradeTier, 4),
        (K::TradeExperience, 0),
        (K::SkinId, 0),
    ] {
        assert_eq!(
            actor.entity_metadata.get(&(key as u32)),
            Some(&Int32(value)),
            "{key:?}"
        );
    }
    let order: Vec<&str> = packets
        .iter()
        .filter_map(|p| match p {
            Packet::PlayerList(l) if l.entries[0].action_type == 0 => Some("add"),
            Packet::PlayerList(_) => Some("remove"),
            Packet::AddPlayer(a) if a.entity_runtime_id == gr => Some("player"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["add", "player", "remove"]);
}

/// An NPC's held item shows in its hand and is saved with the NPC.
/// A new viewer gets it after the NPC's actor (a human's with `AddPlayer`), and the viewers get
/// it when it changes. Other kinds hold nothing.
#[test]
fn an_npcs_held_item_shows_to_its_viewers_and_is_saved() {
    let mut h = Harness::with_player();
    h.take(1);
    let v = villager(&mut h, at(2.5, -60.0, 0.5));
    let g = human(&mut h, at(4.5, -60.0, 0.5), "Guide");
    let sword = stack("minecraft:diamond_sword", 1);
    h.entities();
    h.take(1);
    let mut cx = h.context();
    assert_eq!(cx.entity(v).unwrap().held_item(), Some(&Stack::EMPTY));
    for id in [v, g] {
        cx.entity_mut(id)
            .unwrap()
            .set_held_item(sword.clone())
            .unwrap();
        assert_eq!(cx.entity(id).unwrap().held_item(), Some(&sword));
    }
    h.entities();
    let (vr, gr) = (e_runtime(&h, v), e_runtime(&h, g));
    let held = |packets: &[Packet], runtime_id: u64| -> Vec<Stack> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::MobEquipment(m) if m.entity_runtime_id == runtime_id => {
                    Stack::from_network(&m.new_item.stack)
                }
                Packet::AddPlayer(a) if a.entity_runtime_id == runtime_id => {
                    Stack::from_network(&a.held_item.stack)
                }
                _ => None,
            })
            .collect()
    };
    let packets = h.take(1);
    assert_eq!(
        held(&packets, vr),
        std::slice::from_ref(&sword),
        "a change shows"
    );
    assert_eq!(held(&packets, gr), std::slice::from_ref(&sword));
    h.spawn(2, standing(0.0, 3.0));
    h.entities();
    let packets = h.take(2);
    let add = packets
        .iter()
        .position(|p| matches!(p, Packet::AddActor(a) if a.entity_runtime_id == vr));
    let equipment = packets
        .iter()
        .position(|p| matches!(p, Packet::MobEquipment(m) if m.entity_runtime_id == vr));
    assert!(add.is_some() && add < equipment, "after its actor");
    assert_eq!(held(&packets, vr), std::slice::from_ref(&sword));
    assert_eq!(held(&packets, gr), std::slice::from_ref(&sword));
    let mut cx = h.context();
    cx.entity_mut(v)
        .unwrap()
        .set_held_item(Stack::EMPTY)
        .unwrap();
    h.entities();
    assert_eq!(held(&h.take(1), vr), [Stack::EMPTY], "emptied");
    h.entities();
    assert_eq!(held(&h.take(1), vr), [], "once");
    let key = (0, ChunkPos::new(0, 0));
    let all = h.state.rules.entities.unload(key).expect("changed");
    h.state
        .rules
        .entities
        .load_list(key, &all, &crate::custom::Registry::default());
    let entities = &h.state.rules.entities;
    let loaded: Vec<_> = entities
        .ids()
        .filter_map(|id| entities.get(id))
        .filter_map(crate::tick::Entity::held_item)
        .cloned()
        .collect();
    assert_eq!(loaded, [Stack::EMPTY, sword]);
    assert!(crate::tick::entity::drop_item(
        &mut h.state,
        0,
        at(2.5, -60.0, 0.5),
        stack("minecraft:stick", 1)
    ));
    let entities = &h.state.rules.entities;
    let item = entities
        .ids()
        .find(|&id| matches!(entities.get(id).unwrap().kind, Kind::Item(_)))
        .unwrap();
    let mut cx = h.context();
    assert_eq!(cx.entity(item).unwrap().held_item(), None);
    assert!(
        cx.entity_mut(item)
            .unwrap()
            .set_held_item(Stack::EMPTY)
            .is_err()
    );
}

/// The hurt event of an NPC carries the official cause number of an explosion.
/// It is Mojang's `ActorDamageCause`: 10 for a block's or nothing's, 11 for an entity's.
#[test]
fn an_npc_hurt_by_an_explosion_shows_whose_explosion_it_was() {
    let mut h = Harness::with_player();
    let target = human(&mut h, at(3.5, -60.0, 0.5), "Target");
    let source = human(&mut h, at(-3.5, -60.0, 0.5), "Source");
    vulnerable(&mut h, target);
    h.entities();
    let runtime_id = e_runtime(&h, target);
    let mut causes = Vec::new();
    for by in [Source::None, Source::Entity(source)] {
        with(&mut h.state, target, |e| e.health = 20.0);
        h.take(1);
        let blast = crate::tick::entity::Blast::new(2.0, false);
        let center = Vec3::new(0.5, -59.0, 0.5);
        explode_with(
            &mut h.state,
            0,
            center,
            blast,
            by,
            TEST_HOOKS,
            crate::tick::entity::Raise::Events,
        );
        assert!(health(&h, target) > 0.0, "hurt, alive");
        h.entities();
        causes.extend(h.take(1).iter().filter_map(|p| match p {
            Packet::ActorEvent(a)
                if a.entity_runtime_id == runtime_id && a.event_type == ActorEvent::Hurt as u8 =>
            {
                Some(a.event_data)
            }
            _ => None,
        }));
        for _ in 0..10 {
            h.entities();
        }
    }
    assert_eq!(causes, [10, 11]);
}

/// The hurt event of an NPC carries the lightning cause number, as a player's does.
/// This is P6 `weather`.
#[test]
fn an_npc_hurt_by_lightning_shows_the_lightning_cause() {
    let mut h = Harness::with_player();
    let target = human(&mut h, at(3.5, -60.0, 0.5), "Target");
    vulnerable(&mut h, target);
    h.entities();
    let runtime_id = e_runtime(&h, target);
    h.take(1);
    let bolt = crate::world::Lightning { damage: true };
    assert!(crate::tick::entity::strike_lightning(
        &mut h.state,
        0,
        at(3.5, -60.0, 0.5),
        bolt
    ));
    h.entities();
    let causes: Vec<i32> = h
        .take(1)
        .iter()
        .filter_map(|p| match p {
            Packet::ActorEvent(a)
                if a.entity_runtime_id == runtime_id && a.event_type == ActorEvent::Hurt as u8 =>
            {
                Some(a.event_data)
            }
            _ => None,
        })
        .collect();
    assert_eq!(causes, [24]);
}

/// A lightning bolt hurts but sets no block on fire, whatever the blocks around the strike.
#[test]
fn lightning_sets_no_block_on_fire() {
    let mut h = Harness::with_player();
    let planks = named("minecraft:oak_planks");
    let below = BlockPos::new(3, -61, 0);
    h.state.worlds[0].set_block(below, planks).unwrap();
    let bolt = crate::world::Lightning { damage: true };
    assert!(crate::tick::entity::strike_lightning(
        &mut h.state,
        0,
        at(3.5, -60.0, 0.5),
        bolt
    ));
    h.entities();
    let fire = named("minecraft:fire");
    for x in -1..=7 {
        for y in -62..=-58 {
            for z in -4..=4 {
                let pos = BlockPos::new(x, y, z);
                assert_ne!(h.state.worlds[0].block(pos), Some(fire), "{pos:?}");
            }
        }
    }
    assert_eq!(h.state.worlds[0].block(below), Some(planks));
}

/// A sweet berry bush past its first stage and magma hurt every living thing inside them.
/// A vulnerable NPC in a bush and on magma takes their damage, as a player does.
#[test]
fn a_berry_bush_and_magma_hurt_a_vulnerable_npc_inside_them() {
    let mut h = Harness::with_player();
    let bush = crate::block::Kind::SweetBerryBush
        .default_state()
        .with("growth", crate::block::PropertyValue::Int(1))
        .unwrap();
    h.state.worlds[0]
        .set_block(BlockPos::new(3, -60, 0), bush)
        .unwrap();
    let pricked = human(&mut h, at(3.5, -60.0, 0.5), "Pricked");
    vulnerable(&mut h, pricked);
    h.entities();
    assert_eq!(health(&h, pricked), 19.0, "1 contact damage");
    // A magma block the NPC stands in: its box reaches into the block.
    h.state.worlds[0]
        .set_block(BlockPos::new(-3, -60, 0), named("minecraft:magma"))
        .unwrap();
    let burnt = human(&mut h, at(-2.5, -59.5, 0.5), "Burnt");
    vulnerable(&mut h, burnt);
    h.entities();
    assert!(health(&h, burnt) < 20.0, "fire damage");
}

// api: Entity::isInsideOfSolid
/// A zombie twice its size suffocates in a block at its scaled eye height.
#[test]
fn a_scaled_zombie_suffocates_at_its_scaled_eyes() {
    let mut h = Harness::with_player();
    h.state.worlds[0]
        .set_block(BlockPos::new(3, -58, 0), named("minecraft:stone"))
        .unwrap();
    let z = zombie(&mut h, at(3.5, -60.0, 0.5));
    let mut cx = h.context();
    cx.entity_mut(z).unwrap().set_scale(2.0).unwrap();
    h.entities();
    assert_eq!(health(&h, z), 19.0);
}

// api: Entity::getEyeHeight
/// The scale multiplies the whole eye height, the 0.1 above half the height included.
#[test]
fn a_scaled_zombie_has_its_whole_eye_height_scaled() {
    let mut h = Harness::with_player();
    let z = zombie(&mut h, at(3.5, -60.0, 0.5));
    let mut cx = h.context();
    for (scale, eye) in [(2.0, 2.1), (0.5, 0.525)] {
        cx.entity_mut(z).unwrap().set_scale(scale).unwrap();
        let got = cx.entity(z).unwrap().eye_height();
        assert!((got - eye).abs() < 1e-9, "scale {scale}: {got}");
    }
}
