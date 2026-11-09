//! Tests of paintings, end crystals, firework rockets, thrown tridents and ice bombs: their item
//! uses as packets, their rules, what viewers are sent and what is saved.

use std::cell::Cell;

use super::*;
use crate::block::Kind as BlockKind;
use crate::config::GameMode;
use crate::entity::Projectile;
use crate::event::{EntityItemPickup, Listeners, Priority, ProjectileLaunch};
use crate::protocol::consts::{EntityDataFlag, EntityDataKey, LevelEvent, UseItemAction};
use crate::protocol::packet::{InventoryTransaction, Packet};
use crate::protocol::{
    FlagSet, InventoryTransactionData, MetadataValue, ReleaseItemTransactionData,
    UseItemOnEntityTransactionData, UseItemTransactionData,
};
use crate::tick::entity::kind::Shot;
use crate::tick::entity::saved::tests::stack;
use crate::tick::entity::tests::{Harness, adds, at, removes, session, standing};
use crate::tick::entity::{
    Entities, Entity, MAX_ENTITIES_PER_WORLD, drop_item, release_trident, saved,
};
use crate::tick::tests::{input, packet};
use crate::tick::{Context, inventory};
use crate::world;

thread_local! {
    /// Whether the launch listener cancels (listeners run on the test's thread).
    static CANCEL_LAUNCH: Cell<bool> = const { Cell::new(true) };
    /// Whether the particle listener cancels.
    static CANCEL_PARTICLE: Cell<bool> = const { Cell::new(false) };
    /// Whether the pickup listener cancels.
    static CANCEL_PICKUP: Cell<bool> = const { Cell::new(true) };
    /// The particles a `WorldParticle` listener saw.
    static PARTICLES: Cell<Vec<(Vec3, Particle)>> = const { Cell::new(Vec::new()) };
}

/// The world the tests' entities live in.
const WORLD: world::Id = world::Id::DEFAULT;

fn set(h: &mut Harness, pos: BlockPos, block: block::State) {
    h.state.worlds[0].set_block(pos, block).unwrap();
}

/// Builds a stone wall facing west at x = 3: z from -2 to 2, y from -60 to -57.
fn wall(h: &mut Harness) {
    let stone = BlockKind::Stone.default_state();
    for z in -2..=2 {
        for y in -60..=-57 {
            set(h, BlockPos::new(3, y, z), stone);
        }
    }
}

/// Sends player 1's `InventoryTransaction` of `data` and runs one tick.
fn transaction(h: &mut Harness, data: InventoryTransactionData) {
    let p = Packet::InventoryTransaction(Box::new(InventoryTransaction {
        transaction_data: data,
        ..InventoryTransaction::default()
    }));
    h.step(vec![input(1, 1_000_000, packet(p))]);
}

fn entity(h: &Harness, id: Id) -> &Entity {
    h.state.rules.entities.get(id).unwrap()
}

/// Every item on the ground of `item`, by count.
fn on_ground(h: &Harness, item: item::Kind) -> u64 {
    h.live()
        .iter()
        .filter(|e| !e.flagged)
        .filter_map(|e| match &e.kind {
            Kind::Item(i) if i.stack.item().and_then(item::Type::kind) == Some(item) => {
                Some(u64::from(i.stack.count()))
            }
            _ => None,
        })
        .sum()
}

fn game_mode(h: &mut Harness, mode: GameMode) {
    let p = h.state.players.get_mut(&session(1)).unwrap();
    p.set_game_mode(mode);
    h.step(Vec::new());
}

/// Makes player 1 hit the entity `id`.
fn hit(h: &mut Harness, id: Id) {
    let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
    let data = UseItemOnEntityTransactionData {
        target_entity_runtime_id: runtime_id,
        action_type: 1,
        ..UseItemOnEntityTransactionData::default()
    };
    transaction(h, InventoryTransactionData::UseItemOnEntity(Box::new(data)));
}

// api: Painting::__construct, Painting::canFit
#[test]
fn paintings_from_the_api_fit_break_by_hits_and_save_their_wall() {
    let mut h = Harness::with_player();
    h.look(1, 270.0, 0.0);
    wall(&mut h);
    let mut cx = h.context();
    let pos = BlockPos::new(2, -59, 0);
    let big = PaintingMotive::Pointer;
    assert!(cx.can_hang_painting(WORLD, pos, Face::West, big));
    assert!(
        !cx.can_hang_painting(WORLD, pos, Face::East, big),
        "no wall behind"
    );
    assert!(!cx.can_hang_painting(WORLD, pos, Face::Up, PaintingMotive::Kebab));
    assert!(
        !cx.can_hang_painting(WORLD, BlockPos::new(3, -59, 0), Face::West, big),
        "inside the wall"
    );
    assert!(cx.spawn_painting(WORLD, pos, Face::Up, big).is_err());
    let id = cx.spawn_painting(WORLD, pos, Face::West, big).unwrap();
    assert!(
        !cx.can_hang_painting(WORLD, pos, Face::West, PaintingMotive::Kebab),
        "taken"
    );
    // Saved as the wall block, the facing twice and the motive.
    let saved = saved::save(entity(&h, id));
    assert_eq!(
        saved.get("identifier"),
        Some(&Value::String("minecraft:painting".to_owned()))
    );
    assert_eq!(saved.get("TileX"), Some(&Value::Int(3)));
    assert_eq!(saved.get("TileY"), Some(&Value::Int(-59)));
    assert_eq!(saved.get("TileZ"), Some(&Value::Int(0)));
    assert_eq!(saved.get("Direction"), Some(&Value::Byte(1)));
    assert_eq!(saved.get("Facing"), Some(&Value::Byte(1)));
    assert_eq!(
        saved.get("Motive"),
        Some(&Value::String("Pointer".to_owned()))
    );
    // A survival player's hit breaks it and it drops; a creative player's drops nothing.
    h.entities();
    hit(&mut h, id);
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(id).is_none());
    assert_eq!(on_ground(&h, item::Kind::Painting), 1);
    game_mode(&mut h, GameMode::Creative);
    let id = h
        .context()
        .spawn_painting(WORLD, pos, Face::West, big)
        .unwrap();
    h.step(Vec::new());
    hit(&mut h, id);
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(id).is_none());
    assert_eq!(on_ground(&h, item::Kind::Painting), 1, "nothing more");
}

