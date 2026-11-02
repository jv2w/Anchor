//! Tick tests of custom content: a custom block placed, used and broken with its behaviour hooks
//! and drops, a villager's offers read, changed and checked as typed values, custom items kept
//! through a save and a restart, and a custom item that breaks nothing in creative mode.

use std::sync::Arc;

use super::*;
use crate::Context as Ctx;
use crate::custom::tests::{ORE, RUBY, SWORD};
use crate::custom::{Behaviour, Block, ItemRef, Offer, Property, Registry};
use crate::item::Extra;
use crate::save::{Request, Response};
use crate::world;

/// A custom block with a `lit` property that its behaviour toggles.
static SWITCH: Block = Block::new("test:switch", "test_switch")
    .with_properties(&[Property::bool("lit")])
    .with_hardness(0.0)
    .with_behaviour(&Switch);

/// The block above `pos`.
fn above(pos: BlockPos) -> BlockPos {
    BlockPos::new(pos.x, pos.y + 1, pos.z)
}

/// A switch that lights when used, and marks the block above it with stone when placed and
/// dirt when broken. Placed, it notes the placer's held count; broken, the world's entities.
struct Switch;

impl Behaviour for Switch {
    fn on_use(
        &self,
        cx: &mut Ctx<'_>,
        _player: crate::player::Id,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) -> bool {
        let lit = state.with("lit", block::PropertyValue::Byte(1)).unwrap();
        cx.world_mut(world).unwrap().set_block(pos, lit).unwrap();
        true
    }

    fn on_place(
        &self,
        cx: &mut Ctx<'_>,
        player: crate::player::Id,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) {
        let session = player.session();
        assert_eq!(state.value("lit"), Some(block::PropertyValue::Byte(0)));
        let held = cx.state.players[&session].inventory.held().count();
        note(SEEN, ("placed", held));
        let stone = Kind::Stone.default_state();
        cx.world_mut(world)
            .unwrap()
            .set_block(above(pos), stone)
            .unwrap();
    }

    fn on_break(
        &self,
        cx: &mut Ctx<'_>,
        _player: Option<crate::player::Id>,
        world: world::Id,
        pos: BlockPos,
        _state: block::State,
    ) {
        note(SEEN, ("broken", cx.state.rules.entities.count(0)));
        let w = cx.world_mut(world).unwrap();
        assert_eq!(w.block(pos), Some(block::AIR));
        w.set_block(above(pos), Kind::Dirt.default_state()).unwrap();
    }
}

/// The test server's custom content: `RUBY`, `ORE`, `SWITCH` and `SWORD`.
fn registry() -> Registry {
    let mut r = Registry::default();
    r.add_item(&RUBY);
    r.add_block(&ORE);
    r.add_block(&SWITCH);
    r.add_item(&SWORD);
    r.finish().unwrap();
    r
}

/// A harness whose server has [`registry`].
fn harness() -> (Harness, Arc<Registry>) {
    harness_with(|_| {})
}

/// [`harness`] with the settings `edit` makes.
fn harness_with(edit: impl FnOnce(&mut Config)) -> (Harness, Arc<Registry>) {
    let mut h = Harness::new(edit);
    let r = Arc::new(registry());
    h.state.custom = Arc::clone(&r);
    for w in &mut h.state.worlds {
        w.set_custom(Arc::clone(&r));
    }
    (h, r)
}

// api: Item::nbtSerialize, Item::nbtDeserialize
/// A stack's disk form reads back through the context, which knows the server's custom items
/// and blocks; what is no item stack reads as nothing.
#[test]
fn a_saved_stack_reads_back_through_the_context_custom_ones_too() {
    let (mut h, r) = harness();
    let cx = Ctx {
        state: &mut h.state,
    };
    let switch = r.resolve(ItemRef::CustomBlock(&SWITCH)).unwrap();
    let named = Stack::new(crate::item::Kind::Diamond.default_type(), 7)
        .with_extra(Some(Extra {
            custom_name: Some("Rock".to_owned()),
            ..Extra::default()
        }))
        .unwrap();
    for stack in [
        Stack::new(r.item_type(&RUBY).unwrap(), 3),
        Stack::new(switch, 2),
        named,
        Stack::EMPTY,
    ] {
        assert_eq!(cx.stack_from_nbt(&stack.to_nbt()), Some(stack));
    }
    assert_eq!(cx.stack_from_nbt(&crate::nbt::Compound::new()), None);
}

