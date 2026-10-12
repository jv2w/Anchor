//! The entity rules' events: each is raised at its seam with the right fields; cancelling keeps the
//! game as it was; changed fields change the outcome; monitors see the applied state; a listener
//! that changes the game so that the check fails cancels; items are conserved whatever the
//! listeners do.

use std::cell::Cell;

use tokio::sync::mpsc;

use super::*;
use crate::action::{Attack, Explode};
use crate::event::{
    AreaEffectCloudApply, EntityExplode, EntityPreExplode, EntityShootBow, EntityTeleport,
    Priority, ProjectileHitBlock, ProjectileHitEntity, ProjectileLaunch,
};
use crate::tick::Context;
use crate::tick::dispatch::run_deferred;

/// What listeners saw: they run on the test's thread (the tick's serial step), and send it
/// through a bounded channel.
struct Log<T> {
    tx: mpsc::Sender<T>,
    rx: Cell<Option<mpsc::Receiver<T>>>,
    seen: Cell<Vec<T>>,
}

impl<T> Log<T> {
    /// A sender for a listener.
    fn sink(&self) -> Sink<T> {
        Sink(self.tx.clone())
    }
}

/// A listener's end of a [`Log`].
struct Sink<T>(mpsc::Sender<T>);

impl<T> Sink<T> {
    fn push(&self, v: T) {
        assert!(self.0.try_send(v).is_ok(), "room in the log");
    }
}

fn log<T>() -> Log<T> {
    let (tx, rx) = mpsc::channel(4096);
    Log {
        tx,
        rx: Cell::new(Some(rx)),
        seen: Cell::new(Vec::new()),
    }
}

fn got<T: Clone>(l: &Log<T>) -> Vec<T> {
    let mut rx = l.rx.take().expect("one reader");
    let mut seen = l.seen.take();
    while let Ok(v) = rx.try_recv() {
        seen.push(v);
    }
    let out = seen.clone();
    l.seen.set(seen);
    l.rx.set(Some(rx));
    out
}

fn clear<T: Clone>(l: &Log<T>) {
    got(l);
    l.seen.set(Vec::new());
}

thread_local! {
    /// The modes tests switch their listeners between, by [`Mode`].
    static MODES: Cell<[u8; 4]> = const { Cell::new([0; 4]) };
    /// The modes handed out on this thread.
    static NEXT_MODE: Cell<usize> = const { Cell::new(0) };
    /// The state of [`roll`].
    static DICE: Cell<u64> = const { Cell::new(0) };
    /// An entity a listener acts on.
    static TARGET: Cell<Option<Id>> = const { Cell::new(None) };
}

/// A mode a test switches its listener between (0 at first).
#[derive(Debug, Clone, Copy)]
struct Mode(usize);

impl Mode {
    fn new() -> Self {
        let i = NEXT_MODE.with(Cell::get);
        NEXT_MODE.with(|n| n.set(i + 1));
        set(Self(i), 0);
        Self(i)
    }
}

fn mode(m: Mode) -> u8 {
    MODES.with(Cell::get)[m.0]
}

fn set(m: Mode, v: u8) {
    MODES.with(|c| {
        let mut all = c.get();
        all[m.0] = v;
        c.set(all);
    });
}

/// A roll below `n` from the test's fixed seed.
fn roll(n: usize) -> usize {
    let mut r = Rng(DICE.with(Cell::get));
    let v = r.below(n);
    DICE.with(|d| d.set(r.0));
    v
}

fn session(n: u64) -> SessionId {
    SessionId::new(n)
}

fn player_id(n: u64) -> crate::player::Id {
    crate::player::Id::new(session(n))
}

/// The newest entity of the store.
fn newest(h: &Harness) -> Id {
    h.state.rules.entities.ids().last().unwrap()
}

/// A human NPC at `pos` that takes damage.
fn npc_at(h: &mut Harness, pos: Vec3) -> Id {
    let npc = Npc::Human {
        name: "target".to_owned(),
        skin: None,
    };
    let id = spawn_npc(&mut h.state, 0, pos, npc).unwrap();
    with(&mut h.state, id, |e| {
        if let Kind::Npc(c) = &mut e.kind {
            c.invulnerable = false;
        }
    });
    id
}

fn health(h: &Harness, id: Id) -> f64 {
    h.state.rules.entities.get(id).unwrap().health
}

/// Player 1 (standing at block 0, 0, looking along +z) hits `target` in melee.
fn hit(h: &mut Harness, target: Id) {
    npc::attack_with(&mut h.state, session(1), target, TEST_HOOKS);
}

fn fill(h: &mut Harness, from: BlockPos, to: BlockPos, name: &str) {
    let s = named(name);
    for x in from.x..=to.x {
        for y in from.y..=to.y {
            for z in from.z..=to.z {
                h.state.worlds[0]
                    .set_block(BlockPos::new(x, y, z), s)
                    .unwrap();
            }
        }
    }
}

fn block(h: &Harness, pos: BlockPos) -> block::State {
    h.state.worlds[0].block(pos).unwrap()
}

fn held(h: &mut Harness, n: u64, stack: Stack) {
    inventory::set_held(h.state.players.get_mut(&session(n)).unwrap(), stack);
}

fn projectile_count(h: &Harness) -> usize {
    h.live()
        .iter()
        .filter(|e| matches!(e.kind, Kind::Projectile(_)) && !e.flagged)
        .count()
}