fn block(h: &Harness, pos: BlockPos) -> block::State {
    h.state.worlds[0].block(pos).unwrap()
}

/// The metadata of the `AddActor` of `runtime_id` in `packets`.
fn added(packets: &[Packet], runtime_id: u64) -> crate::protocol::EntityMetadata {
    packets
        .iter()
        .find_map(|p| match p {
            Packet::AddActor(a) if a.entity_runtime_id == runtime_id => {
                Some(a.entity_metadata.clone())
            }
            _ => None,
        })
        .expect("added")
}

fn flags_of(m: &crate::protocol::EntityMetadata) -> FlagSet<EntityDataFlag> {
    let Some(MetadataValue::Int64(f)) = m.get(&(EntityDataKey::Flags as u32)) else {
        panic!("flags");
    };
    let mut set = FlagSet::<EntityDataFlag>::new();
    for &flag in EntityDataFlag::all() {
        let bit = flag as u32;
        if bit < 64 && (f.cast_unsigned() >> bit) & 1 == 1 {
            set.insert(flag);
        }
    }
    set
}

// api: entity\object\EndCrystal::setShowBase, entity\object\EndCrystal::setBeamTarget
#[test]
fn an_end_crystal_stands_on_obsidian_and_explodes_when_hit() {
    let mut h = Harness::with_player();
    let obsidian = BlockKind::Obsidian.default_state();
    let base = BlockPos::new(3, -61, 0);
    set(&mut h, base, obsidian);
    h.step(Vec::new());
    h.take(1);
    let id = h
        .context()
        .spawn_end_crystal(WORLD, at(3.5, -60.0, 0.5))
        .unwrap();
    assert_eq!(entity(&h, id).body.position, at(3.5, -60.0, 0.5));
    h.step(Vec::new());
    // Shown as `minecraft:ender_crystal` without its base, the beam at (0, 0, 0).
    let sent = h.take(1);
    let runtime_id = entity(&h, id).runtime_id;
    let m = added(&sent, runtime_id);
    assert_eq!(
        m.get(&(EntityDataKey::BlockTarget as u32)),
        Some(&MetadataValue::BlockPos([0, 0, 0]))
    );
    assert!(!flags_of(&m).contains(EntityDataFlag::ShowBottom));
    let mut cx = h.context();
    let mut e = cx.entity_mut(id).unwrap();
    e.set_base_visible(true).unwrap();
    e.set_beam_target(Some(BlockPos::new(0, 80, 0))).unwrap();
    assert!(e.is_base_visible());
    assert_eq!(e.beam_target(), Some(BlockPos::new(0, 80, 0)));
    h.step(Vec::new());
    let data: Vec<_> = h
        .take(1)
        .into_iter()
        .filter_map(|p| match p {
            Packet::SetActorData(d) if d.entity_runtime_id == runtime_id => Some(d.entity_metadata),
            _ => None,
        })
        .collect();
    let m = data.last().expect("sent");
    assert!(flags_of(m).contains(EntityDataFlag::ShowBottom));
    assert_eq!(
        m.get(&(EntityDataKey::BlockTarget as u32)),
        Some(&MetadataValue::BlockPos([0, 80, 0]))
    );
    let saved = saved::save(entity(&h, id));
    assert_eq!(saved.get("ShowBottom"), Some(&Value::Byte(1)));
    assert_eq!(saved.get("BlockTargetY"), Some(&Value::Int(80)));
    // Fire leaves it alone; a hit makes it explode at its tick (size 6: the dirt beside it goes,
    // the obsidian stays).
    assert!(entity(&h, id).kind.is_fire_proof());
    let dirt = BlockPos::new(5, -60, 0);
    set(&mut h, dirt, BlockKind::Dirt.default_state());
    hit(&mut h, id);
    assert!(
        h.state.rules.entities.get(id).is_none(),
        "gone at once: it ticks after the hit"
    );
    assert_eq!(block(&h, base), obsidian);
    assert_eq!(block(&h, dirt), block::AIR);
    assert!(h.take(1).iter().any(|p| matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(LevelEvent::ParticlesExplosion))));
}

