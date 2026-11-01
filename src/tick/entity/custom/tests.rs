//! Tests of custom entity types: registration, the identifier list, tracking, the behaviours'
//! calls, saving, the entity cap, leaks and determinism.

use std::num::NonZeroU32;

use tokio::sync::mpsc;

use super::super::tests::{Harness, TEST_HOOKS, adds, at, removes, standing};
use super::super::{MAX_ENTITIES_PER_WORLD, Raise, npc, saved};
use super::*;
use crate::entity::Type;
use crate::protocol::consts::EntityDataKey;
use crate::protocol::packet::Packet;
use crate::protocol::{MetadataValue, Rng};
use crate::world::ChunkPos;

const WORLD: world::Id = world::Id::DEFAULT;
/// The chunk the tests' entities stand in.
const KEY: (usize, ChunkPos) = (0, ChunkPos::new(0, 0));

/// A behaviour call a test saw.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Tick(&'static str, u64, Id),
    Interact(&'static str, Id, player::Id),
}

/// A behaviour that reports its calls through a bounded channel.
struct Recorder {
    name: &'static str,
    tx: mpsc::Sender<Call>,
}

impl Behaviour for Recorder {
    fn on_tick(&mut self, cx: &mut Context<'_>, entity: Id) {
        assert!(cx.entity(entity).is_some(), "a live entity");
        let call = Call::Tick(self.name, cx.tick(), entity);
        assert!(self.tx.try_send(call).is_ok(), "room in the log");
    }

    fn on_interact(&mut self, cx: &mut Context<'_>, entity: Id, player: player::Id) {
        assert!(cx.entity(entity).is_some(), "a live entity");
        let call = Call::Interact(self.name, entity, player);
        assert!(self.tx.try_send(call).is_ok(), "room in the log");
    }
}

fn registry(types: Vec<Definition>) -> Customs {
    let mut customs = Customs::default();
    for t in types {
        customs.register(t);
    }
    customs.finish().expect("valid types");
    customs
}

/// A harness with `types`, and player 1 standing at block (0, 0) looking along +z.
fn harness(threads: usize, types: Vec<Definition>) -> Harness {
    let mut h = Harness::new(threads);
    h.state.rules.entities.set_customs(registry(types));
    h.spawn(1, standing(0.0, 0.0));
    h
}

fn context(h: &mut Harness) -> Context<'_> {
    Context {
        state: &mut h.state,
    }
}

fn spawn(h: &mut Harness, identifier: &str, pos: Vec3) -> Id {
    context(h)
        .spawn_custom_entity(WORLD, pos, identifier)
        .expect("spawned")
}

fn ghost() -> Definition {
    Definition::new("test:ghost")
        .with_size(0.8, 2.5)
        .with_max_health(30.0)
        .with_name_tag("Ghost")
        .summonable()
}

fn calls(rx: &mut mpsc::Receiver<Call>) -> Vec<Call> {
    let mut out = Vec::new();
    while let Ok(c) = rx.try_recv() {
        out.push(c);
    }
    out
}

fn no_trade(_: &mut State, _: SessionId, _: Id) -> bool {
    false
}

/// A compound whose saved form is over [`MAX_DATA_BYTES`].
fn too_big() -> Compound {
    Compound::from([("big".to_owned(), Value::ByteArray(vec![0; MAX_DATA_BYTES]))])
}

// pmmp: EntityFactory, EntityFactory::register
#[test]
fn custom_data_fits_up_to_its_limit_and_a_huge_compound_is_refused() {
    let data = |len: usize| Compound::from([("d".to_owned(), Value::ByteArray(vec![0; len]))]);
    // Root tag and empty name (3), the array's tag and name (4), its length (4), the end tag.
    let overhead = 12;
    assert!(fits(&data(MAX_DATA_BYTES - overhead)));
    assert!(!fits(&data(MAX_DATA_BYTES - overhead + 1)));
    assert!(!fits(&too_big()));
    // Refused while writing, not after copying 64 MiB.
    assert!(!fits(&data(64 << 20)));
}