// pmmp: EntitySpawnEvent, ItemSpawnEvent, EntityDespawnEvent
#[test]
fn spawns_and_despawns_are_announced_once_by_the_entity_tick() {
    let seen: Log<(&'static str, Id, bool)> = log();
    let mut listeners = Listeners::default();
    let sink = seen.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntitySpawn, cx: &mut Context<'_>| {
            let live = cx.entity(e.entity()).is_some();
            sink.push(("spawn", e.entity(), live));
        },
    );
    let sink = seen.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut ItemSpawn, cx: &mut Context<'_>| {
            let live = cx.entity(e.entity()).is_some();
            sink.push(("item", e.entity(), live));
        },
    );
    let sink = seen.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityDespawn, cx: &mut Context<'_>| {
            let live = cx.entity(e.entity()).is_some();
            sink.push(("despawn", e.entity(), live));
        },
    );
    let sink = seen.sink();
    listeners.push_monitor(move |e: &EntitySpawn, cx: &Context<'_>| {
        let live = cx.entity(e.entity()).is_some();
        sink.push(("watched", e.entity(), live));
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    clear(&seen);
    assert!(drop_item(
        &mut h.state,
        0,
        at(0.5, -59.0, 0.5),
        stack("minecraft:stick", 1)
    ));
    let item = newest(&h);
    drop_experience(&mut h.state, 0, at(12.5, -59.0, 12.5), 3);
    let orb = newest(&h);
    assert!(got(&seen).is_empty(), "the entity tick announces");
    h.entities();
    assert_eq!(
        got(&seen),
        [
            ("item", item, true),
            ("spawn", item, true),
            ("watched", item, true),
            ("spawn", orb, true),
            ("watched", orb, true)
        ]
    );
    clear(&seen);
    h.entities();
    assert!(got(&seen).is_empty(), "once");
    assert!(remove(&mut h.state, item));
    h.entities();
    assert_eq!(got(&seen), [("despawn", item, false)]);
    // Unloading: the announced orb leaves with its chunk and is announced gone; an item made and
    // unloaded before the entity tick was never announced, so it goes unannounced.
    clear(&seen);
    assert!(drop_item(
        &mut h.state,
        0,
        at(12.5, -59.0, 13.5),
        stack("minecraft:stick", 1)
    ));
    let key = (0, chunk_of(at(12.5, -59.0, 12.5)));
    let _list = h.state.rules.entities.unload(key);
    h.entities();
    assert_eq!(got(&seen), [("despawn", orb, false)]);
}

// pmmp: ItemDespawnEvent
#[test]
fn a_cancelled_item_despawn_gives_the_whole_time_again() {
    let switch = Mode::new();
    let watched: Log<bool> = log();
    let mut listeners = Listeners::default();
    let sw = switch;
    listeners.push(
        Priority::Normal,
        move |e: &mut ItemDespawn, cx: &mut Context<'_>| match mode(sw) {
            0 => e.cancel(),
            1 => {
                let mut item = cx.entity_mut(e.entity()).unwrap();
                assert!(item.set_despawn_delay_ticks(100));
            }
            _ => {}
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &ItemDespawn, cx: &Context<'_>| {
        sink.push(cx.entity(e.entity()).is_none());
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    assert!(drop_item(
        &mut h.state,
        0,
        at(0.5, -59.0, 0.5),
        stack("minecraft:stick", 1)
    ));
    let id = newest(&h);
    let delay = |h: &mut Harness, d: i32| {
        with(&mut h.state, id, |e| {
            if let Kind::Item(i) = &mut e.kind {
                i.despawn_delay = d;
            }
        });
    };
    let left = |h: &Harness| {
        h.state
            .rules
            .entities
            .get(id)
            .and_then(Entity::despawn_delay_ticks)
    };
    delay(&mut h, 1);
    h.entities();
    assert_eq!(
        left(&h),
        Some(DEFAULT_DESPAWN_DELAY),
        "cancelled: the time again"
    );
    set(switch, 1);
    delay(&mut h, 1);
    h.entities();
    assert_eq!(left(&h), Some(100), "a listener's own time stays");
    set(switch, 2);
    delay(&mut h, 1);
    h.entities();
    assert!(h.live().is_empty());
    assert_eq!(got(&watched), [true], "the monitor sees it gone");
}

// pmmp: ItemMergeEvent
#[test]
fn item_merges_ask_their_listeners_and_a_changed_target_stops_them() {
    let switch = Mode::new();
    let watched: Log<(Id, Option<u8>)> = log();
    let asked: Log<u8> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, asked.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut ItemMerge, cx: &mut Context<'_>| {
            assert_ne!(e.entity(), e.target());
            sink.push(mode(sw));
            match mode(sw) {
                0 => e.cancel(),
                1 => {
                    assert!(
                        cx.entity_mut(e.target())
                            .unwrap()
                            .set_pickup_delay_ticks(-1)
                    );
                }
                _ => {}
            }
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &ItemMerge, cx: &Context<'_>| {
        let count = cx
            .entity(e.target())
            .and_then(|t| t.item().map(Stack::count));
        sink.push((e.target(), count));
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    for count in [5, 3] {
        assert!(drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", count)
        ));
    }
    // Merging checks only moving items: each round throws them up again.
    let toss = |h: &mut Harness| {
        let ids: Vec<Id> = h.state.rules.entities.ids().collect();
        for id in ids {
            with(&mut h.state, id, |e| {
                e.body.motion = Vec3::new(0.0, 0.3, 0.0);
                if let Kind::Item(i) = &mut e.kind {
                    i.pickup_delay = 0;
                }
            });
        }
        for _ in 0..30 {
            h.entities();
        }
    };
    toss(&mut h);
    assert_eq!(h.live().len(), 2, "cancelled");
    assert!(got(&asked).contains(&0));
    set(switch, 1);
    toss(&mut h);
    assert!(got(&asked).contains(&1));
    assert_eq!(h.live().len(), 2, "the target can no longer merge");
    assert!(got(&watched).is_empty());
    set(switch, 2);
    toss(&mut h);
    assert_eq!(h.ground(), 8);
    assert_eq!(h.live().len(), 1, "merged");
    let w = got(&watched);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].1, Some(8), "the monitor sees the merged stack");
}