#[test]
fn an_end_crystal_the_void_kills_does_not_explode_and_chains_go_off() {
    let mut h = Harness::with_player();
    // Away from the blasts below.
    h.teleport(1, standing(-20.0, 0.0));
    let id = h
        .context()
        .spawn_end_crystal(WORLD, at(0.5, -90.0, 0.5))
        .unwrap();
    for _ in 0..3 {
        h.step(Vec::new());
    }
    assert!(h.state.rules.entities.get(id).is_none(), "the void took it");
    assert!(h.take(1).iter().all(|p| !matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(LevelEvent::ParticlesExplosion))));
    // Two crystals side by side: the explosion of one sets off the other.
    let a = h
        .context()
        .spawn_end_crystal(WORLD, at(4.5, -60.0, 0.5))
        .unwrap();
    let b = h
        .context()
        .spawn_end_crystal(WORLD, at(7.5, -60.0, 0.5))
        .unwrap();
    h.step(Vec::new());
    assert!(h.context().entity_mut(a).unwrap().attack(1.0));
    let blasts = |h: &mut Harness| {
        h.take(1)
            .iter()
            .filter(|p| {
                matches!(p, Packet::LevelEvent(e)
                if e.event_type == i32::from(LevelEvent::ParticlesExplosion))
            })
            .count()
    };
    h.step(Vec::new());
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(a).is_none());
    assert!(h.state.rules.entities.get(b).is_none());
    assert_eq!(blasts(&mut h), 2);
}

// api: entity\object\FireworkRocket::__construct, entity\object\FireworkRocket::setExplosions,
// api: entity\object\FireworkRocket::setMaxFlightTimeTicks
#[test]
fn a_firework_rocket_bursts_at_its_flight_time_and_hurts_who_is_near() {
    let mut h = Harness::with_player();
    let explosion = Compound::from([
        (TAG_TYPE.to_owned(), Value::Byte(LARGE_BALL)),
        (TAG_COLOURS.to_owned(), Value::ByteArray(vec![1, 14])),
        (TAG_TWINKLE.to_owned(), Value::Byte(1)),
    ]);
    let npc = h.vulnerable_human(at(8.5, -60.0, 0.5));
    let mut cx = h.context();
    let bad = Compound::from([(TAG_TYPE.to_owned(), Value::Byte(9))]);
    let at_burst = at(6.5, -60.0, 0.5);
    assert!(
        cx.spawn_firework_rocket(WORLD, at_burst, vec![bad.clone()])
            .is_err()
    );
    let id = cx
        .spawn_firework_rocket(WORLD, at_burst, Vec::new())
        .unwrap();
    // Without drift, so the burst point is where it climbs to.
    cx.entity_mut(id).unwrap().set_motion(Vec3::ZERO).unwrap();
    h.take(1);
    // Shown first; then its flight ends at once.
    h.step(Vec::new());
    let mut cx = h.context();
    let mut e = cx.entity_mut(id).unwrap();
    assert!(e.set_firework_explosions(vec![bad]).is_err());
    e.set_firework_explosions(vec![explosion.clone()]).unwrap();
    e.set_max_flight_time_ticks(1).unwrap();
    assert_eq!(e.max_flight_time_ticks(), Some(1));
    assert!(e.set_can_save_with_chunk(true).is_err());
    assert!(!e.can_save_with_chunk(), "never saved");
    let burst_at = e.position() + e.motion();
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(id).is_none(), "burst");
    // The damage is (1 × 2 + 5) × √((5 − d) / 5) to the NPC two blocks away; player 1 is farther.
    let d = burst_at.distance_squared(at(8.5, -60.0, 0.5)).sqrt();
    let expected = 7.0 * ((5.0 - d) / 5.0).sqrt();
    let health = entity(&h, npc).health;
    assert!((20.0 - health - expected).abs() < 1e-9, "{health}");
    let sent = h.take(1);
    assert!(
        sent.iter()
            .any(|p| matches!(p, Packet::ActorEvent(a) if a.event_type == 25))
    );
    let sounds: Vec<&str> = sent
        .iter()
        .filter_map(|p| match p {
            Packet::LevelSoundEvent(s) => Some(s.sound_type.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        sounds.contains(&"launch")
            && sounds.contains(&"large.blast")
            && sounds.contains(&"twinkle"),
        "{sounds:?}"
    );
    // Without explosions a rocket bursts quietly and harmlessly, its burst event shown as the
    // official server shows it (P6 `entities`).
    let mut cx = h.context();
    let quiet = cx
        .spawn_firework_rocket(WORLD, at_burst, Vec::new())
        .unwrap();
    cx.entity_mut(quiet)
        .unwrap()
        .set_max_flight_time_ticks(2)
        .unwrap();
    let before = entity(&h, npc).health;
    let quiet_runtime_id = entity(&h, quiet).runtime_id;
    // Shown first; it bursts at its second tick.
    h.step(Vec::new());
    h.take(1);
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(quiet).is_none());
    assert_eq!(entity(&h, npc).health, before);
    let sent = h.take(1);
    assert!(sent.iter().any(|p| matches!(p,
        Packet::ActorEvent(a) if a.entity_runtime_id == quiet_runtime_id && a.event_type == 25)));
    assert!(!sent.iter().any(|p| matches!(p, Packet::LevelSoundEvent(s)
        if s.sound_type == "blast" || s.sound_type == "large.blast")));
}