#[test]
fn registration_refuses_bad_identifiers_duplicates_and_values() {
    let refused = |types: Vec<Definition>| {
        let mut customs = Customs::default();
        for t in types {
            customs.register(t);
        }
        customs.finish().err()
    };
    assert_eq!(refused(vec![ghost(), Definition::new("test:other")]), None);
    let long = format!("test:{}", "a".repeat(124));
    for bad in [
        "minecraft:pig",
        "ghost",
        ":ghost",
        "test:",
        "Test:ghost",
        "test:gh ost",
        "te/st:ghost",
        long.as_str(),
    ] {
        assert_eq!(
            refused(vec![Definition::new(bad)]),
            Some(Error::InvalidIdentifier {
                identifier: bad.to_owned()
            }),
            "{bad}"
        );
    }
    assert_eq!(refused(vec![Definition::new("test:a/b.c-d_1")]), None);
    assert_eq!(
        refused(vec![ghost(), ghost()]),
        Some(Error::DuplicateIdentifier {
            identifier: "test:ghost".to_owned()
        })
    );
    let field = |e: Definition| match refused(vec![e]) {
        Some(Error::InvalidValue { field, .. }) => field,
        other => panic!("{other:?}"),
    };
    let t = || Definition::new("test:t");
    assert_eq!(field(t().with_size(0.0, 1.0)), "width");
    assert_eq!(field(t().with_size(1.0, f64::NAN)), "height");
    assert_eq!(field(t().with_size(17.0, 1.0)), "width");
    assert_eq!(field(t().with_gravity(-0.1)), "gravity");
    assert_eq!(field(t().with_drag(1.0)), "drag");
    assert_eq!(field(t().with_max_health(0.0)), "max health");
    assert_eq!(field(t().with_max_health(f64::INFINITY)), "max health");
    assert_eq!(field(t().with_name_tag("n".repeat(1025))), "name tag");
    let many: Vec<Definition> = (0..=MAX_TYPES)
        .map(|i| Definition::new(format!("test:t{i}")))
        .collect();
    assert_eq!(refused(many), Some(Error::TooManyTypes));
    let built = crate::Server::builder()
        .custom_entity(Definition::new("minecraft:pig"))
        .build();
    assert!(matches!(
        built,
        Err(crate::Error::CustomEntity(Error::InvalidIdentifier { .. }))
    ));
    assert!(
        crate::Server::builder()
            .custom_entity(ghost())
            .build()
            .is_ok()
    );
}