// pmmp: EntityItemPickupEvent
#[test]
fn a_pickup_offers_what_fits_and_listeners_decide_what_moves() {
    let switch = Mode::new();
    let seen: Log<(Actor, Id, u8)> = log();
    let watched: Log<Option<u8>> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityItemPickup, cx: &mut Context<'_>| {
            sink.push((e.entity(), e.origin(), e.item().count()));
            match mode(sw) {
                0 => e.cancel(),
                1 => e.set_item(stack("minecraft:diamond", 2)),
                2 => {
                    let far = Vec3::new(20.5, -59.0, 20.5);
                    assert!(
                        cx.entity_mut(e.origin())
                            .unwrap()
                            .teleport(world::Id::DEFAULT, far)
                    );
                }
                _ => {}
            }
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &EntityItemPickup, cx: &Context<'_>| {
        let left = cx
            .entity(e.origin())
            .and_then(|o| o.item().map(Stack::count));
        sink.push(left);
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    room(3);
    assert!(drop_item(
        &mut h.state,
        0,
        at(0.5, -59.0, 0.5),
        stack("minecraft:stick", 10)
    ));
    let item = newest(&h);
    for _ in 0..20 {
        h.entities();
    }
    assert_eq!((h.ground(), taken()), (10, 0), "cancelled");
    let s = got(&seen);
    assert!(!s.is_empty());
    assert!(
        s.iter()
            .all(|&e| e == (Actor::Player(player_id(1)), item, 3))
    );
    assert!(got(&watched).is_empty());
    set(switch, 1);
    h.entities();
    assert_eq!(taken(), 2, "the replacement goes to the player");
    assert_eq!(
        h.ground(),
        7,
        "the item on the ground loses what was offered"
    );
    assert_eq!(got(&watched), [Some(7)]);
    set(switch, 3);
    room(3);
    h.entities();
    assert_eq!((h.ground(), taken()), (4, 5));
    set(switch, 2);
    room(3);
    h.entities();
    assert_eq!(
        (h.ground(), taken()),
        (4, 5),
        "moved out of reach: no pickup"
    );
    let e = h.state.rules.entities.get(item).unwrap();
    assert_eq!(e.body.position.x, 20.5);
}

// pmmp: EntityItemPickupEvent, ItemMergeEvent
#[test]
fn listeners_that_cancel_move_items_and_change_room_neither_make_nor_lose_items() {
    DICE.with(|d| d.set(0xe8_b401));
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityItemPickup, cx: &mut Context<'_>| {
            let roll = roll(5);
            match roll {
                0 => e.cancel(),
                1 => {
                    let pos = cx.entity(e.origin()).unwrap().position() + Vec3::new(0.7, 0.0, 0.0);
                    cx.entity_mut(e.origin())
                        .unwrap()
                        .teleport(world::Id::DEFAULT, pos);
                }
                2 => room(0),
                3 => room(1),
                _ => {}
            }
        },
    );
    listeners.push(
        Priority::Normal,
        move |e: &mut ItemMerge, cx: &mut Context<'_>| {
            let roll = roll(3);
            match roll {
                0 => e.cancel(),
                1 => {
                    cx.entity_mut(e.target()).unwrap().set_pickup_delay_ticks(5);
                }
                _ => {}
            }
        },
    );
    let mut h = Harness::with_listeners(2, listeners);
    h.spawn(1, standing(0.0, 0.0));
    room(400);
    let mut drops = Rng(0xe8_b402);
    let mut dropped = 0u64;
    for tick in 0..300 {
        if tick < 150 && tick % 2 == 0 {
            for _ in 0..3 {
                let s = stack(
                    "minecraft:stick",
                    u8::try_from(drops.below(16) + 1).unwrap(),
                );
                let x = f64::from(u32::try_from(drops.below(6)).unwrap()) - 3.0;
                let z = f64::from(u32::try_from(drops.below(6)).unwrap()) - 3.0;
                dropped += u64::from(s.count());
                assert!(drop_item(&mut h.state, 0, at(x + 0.5, -58.0, z + 0.5), s));
            }
        }
        if tick % 25 == 0 {
            room(400);
        }
        h.entities();
        assert_eq!(h.ground() + taken(), dropped, "tick {tick}");
    }
    assert!(taken() > 0);
}

// pmmp: EntityDamageEvent, EntityDamageByEntityEvent
#[test]
fn an_npc_hit_raises_damage_with_its_source_and_listeners_decide() {
    let switch = Mode::new();
    let seen: Log<(Actor, DamageCause, DamageSource, f64)> = log();
    let watched: Log<f64> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityDamage, cx: &mut Context<'_>| {
            sink.push((e.entity(), e.cause(), e.source(), e.base_damage()));
            match mode(sw) {
                0 => e.cancel(),
                1 => e.set_base_damage(5.0),
                _ => {
                    let Actor::Entity(id) = e.entity() else {
                        panic!("an entity")
                    };
                    cx.entity_mut(id).unwrap().flag_for_despawn();
                }
            }
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &EntityDamage, cx: &Context<'_>| {
        let Actor::Entity(id) = e.entity() else {
            panic!("an entity")
        };
        sink.push(cx.entity(id).map_or(-1.0, Entity::health));
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    hit(&mut h, npc);
    assert_eq!(health(&h, npc), 20.0, "cancelled");
    assert_eq!(
        h.state.rules.entities.get(npc).unwrap().body.motion,
        Vec3::ZERO,
        "no knockback"
    );
    let s = got(&seen);
    assert_eq!(s.len(), 1);
    let source = DamageSource::Entity(Actor::Player(player_id(1)));
    assert_eq!(
        (s[0].0, s[0].1, s[0].2),
        (Actor::Entity(npc), DamageCause::EntityAttack, source)
    );
    let base = s[0].3;
    assert!(base > 0.0);
    set(switch, 1);
    hit(&mut h, npc);
    assert_eq!(health(&h, npc), 15.0, "the listener's damage");
    assert_ne!(
        h.state.rules.entities.get(npc).unwrap().body.motion,
        Vec3::ZERO
    );
    assert_eq!(got(&watched), [15.0], "the monitor sees the new health");
    set(switch, 2);
    let other = npc_at(&mut h, at(1.5, -60.0, 2.5));
    hit(&mut h, other);
    assert_eq!(health(&h, other), 20.0, "removed by the listener: not hurt");
}