/// In creative mode a block breaks with a ruby in the hand but not with `SWORD`, made
/// `without_creative_breaking`.
#[test]
fn a_creative_player_breaks_nothing_with_an_item_made_so() {
    let (mut h, r) = harness_with(creative);
    for (item, broken) in [(&RUBY, true), (&SWORD, false)] {
        h.set(TARGET, Kind::Stone.default_state());
        h.hold(Stack::new(r.item_type(item).unwrap(), 1));
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(
            h.block(TARGET) == block::AIR,
            broken,
            "{}",
            item.identifier()
        );
    }
}

/// A custom block is placed, used and broken through its behaviour, whose hooks run once their
/// action is applied: `on_place` after the item was used, `on_break` after the drop fell.
#[test]
fn a_custom_block_is_placed_used_and_broken_through_its_behaviour() {
    let (mut h, r) = harness();
    let item = r
        .resolve(crate::custom::ItemRef::CustomBlock(&SWITCH))
        .unwrap();
    let cx = Ctx {
        state: &mut h.state,
    };
    assert_eq!(
        cx.item_of(crate::custom::ItemRef::CustomBlock(&SWITCH)),
        Some(item)
    );
    assert_eq!(cx.custom_item(&RUBY), r.item_type(&RUBY));
    let switch = cx.custom_block(&SWITCH).unwrap();
    h.hold(Stack::new(item, 3));
    let at = place_on_grass(&mut h, &Stack::new(item, 3));
    assert_eq!(h.block(at), switch);
    assert_eq!(h.held().count(), 2);
    assert_eq!(h.block(above(at)).kind(), Kind::Stone, "on_place ran");
    assert_eq!(notes(SEEN), [dbg(("placed", 2))], "after the item was used");
    // The client is told the custom state's hash (with the stone above, in one packet).
    let sent = h.packets();
    let told = sent.iter().any(|p| match p {
        Packet::UpdateSubChunkBlocks(u) => u
            .blocks
            .iter()
            .any(|b| b.block_runtime_id == switch.network_id()),
        Packet::UpdateBlock(u) => u.new_block_runtime_id == switch.network_id(),
        _ => false,
    });
    assert!(told, "{sent:?}");
    // Using it lights it and places nothing.
    let input = h.auth(Vec::new(), Some(click(at, Face::West, &h.held())));
    h.run(vec![input]);
    assert_eq!(
        h.block(at).value("lit"),
        Some(block::PropertyValue::Byte(1))
    );
    assert_eq!(h.held().count(), 2);
    // Breaking it removes it and drops it, then calls the hook.
    let session = SessionId::new(1);
    let entities = h.state.rules.entities.count(0);
    assert!(destroy(
        &mut h.state,
        0,
        at,
        Destroy::Player {
            session,
            tool: Stack::EMPTY
        }
    ));
    assert_eq!(h.block(at), block::AIR);
    assert_eq!(h.block(above(at)).kind(), Kind::Dirt, "on_break ran");
    assert_eq!(
        notes(SEEN),
        [dbg(("broken", entities + 1))],
        "after the drop fell"
    );
}

/// The drops of the custom ore at the target with `tool`.
fn ore_drops(h: &mut Harness, tool: Stack) -> Vec<Stack> {
    let ore = h.state.custom.block_state(&ORE).unwrap();
    h.set(TARGET, ore);
    let cause = Destroy::Player {
        session: SessionId::new(1),
        tool,
    };
    let plan = plan_destruction(&h.state, 0, TARGET, &cause).unwrap();
    plan.items
}

#[test]
fn a_custom_ore_drops_its_items_with_a_compatible_tool() {
    let (mut h, r) = harness();
    let ruby = r.item_type(&RUBY).unwrap();
    let pick = |kind: crate::item::Kind| Stack::new(kind.default_type(), 1);
    // Harvest level 2: a stone pickaxe and better.
    assert_eq!(ore_drops(&mut h, Stack::EMPTY), Vec::new());
    assert_eq!(
        ore_drops(&mut h, pick(crate::item::Kind::WoodenPickaxe)),
        Vec::new()
    );
    let items = ore_drops(&mut h, pick(crate::item::Kind::IronPickaxe));
    assert_eq!(items, [Stack::new(ruby, 2)]);
    let silk = pick(crate::item::Kind::StonePickaxe)
        .with_extra(Some(Extra {
            enchantments: vec![(crate::item::Enchantment::SilkTouch, 1)],
            ..Extra::default()
        }))
        .unwrap();
    let own = r
        .resolve(crate::custom::ItemRef::CustomBlock(&ORE))
        .unwrap();
    assert_eq!(ore_drops(&mut h, silk), vec![Stack::new(own, 1)]);
    // The break time is 3 × 1.5 / 6 with an iron pickaxe.
    let ore = r.block_state(&ORE).unwrap();
    assert_eq!(
        ore.break_time_secs(&pick(crate::item::Kind::IronPickaxe)),
        Some(0.75)
    );
}