#[test]
fn explosions_past_the_colour_and_key_bounds_are_refused() {
    let explosion = |colours: usize| {
        Compound::from([
            (TAG_TYPE.to_owned(), Value::Byte(0)),
            (TAG_COLOURS.to_owned(), Value::ByteArray(vec![1; colours])),
        ])
    };
    let dyes = usize::from(COLOURS);
    assert!(checked_explosions(vec![explosion(dyes)]).is_some());
    assert!(checked_explosions(vec![explosion(dyes + 1)]).is_none());
    let mut long_fade = explosion(1);
    long_fade.insert(TAG_FADE.to_owned(), Value::ByteArray(vec![2; dyes + 1]));
    assert!(checked_explosions(vec![long_fade]).is_none());
    let mut stranger = explosion(1);
    stranger.insert("Stranger".to_owned(), Value::ByteArray(vec![0; 1 << 20]));
    assert!(checked_explosions(vec![stranger]).is_none());
}

#[test]
fn the_multiplier_an_api_rocket_shows_inverts_the_flight_time_of_an_item() {
    for multiplier in 1..=MAX_FLIGHT_MULTIPLIER {
        let flight_ticks = (u32::from(multiplier) + 1) * FLIGHT_STEP_TICKS;
        assert_eq!(flight_multiplier(flight_ticks), multiplier);
    }
}

/// An API rocket flies as a rocket item of flight multiplier 1: 20 ticks and up to 12 more, a
/// slow start up with its own drift, looking up, without owner, never saved.
#[test]
fn an_api_rocket_flies_as_a_rocket_item_of_multiplier_one() {
    let mut h = Harness::with_player();
    let mut cx = h.context();
    assert!(
        cx.spawn_firework_rocket(WORLD, at(f64::NAN, -60.0, 0.5), Vec::new())
            .is_err()
    );
    let ids = [0; 2].map(|_| {
        cx.spawn_firework_rocket(WORLD, at(6.5, -60.0, 0.5), Vec::new())
            .unwrap()
    });
    for id in ids {
        let rocket = entity(&h, id);
        let Kind::Firework(firework) = &rocket.kind else {
            panic!("a rocket");
        };
        assert!((20..=32).contains(&firework.max_flight));
        assert!(firework.explosions.is_empty() && firework.owner.is_none());
        assert!(!rocket.saved);
        assert_eq!((rocket.body.motion.y, rocket.pitch), (0.05, 90.0));
    }
    assert_ne!(
        entity(&h, ids[0]).body.motion,
        entity(&h, ids[1]).body.motion,
        "each its own drift"
    );
}

fn hold(h: &mut Harness, stack: Stack) {
    inventory::set_held(h.state.players.get_mut(&session(1)).unwrap(), stack);
}

fn held(h: &Harness) -> Stack {
    h.state.players[&session(1)].inventory.held().clone()
}

/// The held item as a client sends it with a use.
fn use_data(h: &Harness, action: UseItemAction) -> UseItemTransactionData {
    let held = held(h);
    let mut data = UseItemTransactionData {
        action_type: action.into(),
        hot_bar_slot: i32::from(h.state.players[&session(1)].inventory.held_slot()),
        ..UseItemTransactionData::default()
    };
    data.held_item.stack.item_type.network_id =
        held.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.held_item.stack.count = u16::from(held.count());
    data
}

/// Makes player 1 click `face` of the block `pos` at `click` with the held item.
fn use_on(h: &mut Harness, pos: BlockPos, face: Face, click: [f32; 3]) {
    let mut data = use_data(h, UseItemAction::ClickBlock);
    data.block_position = [pos.x, pos.y, pos.z];
    data.block_face = match face {
        Face::Down => 0,
        Face::Up => 1,
        Face::North => 2,
        Face::South => 3,
        Face::West => 4,
        Face::East => 5,
    };
    data.clicked_position = click;
    transaction(h, InventoryTransactionData::UseItem(Box::new(data)));
}

/// The live, unflagged entities of type `t`.
fn of_type(h: &Harness, t: crate::entity::Type) -> Vec<Id> {
    let entities = &h.state.rules.entities;
    entities
        .ids()
        .filter(|&id| {
            entities
                .get(id)
                .is_some_and(|e| !e.flagged && e.kind.entity_type() == t)
        })
        .collect()
}