// pmmp: EntityDeathEvent
#[test]
fn a_dying_npc_drops_what_its_death_listeners_give_it() {
    let watched: Log<usize> = log();
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |e: &mut EntityDeath, _: &mut Context<'_>| {
            assert!(
                e.drops().is_empty() && e.xp_drop_amount() == 0,
                "NPCs drop nothing"
            );
            e.set_drops(vec![
                stack("minecraft:stick", 3),
                stack("minecraft:apple", 2),
            ]);
            e.set_xp_drop_amount(7);
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &EntityDeath, _: &Context<'_>| {
        sink.push(e.drops().len());
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    with(&mut h.state, npc, |e| e.health = 0.5);
    hit(&mut h, npc);
    assert!(npc::is_dying(&h.state, npc));
    assert_eq!(h.ground(), 0, "the drops come at its next tick");
    h.entities();
    assert_eq!(h.ground(), 5);
    let xp: u16 = h
        .live()
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::ExperienceOrb(o) => Some(o.value),
            _ => None,
        })
        .sum();
    assert_eq!(xp, 7);
    assert_eq!(got(&watched), [2]);
}

// pmmp: EntityMotionEvent
#[test]
fn a_cancelled_motion_keeps_the_npc_still() {
    let switch = Mode::new();
    let seen: Log<Vec3> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityMotion, _: &mut Context<'_>| {
            sink.push(e.motion());
            if mode(sw) == 0 {
                e.cancel();
            }
        },
    );
    let sink = seen.sink();
    listeners.push_monitor(move |e: &EntityMotion, cx: &Context<'_>| {
        let Actor::Entity(id) = e.entity() else {
            panic!("an entity")
        };
        assert_eq!(cx.entity(id).unwrap().motion(), e.motion());
        sink.push(Vec3::ZERO);
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    hit(&mut h, npc);
    assert_eq!(
        h.state.rules.entities.get(npc).unwrap().body.motion,
        Vec3::ZERO
    );
    let s = got(&seen);
    assert_eq!(s.len(), 1);
    assert!(s[0].z > 0.0, "away from the player: {:?}", s[0]);
    set(switch, 1);
    let other = npc_at(&mut h, at(-0.5, -60.0, 2.5));
    hit(&mut h, other);
    assert_ne!(
        h.state.rules.entities.get(other).unwrap().body.motion,
        Vec3::ZERO
    );
    assert_eq!(got(&seen).len(), 3, "listener and monitor");
}

// pmmp: EntityDamageByEntityEvent
/// A knockback motion a damage listener sets is the NPC's motion after a hit, the weapon's
/// Knockback adding nothing, sent to its viewers as many times as the listener asked, and a
/// stronger hit within its cooldown pushes it too.
#[test]
fn a_knockback_motion_pushes_the_npc_by_exactly_it() {
    let motion = Vec3::new(0.25, 0.375, -0.5);
    let twice = std::num::NonZeroU8::new(2).unwrap();
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityDamage, _: &mut Context<'_>| e.set_knockback_motion(motion, twice),
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    h.step(Vec::new());
    let runtime_id = h.state.rules.entities.get(npc).unwrap().runtime_id;
    h.take(1);
    let extra = crate::item::Extra {
        enchantments: vec![(crate::item::Enchantment::Knockback, 2)],
        ..crate::item::Extra::default()
    };
    let sword = stack("minecraft:wooden_sword", 1);
    held(&mut h, 1, sword.with_extra(Some(extra)).unwrap());
    hit(&mut h, npc);
    let motion_of = |h: &Harness| h.state.rules.entities.get(npc).unwrap().body.motion;
    assert_eq!(motion_of(&h), motion);
    h.collect();
    let sent = h
        .take(1)
        .into_iter()
        .filter(|p| matches!(p, Packet::SetActorMotion(m) if m.entity_runtime_id == runtime_id))
        .collect::<Vec<_>>();
    let want = Packet::SetActorMotion(crate::protocol::packet::SetActorMotion {
        entity_runtime_id: runtime_id,
        velocity: motion.to_f32(),
        tick: 0,
    });
    assert_eq!(sent, [want.clone(), want], "sent twice, at the hit");
    with(&mut h.state, npc, |e| e.body.motion = Vec3::ZERO);
    held(&mut h, 1, stack("minecraft:diamond_sword", 1));
    hit(&mut h, npc);
    assert_eq!(motion_of(&h), motion, "a stronger hit within the cooldown");
}

// pmmp: EntityDamageEvent::uncancel
/// A listener can make a hit within an NPC's cooldown act as one after it (it starts the cooldown
/// again and shows the hurt animation) and set the health it takes outright.
#[test]
fn a_hit_on_an_npc_can_restart_the_cooldown_and_take_set_health() {
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |e: &mut EntityDamage, _: &mut Context<'_>| {
            e.uncancel();
            e.set_restarts_cooldown(true);
            e.set_dealt(1.5);
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    h.step(Vec::new());
    let runtime_id = h.state.rules.entities.get(npc).unwrap().runtime_id;
    hit(&mut h, npc);
    let attack_time = |h: &Harness| match &h.state.rules.entities.get(npc).unwrap().kind {
        Kind::Npc(c) => c.attack_time,
        _ => panic!("an NPC"),
    };
    with(&mut h.state, npc, |e| {
        if let Kind::Npc(c) = &mut e.kind {
            c.attack_time = 5;
        }
    });
    h.take(1);
    hit(&mut h, npc);
    assert_eq!(health(&h, npc), 17.0, "1.5 a hit, within the cooldown too");
    assert_eq!(attack_time(&h), 10, "the cooldown starts again");
    h.collect();
    let hurt = h.take(1).into_iter().any(|p| {
        matches!(p, Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id
            && a.event_type == u8::from(ActorEvent::Hurt))
    });
    assert!(hurt, "the hurt animation shows again");
}