#[test]
fn the_actor_identifier_list_gets_one_entry_per_type() {
    const BASE: &[u8] = include_bytes!("../../../../xtask/data/reference/entity_identifiers.dat");
    let decode = |bytes: &[u8]| {
        let root = Reader::new(bytes, LIST_BUDGET, 0)
            .nbt(nbt::Encoding::NetworkLittleEndian)
            .unwrap();
        match root.get("idlist") {
            Some(Value::List(List::Compound(list))) => list.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(Customs::default().identifier_list(BASE).unwrap(), BASE);
    let customs = registry(vec![ghost(), Definition::new("test:prop").spawn_egg()]);
    let base = decode(BASE);
    let list = decode(&customs.identifier_list(BASE).unwrap());
    assert_eq!(list.len(), base.len() + 2);
    assert_eq!(
        list[..base.len()],
        base[..],
        "the vanilla entries as they were"
    );
    let max = base
        .iter()
        .filter_map(|c| match c.get("rid") {
            Some(Value::Int(r)) => Some(*r),
            _ => None,
        })
        .max()
        .unwrap();
    let entry = |id: &str, egg: u8, rid: i32, summonable: u8| {
        Compound::from([
            ("bid".to_owned(), Value::String(String::new())),
            ("hasspawnegg".to_owned(), Value::Byte(egg)),
            ("id".to_owned(), Value::String(id.to_owned())),
            ("rid".to_owned(), Value::Int(rid)),
            ("summonable".to_owned(), Value::Byte(summonable)),
        ])
    };
    assert_eq!(list[base.len()], entry("test:ghost", 0, max + 1, 1));
    assert_eq!(list[base.len() + 1], entry("test:prop", 1, max + 2, 0));
}

// pmmp: Entity::spawnToAll
#[test]
fn custom_entities_are_shown_with_their_identifier_health_and_box_and_removed() {
    let mut h = harness(1, vec![ghost()]);
    h.take(1);
    let id = spawn(&mut h, "test:ghost", at(2.5, -60.0, 2.5));
    let e = context(&mut h).entity(id).unwrap().clone();
    assert_eq!(e.entity_type(), Type::Custom);
    assert_eq!(e.custom_type(), Some("test:ghost"));
    assert_eq!(e.network_type(), "test:ghost");
    assert_eq!((e.health(), e.max_health()), (30.0, 30.0));
    assert_eq!(e.name_tag(), "Ghost");
    assert_eq!(e.custom_data(), Some(&Compound::new()));
    assert!(e.can_save_with_chunk() && !e.is_invulnerable() && e.can_be_collided_with());
    let b = e.bounding_box();
    assert!((b.max.x - b.min.x - 0.8).abs() < 1e-9 && (b.max.y - b.min.y - 2.5).abs() < 1e-9);
    assert!(
        (e.eye_height() - 1.35).abs() < 1e-9,
        "half the height plus 0.1"
    );
    h.step(Vec::new());
    let packets = h.take(1);
    let runtime_id = e.runtime_id().unwrap();
    let add = packets
        .iter()
        .find_map(|p| match p {
            Packet::AddActor(a) if a.entity_runtime_id == runtime_id => Some(a.clone()),
            _ => None,
        })
        .expect("shown");
    assert_eq!(add.entity_type, "test:ghost");
    assert_eq!(add.attributes.len(), 1);
    let health = &add.attributes[0];
    assert_eq!(
        (health.name.as_str(), health.value, health.max, health.min),
        ("minecraft:health", 30.0, 30.0, 0.0)
    );
    let meta = |k: EntityDataKey| add.entity_metadata.get(&(k as u32)).cloned();
    assert_eq!(
        meta(EntityDataKey::Width),
        Some(MetadataValue::Float32(0.8))
    );
    assert_eq!(
        meta(EntityDataKey::Height),
        Some(MetadataValue::Float32(2.5))
    );
    assert_eq!(
        meta(EntityDataKey::Scale),
        Some(MetadataValue::Float32(1.0))
    );
    assert_eq!(
        meta(EntityDataKey::Name),
        Some(MetadataValue::String("Ghost".to_owned()))
    );
    // A hit shows the new health, rounded up as PMMP shows it.
    assert!(context(&mut h).entity_mut(id).unwrap().attack(4.5));
    h.step(Vec::new());
    let health: Vec<f32> = h
        .take(1)
        .iter()
        .filter_map(|p| match p {
            Packet::UpdateAttributes(u) if u.entity_runtime_id == runtime_id => {
                Some(u.attributes[0].attribute_value.value)
            }
            _ => None,
        })
        .collect();
    assert_eq!(health, [26.0], "once");
    // Death: the animation, then the removal 25 ticks later.
    context(&mut h).entity_mut(id).unwrap().kill();
    for _ in 0..30 {
        h.step(Vec::new());
    }
    assert!(context(&mut h).entity(id).is_none());
    assert_eq!(removes(&h.take(1)), [runtime_id]);
    // A despawn removes it at the end of the tick.
    let other = spawn(&mut h, "test:ghost", at(2.5, -60.0, 2.5));
    h.step(Vec::new());
    let runtime_id = context(&mut h).entity(other).unwrap().runtime_id().unwrap();
    assert_eq!(adds(&h.take(1)), [runtime_id]);
    context(&mut h)
        .entity_mut(other)
        .unwrap()
        .flag_for_despawn();
    h.step(Vec::new());
    assert_eq!(removes(&h.take(1)), [runtime_id]);
}

// pmmp: Entity::onUpdate, Entity::onInteract
#[test]
fn behaviours_tick_every_period_in_id_order_and_answer_interactions() {
    let (tx, mut rx) = mpsc::channel(4096);
    let types = vec![
        Definition::new("test:a").with_behaviour(Recorder {
            name: "a",
            tx: tx.clone(),
        }),
        Definition::new("test:b")
            .with_tick_period(NonZeroU32::new(3).unwrap())
            .with_behaviour(Recorder { name: "b", tx }),
        Definition::new("test:quiet"),
    ];
    let mut h = harness(1, types);
    let a1 = spawn(&mut h, "test:a", at(0.5, -60.0, 3.5));
    let b1 = spawn(&mut h, "test:b", at(1.5, -60.0, 3.5));
    let a2 = spawn(&mut h, "test:a", at(2.5, -60.0, 3.5));
    let quiet = spawn(&mut h, "test:quiet", at(-0.5, -60.0, 3.5));
    assert!(a1 < b1 && b1 < a2);
    let start = h.state.tick;
    for _ in 0..9 {
        h.step(Vec::new());
    }
    let mut expected = Vec::new();
    for t in 0..9 {
        let tick = start + t;
        expected.push(Call::Tick("a", tick, a1));
        // Ages 3, 6 and 9.
        if (t + 1) % 3 == 0 {
            expected.push(Call::Tick("b", tick, b1));
        }
        expected.push(Call::Tick("a", tick, a2));
    }
    assert_eq!(calls(&mut rx), expected);
    // Interactions within reach call the type's behaviour; others do nothing.
    let player = player::Id::new(SessionId::new(1));
    let interact = |h: &mut Harness, target| {
        npc::interact_with(
            &mut h.state,
            SessionId::new(1),
            target,
            TEST_HOOKS,
            no_trade,
        )
    };
    assert!(interact(&mut h, b1));
    assert!(
        interact(&mut h, quiet),
        "an interaction without a behaviour"
    );
    assert_eq!(calls(&mut rx), [Call::Interact("b", b1, player)]);
    let far = spawn(&mut h, "test:a", at(0.5, -60.0, 12.5));
    assert!(!interact(&mut h, far), "out of reach");
    context(&mut h).entity_mut(a2).unwrap().kill();
    assert!(!interact(&mut h, a2), "dying");
    assert!(calls(&mut rx).is_empty());
    // A dying entity ticks no more; the behaviours are back in the registry.
    h.step(Vec::new());
    assert!(
        calls(&mut rx)
            .iter()
            .all(|c| !matches!(c, Call::Tick(_, _, e) if *e == a2))
    );
    assert!(
        h.state.rules.entities.custom.behaviours[..2]
            .iter()
            .all(Option::is_some)
    );
}

#[test]
fn custom_entities_round_trip_with_their_data_and_unregistered_types_stay_as_saved() {
    let types = || vec![ghost(), Definition::new("test:prop").temporary()];
    let mut h = harness(1, types());
    let id = spawn(&mut h, "test:ghost", at(2.5, -60.0, 2.5));
    let prop = spawn(&mut h, "test:prop", at(3.5, -60.0, 3.5));
    let data = Compound::from([
        ("kills".to_owned(), Value::Int(3)),
        ("team".to_owned(), Value::String("red".to_owned())),
    ]);
    {
        let mut cx = context(&mut h);
        let mut m = cx.entity_mut(id).unwrap();
        assert!(m.set_custom_data(data.clone()));
        assert!(!m.set_custom_data(too_big()));
        assert!(m.set_health(12.0));
        assert!(m.set_invulnerable(true));
        assert!(m.set_name_tag("Boo"));
        assert!(!cx.entity(prop).unwrap().can_save_with_chunk());
        let tnt = cx.spawn_tnt(WORLD, at(3.5, -60.0, 2.5), 80).unwrap();
        let mut m = cx.entity_mut(tnt).unwrap();
        assert!(!m.set_custom_data(data.clone()), "not a custom entity");
        assert_eq!((m.custom_data(), m.custom_type()), (None, None));
        m.flag_for_despawn();
    }
    let e = context(&mut h).entity(id).unwrap().clone();
    // The harness's chunks hold saved compounds of their own that are not entities.
    let list: Vec<Compound> = h
        .state
        .rules
        .entities
        .list(KEY)
        .into_iter()
        .filter(|c| c.contains_key("identifier"))
        .collect();
    assert_eq!(list, [saved::save(&e)], "the temporary prop is not saved");
    let c = &list[0];
    assert_eq!(
        c.get("identifier"),
        Some(&Value::String("test:ghost".to_owned()))
    );
    assert_eq!(c.get(saved::DATA_KEY), Some(&Value::Compound(data.clone())));
    // Loaded by a server with the type: the same entity.
    let mut again = Entities::default();
    again.set_customs(registry(types()));
    again.load_list(KEY, &list, &crate::custom::Registry::default());
    let ids: Vec<Id> = again.ids().collect();
    assert_eq!(ids.len(), 1);
    let loaded = again.get(ids[0]).unwrap();
    assert_eq!(loaded.custom_type(), Some("test:ghost"));
    assert_eq!(loaded.custom_data(), Some(&data));
    assert_eq!(loaded.health(), 12.0);
    assert!(loaded.is_invulnerable());
    assert_eq!(loaded.name_tag(), "Boo");
    assert_eq!(saved::save(loaded), list[0], "saved again the same");
    // Loaded by a server without it: kept as saved, written back unchanged.
    let mut bare = Entities::default();
    bare.load_list(KEY, &list, &crate::custom::Registry::default());
    assert_eq!(bare.count(0), 0);
    assert_eq!(bare.list(KEY), list);
    assert_eq!(bare.unload(KEY), None, "nothing changed");
    // Data over the bound on disk is not an entity either.
    let mut big = list[0].clone();
    big.insert(saved::DATA_KEY.to_owned(), Value::Compound(too_big()));
    let mut store = Entities::default();
    store.set_customs(registry(types()));
    store.load_list(
        KEY,
        std::slice::from_ref(&big),
        &crate::custom::Registry::default(),
    );
    assert_eq!(store.count(0), 0);
    assert_eq!(store.list(KEY), [big]);
}

/// A scale is refused, by the API and on disk, when it would make the box larger than 16 blocks
/// on a side; a small kind may scale further.
#[test]
fn scale_is_refused_when_the_box_would_exceed_16_blocks() {
    let big = || vec![Definition::new("test:big").with_size(16.0, 16.0)];
    let mut h = harness(1, big());
    let id = spawn(&mut h, "test:big", at(2.5, -60.0, 2.5));
    let mut cx = context(&mut h);
    let mut m = cx.entity_mut(id).unwrap();
    assert!(!m.set_scale(64.0));
    assert!(m.set_scale(1.0));
    let stick = crate::item::Stack::new(crate::item::Kind::Stick.default_type(), 1);
    let item = cx
        .drop_item(WORLD, at(3.5, -60.0, 3.5), stick, None)
        .unwrap();
    assert!(
        cx.entity_mut(item).unwrap().set_scale(64.0),
        "0.25 × 64 = 16"
    );
    // A saved big entity with scale 64 is not loaded.
    let mut c = saved::save(context(&mut h).entity(id).unwrap());
    c.insert("Scale".to_owned(), Value::Float(64.0));
    let mut store = Entities::default();
    store.set_customs(registry(big()));
    store.load_list(
        KEY,
        std::slice::from_ref(&c),
        &crate::custom::Registry::default(),
    );
    assert_eq!(store.count(0), 0);
}

#[test]
fn the_entity_cap_and_the_spawn_checks_apply_to_custom_entities() {
    let mut h = harness(1, vec![ghost()]);
    let mut cx = context(&mut h);
    let pos = at(2.5, -60.0, 2.5);
    assert_eq!(
        cx.spawn_custom_entity(WORLD, pos, "test:nothing"),
        Err(Error::UnknownType {
            identifier: "test:nothing".to_owned()
        })
    );
    for bad in [
        at(f64::NAN, -60.0, 2.5),
        at(2.5, 30_000_001.0, 2.5),
        at(5000.5, -60.0, 2.5),
    ] {
        assert_eq!(
            cx.spawn_custom_entity(WORLD, bad, "test:ghost"),
            Err(Error::NotSpawned)
        );
    }
    let other = world::Id::new(7).unwrap();
    assert_eq!(
        cx.spawn_custom_entity(other, pos, "test:ghost"),
        Err(Error::NotSpawned)
    );
    for _ in 0..MAX_ENTITIES_PER_WORLD {
        cx.spawn_custom_entity(WORLD, pos, "test:ghost").unwrap();
    }
    assert_eq!(
        cx.spawn_custom_entity(WORLD, pos, "test:ghost"),
        Err(Error::NotSpawned)
    );
    assert_eq!(cx.entities(WORLD).count(), MAX_ENTITIES_PER_WORLD);
}

#[test]
fn spawning_and_removing_custom_entities_leaves_every_table_at_its_start() {
    let (tx, mut rx) = mpsc::channel(65_536);
    let mut h = harness(2, vec![ghost().with_behaviour(Recorder { name: "g", tx })]);
    for _ in 0..4 {
        h.entities();
    }
    h.state.rules.entities.take_changed();
    for round in 0..3 {
        let mut ids = Vec::new();
        for i in 0..1000i32 {
            let (x, z) = (f64::from(i % 40 - 20), f64::from(i / 40 - 12));
            ids.push(spawn(&mut h, "test:ghost", at(x + 0.5, -60.0, z + 0.5)));
        }
        for _ in 0..3 {
            h.entities();
        }
        for (i, &id) in ids.iter().enumerate() {
            let mut cx = context(&mut h);
            let mut m = cx.entity_mut(id).unwrap();
            if (i + round) % 2 == 0 {
                m.flag_for_despawn();
            } else {
                m.kill();
            }
        }
        for _ in 0..40 {
            h.entities();
        }
        assert!(!calls(&mut rx).is_empty());
    }
    h.state.rules.entities.take_changed();
    let Entities {
        slots,
        free,
        counts,
        columns,
        runtime_ids,
        unassigned,
        kept,
        changed,
        departed,
        doomed,
        restep,
        spawned,
        left,
        custom,
        ..
    } = &h.state.rules.entities;
    assert!(slots.is_empty() && free.is_empty() && counts.is_empty());
    assert!(columns.is_empty() && runtime_ids.is_empty() && unassigned.is_empty());
    assert!(changed.is_empty() && departed.is_empty() && doomed.is_empty());
    assert!(restep.is_empty() && spawned.is_empty() && left.is_empty());
    assert!(kept.values().all(|k| !k.is_empty()));
    assert!(custom.behaviours.iter().all(Option::is_some));
    let packets = h.take(1);
    assert_eq!(adds(&packets).len(), removes(&packets).len());
}

/// Moves, hurts, spawns and despawns custom entities from their own ticks.
struct Restless;

impl Behaviour for Restless {
    fn on_tick(&mut self, cx: &mut Context<'_>, entity: Id) {
        let tick = cx.tick();
        let Some(e) = cx.entity(entity) else {
            return;
        };
        let at = e.position();
        let key = entity
            .index()
            .wrapping_add(u32::try_from(tick % 97).unwrap());
        if key % 5 == 0 {
            let x = f64::from(key % 7) * 0.05 - 0.15;
            cx.entity_mut(entity)
                .unwrap()
                .set_motion(Vec3::new(x, 0.42, -x));
        }
        if key % 11 == 0 {
            cx.entity_mut(entity).unwrap().attack(3.0);
        }
        if key % 13 == 0 && cx.entities(WORLD).count() < 200 {
            let _spawned = cx.spawn_custom_entity(WORLD, at, "test:restless");
        }
        if key % 29 == 0 {
            cx.entity_mut(entity).unwrap().flag_for_despawn();
        }
    }
}

#[test]
fn custom_entities_tick_the_same_on_one_or_many_threads() {
    let mut hashes = Vec::new();
    for threads in [1, 4] {
        let types = vec![
            Definition::new("test:restless").with_behaviour(Restless),
            Definition::new("test:slow")
                .with_gravity(0.02)
                .with_tick_period(NonZeroU32::new(2).unwrap())
                .with_behaviour(Restless),
        ];
        let mut h = harness(threads, types);
        let mut rng = Rng(0xc6_b001);
        let mut trace = Vec::new();
        for tick in 0..90 {
            if tick % 4 == 0 {
                let xi = i32::try_from(rng.below(16)).unwrap() - 8;
                let zi = i32::try_from(rng.below(16)).unwrap() - 8;
                let pos = at(f64::from(xi) + 0.5, -52.0, f64::from(zi) + 0.5);
                let name = if rng.below(2) == 0 {
                    "test:restless"
                } else {
                    "test:slow"
                };
                let _spawned = context(&mut h).spawn_custom_entity(WORLD, pos, name);
            }
            if tick % 9 == 0 {
                let center = at(0.5, -59.0, 0.5);
                super::super::explode(&mut h.state, 0, center, 2.0, false, Raise::Quiet);
            }
            h.step(Vec::new());
            trace.push(h.state.hash());
        }
        assert!(
            h.state.rules.entities.count(0) > 10,
            "the entities lived and multiplied"
        );
        hashes.push(trace);
    }
    assert_eq!(hashes[0], hashes[1]);
}

// pmmp: Entity::isInsideOfSolid
/// A custom entity suffocates when its eyes, not its feet, are in a solid block.
#[test]
fn a_custom_entity_suffocates_with_its_eyes_in_a_solid_block() {
    let mut h = harness(1, vec![ghost()]);
    let stone = super::super::physics::tests::named("minecraft:stone");
    h.state.worlds[0]
        .set_block(world::BlockPos::new(3, -59, 3), stone)
        .unwrap();
    let id = spawn(&mut h, "test:ghost", at(3.5, -60.0, 3.5));
    h.entities();
    assert_eq!(context(&mut h).entity(id).unwrap().health(), 29.0);
}