#[test]
fn a_firework_used_on_a_block_flies_for_its_items_flight_and_climbs() {
    let mut h = Harness::with_player();
    let fireworks = Compound::from([
        (TAG_FLIGHT.to_owned(), Value::Byte(2)),
        (
            TAG_EXPLOSIONS.to_owned(),
            Value::List(List::Compound(vec![Compound::from([
                (TAG_TYPE.to_owned(), Value::Byte(0)),
                (TAG_COLOURS.to_owned(), Value::ByteArray(vec![3])),
            ])])),
        ),
    ]);
    let rocket = stack("minecraft:firework_rocket", 3)
        .with_extra(Some(item::Extra {
            nbt: Compound::from([(TAG_FIREWORKS.to_owned(), Value::Compound(fireworks))]),
            ..item::Extra::default()
        }))
        .unwrap();
    hold(&mut h, rocket);
    h.take(1);
    use_on(
        &mut h,
        BlockPos::new(2, -61, 0),
        Face::Up,
        [0.25, 1.0, 0.75],
    );
    let [id] = of_type(&h, crate::entity::Type::FireworkRocket)[..] else {
        panic!("one rocket");
    };
    assert_eq!(held(&h).count(), 2);
    let rocket = entity(&h, id);
    let Kind::Firework(firework) = &rocket.kind else {
        panic!("a rocket");
    };
    let flight = firework.max_flight;
    assert!((30..=42).contains(&flight), "{flight}");
    assert_eq!(firework.explosions.len(), 1);
    assert_eq!(firework.owner, Some(session(1)));
    assert!(!rocket.saved);
    let start = at(2.25, -60.0 + 0.15, 0.75);
    let position = rocket.body.position;
    assert!(position.distance_squared(start) < 1.0, "{position:?}");
    let runtime_id = rocket.runtime_id;
    // Shown with its explosions and its owner.
    let sent = h.take(1);
    let meta = added(&sent, runtime_id);
    let Some(MetadataValue::CompoundTag(item_nbt)) =
        meta.get(&(EntityDataKey::DISPLAY_FIREWORK as u32))
    else {
        panic!("firework item");
    };
    // As the official server shows it (P6 `entities`): the item's explosions and flight, the
    // direction (0, 1, 0), no attached entity, no owner.
    assert!(matches!(item_nbt.get(TAG_FIREWORKS),
        Some(Value::Compound(fireworks)) if fireworks.contains_key(TAG_EXPLOSIONS)
            && fireworks.get(TAG_FLIGHT) == Some(&Value::Byte(2))));
    assert_eq!(
        meta.get(&(EntityDataKey::DisplayOffset as u32)),
        Some(&MetadataValue::Vec3([0.0, 1.0, 0.0]))
    );
    assert_eq!(
        meta.get(&(EntityDataKey::CustomDisplay as u32)),
        Some(&MetadataValue::Int64(-1))
    );
    assert_eq!(
        meta.get(&(EntityDataKey::Owner as u32)),
        Some(&MetadataValue::Int64(-1))
    );
    let y0 = entity(&h, id).body.position.y;
    for _ in 0..10 {
        h.step(Vec::new());
    }
    assert!(entity(&h, id).body.position.y > y0 + 1.0, "it climbs");
    for _ in 0..40 {
        h.step(Vec::new());
    }
    assert!(h.state.rules.entities.get(id).is_none(), "burst by tick 42");
}

/// Makes player 1 use the held item in the air.
fn use_in_air(h: &mut Harness) {
    let data = use_data(h, UseItemAction::ClickAir);
    transaction(h, InventoryTransactionData::UseItem(Box::new(data)));
}

/// Makes player 1 let go of the held item.
fn release(h: &mut Harness) {
    let data = ReleaseItemTransactionData::default();
    transaction(h, InventoryTransactionData::ReleaseItem(data));
}

/// Makes player 1, looking east and down, draw the held trident for `ticks` and let it go.
fn throw_trident(h: &mut Harness, ticks: u32) {
    h.look(1, 270.0, 40.0);
    use_in_air(h);
    for _ in 0..ticks {
        h.step(Vec::new());
    }
    release(h);
}

/// A trident's throw sounds at the middle of the thrower's body, as on the official server (P6
/// `itemuse`).
#[test]
fn a_trident_throw_sounds_at_the_middle_of_the_body() {
    let mut h = Harness::with_player();
    hold(&mut h, stack("minecraft:trident", 1));
    h.take(1);
    throw_trident(&mut h, 20);
    let feet = h.state.players[&session(1)].position.y - 1.62;
    let throw = h.take(1).into_iter().find_map(|p| match p {
        Packet::LevelSoundEvent(s) if s.sound_type == "item.trident.throw" => Some(s.position),
        _ => None,
    });
    let [_, y, _] = throw.expect("the throw sound");
    assert!((f64::from(y) - (feet + 0.9)).abs() < 1e-4, "{y} {feet}");
}

/// How many of `item` player 1 carries.
fn carried(h: &Harness, item: item::Kind) -> u64 {
    let inv = &h.state.players[&session(1)].inventory;
    (0..=u8::MAX)
        .map_while(|i| inv.main_stack(i))
        .filter(|s| s.item().and_then(item::Type::kind) == Some(item))
        .map(|s| u64::from(s.count()))
        .sum()
}