// pmmp: EntityDamageEvent::uncancel
/// A hit within an NPC's cooldown that is no stronger than the last is raised cancelled while
/// there are damage listeners: left cancelled it does nothing, uncancelled it lands as a stronger
/// hit within the cooldown does, the last damage counted off.
#[test]
fn a_hit_the_npc_cooldown_refuses_is_raised_cancelled_and_lands_when_uncancelled() {
    let switch = Mode::new();
    let seen: Log<(f64, bool)> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityDamage, _: &mut Context<'_>| {
            sink.push((e.base_damage(), e.is_cancelled()));
            if mode(sw) == 1 {
                e.uncancel();
                e.set_base_damage(10.0);
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    hit(&mut h, npc);
    let first = health(&h, npc);
    hit(&mut h, npc);
    assert_eq!(health(&h, npc), first, "left cancelled, it does nothing");
    set(switch, 1);
    hit(&mut h, npc);
    let s = got(&seen);
    let base = s[0].0;
    assert_eq!(s, [(base, false), (base, true), (base, true)]);
    assert_eq!(health(&h, npc), first - (10.0 - base));
}

// pmmp: EntityCombustEvent, EntityCombustByBlockEvent, EntityExtinguishEvent
#[test]
fn fire_sets_entities_burning_through_combust_and_water_puts_them_out() {
    let switch = Mode::new();
    let seen: Log<(Actor, u32, CombustSource)> = log();
    let out: Log<ExtinguishCause> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityCombust, _: &mut Context<'_>| {
            sink.push((e.entity(), e.duration_secs(), e.source()));
            if mode(sw) == 0 {
                e.cancel();
            } else {
                e.set_duration_secs(3);
            }
        },
    );
    let sink = out.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityExtinguish, _: &mut Context<'_>| {
            sink.push(e.cause());
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    // An invulnerable NPC takes no fire damage but burns.
    let npc = spawn_npc(
        &mut h.state,
        0,
        at(0.5, -60.0, 0.5),
        Npc::Human {
            name: "n".to_owned(),
            skin: None,
        },
    )
    .unwrap();
    let cell = BlockPos::new(0, -60, 0);
    h.state.worlds[0]
        .set_block(cell, named("minecraft:fire"))
        .unwrap();
    let fire = |h: &Harness| h.state.rules.entities.get(npc).unwrap().fire_ticks;
    h.entities();
    assert_eq!(fire(&h), 0, "cancelled");
    let s = got(&seen);
    assert_eq!(s, [(Actor::Entity(npc), 8, CombustSource::Block(cell))]);
    set(switch, 1);
    h.entities();
    assert_eq!(fire(&h), 59, "3 seconds, then one tick of burning");
    h.state.worlds[0]
        .set_block(cell, named("minecraft:water"))
        .unwrap();
    h.entities();
    assert_eq!(fire(&h), 0);
    assert_eq!(got(&out), [ExtinguishCause::Water]);
    assert_eq!(health(&h, npc), 20.0);
}

// pmmp: EntityPreExplodeEvent, EntityExplodeEvent
#[test]
fn tnt_asks_before_it_explodes_and_listeners_choose_its_blocks() {
    let pre = Mode::new();
    let boom = Mode::new();
    let seen: Log<(f64, f64, bool)> = log();
    let lists: Log<(usize, f64)> = log();
    let mut listeners = Listeners::default();
    let (pre_mode, sink) = (pre, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityPreExplode, _: &mut Context<'_>| {
            sink.push((e.radius(), e.fire_chance(), e.is_block_breaking()));
            match mode(pre_mode) {
                0 => e.cancel(),
                1 => e.set_block_breaking(false),
                _ => {}
            }
        },
    );
    let (boom_mode, sink) = (boom, lists.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityExplode, _: &mut Context<'_>| {
            sink.push((e.blocks().len(), e.yield_percent()));
            if mode(boom_mode) == 0 {
                e.cancel();
            } else {
                e.set_blocks(vec![BlockPos::new(1, -50, 0)]);
                e.set_yield_percent(0.0);
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(20.0, 20.0));
    let (from, to) = (BlockPos::new(-3, -53, -3), BlockPos::new(3, -47, 3));
    fill(&mut h, from, to, "minecraft:dirt");
    let center = BlockPos::new(0, -50, 0);
    // On top of the dirt, within reach of the blast.
    let npc = npc_at(&mut h, at(0.5, -46.0, 0.5));
    let explode = |h: &mut Harness| {
        h.state.worlds[0]
            .set_block(center, named("minecraft:tnt"))
            .unwrap();
        assert!(ignite(&mut h.state, 0, center, 1));
        for _ in 0..3 {
            h.entities();
        }
        assert!(
            h.live().iter().all(|e| !matches!(e.kind, Kind::Tnt(_))),
            "the TNT is gone either way"
        );
    };
    let dirt = named("minecraft:dirt");
    explode(&mut h);
    assert_eq!(got(&seen), [(4.0, 0.0, true)]);
    assert_eq!(block(&h, BlockPos::new(1, -50, 0)), dirt, "cancelled first");
    assert!(got(&lists).is_empty());
    set(pre, 1);
    explode(&mut h);
    assert_eq!(got(&lists), [(0, 25.0)], "no blocks found, yield 1 / size");
    assert_eq!(
        block(&h, BlockPos::new(1, -50, 0)),
        dirt,
        "the explosion was cancelled"
    );
    assert_eq!(health(&h, npc), 20.0, "nothing hurt when cancelled");
    set(pre, 2);
    set(boom, 1);
    clear(&lists);
    explode(&mut h);
    assert!(got(&lists)[0].0 > 1, "the blocks the rays found");
    assert_eq!(
        block(&h, BlockPos::new(1, -50, 0)),
        block::AIR,
        "the listener's block"
    );
    assert_eq!(block(&h, BlockPos::new(-1, -50, 0)), dirt, "only that one");
    assert_eq!(h.ground(), 0, "a yield of 0 drops nothing");
    assert!(health(&h, npc) < 20.0, "the blast hurts once it goes off");
}