#[test]
fn a_villagers_offers_are_read_and_changed_as_typed_values() {
    let mut h = Harness::new(|_| {});
    let emerald = Stack::new(crate::item::Kind::Emerald.default_type(), 3);
    let bread = Stack::new(crate::item::Kind::Bread.default_type(), 6);
    let wheat = Stack::new(crate::item::Kind::Wheat.default_type(), 20);
    let mut cx = Ctx {
        state: &mut h.state,
    };
    let id = cx
        .spawn_villager(
            crate::world::Id::DEFAULT,
            Vec3::new(4.5, -60.0, 4.5),
            1,
            &[],
        )
        .expect("a villager spawns");
    assert_eq!(cx.trade_offers(id), Some(Vec::new()));
    let first = Offer::new(wheat.clone(), emerald.clone(), 16)
        .with_trader_xp(2)
        .with_any_damage();
    let second = Offer::new(emerald.clone(), bread.clone(), 8)
        .with_second_payment(wheat.clone())
        .with_tier(1)
        .without_reward_xp()
        .with_price_multipliers(0.2, 0.1)
        .with_demand(-3);
    cx.set_trade_offers(id, &[first.clone(), second.clone()])
        .unwrap();
    assert_eq!(
        cx.trade_offers(id),
        Some(vec![first.clone(), second.clone()])
    );
    assert_eq!(second.second_payment(), &wheat);
    assert!(first.takes_any_damage() && !second.rewards_xp());
    // Refused changes change nothing.
    let free = Offer::new(Stack::EMPTY, bread.clone(), 1);
    let refused = cx.add_trade_offer(id, &free).unwrap_err();
    assert_eq!(
        refused,
        crate::custom::Error::Offer {
            reason: "the offer has no payment"
        }
    );
    assert!(cx.set_trade_tier(id, 5).is_err());
    assert!(cx.remove_trade_offer(id, 2).is_err());
    assert_eq!(cx.trade_offers(id).map(|o| o.len()), Some(2));
    // The other changes.
    let removed = cx.remove_trade_offer(id, 0).unwrap();
    assert_eq!(removed, first);
    cx.replace_trade_offer(id, 0, &first.clone().with_uses(4))
        .unwrap();
    assert_eq!(cx.trade_offers(id).unwrap()[0].uses(), 4);
    cx.reset_trade_uses(id).unwrap();
    assert_eq!(cx.trade_offers(id).unwrap()[0].uses(), 0);
    assert_eq!(cx.trade_tier_xp(id), Some([0, 10, 70, 150, 250]));
    assert_eq!(cx.trade_xp(id), Some(0));
    cx.set_trade_tier_xp(id, [0, 5, 20, 40, 80]).unwrap();
    cx.set_trade_tier(id, 3).unwrap();
    assert_eq!(cx.trade_tier(id), Some(3));
    assert_eq!(cx.trade_tier_xp(id), Some([0, 5, 20, 40, 80]));
    assert_eq!(cx.trade_xp(id), Some(40), "the tier's requirement");
    cx.set_trade_xp(id, 7).unwrap();
    assert_eq!(cx.trade_xp(id), Some(7));
    assert_eq!(cx.trade_tier(id), Some(3), "the tier stays");
    assert!(cx.set_trade_xp(id, u32::MAX).is_err());
    assert_eq!(cx.trade_xp(id), Some(7));
    cx.add_trade_offer(id, &second).unwrap();
    assert_eq!(cx.trade_offers(id).map(|o| o.len()), Some(2));
    // Not a villager.
    let gone = crate::entity::Id::new(4_000_000, 7);
    assert_eq!(cx.trade_offers(gone), None);
    assert_eq!(cx.trade_xp(gone), None);
    assert_eq!(cx.trade_tier_xp(gone), None);
    assert_eq!(
        cx.set_trade_offers(gone, &[]),
        Err(crate::custom::Error::NotVillager)
    );
}