#[test]
fn a_thrown_trident_hits_sticks_and_comes_back_once() {
    let mut h = Harness::with_player();
    let trident = stack("minecraft:trident", 1);
    hold(&mut h, trident.clone());
    // Drawn too short: nothing.
    throw_trident(&mut h, 5);
    assert!(of_type(&h, crate::entity::Type::Trident).is_empty());
    assert_eq!(held(&h), trident);
    throw_trident(&mut h, 20);
    let [id] = of_type(&h, crate::entity::Type::Trident)[..] else {
        panic!("one trident");
    };
    assert!(held(&h).is_empty(), "it left the hand");
    let thrown = entity(&h, id).item().cloned().unwrap();
    assert_eq!(
        (thrown.count(), thrown.damage()),
        (1, 1),
        "worn by the throw"
    );
    assert_eq!(entity(&h, id).base_damage(), Some(8.0));
    // It lands and sticks; the hand stays empty until a player picks it up.
    for _ in 0..40 {
        h.step(Vec::new());
    }
    let e = entity(&h, id);
    let Kind::Projectile(p) = &e.kind else {
        panic!("a projectile");
    };
    let landed = e.body.position;
    assert!(p.stuck.is_some(), "stuck at {landed:?}");
    let total = |h: &Harness| {
        carried(h, item::Kind::Trident) + of_type(h, crate::entity::Type::Trident).len() as u64
    };
    assert_eq!(total(&h), 1);
    h.teleport(1, (landed + Vec3::new(0.0, 1.62, 0.0)).to_f32());
    h.step(Vec::new());
    assert!(
        h.state.rules.entities.get(id).is_none_or(|e| e.flagged),
        "picked up"
    );
    assert_eq!(carried(&h, item::Kind::Trident), 1);
    assert_eq!(total(&h), 1, "one trident, no copy");
    for _ in 0..5 {
        h.step(Vec::new());
    }
    assert_eq!(total(&h), 1);
}

#[test]
fn a_trident_hurts_by_eight_bounces_back_and_a_creative_one_gives_nothing() {
    let mut h = Harness::with_player();
    let npc = h.vulnerable_human(at(4.5, -60.0, 0.5));
    let trident = stack("minecraft:trident", 1);
    let id = h
        .context()
        .spawn_trident(
            WORLD,
            at(1.5, -59.0, 0.5),
            trident.clone(),
            Vec3::new(1.0, 0.0, 0.0),
        )
        .unwrap();
    for _ in 0..4 {
        h.step(Vec::new());
    }
    assert_eq!(entity(&h, npc).health, 12.0);
    let e = entity(&h, id);
    let Kind::Projectile(p) = &e.kind else {
        panic!("a projectile");
    };
    let Shot::Trident(t) = &p.shot else {
        panic!("a trident");
    };
    assert!(!t.can_collide, "no more hits until a block");
    assert!(!e.flagged, "it bounced");
    // Thrown in creative: the hand keeps it and the pickup takes it away.
    let mut h = Harness::with_player();
    game_mode(&mut h, GameMode::Creative);
    hold(&mut h, trident.clone());
    throw_trident(&mut h, 20);
    let [id] = of_type(&h, crate::entity::Type::Trident)[..] else {
        panic!("one trident");
    };
    assert_eq!(held(&h), trident, "creative keeps it");
    for _ in 0..40 {
        h.step(Vec::new());
    }
    let landed = entity(&h, id).body.position;
    hold(&mut h, Stack::EMPTY);
    h.teleport(1, (landed + Vec3::new(0.0, 1.62, 0.0)).to_f32());
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(id).is_none_or(|e| e.flagged));
    assert_eq!(
        carried(&h, item::Kind::Trident),
        0,
        "nothing out of creative"
    );
}

// api: entity\projectile\Trident::__construct, entity\projectile\Trident::setItem
#[test]
fn tridents_save_their_item_and_show_its_glint() {
    let mut h = Harness::with_player();
    let mut cx = h.context();
    assert_eq!(
        cx.spawn_trident(WORLD, at(0.5, -59.0, 3.5), Stack::EMPTY, Vec3::ZERO),
        Err(Stack::EMPTY)
    );
    let plain = stack("minecraft:trident", 1);
    let enchanted = plain
        .with_extra(Some(item::Extra {
            enchantments: vec![(item::Enchantment::all()[0], 1)],
            ..item::Extra::default()
        }))
        .unwrap();
    let id = cx
        .spawn_trident(WORLD, at(0.5, -59.0, 3.5), enchanted.clone(), Vec3::ZERO)
        .unwrap();
    h.take(1);
    h.step(Vec::new());
    let runtime_id = entity(&h, id).runtime_id;
    let sent = h.take(1);
    assert!(sent.iter().any(|p| matches!(p, Packet::AddActor(a)
        if a.entity_runtime_id == runtime_id && a.entity_type == "minecraft:thrown_trident")));
    assert!(flags_of(&added(&sent, runtime_id)).contains(EntityDataFlag::Enchanted));
    let saved = saved::save(entity(&h, id));
    assert_eq!(
        saved.get("Trident"),
        Some(&Value::Compound(enchanted.to_nbt()))
    );
    assert_eq!(saved.get("isCreative"), Some(&Value::Byte(0)));
    assert_eq!(saved.get("damage"), Some(&Value::Double(8.0)));
    // A plain item takes the glint away.
    let mut cx = h.context();
    let mut e = cx.entity_mut(id).unwrap();
    assert!(e.set_item(Stack::EMPTY).is_err());
    e.set_item(plain.clone()).unwrap();
    assert_eq!(e.item(), Some(&plain));
    h.step(Vec::new());
    let flags: Vec<_> = h
        .take(1)
        .into_iter()
        .filter_map(|p| match p {
            Packet::SetActorData(d) if d.entity_runtime_id == runtime_id => Some(d.entity_metadata),
            _ => None,
        })
        .collect();
    assert!(!flags_of(flags.last().expect("sent")).contains(EntityDataFlag::Enchanted));
}