// pmmp: ProjectileLaunchEvent, EntityShootBowEvent
#[test]
fn launches_and_bow_shots_ask_their_listeners() {
    let switch = Mode::new();
    let forces: Log<f64> = log();
    let mut listeners = Listeners::default();
    let sw = switch;
    listeners.push(
        Priority::Normal,
        move |e: &mut ProjectileLaunch, cx: &mut Context<'_>| {
            assert!(cx.entity(e.entity()).is_some());
            match mode(sw) {
                0 => e.cancel(),
                1 => {
                    // The thrower's hand changes: the throw no longer holds.
                    let p = cx.state.players.get_mut(&session(1)).unwrap();
                    inventory::set_held(p, Stack::EMPTY);
                }
                _ => {}
            }
        },
    );
    let (sw, sink) = (switch, forces.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityShootBow, _: &mut Context<'_>| {
            sink.push(e.force());
            assert_eq!(e.entity(), Actor::Player(player_id(1)));
            assert!(
                e.bow()
                    .item()
                    .is_some_and(|t| t.is(crate::item::Class::Bow))
            );
            if mode(sw) == 3 {
                e.cancel();
            } else {
                e.set_force(1.0);
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let snowball = stack("minecraft:snowball", 1);
    held(&mut h, 1, snowball.clone());
    let throw =
        |h: &mut Harness| projectile::throw_with(&mut h.state, session(1), &snowball, TEST_HOOKS);
    assert!(!throw(&mut h), "cancelled");
    assert_eq!(projectile_count(&h), 0);
    set(switch, 1);
    assert!(!throw(&mut h), "the hand changed");
    assert_eq!(projectile_count(&h), 0);
    held(&mut h, 1, snowball.clone());
    set(switch, 2);
    assert!(throw(&mut h));
    assert_eq!(projectile_count(&h), 1);
    for _ in 0..80 {
        h.entities();
    }
    calls(|c| c.arrows = 1);
    held(&mut h, 1, stack("minecraft:bow", 1));
    set(switch, 3);
    assert!(!projectile::release_bow_with(
        &mut h.state,
        session(1),
        20,
        TEST_HOOKS
    ));
    assert_eq!(calls(|c| c.arrows), 1, "the arrow stays");
    assert_eq!(projectile_count(&h), 0);
    set(switch, 2);
    assert!(projectile::release_bow_with(
        &mut h.state,
        session(1),
        20,
        TEST_HOOKS
    ));
    assert_eq!(got(&forces), [3.0, 3.0], "a full draw");
    let speed = h
        .live()
        .iter()
        .find(|e| matches!(e.kind, Kind::Projectile(_)))
        .unwrap()
        .body
        .motion
        .length();
    assert!((speed - 1.0).abs() < 1e-9, "the listener's force: {speed}");
}

// pmmp: ProjectileHitBlockEvent, ProjectileHitEntityEvent
#[test]
fn projectile_hits_name_what_they_hit() {
    let blocks: Log<(Id, BlockPos, block::State, Vec3)> = log();
    let entities: Log<(Id, Actor)> = log();
    let mut listeners = Listeners::default();
    let sink = blocks.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut ProjectileHitBlock, _: &mut Context<'_>| {
            assert_eq!(e.world(), world::Id::DEFAULT);
            sink.push((e.entity(), e.pos(), e.block(), e.hit_position()));
        },
    );
    let sink2 = entities.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut ProjectileHitEntity, cx: &mut Context<'_>| {
            sink2.push((e.entity(), e.entity_hit()));
            // Taking the projectile away stops the hit.
            cx.entity_mut(e.entity()).unwrap().flag_for_despawn();
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let wall = BlockPos::new(0, -59, 6);
    let stone = named("minecraft:stone");
    h.state.worlds[0].set_block(wall, stone).unwrap();
    calls(|c| c.look = (0.0, 0.0));
    let snowball = stack("minecraft:snowball", 1);
    assert!(projectile::throw_with(
        &mut h.state,
        session(1),
        &snowball,
        TEST_HOOKS
    ));
    let ball = newest(&h);
    for _ in 0..6 {
        h.entities();
    }
    let b = got(&blocks);
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].0, b[0].1, b[0].2), (ball, wall, stone));
    assert_eq!(b[0].3.z, 6.0, "on the wall's face");
    // An NPC in the way of an arrow: the hit names it, and the listener's removal spares it.
    let npc = npc_at(&mut h, at(0.5, -60.0, 3.5));
    calls(|c| c.arrows = 1);
    held(&mut h, 1, stack("minecraft:bow", 1));
    assert!(projectile::release_bow_with(
        &mut h.state,
        session(1),
        20,
        TEST_HOOKS
    ));
    let arrow = newest(&h);
    for _ in 0..4 {
        h.entities();
    }
    assert_eq!(got(&entities), [(arrow, Actor::Entity(npc))]);
    assert_eq!(health(&h, npc), 20.0);
}