#[test]
fn an_offer_list_check_refuses_what_spawning_a_villager_refuses() {
    let mut h = Harness::new(|_| {});
    let emerald = Stack::new(crate::item::Kind::Emerald.default_type(), 3);
    let bread = Stack::new(crate::item::Kind::Bread.default_type(), 6);
    let good = Offer::new(emerald, bread.clone(), 4).with_uses(4);
    let lists = [
        vec![good.clone()],
        vec![good.clone().with_uses(5)],
        vec![Offer::new(Stack::EMPTY, bread, 1)],
        vec![good.clone().with_tier(5)],
        vec![good.clone().with_price_multipliers(0.0, f32::NAN)],
        vec![good; crate::custom::MAX_OFFERS + 1],
    ];
    let mut cx = Ctx {
        state: &mut h.state,
    };
    for offers in &lists {
        let spawned = cx
            .spawn_villager(
                crate::world::Id::DEFAULT,
                Vec3::new(4.5, -60.0, 4.5),
                1,
                offers,
            )
            .map(drop);
        assert_eq!(crate::custom::check_offers(offers), spawned);
    }
    assert_eq!(crate::custom::check_offers(&lists[0]), Ok(()));
    assert!(
        lists[1..]
            .iter()
            .all(|l| crate::custom::check_offers(l).is_err())
    );
}

/// `count` of the custom ruby.
fn rubies(r: &Registry, count: u8) -> Stack {
    Stack::new(r.item_type(&RUBY).unwrap(), count)
}

/// A server file in a temporary folder, served by the real save thread.
struct Disk(crate::save::tests::TempDir);

impl Disk {
    /// Runs the save thread on the file for `requests` of `state`, with its custom content as
    /// the server gives it, and returns its answers.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test stands in for the tick waiting for the save thread"
    )]
    fn serve(&self, state: &crate::tick::State, requests: Vec<Request>) -> Vec<Response> {
        if requests.is_empty() {
            return Vec::new();
        }
        let opened = crate::save::open(&self.0.0, Self::specs()).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(crate::save::REQUEST_CAPACITY);
        let (answer, mut answers) = tokio::sync::mpsc::channel(crate::save::RESPONSE_CAPACITY);
        let mut thread = opened.thread;
        thread.set_custom(Arc::clone(state.custom()), Arc::clone(state.entity_types()));
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = crate::save::run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        for request in requests {
            tx.blocking_send(request).unwrap();
        }
        tx.blocking_send(Request::Finish).unwrap();
        drop(tx);
        handle.join().unwrap().unwrap();
        let mut out = Vec::new();
        while let Ok(a) = answers.try_recv() {
            out.push(a);
        }
        out
    }

    fn specs() -> Vec<crate::save::WorldSpec> {
        vec![crate::save::WorldSpec {
            name: "world".to_owned(),
            kind: crate::world::Kind::Persistent,
            generator: Arc::new(crate::world::Flat),
            seed: Some(0),
            spawn: None,
        }]
    }
}

impl Harness {
    /// A new server on `disk` with [`registry`] (a restart), session 1 joined and spawned.
    fn restarted(disk: &Disk) -> Self {
        let opened = crate::save::open(&disk.0.0, Disk::specs()).unwrap();
        let (worlds, tick) = (opened.worlds, opened.tick);
        drop(opened.thread);
        let shared = crate::tick::Shared::new(
            crate::protocol::batch::Settings::default(),
            &registry(),
            &crate::tick::Customs::default(),
        )
        .unwrap();
        let setup = crate::tick::Setup {
            custom: registry(),
            ..crate::tick::Setup::default()
        };
        let mut state = State::new(settings(|_| {}), shared, setup, worlds, tick);
        state.saving.players = true;
        let (j, rx) = join(1, 0);
        let mut h = Self {
            state,
            output: Output::default(),
            rx,
            seq: 2,
            yaw: EAST,
        };
        h.run_on(disk, vec![j]);
        h.run_on(disk, Vec::new());
        h.run_on(disk, vec![radius(1, 1, 4)]);
        for _ in 0..100 {
            if h.player().phase == Phase::Spawning {
                break;
            }
            h.run_on(disk, Vec::new());
        }
        let runtime_id = h.player().runtime_id;
        let init = h.input(Packet::SetLocalPlayerAsInitialised(
            SetLocalPlayerAsInitialised {
                entity_runtime_id: runtime_id,
            },
        ));
        h.run_on(disk, vec![init]);
        assert_eq!(h.player().phase, Phase::Spawned);
        h
    }