#[test]
fn an_ice_bomb_breaks_where_it_lands_leaves_the_water_as_it_is_and_hurts_no_one() {
    let mut h = Harness::with_player();
    let water = BlockKind::Water.default_state();
    let mut pool = Vec::new();
    for x in 3..=6 {
        for z in -2..=2 {
            let pos = BlockPos::new(x, -61, z);
            set(&mut h, pos, water);
            pool.push(pos);
        }
    }
    h.step(Vec::new());
    let id = h
        .context()
        .spawn_projectile(
            WORLD,
            at(4.5, -58.0, 0.5),
            Projectile::IceBomb,
            Vec3::new(0.0, -0.5, 0.0),
        )
        .unwrap();
    for _ in 0..10 {
        h.step(Vec::new());
    }
    assert!(
        h.state.rules.entities.get(id).is_none(),
        "it broke on the water"
    );
    for pos in pool {
        assert_eq!(block(&h, pos), water, "{pos:?}");
    }
    // From the hand: one used up, an ice bomb flies; on an NPC it breaks without damage.
    let npc = h.vulnerable_human(at(0.5, -60.0, 3.5));
    hold(&mut h, stack("minecraft:ice_bomb", 2));
    use_in_air(&mut h);
    assert_eq!(held(&h).count(), 1);
    assert_eq!(of_type(&h, crate::entity::Type::IceBomb).len(), 1);
    for _ in 0..30 {
        h.step(Vec::new());
    }
    assert!(of_type(&h, crate::entity::Type::IceBomb).is_empty());
    assert_eq!(entity(&h, npc).health, 20.0);
}

#[test]
fn at_the_entity_cap_rockets_and_tridents_are_refused_and_items_stay() {
    let mut h = Harness::with_player();
    wall(&mut h);
    h.step(Vec::new());
    let have = h.state.rules.entities.count(0);
    for i in 0..u32::try_from(MAX_ENTITIES_PER_WORLD - have).unwrap() {
        let x = f64::from(i % 64) - 32.0;
        let z = f64::from(i / 64 % 64) - 32.0;
        assert!(drop_item(
            &mut h.state,
            0,
            at(x, -40.0, z),
            stack("minecraft:stick", 1)
        ));
    }
    let mut random = Random::new(0, 0, 0);
    let rocket = stack("minecraft:firework_rocket", 1);
    assert!(!launch_firework(
        &mut h.state,
        session(1),
        0,
        at(0.5, -60.0, 2.5),
        &rocket,
        &mut random
    ));
    let trident = stack("minecraft:trident", 1);
    hold(&mut h, trident.clone());
    assert!(!release_trident(&mut h.state, session(1), 20));
    assert_eq!(held(&h), trident);
    let mut cx = h.context();
    assert_eq!(
        cx.spawn_trident(WORLD, at(0.5, -59.0, 0.5), trident.clone(), Vec3::ZERO),
        Err(trident)
    );
    assert!(cx.spawn_end_crystal(WORLD, at(0.5, -59.0, 0.5)).is_err());
    h.state.rules.entities = Entities::default();
}

#[test]
fn spawning_and_removing_each_kind_leaves_every_table_at_its_start() {
    let mut h = Harness::with_player();
    wall(&mut h);
    for _ in 0..3 {
        h.step(Vec::new());
    }
    h.state.rules.entities.take_changed();
    let start = h.state.rules.entities.count(0);
    for round in 0..200_i32 {
        let mut cx = h.context();
        let z = f64::from(round % 5) - 2.0;
        let mut ids = vec![
            cx.spawn_painting(
                WORLD,
                BlockPos::new(2, -59, round % 5 - 2),
                Face::West,
                PaintingMotive::Kebab,
            )
            .unwrap(),
            cx.spawn_end_crystal(WORLD, at(-3.5, -60.0, z)).unwrap(),
            cx.spawn_firework_rocket(WORLD, at(-6.5, -60.0, z), Vec::new())
                .unwrap(),
            cx.spawn_trident(
                WORLD,
                at(10.5, -59.0, z),
                stack("minecraft:trident", 1),
                Vec3::ZERO,
            )
            .unwrap(),
            cx.spawn_projectile(WORLD, at(-1.5, -50.0, z), Projectile::IceBomb, Vec3::ZERO)
                .unwrap(),
        ];
        h.step(Vec::new());
        for id in ids.drain(..) {
            if let Some(mut e) = h.context().entity_mut(id) {
                e.flag_for_despawn();
            }
        }
        h.step(Vec::new());
    }
    for _ in 0..3 {
        h.step(Vec::new());
    }
    h.state.rules.entities.take_changed();
    let e = &h.state.rules.entities;
    assert_eq!(e.count(0), start);
    assert!(e.doomed.is_empty() && e.restep.is_empty() && e.spawned.is_empty());
    assert!(e.departed.is_empty() && e.changed.is_empty());
    assert_eq!(
        on_ground(&h, item::Kind::Painting),
        0,
        "removed, not broken"
    );
    let packets = h.take(1);
    let removed = removes(&packets).len();
    assert_eq!(adds(&packets).len(), removed);
}