// pmmp: EntityTeleportEvent
#[test]
fn an_ender_pearl_landing_teleports_through_its_listeners() {
    let switch = Mode::new();
    let seen: Log<(Actor, Vec3, Vec3)> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityTeleport, _: &mut Context<'_>| {
            assert_eq!(
                (e.origin_world(), e.target_world()),
                (world::Id::DEFAULT, world::Id::DEFAULT)
            );
            sink.push((e.entity(), e.origin(), e.target()));
            if mode(sw) == 0 {
                e.cancel();
            } else {
                e.set_target(world::Id::DEFAULT, Vec3::new(5.5, -60.0, 5.5));
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    calls(|c| c.look = (0.0, 20.0));
    let pearl = stack("minecraft:ender_pearl", 1);
    let land = |h: &mut Harness| {
        assert!(projectile::throw_with(
            &mut h.state,
            session(1),
            &pearl,
            TEST_HOOKS
        ));
        for _ in 0..60 {
            h.entities();
        }
        assert_eq!(projectile_count(h), 0, "it landed");
    };
    land(&mut h);
    let s = got(&seen);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].0, Actor::Player(player_id(1)));
    assert!(
        (s[0].1.y + 60.0).abs() < 1e-3,
        "from the feet: {:?}",
        s[0].1
    );
    assert!(s[0].2.z > 2.0, "to where it landed: {:?}", s[0].2);
    assert!(
        calls(|c| c.teleports.is_empty() && c.damage.is_empty()),
        "cancelled"
    );
    set(switch, 1);
    land(&mut h);
    let teleports = calls(|c| c.teleports.clone());
    assert_eq!(
        teleports,
        [(1, 0, Vec3::new(5.5, -60.0 + EYE_HEIGHT, 5.5))],
        "the listener's place"
    );
    assert_eq!(calls(|c| c.damage.len()), 1);
}

// pmmp: AreaEffectCloudApplyEvent
#[test]
fn a_cancelled_cloud_application_gives_no_effects() {
    let switch = Mode::new();
    let seen: Log<Vec<Actor>> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut AreaEffectCloudApply, cx: &mut Context<'_>| {
            assert!(cx.entity(e.entity()).is_some());
            sink.push(e.affected().to_vec());
            if mode(sw) == 0 {
                e.cancel();
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    calls(|c| c.look = (0.0, 90.0));
    let p = *crate::effect::Potion::all()
        .iter()
        .find(|p| format!("{p:?}") == "Regeneration")
        .unwrap();
    let t = crate::item::Type::from_name(
        "minecraft:lingering_potion",
        u16::from(p.id()),
        None,
        &Compound::new(),
    )
    .unwrap();
    assert!(projectile::throw_with(
        &mut h.state,
        session(1),
        &Stack::new(t, 1),
        TEST_HOOKS
    ));
    for _ in 0..40 {
        h.entities();
    }
    let s = got(&seen);
    assert!(!s.is_empty());
    assert!(s.iter().all(|a| a == &[Actor::Player(player_id(1))]));
    assert!(calls(|c| c.effects.is_empty()), "cancelled");
    set(switch, 1);
    for _ in 0..60 {
        h.entities();
    }
    assert!(calls(|c| !c.effects.is_empty()));
}

// pmmp: EntityBlockChangeEvent
#[test]
fn a_landing_falling_block_asks_before_it_becomes_a_block() {
    let switch = Mode::new();
    let seen: Log<(block::State, block::State, BlockPos)> = log();
    let watched: Log<block::State> = log();
    let mut listeners = Listeners::default();
    let (sw, sink) = (switch, seen.sink());
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityBlockChange, cx: &mut Context<'_>| {
            assert!(cx.entity(e.entity()).is_some());
            sink.push((e.block(), e.to(), e.pos()));
            match mode(sw) {
                0 => e.cancel(),
                1 => {
                    let w = cx.world_mut(e.world()).unwrap();
                    w.set_block(e.pos(), named("minecraft:stone")).unwrap();
                }
                _ => {}
            }
        },
    );
    let sink = watched.sink();
    listeners.push_monitor(move |e: &EntityBlockChange, cx: &Context<'_>| {
        sink.push(cx.world(e.world()).unwrap().block(e.pos()).unwrap());
    });
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    let sand = named("minecraft:sand");
    let landing = |h: &mut Harness, x: i32| {
        let pos = BlockPos::new(x, -55, 0);
        h.state.worlds[0].set_block(pos, sand).unwrap();
        assert!(fall(&mut h.state, 0, pos, sand));
        for _ in 0..40 {
            h.entities();
        }
        assert!(h.live().is_empty(), "the falling block goes either way");
        block(h, BlockPos::new(x, -60, 0))
    };
    assert_eq!(landing(&mut h, 0), block::AIR, "cancelled");
    assert_eq!(got(&seen), [(block::AIR, sand, BlockPos::new(0, -60, 0))]);
    set(switch, 1);
    assert_eq!(
        landing(&mut h, 2),
        named("minecraft:stone"),
        "the listener's block stays"
    );
    set(switch, 2);
    assert_eq!(landing(&mut h, 4), sand);
    assert_eq!(got(&watched), [sand]);
    assert_eq!(h.ground(), 0, "nothing dropped");
}