    /// Commits a snapshot of this server to `disk`, as the next tick boundary takes it.
    fn commit_to(&mut self, disk: &Disk) {
        self.state.saving.requested = true;
        let mut output = Output::default();
        boundary(&mut self.state, &mut output, &mut InputLog::default());
        let commits: Vec<Request> = output
            .requests
            .into_iter()
            .filter(|r| matches!(r, Request::Commit(_)))
            .collect();
        assert_eq!(commits.len(), 1, "one snapshot");
        let answers = disk.serve(&self.state, commits);
        assert!(
            matches!(answers.as_slice(), [Response::Committed { .. }]),
            "{answers:?}"
        );
    }

    /// [`Harness::run`] with the save thread's requests answered from `disk`.
    fn run_on(&mut self, disk: &Disk, mut batch: Vec<Input>) {
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut self.output.answers);
        POOL.with(|pool| {
            step(
                &mut self.state,
                &mut batch,
                &mut answers,
                pool,
                &mut self.output,
                &mut log,
            );
        });
        deliver(&mut self.state, &mut self.output, &mut log);
        boundary(&mut self.state, &mut self.output, &mut log);
        let requests = std::mem::take(&mut self.output.requests);
        self.output.answers = disk.serve(&self.state, requests);
    }
}

/// Places `block` at `pos` with its block entity, as placing it would.
fn place_with_entity(h: &mut Harness, pos: BlockPos, block: block::State) {
    h.set(pos, block);
    let empty = Stack::EMPTY;
    crate::tick::block_entity::placed(&mut h.state, 0, pos, None, &empty, Face::West);
}

#[test]
fn custom_items_survive_a_save_and_a_restart_wherever_they_are_kept() {
    let (mut h, r) = harness();
    h.state.saving.players = true;
    let ruby = rubies(&r, 5);
    let emerald = Stack::new(crate::item::Kind::Emerald.default_type(), 1);
    let offer = Offer::new(ruby.clone(), emerald.clone(), 4).with_second_payment(emerald);
    let player = crate::player::Id::new(SessionId::new(1));
    // In the player's inventory, a chest, on the ground and in a villager's offers.
    place_with_entity(&mut h, TARGET, Kind::Chest.default_state());
    let mut cx = Ctx {
        state: &mut h.state,
    };
    cx.player_mut(player)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .set(4, ruby.clone())
        .unwrap();
    cx.block_inventory_mut(world::Id::DEFAULT, TARGET)
        .unwrap()
        .set(3, ruby.clone())
        .unwrap();
    cx.drop_item(
        world::Id::DEFAULT,
        Vec3::new(6.5, -60.0, 6.5),
        ruby.clone(),
        Some(Vec3::ZERO),
    )
    .unwrap();
    let villager = cx
        .spawn_villager(
            crate::world::Id::DEFAULT,
            Vec3::new(-4.5, -60.0, 4.5),
            1,
            std::slice::from_ref(&offer),
        )
        .unwrap();
    assert_eq!(cx.trade_offers(villager), Some(vec![offer.clone()]));
    for _ in 0..3 {
        h.run(Vec::new());
    }
    let disk = Disk(crate::save::tests::TempDir::new("custom-items"));
    h.commit_to(&disk);

    let mut h = Harness::restarted(&disk);
    for _ in 0..3 {
        h.run_on(&disk, Vec::new());
    }
    let main = h.player().inventory(crate::inventory::Kind::Main);
    assert_eq!(main.item(4), Some(&ruby), "the inventory");
    let entities = &h.state.rules.block_entities;
    assert_eq!(entities.get(0, TARGET).unwrap().items[3], ruby, "the chest");
    let cx = Ctx {
        state: &mut h.state,
    };
    let ground: Vec<Stack> = cx
        .entities(world::Id::DEFAULT)
        .filter_map(|(_, e)| e.item().cloned())
        .collect();
    assert_eq!(ground, [ruby], "the dropped item");
    let villagers: Vec<_> = cx
        .entities(world::Id::DEFAULT)
        .filter(|(_, e)| e.profession().is_some())
        .map(|(id, _)| cx.trade_offers(id))
        .collect();
    assert_eq!(villagers, [Some(vec![offer])], "the villager's offers");
}
