//! Picking an entity: the middle click on an entity.

use super::find;
use super::kind::{Character, Form, Kind};
use crate::block;
use crate::event::PlayerEntityPick;
use crate::item::{self, Stack};
use crate::protocol::packet::ActorPickRequest;
use crate::tick::interact::props;
use crate::tick::{Phase, SessionId, State, dispatch, inventory, survival};

/// Handles `ActorPickRequest` from the client: the item of the entity with that unique ID in the
/// player's world goes to its hand ([`inventory::equip_or_add`]) after [`PlayerEntityPick`].
/// Only a creative player gets an item it does not have; others select or swap one they carry.
pub(in crate::tick) fn on_actor_pick_request(
    state: &mut State,
    session: SessionId,
    packet: &ActorPickRequest,
) {
    let Ok(runtime_id) = u64::try_from(packet.entity_unique_id) else {
        return;
    };
    let Some((entity, picked)) = pick(state, session, runtime_id) else {
        return;
    };
    let mut event = None;
    if dispatch::heard::<PlayerEntityPick>(state) {
        let mut e = PlayerEntityPick {
            player: crate::player::Id::new(session),
            entity,
            result_item: picked.clone(),
            cancelled: false,
        };
        let held = inventory::held_of(state, session);
        dispatch::call(state, &mut e);
        // A listener that changed the entity, the player or its hand cancels the pick.
        if e.cancelled
            || (dispatch::listened::<PlayerEntityPick>(state)
                && (pick(state, session, runtime_id) != Some((entity, picked.clone()))
                    || inventory::held_of(state, session) != held))
        {
            return;
        }
        event = Some(e);
    }
    if let Some(player) = state.players.get_mut(&session) {
        inventory::equip_or_add(player, picked);
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// What picking the entity of `runtime_id` gives `session` now: the entity and its item; `None`
/// if the player is not spawned and alive, the entity is not in its world, gives no item, or the
/// player may not take an item it does not have.
fn pick(state: &State, session: SessionId, runtime_id: u64) -> Option<(crate::entity::Id, Stack)> {
    let player = state.players.get(&session)?;
    if player.phase != Phase::Spawned || !player.is_alive() {
        return None;
    }
    let id = find(state, runtime_id)?;
    let e = state
        .rules
        .entities
        .get(id)
        .filter(|e| !e.flagged && e.world == player.world)?;
    let picked = picked_item(&e.kind)?;
    let finite = survival::has_finite_resources(survival::game_mode(state, player));
    (!finite || inventory::has(player, &picked)).then_some((id, picked))
}

/// The item picking an entity of `kind` gives; `None` for the kinds that give none (items, orbs, projectiles, clouds, rockets, human and custom NPCs).
pub(super) fn picked_item(kind: &Kind) -> Option<Stack> {
    let one = |t: item::Type| Stack::new(t, 1);
    let block_item = |b: block::State| match b.item() {
        block::ItemForm::Item(t) => Some(one(t)),
        block::ItemForm::BlockEntity | block::ItemForm::None => None,
    };
    match kind {
        Kind::Item(_)
        | Kind::XpOrb(_)
        | Kind::Projectile(_)
        | Kind::Cloud(_)
        | Kind::Firework(_) => None,
        Kind::FallingBlock(f) => block_item(f.block),
        Kind::Tnt(t) => {
            let tnt = block::Kind::Tnt.default_state();
            if t.underwater {
                block_item(props::renamed(tnt, "minecraft:underwater_tnt")?)
            } else {
                block_item(tnt)
            }
        }
        Kind::Npc(Character { form, .. }) => match form {
            Form::Villager { .. } => Some(one(item::Kind::VillagerSpawnEgg.default_type())),
            Form::Zombie { .. } => Some(one(item::Kind::ZombieSpawnEgg.default_type())),
            Form::Human { .. } | Form::Custom(_) => None,
        },
        Kind::Painting(_) => Some(one(item::Kind::Painting.default_type())),
        Kind::EndCrystal(_) => Some(one(item::Kind::EndCrystal.default_type())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::Face;
    use crate::config::GameMode;
    use crate::entity::{Id, PaintingMotive};
    use crate::event::{Listeners, Priority};
    use crate::protocol::packet::Packet;
    use crate::tick::Context;
    use crate::tick::entity::tests::{Harness, at, standing};
    use crate::tick::tests::{input, packet};
    use crate::world::{self, BlockPos};

    thread_local! {
        /// What listeners and monitors wrote down; they run on the test's thread.
        static NOTES: std::cell::Cell<Vec<String>> = const { std::cell::Cell::new(Vec::new()) };
    }

    /// The world the tests' entities live in.
    const WORLD: world::Id = world::Id::DEFAULT;

    fn item(kind: item::Kind) -> Stack {
        Stack::new(kind.default_type(), 1)
    }

    fn block_item(state: block::State) -> Option<Stack> {
        match state.item() {
            block::ItemForm::Item(t) => Some(Stack::new(t, 1)),
            _ => None,
        }
    }

    fn picked(h: &Harness, id: Id) -> Option<Stack> {
        h.state.rules.entities.get(id).unwrap().picked_item()
    }

    // api: Entity::getPickedItem, entity\object\EndCrystal::getPickedItem,
    // api: FallingBlock::getPickedItem, Painting::getPickedItem, PrimedTNT::getPickedItem,
    // api: Villager::getPickedItem, Zombie::getPickedItem
    #[test]
    fn each_kind_gives_its_picked_item() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let mut cx = Context {
            state: &mut h.state,
        };
        let crystal = cx.spawn_end_crystal(WORLD, at(3.5, -60.0, 0.5)).unwrap();
        let sand = block::State::find("minecraft:sand", &[]).unwrap();
        let falling = cx
            .spawn_falling_block(WORLD, at(4.5, -50.0, 0.5), sand)
            .unwrap();
        let wall = BlockPos::new(6, -60, 0);
        let painting = cx
            .spawn_painting(WORLD, wall, Face::North, PaintingMotive::Kebab)
            .unwrap();
        let tnt = cx.spawn_tnt(WORLD, at(7.5, -60.0, 0.5), 80).unwrap();
        let wet = cx.spawn_tnt(WORLD, at(8.5, -60.0, 0.5), 80).unwrap();
        cx.entity_mut(wet)
            .unwrap()
            .set_works_underwater(true)
            .unwrap();
        let villager = cx
            .spawn_villager(WORLD, at(9.5, -60.0, 0.5), 0, &[])
            .unwrap();
        let zombie = cx.spawn_zombie(WORLD, at(10.5, -60.0, 0.5)).unwrap();
        let human = cx
            .spawn_human(WORLD, at(11.5, -60.0, 0.5), "Guide", None)
            .unwrap();
        let stick = Stack::new(item::Kind::Stick.default_type(), 1);
        let dropped = cx
            .drop_item(WORLD, at(12.5, -60.0, 0.5), stick, None)
            .unwrap();
        assert_eq!(picked(&h, crystal), Some(item(item::Kind::EndCrystal)));
        assert_eq!(picked(&h, falling), block_item(sand));
        assert_eq!(picked(&h, painting), Some(item(item::Kind::Painting)));
        let lit = block::Kind::Tnt.default_state();
        let underwater = props::renamed(lit, "minecraft:underwater_tnt").unwrap();
        assert_ne!(block_item(lit), block_item(underwater));
        assert_eq!(picked(&h, tnt), block_item(lit));
        assert_eq!(picked(&h, wet), block_item(underwater));
        assert_eq!(
            picked(&h, villager),
            Some(item(item::Kind::VillagerSpawnEgg))
        );
        assert_eq!(picked(&h, zombie), Some(item(item::Kind::ZombieSpawnEgg)));
        assert_eq!(picked(&h, human), None, "a human NPC gives none");
        assert_eq!(picked(&h, dropped), None);
    }

    /// Spawns a zombie east of player 1 and shows it; returns its ID and runtime ID.
    fn zombie(h: &mut Harness) -> (Id, u64) {
        let id = Context {
            state: &mut h.state,
        }
        .spawn_zombie(WORLD, at(2.5, -60.0, 0.5))
        .unwrap();
        h.step(Vec::new());
        (id, h.state.rules.entities.get(id).unwrap().runtime_id)
    }

    /// Makes player 1 pick the entity of `runtime_id`.
    fn pick(h: &mut Harness, runtime_id: u64) {
        let p = ActorPickRequest {
            entity_unique_id: runtime_id.cast_signed(),
            hot_bar_slot: 0,
            with_data: false,
        };
        let i = input(1, 50, packet(Packet::ActorPickRequest(p)));
        h.step(vec![i]);
    }

    fn player_mut(h: &mut Harness) -> &mut crate::tick::Player {
        h.state.players.get_mut(&SessionId::new(1)).unwrap()
    }

    fn egg() -> Stack {
        Stack::new(item::Kind::ZombieSpawnEgg.default_type(), 1)
    }

    // api: PlayerEntityPickEvent
    #[test]
    fn creative_players_get_the_entity_item_and_others_only_select_one_they_carry() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let (_, zombie) = zombie(&mut h);
        // Survival without an egg: nothing.
        pick(&mut h, zombie);
        assert!(player_mut(&mut h).inventory.held().is_empty());
        // Survival with an egg outside the hotbar: swapped into the held slot.
        inventory::set_main(player_mut(&mut h), 20, egg());
        pick(&mut h, zombie);
        assert_eq!(player_mut(&mut h).inventory.held(), &egg());
        assert!(
            player_mut(&mut h)
                .inventory
                .main_stack(20)
                .unwrap()
                .is_empty()
        );
        // Creative: a new egg, but none more if it has one.
        inventory::set_held(player_mut(&mut h), Stack::EMPTY);
        player_mut(&mut h).set_game_mode(GameMode::Creative);
        h.step(Vec::new());
        pick(&mut h, zombie);
        assert_eq!(player_mut(&mut h).inventory.held(), &egg());
        pick(&mut h, zombie);
        let eggs = (0..36)
            .filter(|&s| player_mut(&mut h).inventory.main_stack(s) == Some(&egg()))
            .count();
        assert_eq!(eggs, 1);
        // An unknown entity, or one that gives nothing: nothing.
        inventory::set_held(player_mut(&mut h), Stack::EMPTY);
        pick(&mut h, 999_999);
        let guide = Context {
            state: &mut h.state,
        }
        .spawn_human(WORLD, at(4.5, -60.0, 0.5), "Guide", None)
        .unwrap();
        h.step(Vec::new());
        let guide = h.state.rules.entities.get(guide).unwrap().runtime_id;
        pick(&mut h, guide);
        assert!(player_mut(&mut h).inventory.held().is_empty());
    }

    /// Writes down `note`.
    fn note(note: String) {
        NOTES.with(|n| {
            let mut list = n.take();
            list.push(note);
            n.set(list);
        });
    }

    /// What was written down since the last call.
    fn notes() -> Vec<String> {
        NOTES.with(std::cell::Cell::take)
    }

    // api: PlayerEntityPickEvent, PlayerEntityPickEvent::getEntity,
    // api: PlayerEntityPickEvent::getResultItem, PlayerEntityPickEvent::cancel,
    // api: PlayerEntityPickEvent::uncancel, PlayerEntityPickEvent::isCancelled
    #[test]
    fn an_entity_pick_listener_sees_the_pick_and_may_cancel_it() {
        let mut l = Listeners::default();
        l.push(
            Priority::Normal,
            move |e: &mut PlayerEntityPick, _: &mut Context<'_>| {
                let first = NOTES.with(|n| {
                    let list = n.take();
                    let empty = list.is_empty();
                    n.set(list);
                    empty
                });
                note(format!("{:?}", (e.player(), e.entity(), e.result_item())));
                if first {
                    e.cancel();
                    assert!(e.is_cancelled());
                    e.uncancel();
                    e.cancel();
                }
            },
        );
        l.push_monitor(move |_: &PlayerEntityPick, cx: &Context<'_>| {
            let held = cx.state.players[&SessionId::new(1)]
                .inventory
                .held()
                .clone();
            note(format!("{held:?}"));
        });
        let mut h = Harness::with_listeners(1, l);
        h.spawn(1, standing(0.0, 0.0));
        player_mut(&mut h).set_game_mode(GameMode::Creative);
        let (id, zombie) = zombie(&mut h);
        pick(&mut h, zombie);
        assert!(player_mut(&mut h).inventory.held().is_empty(), "cancelled");
        pick(&mut h, zombie);
        assert_eq!(player_mut(&mut h).inventory.held(), &egg());
        let me = crate::player::Id::new(SessionId::new(1));
        let note = format!("{:?}", (me, id, &egg()));
        assert_eq!(notes(), [note.clone(), note, format!("{:?}", egg())]);
    }

    #[test]
    fn a_listener_that_removes_the_entity_cancels_the_pick() {
        let mut l = Listeners::default();
        l.push(
            Priority::Normal,
            |e: &mut PlayerEntityPick, cx: &mut Context<'_>| {
                cx.entity_mut(e.entity()).unwrap().flag_for_despawn();
            },
        );
        let mut h = Harness::with_listeners(1, l);
        h.spawn(1, standing(0.0, 0.0));
        player_mut(&mut h).set_game_mode(GameMode::Creative);
        let (_, zombie) = zombie(&mut h);
        pick(&mut h, zombie);
        assert!(player_mut(&mut h).inventory.held().is_empty());
    }
}