// pmmp: EntityRegainHealthEvent
#[test]
fn healing_an_npc_asks_its_listeners() {
    let switch = Mode::new();
    let mut listeners = Listeners::default();
    let sw = switch;
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityRegainHealth, _: &mut Context<'_>| {
            assert_eq!(e.regain_reason(), RegainReason::Magic);
            assert_eq!(e.amount(), 4.0);
            if mode(sw) == 0 {
                e.cancel();
            } else {
                e.set_amount(1.0);
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    let npc = npc_at(&mut h, at(0.5, -60.0, 0.5));
    with(&mut h.state, npc, |e| e.health = 10.0);
    super::super::heal(&mut h.state, npc, 4.0, RegainReason::Magic);
    assert_eq!(health(&h, npc), 10.0, "cancelled");
    set(switch, 1);
    super::super::heal(&mut h.state, npc, 4.0, RegainReason::Magic);
    assert_eq!(health(&h, npc), 11.0, "the listener's amount");
}

/// A listener that moves an entity while the steps apply: the entity moves on from where the
/// listener put it, not by the step computed before (which would undo the move).
#[test]
fn an_entity_a_listener_moves_during_the_tick_moves_on_from_there() {
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        move |e: &mut ItemDespawn, cx: &mut Context<'_>| {
            e.cancel();
            let id = TARGET.with(Cell::get).unwrap();
            let far = Vec3::new(5.5, -55.0, 5.5);
            assert!(cx.entity_mut(id).unwrap().teleport(world::Id::DEFAULT, far));
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(8.0, 8.0));
    assert!(drop_item(
        &mut h.state,
        0,
        at(0.5, -59.0, 0.5),
        stack("minecraft:stick", 1)
    ));
    let first = newest(&h);
    assert!(drop_item(
        &mut h.state,
        0,
        at(-2.5, -50.0, -2.5),
        stack("minecraft:apple", 1)
    ));
    let second = newest(&h);
    assert!(
        first < second,
        "the listener runs before the second one's turn"
    );
    TARGET.with(|t| t.set(Some(second)));
    with(&mut h.state, first, |e| {
        if let Kind::Item(i) = &mut e.kind {
            i.despawn_delay = 1;
        }
    });
    h.entities();
    let p = h.state.rules.entities.get(second).unwrap().body.position;
    assert_eq!((p.x, p.z), (5.5, 5.5), "the move stays");
    assert!(
        p.y <= -55.0 && p.y > -56.0,
        "and it falls from there: {p:?}"
    );
}

/// Listeners that cancel by the tick keep whole ticks the same on one or many threads.
#[test]
fn ticks_with_listeners_are_the_same_on_one_or_many_threads() {
    let mut hashes = Vec::new();
    for threads in [1, 4] {
        let mut listeners = Listeners::default();
        listeners.push(
            Priority::Normal,
            |e: &mut ItemMerge, cx: &mut Context<'_>| {
                if cx.tick().is_multiple_of(2) {
                    e.cancel();
                }
            },
        );
        listeners.push(
            Priority::Normal,
            |e: &mut EntityBlockChange, cx: &mut Context<'_>| {
                if cx.tick().is_multiple_of(3) {
                    e.cancel();
                }
            },
        );
        listeners.push(
            Priority::Normal,
            |_: &mut EntitySpawn, _: &mut Context<'_>| {},
        );
        listeners.push_monitor(|_: &EntityDespawn, _: &Context<'_>| {});
        let mut h = Harness::with_listeners(threads, listeners);
        h.spawn(1, standing(0.0, 0.0));
        let mut rng = Rng(0xe8_b403);
        let mut trace = Vec::new();
        for tick in 0..90 {
            if tick % 3 == 0 {
                let xi = i32::try_from(rng.below(16)).unwrap() - 8;
                let zi = i32::try_from(rng.below(16)).unwrap() - 8;
                let (x, z) = (f64::from(xi), f64::from(zi));
                let s = stack("minecraft:stick", u8::try_from(rng.below(64) + 1).unwrap());
                assert!(drop_item(&mut h.state, 0, at(x + 0.5, -55.0, z + 0.5), s));
                let pos = BlockPos::new(xi, -50, zi);
                assert!(fall(&mut h.state, 0, pos, named("minecraft:sand")));
            }
            h.step(Vec::new());
            trace.push(h.state.hash());
        }
        hashes.push(trace);
    }
    assert_eq!(hashes[0], hashes[1]);
}

// pmmp: Player::attackEntity, Explosion::explodeA, Explosion::explodeB, Explosion::setFireChance
#[test]
fn actions_raise_the_events_that_direct_calls_do_not() {
    let causes: Log<DamageCause> = log();
    let mut listeners = Listeners::default();
    let sink = causes.sink();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityDamage, _: &mut Context<'_>| {
            sink.push(e.cause());
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    h.look(1, 0.0, 0.0);
    let npc = npc_at(&mut h, at(0.5, -60.0, 2.5));
    let behind = npc_at(&mut h, at(0.5, -60.0, -3.5));
    assert!(reaches(&h.state, session(1), npc));
    assert!(!reaches(&h.state, session(1), behind), "behind the look");
    let mut cx = Context {
        state: &mut h.state,
    };
    cx.perform(Attack::new(player_id(1), npc)).unwrap();
    assert!(got(&causes).is_empty(), "performed after the callback");
    let mut runs = 0;
    run_deferred(&mut h.state, &mut runs);
    assert_eq!(got(&causes), [DamageCause::EntityAttack]);
    assert!(health(&h, npc) < 20.0);
    // A direct explosion hurts without events; the action raises them.
    clear(&causes);
    let far = npc_at(&mut h, at(30.5, -60.0, 30.5));
    let center = Vec3::new(30.5, -59.0, 31.5);
    let mut cx = Context {
        state: &mut h.state,
    };
    assert!(cx.explode(world::Id::DEFAULT, center, 2.0));
    assert!(health(&h, far) < 20.0);
    assert!(got(&causes).is_empty(), "direct calls raise nothing");
    let before = health(&h, far);
    // Past the NPC's hit cooldown.
    for _ in 0..12 {
        h.entities();
    }
    let blast = Explode::new(world::Id::DEFAULT, center, 3.0).with_fire();
    assert!(blast.fire);
    let mut cx = Context {
        state: &mut h.state,
    };
    cx.perform(blast).unwrap();
    run_deferred(&mut h.state, &mut runs);
    // The NPC and the items the first blast dropped.
    let causes = got(&causes);
    assert!(!causes.is_empty());
    assert!(causes.iter().all(|&c| c == DamageCause::BlockExplosion));
    assert!(health(&h, far) < before);
}