#[test]
fn paintings_whose_blocks_are_not_loaded_stay() {
    let world = crate::tick::entity::physics::tests::flat();
    let pos = BlockPos::new(1000, -59, 0);
    assert_eq!(fits(&world, pos, Face::West, PaintingMotive::Kebab), None);
    let near = BlockPos::new(2, -60, 0);
    assert_eq!(
        fits(&world, near, Face::West, PaintingMotive::Kebab),
        Some(false)
    );
}

#[test]
fn a_tridents_launch_and_pickup_ask_their_listeners() {
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |e: &mut ProjectileLaunch, _: &mut Context<'_>| {
            if CANCEL_LAUNCH.with(Cell::get) {
                e.cancel();
            }
        },
    );
    listeners.push(
        Priority::Normal,
        |e: &mut EntityItemPickup, cx: &mut Context<'_>| {
            let trident = cx
                .entity(e.origin())
                .is_some_and(|o| o.entity_type() == crate::entity::Type::Trident);
            assert!(trident && e.item().item().is_some_and(|t| t.is(item::Class::Trident)));
            if CANCEL_PICKUP.with(Cell::get) {
                e.cancel();
            }
        },
    );
    let mut h = Harness::with_listeners(1, listeners);
    h.spawn(1, standing(0.0, 0.0));
    let trident = stack("minecraft:trident", 1);
    hold(&mut h, trident.clone());
    throw_trident(&mut h, 20);
    assert!(
        of_type(&h, crate::entity::Type::Trident).is_empty(),
        "cancelled"
    );
    assert_eq!(held(&h), trident, "the hand keeps it");
    CANCEL_LAUNCH.with(|c| c.set(false));
    throw_trident(&mut h, 20);
    let [id] = of_type(&h, crate::entity::Type::Trident)[..] else {
        panic!("one trident");
    };
    assert!(held(&h).is_empty());
    for _ in 0..40 {
        h.step(Vec::new());
    }
    let landed = entity(&h, id).body.position;
    h.teleport(1, (landed + Vec3::new(0.0, 1.62, 0.0)).to_f32());
    h.step(Vec::new());
    assert!(
        h.state.rules.entities.get(id).is_some_and(|e| !e.flagged),
        "left there"
    );
    assert_eq!(carried(&h, item::Kind::Trident), 0);
    CANCEL_PICKUP.with(|c| c.set(false));
    h.step(Vec::new());
    assert!(h.state.rules.entities.get(id).is_none_or(|e| e.flagged));
    assert_eq!(carried(&h, item::Kind::Trident), 1);
}

/// A painting stays where it hangs when the wall behind it goes.
#[test]
fn a_painting_stays_when_its_wall_goes() {
    let mut h = Harness::with_player();
    wall(&mut h);
    let id = h
        .context()
        .spawn_painting(
            WORLD,
            BlockPos::new(2, -59, 0),
            Face::West,
            PaintingMotive::Kebab,
        )
        .unwrap();
    h.step(Vec::new());
    set(&mut h, BlockPos::new(3, -59, 0), block::AIR);
    for _ in 0..5 {
        h.step(Vec::new());
    }
    assert!(h.state.rules.entities.get(id).is_some_and(|e| !e.flagged));
    assert_eq!(on_ground(&h, item::Kind::Painting), 0);
}

#[test]
fn a_breaking_painting_shows_its_planks_through_world_particle() {
    let listeners = || {
        let mut listeners = Listeners::default();
        listeners.push(
            Priority::Normal,
            |e: &mut crate::event::WorldParticle, _: &mut Context<'_>| {
                PARTICLES.with(|p| {
                    let mut seen = p.take();
                    seen.push((e.position(), e.particle()));
                    p.set(seen);
                });
                if CANCEL_PARTICLE.with(Cell::get) {
                    e.cancel();
                }
            },
        );
        listeners
    };
    let planks_shown = |packets: &[Packet]| {
        packets
            .iter()
            .filter(|p| {
                matches!(p, Packet::LevelEvent(e)
                    if e.event_type == i32::from(LevelEvent::ParticlesDestroyBlock))
            })
            .count()
    };
    let planks = Particle::BlockBreak(BlockKind::OakPlanks.default_state());
    for cancel in [false, true] {
        CANCEL_PARTICLE.with(|c| c.set(cancel));
        PARTICLES.with(Cell::take);
        let mut h = Harness::with_listeners(1, listeners());
        h.spawn(1, standing(0.0, 0.0));
        h.look(1, 270.0, 0.0);
        wall(&mut h);
        let pos = BlockPos::new(2, -59, 0);
        let id = h
            .context()
            .spawn_painting(WORLD, pos, Face::West, PaintingMotive::Kebab)
            .unwrap();
        h.step(Vec::new());
        let hung = entity(&h, id).body.position;
        h.take(1);
        hit(&mut h, id);
        h.step(Vec::new());
        assert!(h.state.rules.entities.get(id).is_none(), "broke");
        let seen = PARTICLES.with(Cell::take);
        assert_eq!(seen, [(hung + Vec3::new(0.5, 0.5, 0.5), planks)]);
        let shown = planks_shown(&h.take(1));
        assert_eq!(shown, usize::from(!cancel), "cancelled: {cancel}");
    }
}
