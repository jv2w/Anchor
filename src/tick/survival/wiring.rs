//! Tests of the events the survival, movement, chat and leave rules raise: the listener sees the
//! right fields, cancelling leaves the game as it was and resyncs the client, a changed field
//! changes the outcome, a monitor sees the applied state, and a listener that changes the game so
//! the rule's check fails cancels the action.

use tokio::sync::mpsc;

use super::*;
use crate::action::{self, Action};
use crate::block;
use crate::config::{Config, CriticalHits};
use crate::event::{
    Actor, CombustSource, DamageModifier, EntityCombust, EntityDamage, EntityEffectAdd,
    EntityEffectRemove, EntityExtinguish, EntityMotion, EntityRegainHealth, EntityTeleport,
    ExtinguishCause, Listeners, MAX_DAMAGE, PlayerChangeSkin, PlayerChat, PlayerDeath, PlayerEmote,
    PlayerExhaust, PlayerGameModeChange, PlayerItemConsume, PlayerJoin, PlayerJump, PlayerKick,
    PlayerMissSwing, PlayerMove, PlayerQuit, PlayerRespawn, PlayerToggleFlight, PlayerToggleGlide,
    PlayerToggleSneak, PlayerToggleSprint, PlayerToggleSwim, PlayerTransfer, PlayerWorldChange,
    PlayerXpChange, Priority, QuitReason, TeleportCause,
};
use crate::item::{Kind as ItemKind, Stack};
use crate::protocol::consts::{InputFlag, PlayerAction as ActionType, TextType};
use crate::protocol::packet::{self as p, Packet};
use crate::protocol::{FlagSet, InputFlags};
use crate::tick::queue::{Input, Outbound};
use crate::tick::tests::{initialised, input, join, new_state, packet, radius, received, settings};
use crate::tick::{Output, inventory};
use crate::world;

/// The attributes a food change sends.
const FOOD_ATTRIBUTES: [&str; 3] = [
    "minecraft:player.hunger",
    "minecraft:player.saturation",
    "minecraft:player.exhaustion",
];
/// The seconds of skin changes the skin budget is checked over.
const SECONDS: usize = 60;
/// Feet in the middle of the spawn block, x 0, y -60, z 0, of both test worlds: the flat default
/// one and the arena ([`add_arena`]).
const SPAWN_FEET: Vec3 = Vec3::new(0.5, -60.0, 0.5);
/// The attributes an xp change sends.
const XP_ATTRIBUTES: [&str; 2] = ["minecraft:player.level", "minecraft:player.experience"];

/// Players 1..=n spawned with `listeners`; session k has runtime ID k.
struct Game {
    state: State,
    receivers: Vec<mpsc::Receiver<Outbound>>,
    seq: u64,
}

impl Game {
    fn new(n: u64, listeners: Listeners) -> Self {
        Self::with(n, listeners, |_| {})
    }

    fn with(n: u64, listeners: Listeners, edit: impl FnOnce(&mut Config)) -> Self {
        let mut game = Self {
            state: new_state(settings(edit), listeners),
            receivers: Vec::new(),
            seq: 10,
        };
        let mut first = Vec::new();
        for s in 1..=n {
            let (j, rx) = join(s, 0);
            game.receivers.push(rx);
            first.extend([j, radius(s, 1, 8)]);
        }
        game.run(first);
        game.run((1..=n).map(|s| initialised(s, 2, s)).collect());
        for s in 1..=n {
            game.take(s);
        }
        game
    }

    fn player(&self, s: u64) -> &Player {
        &self.state.players[&sid(s)]
    }

    /// The last packet of session `s`, if it was closed, and what it got before.
    fn closed(&mut self, s: u64) -> (Vec<Packet>, Option<Packet>) {
        let index = usize::try_from(s - 1).unwrap();
        let (packets, close, _) = received(&mut self.receivers[index]);
        (packets, close)
    }

    /// Sends an auth input of session `s` at its position with `flags`.
    fn flags(&mut self, s: u64, flags: &[InputFlag]) -> Input {
        let set: FlagSet<InputFlag> = flags.iter().copied().collect();
        let auth = p::PlayerAuthInput {
            position: self.player(s).position.to_f32(),
            input_data: InputFlags::from_set(&set),
            ..p::PlayerAuthInput::default()
        };
        self.send(s, Packet::PlayerAuthInput(Box::new(auth)))
    }

    /// Runs `action` as a listener or task would, then one tick.
    fn perform(&mut self, action: impl Action) {
        Context {
            state: &mut self.state,
        }
        .perform(action)
        .unwrap();
        self.idle(1);
    }

    fn idle(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.run(Vec::new());
        }
    }

    fn survival_mut(&mut self, s: u64) -> &mut Survival {
        &mut self.player_mut(s).survival
    }

    fn player_mut(&mut self, s: u64) -> &mut Player {
        self.state.players.get_mut(&sid(s)).unwrap()
    }

    fn run(&mut self, mut inputs: Vec<Input>) {
        crate::tick::tests::tick(&mut self.state, &mut inputs, &mut Output::default());
    }

    fn send(&mut self, s: u64, packet_: Packet) -> Input {
        self.seq += 1;
        input(s, self.seq, packet(packet_))
    }

    fn take(&mut self, s: u64) -> Vec<Packet> {
        let index = usize::try_from(s - 1).unwrap();
        received(&mut self.receivers[index]).0
    }
}

fn sid(s: u64) -> SessionId {
    SessionId::new(s)
}

fn pid(s: u64) -> Id {
    Id::new(sid(s))
}

fn stack(kind: ItemKind, count: u8) -> Stack {
    Stack::new(kind.default_type(), count)
}

/// A bounded record of what listeners saw.
fn record<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(1024)
}

/// Everything recorded since the last call.
fn drain<T>(rx: &mut mpsc::Receiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(x) = rx.try_recv() {
        out.push(x);
    }
    out
}

/// Listeners with `listener` at normal priority.
fn on<E: Event>(listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static) -> Listeners {
    let mut listeners = Listeners::default();
    listeners.push(Priority::Normal, listener);
    listeners
}

/// Every item a player carries, counted.
fn carried(player: &Player) -> u32 {
    let inv = &player.inventory;
    (0..36)
        .filter_map(|i| inv.main_stack(i))
        .chain(inv.armor())
        .chain(std::iter::once(inv.offhand()))
        .map(|s| u32::from(s.count()))
        .sum()
}

/// The items of the dropped items `packets` show, counted.
fn dropped(packets: &[Packet]) -> u32 {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::AddItemActor(a) => Some(u32::from(a.item.stack.count)),
            _ => None,
        })
        .sum()
}

// api: EntityDamageEvent, EntityDamageByEntityEvent
#[test]
fn damage_listeners_see_the_hit_and_cancel_or_change_it() {
    let (tx, mut seen) = record();
    let mut listeners = on(move |e: &mut EntityDamage, _: &mut Context<'_>| {
        tx.try_send((
            e.entity(),
            e.cause(),
            e.source(),
            e.modifier(DamageModifier::Armor),
        ))
        .unwrap();
        if e.base_damage() == 3.0 {
            e.cancel();
        } else if e.base_damage() == 4.0 {
            e.set_base_damage(1.0);
        } else if e.base_damage() == 5.0 {
            e.set_modifier(DamageModifier::Armor, 0.0);
        }
    });
    let (mtx, mut watched) = record();
    listeners.push_monitor(move |e: &EntityDamage, cx: &Context<'_>| {
        let Actor::Player(p) = e.entity() else {
            panic!("a player is hurt");
        };
        mtx.try_send(cx.player(p).unwrap().health()).unwrap();
    });
    let mut game = Game::new(1, listeners);
    let before = game.state.hash();
    assert!(!damage(&mut game.state, sid(1), 3.0, Damage::Contact));
    assert_eq!(game.state.hash(), before, "a cancelled hit changes nothing");
    assert!(drain(&mut watched).is_empty());
    assert!(damage(&mut game.state, sid(1), 4.0, Damage::Contact));
    assert_eq!(game.survival_mut(1).health, 19.0);
    assert_eq!(drain(&mut watched), [19.0]);
    game.idle(10);
    game.survival_mut(1).health = 20.0;
    let chest = stack(ItemKind::DiamondChestplate, 1);
    inventory::set_armor(game.player_mut(1), 1, chest);
    assert!(damage(&mut game.state, sid(1), 5.0, Damage::Contact));
    assert_eq!(
        game.survival_mut(1).health,
        15.0,
        "the armor modifier was set to 0"
    );
    let calls = drain(&mut seen);
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|c| c.0 == Actor::Player(pid(1))
        && c.1 == DamageCause::Contact
        && c.2 == DamageSource::None));
    assert!(calls[2].3 < 0.0, "armor reduces the hit");
}

// api: EntityDamageByEntityEvent, EntityDamageEvent
#[test]
fn a_melee_hit_names_its_attacker_and_its_knockback_can_change() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut EntityDamage, _: &mut Context<'_>| {
        tx.try_send((e.source(), e.knockback())).unwrap();
        e.set_knockback(2.0);
        e.set_vertical_knockback_limit(f64::INFINITY);
    });
    let mut game = Game::new(2, listeners);
    let base = game.player(1).position;
    game.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
    game.idle(1);
    game.take(2);
    attack_player(&mut game.state, sid(1), sid(2));
    assert_eq!(
        drain(&mut seen),
        [(DamageSource::Entity(Actor::Player(pid(1))), 0.4)]
    );
    game.idle(1);
    let motion = game.take(2).into_iter().find_map(|p| match p {
        Packet::SetActorMotion(m) => Some(m.velocity),
        _ => None,
    });
    // The force is 2 and the upward limit (infinite, ignored) stays 0.4.
    assert_eq!(motion.map(|v| (v[1], v[2] > 1.9)), Some((0.4, true)));
}

/// The knockback motion of a melee hit on a player in `game`, sent to the target (session 2).
fn melee_knockback(game: &mut Game) -> Option<[f32; 3]> {
    let base = game.player(1).position;
    game.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
    game.idle(1);
    game.take(2);
    attack_player(&mut game.state, sid(1), sid(2));
    game.idle(1);
    game.take(2).into_iter().find_map(|p| match p {
        Packet::SetActorMotion(m) => Some(m.velocity),
        _ => None,
    })
}

// api: EntityMotionEvent, Living::knockBack
/// A melee hit pushes a player as everything else does: through `EntityMotion`, whose
/// cancelling sends no motion.
#[test]
fn a_melee_knockback_on_a_player_goes_through_entity_motion() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut EntityMotion, _: &mut Context<'_>| {
        tx.try_send(e.entity()).unwrap();
        e.cancel();
    });
    let mut game = Game::new(2, listeners);
    assert_eq!(melee_knockback(&mut game), None, "cancelled");
    assert_eq!(drain(&mut seen), [Actor::Player(pid(2))]);
}

/// A listener's motion replaces the push, cut to the speed bound of every push; one that is not
/// finite is ignored.
#[test]
fn a_motion_listener_replaces_a_player_s_push() {
    for (set, sent) in [
        (Vec3::new(0.0, 0.5, 0.0), [0.0, 0.5, 0.0]),
        (Vec3::new(30.0, 0.5, 0.0), [10.0, 0.5, 0.0]),
    ] {
        let listeners = on(move |e: &mut EntityMotion, _: &mut Context<'_>| {
            e.set_motion(set);
            e.set_motion(Vec3::new(f64::NAN, 0.0, 0.0));
        });
        let mut game = Game::new(2, listeners);
        assert_eq!(melee_knockback(&mut game), Some(sent));
    }
}

/// A melee knockback a listener makes strong is cut to the speed bound of every push.
#[test]
fn a_melee_knockback_on_a_player_is_bounded() {
    let listeners = on(|e: &mut EntityDamage, _: &mut Context<'_>| e.set_knockback(16.0));
    let mut game = Game::new(2, listeners);
    let motion = melee_knockback(&mut game).expect("pushed");
    assert!(motion.iter().all(|c| c.abs() <= 10.0), "{motion:?}");
}

// api: Player::attackEntity, Player::hasFiniteResources
/// The weapon wears after the hit for an attacker with finite resources: a listener that makes
/// the attacker a spectator does not spare it.
#[test]
fn an_attacker_made_a_spectator_by_a_listener_still_wears_its_weapon() {
    let listeners = on(|e: &mut EntityDamage, cx: &mut Context<'_>| {
        if let DamageSource::Entity(Actor::Player(attacker)) = e.source() {
            cx.player_mut(attacker)
                .unwrap()
                .set_game_mode(GameMode::Spectator);
        }
    });
    let mut game = Game::new(2, listeners);
    let base = game.player(1).position;
    game.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
    inventory::set_held(game.player_mut(1), stack(ItemKind::DiamondSword, 1));
    attack_player(&mut game.state, sid(1), sid(2));
    assert!(game.survival_mut(2).health < 20.0, "the hit landed");
    assert_eq!(game.player(1).inventory.held().damage(), 1);
}

/// What one client does in a tick cannot fill another player's outbox and disconnect it: the
/// swings and hit sounds of more attacks than an outbox holds leave the target connected.
#[test]
fn many_attacks_in_one_tick_leave_the_target_connected() {
    let mut game = Game::new(2, Listeners::default());
    let attack = Packet::InventoryTransaction(Box::new(p::InventoryTransaction {
        transaction_data: crate::protocol::InventoryTransactionData::UseItemOnEntity(Box::new(
            crate::protocol::UseItemOnEntityTransactionData {
                target_entity_runtime_id: 2,
                action_type: crate::protocol::consts::UseItemOnEntityAction::Attack.into(),
                ..crate::protocol::UseItemOnEntityTransactionData::default()
            },
        )),
        ..p::InventoryTransaction::default()
    }));
    let inputs = (0..=crate::tick::MAX_OUTBOX)
        .map(|_| game.send(1, attack.clone()))
        .collect();
    game.run(inputs);
    assert!(game.state.players.contains_key(&sid(2)));
    assert_eq!(game.closed(2).1, None);
}

// api: EntityDamageEvent
#[test]
fn a_listener_that_makes_the_victim_creative_cancels_the_hit() {
    let listeners = on(|e: &mut EntityDamage, cx: &mut Context<'_>| {
        let Actor::Player(p) = e.entity() else {
            return;
        };
        cx.player_mut(p).unwrap().set_game_mode(GameMode::Creative);
    });
    let mut game = Game::new(1, listeners);
    assert!(!damage(&mut game.state, sid(1), 5.0, Damage::Contact));
    assert_eq!(game.survival_mut(1).health, 20.0);
    // A self-kill is not spared by a game mode.
    let source = DamageSource::Entity(Actor::Player(pid(1)));
    assert!(damage_by(&mut game.state, sid(1), 30.0, Damage::Suicide, source).is_some());
    assert!(game.survival_mut(1).is_dead());
}

/// A listener that weakens a hit leaves the totem and the absorption the hit no longer needs:
/// the absorption and totem modifiers follow the damage the listeners left.
#[test]
fn a_hit_a_listener_weakens_wastes_neither_a_totem_nor_absorption() {
    let listeners = on(|e: &mut EntityDamage, _: &mut Context<'_>| e.set_base_damage(2.0));
    let mut game = Game::new(1, listeners);
    inventory::set_held(game.player_mut(1), stack(ItemKind::Totem, 1));
    game.survival_mut(1).absorption = 4.0;
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    let s = game.survival_mut(1);
    assert_eq!((s.health, s.absorption), (20.0, 2.0));
    assert!(s.effects.is_empty(), "the totem gave nothing");
    assert_eq!(game.player(1).inventory.held(), &stack(ItemKind::Totem, 1));
}

/// A listener that takes the totem out of the victim's hand leaves nothing to save it.
#[test]
fn a_totem_a_listener_takes_out_of_hand_saves_no_one() {
    let listeners = on(|e: &mut EntityDamage, cx: &mut Context<'_>| {
        if let Actor::Player(p) = e.entity() {
            cx.player_mut(p).unwrap().set_held_slot(1).unwrap();
        }
    });
    let mut game = Game::new(1, listeners);
    inventory::set_held(game.player_mut(1), stack(ItemKind::Totem, 1));
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    assert!(game.survival_mut(1).is_dead());
}

// api: EntityRegainHealthEvent
#[test]
fn regain_listeners_change_or_cancel_the_healing() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut EntityRegainHealth, _: &mut Context<'_>| {
        tx.try_send((e.regain_reason(), e.amount())).unwrap();
        match e.regain_reason() {
            RegainReason::Magic => e.set_amount(5.0),
            RegainReason::Regen => e.cancel(),
            RegainReason::Saturation => {}
        }
    });
    let mut game = Game::with(1, listeners, |c| {
        c.general.difficulty = Difficulty::Peaceful;
    });
    game.survival_mut(1).health = 10.0;
    heal(&mut game.state, sid(1), 1.0);
    assert_eq!(game.survival_mut(1).health, 15.0);
    effects::add(
        &mut game.state,
        sid(1),
        effect::Type::Regeneration,
        effects::Active::new(99, 4),
    );
    game.survival_mut(1).food_timer = 59;
    game.idle(1);
    let calls = drain(&mut seen);
    assert!(calls.contains(&(RegainReason::Magic, 1.0)));
    assert!(calls.contains(&(RegainReason::Regen, 1.0)));
    assert!(calls.contains(&(RegainReason::Saturation, 1.0)));
    // The regeneration was cancelled; the peaceful saturation healed.
    assert_eq!(game.survival_mut(1).health, 16.0);
}

/// Healing a player follows the same rule as an NPC: nothing is asked at full health, and an
/// amount a listener leaves that does not heal cancels it.
#[test]
fn healing_asks_only_below_full_health_and_an_amount_that_does_not_heal_cancels_it() {
    let (tx, mut asked) = record();
    let (watch, mut watched) = record();
    let mut listeners = on(move |e: &mut EntityRegainHealth, _: &mut Context<'_>| {
        tx.try_send(e.amount()).unwrap();
        e.set_amount(0.0);
    });
    listeners.push_monitor(move |e: &EntityRegainHealth, _: &Context<'_>| {
        watch.try_send(e.amount()).unwrap();
    });
    let mut game = Game::new(1, listeners);
    heal(&mut game.state, sid(1), 4.0);
    assert!(drain(&mut asked).is_empty(), "full health");
    game.survival_mut(1).health = 10.0;
    heal(&mut game.state, sid(1), 4.0);
    assert_eq!(drain(&mut asked), [4.0]);
    assert!(drain(&mut watched).is_empty(), "cancelled");
    assert_eq!(game.survival_mut(1).health, 10.0);
}

/// A full bar on peaceful heals twice in one tick, each a regain of its own, then exhausts.
#[test]
fn peaceful_and_full_bar_heals_are_regains_of_their_own() {
    let (tx, mut seen) = record();
    let exhausted = tx.clone();
    let mut listeners = on(move |e: &mut EntityRegainHealth, _: &mut Context<'_>| {
        tx.try_send(format!("regain {}", e.amount())).unwrap();
    });
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerExhaust, _: &mut Context<'_>| {
            exhausted
                .try_send(format!("exhaust {}", e.amount()))
                .unwrap();
        },
    );
    let mut game = Game::with(1, listeners, |c| {
        c.general.difficulty = Difficulty::Peaceful;
    });
    game.survival_mut(1).health = 10.0;
    game.survival_mut(1).food_timer = 79;
    game.idle(1);
    assert_eq!(drain(&mut seen), ["regain 1", "regain 1", "exhaust 6"]);
    assert_eq!(game.survival_mut(1).health, 12.0);
}

/// A creative player that dies drops nothing but still loses what it carries, but for the items
/// kept on death.
#[test]
fn a_creative_player_that_dies_loses_its_inventory_and_drops_nothing() {
    let mut game = Game::new(1, Listeners::default());
    game.player_mut(1).set_game_mode(GameMode::Creative);
    assert!(inventory::give(game.player_mut(1), stack(ItemKind::Bread, 5)).is_empty());
    assert!(damage(&mut game.state, sid(1), 30.0, Damage::Suicide));
    assert!(game.survival_mut(1).is_dead());
    assert_eq!(carried(game.player(1)), 0);
    game.idle(1);
    assert_eq!(dropped(&game.take(1)), 0);
}

// api: PlayerDeathEvent
#[test]
fn death_listeners_choose_the_drops_the_xp_and_the_messages() {
    let listeners = on(|e: &mut PlayerDeath, _: &mut Context<'_>| {
        assert_eq!(e.drops().len(), 2);
        assert_eq!(e.xp_drop_amount(), 70);
        e.set_drops(vec![stack(ItemKind::Apple, 3)]);
        e.set_keep_xp(true);
        e.set_death_message("player1 fell".to_owned());
        e.set_death_screen_message("you fell".to_owned());
    });
    let mut game = Game::new(2, listeners);
    let player = game.player_mut(1);
    inventory::give(player, stack(ItemKind::Bread, 5));
    inventory::give(player, stack(ItemKind::Bowl, 2));
    game.survival_mut(1).xp_level = 10;
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    assert_eq!(carried(game.player(1)), 0, "the replaced stacks are gone");
    assert_eq!(game.survival_mut(1).xp_level, 10, "the xp is kept");
    game.idle(1);
    let own = game.take(1);
    assert_eq!(dropped(&own), 3, "only the listener's drops fall");
    assert!(
        own.iter()
            .any(|p| matches!(p, Packet::DeathInfo(d) if d.cause == "you fell"))
    );
    assert!(game.take(2).iter().any(|p| matches!(
        p,
        Packet::Text(t) if t.message == "player1 fell" && t.text_type == u8::from(TextType::Raw)
    )));
}

/// `xp-orbs = false`: the death offers no xp to drop, but the level is lost.
#[test]
fn without_orbs_a_death_offers_no_xp_to_drop() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut PlayerDeath, _: &mut Context<'_>| {
        tx.try_send(e.xp_drop_amount()).unwrap();
    });
    let mut game = Game::with(1, listeners, |c| c.general.xp_orbs = false);
    game.survival_mut(1).xp_level = 10;
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    assert_eq!(drain(&mut seen), [0]);
    assert_eq!(game.survival_mut(1).xp_level, 0);
}

// api: PlayerDeathEvent::getDeathMessage, PlayerDeathEvent::setDeathMessage
/// With `show-death-messages` every player sees the death's message, which the client translates
/// with the dead player's name as on the death screen; a listener's own message replaces it and an
/// empty one hides it. Without, only a listener's own message shows.
#[test]
fn show_death_messages_broadcasts_the_translated_death_message() {
    let deaths = |show: bool, listeners: Listeners| -> Vec<(String, Vec<String>, bool)> {
        let mut game = Game::with(2, listeners, |c| c.general.show_death_messages = show);
        assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
        game.idle(1);
        game.take(2)
            .into_iter()
            .filter_map(|p| match p {
                Packet::Text(t) => Some((t.message, t.parameters, t.needs_translation)),
                _ => None,
            })
            .collect()
    };
    let translated = vec![(
        "death.attack.magic".to_owned(),
        vec!["player1".to_owned()],
        true,
    )];
    assert_eq!(deaths(true, Listeners::default()), translated);
    let kept = on(|e: &mut PlayerDeath, _: &mut Context<'_>| {
        assert_eq!(e.death_message(), "death.attack.magic");
    });
    assert_eq!(deaths(true, kept), translated);
    assert_eq!(deaths(false, Listeners::default()), []);
    let hidden = on(|e: &mut PlayerDeath, _: &mut Context<'_>| {
        e.set_death_message(String::new());
    });
    assert_eq!(deaths(true, hidden), []);
    let own = || {
        on(|e: &mut PlayerDeath, _: &mut Context<'_>| {
            e.set_death_message("gone".to_owned());
        })
    };
    let raw = vec![("gone".to_owned(), Vec::new(), false)];
    assert_eq!(deaths(true, own()), raw);
    assert_eq!(deaths(false, own()), raw);
}

// api: PlayerDeathEvent
#[test]
fn deaths_with_random_listeners_neither_duplicate_nor_lose_items() {
    let mut r = crate::random::Random::new(7, 0, 1);
    for _ in 0..12 {
        let keep = r.range(0, 1) == 1;
        let reverse = r.range(0, 1) == 1;
        let (mtx, mut watched) = record();
        let mut listeners = on(move |e: &mut PlayerDeath, _: &mut Context<'_>| {
            e.set_keep_inventory(keep);
            if reverse {
                let mut drops = e.drops().to_vec();
                drops.reverse();
                e.set_drops(drops);
            }
        });
        listeners.push_monitor(move |e: &PlayerDeath, cx: &Context<'_>| {
            mtx.try_send(cx.player(e.player()).unwrap().is_alive())
                .unwrap();
        });
        let mut game = Game::new(1, listeners);
        let count = u8::try_from(r.range(1, 20)).unwrap();
        for kind in [ItemKind::Bread, ItemKind::Apple, ItemKind::Bowl] {
            inventory::give(game.player_mut(1), stack(kind, count));
        }
        let before = carried(game.player(1));
        assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
        game.idle(1);
        let on_ground = dropped(&game.take(1));
        let left = carried(game.player(1));
        assert_eq!(left + on_ground, before, "keep {keep}, reverse {reverse}");
        assert_eq!(left == before, keep);
        assert_eq!(
            drain(&mut watched),
            [false],
            "the monitor sees the dead player"
        );
    }
}

/// Drops a listener replaced stand only for the stacks the event showed: what else the player
/// carries when it dies (here, what the listener gave it) still drops, so no item is lost.
#[test]
fn replaced_death_drops_lose_nothing_the_event_did_not_show() {
    let listeners = on(|e: &mut PlayerDeath, cx: &mut Context<'_>| {
        e.set_drops(Vec::new());
        let apples = stack(ItemKind::Apple, 3);
        let mut main = cx
            .player_mut(e.player())
            .unwrap()
            .inventory_mut(crate::inventory::Kind::Main);
        assert!(main.add(&apples).is_empty());
    });
    let mut game = Game::new(1, listeners);
    inventory::give(game.player_mut(1), stack(ItemKind::Bread, 5));
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    assert_eq!(carried(game.player(1)), 0);
    game.idle(1);
    assert_eq!(
        dropped(&game.take(1)),
        3,
        "the apples the event did not show drop"
    );
}

/// The death closes the window first, so what the player holds on the cursor is back in its
/// inventory when the event shows the drops: reordered drops lose none of it.
#[test]
fn reordered_death_drops_keep_the_cursor_dropping() {
    let listeners = on(|e: &mut PlayerDeath, _: &mut Context<'_>| {
        let mut drops = e.drops().to_vec();
        drops.reverse();
        e.set_drops(drops);
    });
    let mut game = Game::new(1, listeners);
    inventory::give(game.player_mut(1), stack(ItemKind::Bread, 5));
    let cursor = stack(ItemKind::Apple, 3);
    inventory::set_cursor(game.player_mut(1), cursor);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    game.idle(1);
    assert_eq!(dropped(&game.take(1)), 5 + 3);
}

// api: PlayerRespawnEvent, PlayerGameModeChangeEvent
#[test]
fn respawn_listeners_move_the_respawn_and_hardcore_asks_for_spectator() {
    let target = Vec3::new(4.5, -60.0, 4.5);
    let mut listeners = on(move |e: &mut PlayerRespawn, _: &mut Context<'_>| {
        assert_eq!(e.world(), world::Id::DEFAULT);
        e.set_position(e.world(), target);
    });
    listeners.push(
        Priority::Normal,
        |e: &mut PlayerGameModeChange, _: &mut Context<'_>| {
            assert_eq!(e.new_game_mode(), GameMode::Spectator);
            e.cancel();
        },
    );
    let mut game = Game::with(1, listeners, |c| {
        c.general.hardcore = true;
        c.general.difficulty = Difficulty::Hard;
    });
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    assert_eq!(game.player(1).position(), target);
    assert_eq!(game.player(1).game_mode(), GameMode::Survival);
    // A world the server does not run is ignored.
    let listeners = on(|e: &mut PlayerRespawn, _: &mut Context<'_>| {
        e.set_position(world::Id::new(9).unwrap(), Vec3::ZERO);
    });
    let mut game = Game::new(1, listeners);
    let spawn = game.player(1).position;
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    assert_eq!(game.player(1).position, spawn);
}

/// A respawn listener's change of what the respawn packets do not carry reaches the client.
#[test]
fn a_respawn_listener_turns_off_auto_jump() {
    let listeners = on(|e: &mut PlayerRespawn, cx: &mut Context<'_>| {
        cx.player_mut(e.player()).unwrap().set_auto_jump(false);
    });
    let mut game = Game::new(1, listeners);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    game.idle(1);
    game.take(1);
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    game.idle(1);
    let sent = game.take(1);
    assert!(
        sent.iter()
            .any(|p| matches!(p, Packet::UpdateAdventureSettings(a) if !a.auto_jump)),
        "auto jump stays on"
    );
}

/// A player that died swimming or gliding respawns doing neither: while dead its client's stop
/// flags are not read, so they would stay on (a small box, the flags shown to viewers).
#[test]
fn a_respawned_player_neither_swims_nor_glides() {
    let mut game = Game::new(1, Listeners::default());
    game.player_mut(1).set_swimming(true);
    game.player_mut(1).set_gliding(true);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    let motion = &game.survival_mut(1).motion;
    assert!(!motion.swimming && !motion.gliding);
}

/// Adds the empty, loaded world "arena", the second one; returns its ID.
fn add_arena(game: &mut Game) -> world::Id {
    game.state.add_world(world::World::new(
        "arena",
        world::Kind::Temporary,
        0,
        BlockPos::new(0, -60, 0),
    ));
    world::Id::new(1).unwrap()
}

/// A world unloaded since is ignored too: the player respawns at its respawn point.
#[test]
fn a_respawn_listener_cannot_send_the_player_to_an_unloaded_world() {
    let arena = world::Id::new(1).unwrap();
    let listeners = on(move |e: &mut PlayerRespawn, _: &mut Context<'_>| {
        e.set_position(arena, SPAWN_FEET);
    });
    let mut game = Game::new(1, listeners);
    add_arena(&mut game);
    Context {
        state: &mut game.state,
    }
    .force_unload_world(arena)
    .unwrap();
    game.idle(1);
    let spawn = game.player(1).position;
    game.player_mut(1).position = spawn + Vec3::new(20.0, 0.0, 0.0);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    assert_eq!(game.player(1).world, 0);
    assert_eq!(game.player(1).position, spawn);
}

// api: PlayerRespawnEvent
/// The monitors see where the player respawns, not a place a listener chose that was ignored.
#[test]
fn respawn_monitors_see_the_place_used_not_an_ignored_one() {
    let mut listeners = on(|e: &mut PlayerRespawn, _: &mut Context<'_>| {
        e.set_position(world::Id::new(9).unwrap(), Vec3::ZERO);
    });
    let (tx, mut watched) = record();
    listeners.push_monitor(move |e: &PlayerRespawn, _: &Context<'_>| {
        tx.try_send((e.world(), e.position())).unwrap();
    });
    let mut game = Game::new(1, listeners);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    let feet = game.player(1).position();
    assert_eq!(drain(&mut watched), [(world::Id::DEFAULT, feet)]);
}

/// A respawn point whose middle lies outside the world border counts as none: the player
/// respawns at the default world's spawn instead of staying where it died.
#[test]
fn a_respawn_point_outside_the_border_respawns_at_the_world_spawn() {
    let mut game = Game::new(1, Listeners::default());
    let spawn = game.player(1).position;
    let name = game.state.worlds[0].name().to_owned();
    game.survival_mut(1).spawn = Some(Point {
        world: name,
        pos: BlockPos::new(world::BORDER, -60, 0),
    });
    game.player_mut(1).position = spawn + Vec3::new(20.0, 0.0, 0.0);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    assert_eq!(game.player(1).position, spawn);
}

// api: PlayerGameModeChangeEvent, Player::setGamemode
#[test]
fn the_game_mode_action_asks_and_the_direct_call_does_not() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut PlayerGameModeChange, _: &mut Context<'_>| {
        tx.try_send(e.new_game_mode()).unwrap();
        if e.new_game_mode() == GameMode::Adventure {
            e.cancel();
        }
    });
    let mut game = Game::new(1, listeners);
    game.perform(action::SetGameMode::new(pid(1), GameMode::Adventure));
    assert_eq!(game.player(1).game_mode(), GameMode::Survival);
    game.perform(action::SetGameMode::new(pid(1), GameMode::Creative));
    assert_eq!(game.player(1).game_mode(), GameMode::Creative);
    game.player_mut(1).set_game_mode(GameMode::Spectator);
    assert_eq!(
        drain(&mut seen),
        [GameMode::Adventure, GameMode::Creative],
        "the direct call raised nothing"
    );
}

// api: PlayerMoveEvent
#[test]
fn move_listeners_send_the_player_back_or_elsewhere() {
    let elsewhere = Vec3::new(2.5, -60.0, 2.5);
    let (mtx, mut watched) = record();
    let mut listeners = on(move |e: &mut PlayerMove, _: &mut Context<'_>| {
        if e.target().x > e.origin().x + 1.5 {
            e.cancel();
        } else if e.target().x > e.origin().x {
            e.set_target(elsewhere);
        }
    });
    listeners.push_monitor(move |e: &PlayerMove, cx: &Context<'_>| {
        mtx.try_send(cx.player(e.player()).unwrap().position())
            .unwrap();
    });
    let mut game = Game::new(1, listeners);
    let start = game.player(1).position;
    let far = p::PlayerAuthInput {
        position: (start + Vec3::new(2.0, 0.0, 0.0)).to_f32(),
        ..p::PlayerAuthInput::default()
    };
    let input = game.send(1, Packet::PlayerAuthInput(Box::new(far)));
    game.run(vec![input]);
    assert_eq!(
        game.player(1).position,
        start,
        "cancelled: back to the start"
    );
    assert!(game.take(1).iter().any(|p| matches!(
        p,
        Packet::MovePlayer(m) if m.mode == u8::from(MoveMode::Reset)
    )));
    game.survival_mut(1).motion.force_sync = false;
    let near = p::PlayerAuthInput {
        position: (start + Vec3::new(1.0, 0.0, 0.0)).to_f32(),
        tick: 1,
        ..p::PlayerAuthInput::default()
    };
    let input = game.send(1, Packet::PlayerAuthInput(Box::new(near)));
    game.run(vec![input]);
    assert_eq!(game.player(1).position(), elsewhere);
    assert_eq!(drain(&mut watched), [elsewhere]);
}

// api: PlayerMoveEvent::setTo
/// A move a listener sends past the world border sends the player back, as a cancel does: the
/// monitors hear nothing, and the next move is the one asked about.
#[test]
fn a_move_sent_past_the_world_border_sends_the_player_back() {
    let (mtx, mut watched) = record();
    let mut listeners = on(move |e: &mut PlayerMove, _: &mut Context<'_>| {
        e.set_target(Vec3::new(4.0e7, e.target().y, e.target().z));
    });
    listeners.push_monitor(move |e: &PlayerMove, _: &Context<'_>| {
        mtx.try_send(e.target()).unwrap();
    });
    let mut game = Game::new(1, listeners);
    let start = game.player(1).position;
    let moved = p::PlayerAuthInput {
        position: (start + Vec3::new(1.0, 0.0, 0.0)).to_f32(),
        ..p::PlayerAuthInput::default()
    };
    let input = game.send(1, Packet::PlayerAuthInput(Box::new(moved)));
    game.run(vec![input]);
    assert_eq!(game.player(1).position, start, "back to the start");
    assert!(game.take(1).iter().any(|p| matches!(
        p,
        Packet::MovePlayer(m) if m.mode == u8::from(MoveMode::Reset)
    )));
    assert!(drain(&mut watched).is_empty(), "no monitor");
}

/// Listeners of the movement toggles that record their names in `tx`: sneaking, swimming,
/// flying and missed swings are cancelled.
fn toggle_listeners(tx: mpsc::Sender<&'static str>) -> Listeners {
    let mut listeners = Listeners::default();
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerToggleSneak, _: &mut Context<'_>| {
            t.try_send("sneak").unwrap();
            assert!(e.is_sneaking() && e.is_sneak_pressed());
            e.cancel();
        },
    );
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerToggleSprint, _: &mut Context<'_>| {
            t.try_send("sprint").unwrap();
            assert!(e.is_sprinting());
        },
    );
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerToggleSwim, _: &mut Context<'_>| {
            t.try_send("swim").unwrap();
            assert!(e.is_swimming());
            e.cancel();
        },
    );
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerToggleGlide, _: &mut Context<'_>| {
            t.try_send("glide").unwrap();
            assert!(e.is_gliding());
        },
    );
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerToggleFlight, _: &mut Context<'_>| {
            t.try_send("fly").unwrap();
            assert!(e.is_flying());
            e.cancel();
        },
    );
    let t = tx.clone();
    listeners.push(
        Priority::Normal,
        move |_: &mut PlayerJump, _: &mut Context<'_>| {
            t.try_send("jump").unwrap();
        },
    );
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerMissSwing, _: &mut Context<'_>| {
            tx.try_send("miss").unwrap();
            e.cancel();
        },
    );
    listeners
}

/// The client starting or stopping flight forgets the fall.
#[test]
fn a_client_flight_toggle_forgets_the_fall() {
    let mut game = Game::new(1, Listeners::default());
    game.player_mut(1).set_can_fly(true);
    game.player_mut(1).set_flying(true);
    game.idle(1);
    // In the air: no landing ends the fall instead.
    game.player_mut(1).position += Vec3::new(0.0, 10.0, 0.0);
    game.survival_mut(1).fall_distance = 20.0;
    let input = game.flags(1, &[InputFlag::StopFlying]);
    game.run(vec![input]);
    assert!(!game.survival_mut(1).motion.flying);
    // What is left is the input's own rounding to f32.
    assert!(game.survival_mut(1).fall_distance < 0.001);
}

// api: PlayerToggleSneakEvent, PlayerToggleSprintEvent, PlayerToggleSwimEvent,
// api: PlayerToggleGlideEvent, PlayerToggleFlightEvent, PlayerJumpEvent, PlayerMissSwingEvent
#[test]
fn toggles_ask_their_events_and_a_refused_one_resends_the_data() {
    let (tx, mut seen) = record();
    let mut game = Game::new(2, toggle_listeners(tx));
    game.player_mut(1).set_can_fly(true);
    game.idle(1);
    game.take(1);
    let input = game.flags(
        1,
        &[
            InputFlag::Sneaking,
            InputFlag::StartSneaking,
            InputFlag::StartSprinting,
            InputFlag::StartSwimming,
            InputFlag::StartGliding,
            InputFlag::StartFlying,
            InputFlag::StartJumping,
            InputFlag::MissedSwing,
        ],
    );
    game.take(2);
    game.run(vec![input]);
    assert_eq!(
        drain(&mut seen),
        ["sneak", "sprint", "swim", "glide", "fly", "jump", "miss"]
    );
    let m = &game.player(1).survival.motion;
    assert!(!m.sneaking && m.sneak_pressed && m.sprinting && !m.swimming && m.gliding);
    assert!(!m.flying);
    // The refusals resent the player's whole entity data; the cancelled miss swung nothing.
    assert!(
        game.take(1)
            .iter()
            .any(|p| matches!(p, Packet::SetActorData(d) if d.entity_runtime_id == 1))
    );
    assert!(!game.take(2).iter().any(|p| matches!(
        p,
        Packet::ActorEvent(e) if e.event_type == u8::from(crate::protocol::consts::ActorEvent::StartAttacking)
    )));
    // The older `PlayerAction` toggles ask too.
    let action = p::PlayerAction {
        entity_runtime_id: 1,
        action_type: i32::from(ActionType::StartSneak),
        ..p::PlayerAction::default()
    };
    let input = game.send(1, Packet::PlayerAction(action));
    game.run(vec![input]);
    assert_eq!(drain(&mut seen), ["sneak"]);
    assert!(!game.player(1).survival.motion.sneaking);
}

/// The answer to a sneak carries the tick of the input that toggled it, so the client applies it
/// at that frame: one that came after the client stopped sneaking turned it on again. The viewers'
/// copy carries none (P6 `sneak`).
#[test]
fn a_sneak_answer_carries_its_input_tick() {
    let mut game = Game::new(2, Listeners::default());
    let sneak = |game: &mut Game, flags: &[InputFlag], tick| {
        let set: FlagSet<InputFlag> = flags.iter().copied().collect();
        let auth = p::PlayerAuthInput {
            position: game.player(1).position.to_f32(),
            input_data: InputFlags::from_set(&set),
            tick,
            ..p::PlayerAuthInput::default()
        };
        let input = game.send(1, Packet::PlayerAuthInput(Box::new(auth)));
        game.run(vec![input]);
    };
    let answer_ticks = |packets: Vec<Packet>| -> Vec<u64> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::SetActorData(d) if d.entity_runtime_id == 1 => Some(d.tick),
                _ => None,
            })
            .collect()
    };
    sneak(
        &mut game,
        &[InputFlag::Sneaking, InputFlag::StartSneaking],
        55,
    );
    assert!(game.player(1).survival.motion.sneaking);
    assert_eq!(answer_ticks(game.take(1)), [55]);
    assert_eq!(answer_ticks(game.take(2)), [0]);
    sneak(&mut game, &[InputFlag::StopSneaking], 56);
    assert_eq!(answer_ticks(game.take(1)), [56]);
    // Entity data the input did not toggle has no input tick.
    game.player_mut(1).survival.fire_ticks = 20;
    game.idle(1);
    assert_eq!(answer_ticks(game.take(1)), [0]);
}

/// A sneak's box goes in the height (54), as the official server sends it; the collision box
/// (130) stays the standing one (P6 `sneak`). Sent as the collision box, a sneak made the client
/// crouch by itself when a jump met a ceiling.
#[test]
fn a_sneak_answer_sends_the_height_not_the_collision_box() {
    use crate::protocol::MetadataValue;
    use crate::protocol::consts::EntityDataKey;
    let mut game = Game::new(1, Listeners::default());
    let heights = |game: &mut Game, flags: &[InputFlag]| -> Vec<Option<MetadataValue>> {
        let input = game.flags(1, flags);
        game.run(vec![input]);
        game.take(1)
            .into_iter()
            .filter_map(|p| match p {
                Packet::SetActorData(d) if d.entity_runtime_id == 1 => {
                    assert!(
                        !d.entity_metadata
                            .contains_key(&(EntityDataKey::CollisionBox as u32))
                    );
                    Some(
                        d.entity_metadata
                            .get(&(EntityDataKey::Height as u32))
                            .cloned(),
                    )
                }
                _ => None,
            })
            .collect()
    };
    game.take(1);
    assert_eq!(
        heights(&mut game, &[InputFlag::Sneaking, InputFlag::StartSneaking]),
        [Some(MetadataValue::Float32(1.49))]
    );
    assert_eq!(
        heights(&mut game, &[InputFlag::StopSneaking]),
        [Some(MetadataValue::Float32(1.8))]
    );
}

/// A player that dies sneaking keeps the dying box its viewers got: the flush at the end of the
/// tick does not send the pose's box over it.
#[test]
fn a_dead_players_box_stays_the_dying_box() {
    use crate::protocol::MetadataValue;
    use crate::protocol::consts::EntityDataKey;
    let mut game = Game::new(2, Listeners::default());
    game.player_mut(1).set_sneaking(true);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    game.idle(2);
    let heights: Vec<MetadataValue> = game
        .take(2)
        .into_iter()
        .filter_map(|p| match p {
            Packet::SetActorData(d) if d.entity_runtime_id == 1 => d
                .entity_metadata
                .get(&(EntityDataKey::Height as u32))
                .cloned(),
            _ => None,
        })
        .collect();
    assert_eq!(heights, [MetadataValue::Float32(0.2)]);
}

// api: PlayerExhaustEvent
#[test]
fn exhaust_listeners_change_or_cancel_the_exhaustion() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut PlayerExhaust, _: &mut Context<'_>| {
        tx.try_send((e.player(), e.cause(), e.amount())).unwrap();
        match e.cause() {
            ExhaustCause::SprintJumping => e.set_amount(4.0),
            ExhaustCause::Mining => e.cancel(),
            _ => {}
        }
    });
    let mut game = Game::new(1, listeners);
    let input = game.flags(1, &[InputFlag::StartSprinting, InputFlag::StartJumping]);
    game.run(vec![input]);
    let s = game.survival_mut(1);
    assert_eq!(
        (s.saturation, s.exhaustion),
        (4.0, 0.0),
        "4 cost a saturation point"
    );
    exhaust_by(&mut game.state, sid(1), 3.0, ExhaustCause::Mining);
    assert_eq!(game.survival_mut(1).exhaustion, 0.0);
    let calls = drain(&mut seen);
    assert_eq!(calls[0], (pid(1), ExhaustCause::SprintJumping, 0.2));
    assert_eq!(calls[1], (pid(1), ExhaustCause::Mining, 3.0));
}

// api: PlayerExperienceChangeEvent
#[test]
fn xp_listeners_choose_the_level_or_cancel_the_gain() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut PlayerXpChange, _: &mut Context<'_>| {
        tx.try_send((e.old_level(), e.new_level())).unwrap();
        if e.new_level() == Some(0) {
            e.cancel();
        } else {
            e.set_new_level(Some(30));
            e.set_new_progress(Some(1.0));
        }
    });
    let mut game = Game::new(1, listeners);
    assert!(pickup_xp(&mut game.state, sid(1), 3));
    assert_eq!(game.survival_mut(1).xp_lifetime, 0, "cancelled");
    game.idle(3);
    assert!(pickup_xp(&mut game.state, sid(1), 20));
    let s = game.survival_mut(1);
    assert_eq!(s.xp_level, 30);
    assert!(s.xp_progress < 1.0);
    assert_eq!(s.xp_lifetime, 20);
    assert_eq!(drain(&mut seen), [(0, Some(0)), (0, Some(2))]);
}

// api: PlayerItemConsumeEvent
#[test]
fn consume_listeners_cancel_the_meal_or_replace_its_residue() {
    let mut calls = 0;
    let listeners = on(move |e: &mut PlayerItemConsume, _: &mut Context<'_>| {
        assert_eq!(e.residue(), [stack(ItemKind::Bowl, 1)]);
        assert_eq!(e.item(), &stack(ItemKind::MushroomStew, 1));
        calls += 1;
        if calls == 1 {
            e.cancel();
        } else {
            e.set_residue(vec![stack(ItemKind::Bowl, 1), stack(ItemKind::Apple, 200)]);
        }
    });
    let mut game = Game::new(1, listeners);
    game.survival_mut(1).food = 10.0;
    inventory::set_held(game.player_mut(1), stack(ItemKind::MushroomStew, 1));
    let eat = |game: &mut Game| {
        assert!(start_eating(&mut game.state, sid(1)));
        game.idle(32);
        assert!(finish_eating(&mut game.state, sid(1)));
    };
    eat(&mut game);
    assert_eq!(game.survival_mut(1).food, 10.0, "cancelled");
    assert_eq!(
        game.player(1).inventory.held(),
        &stack(ItemKind::MushroomStew, 1)
    );
    eat(&mut game);
    assert!(game.survival_mut(1).food > 10.0);
    let player = game.player(1);
    assert_eq!(
        player.inventory.held().item(),
        Some(ItemKind::Bowl.default_type())
    );
    // The listener's 200 apples are one stack of at most 64.
    assert_eq!(carried(player), 1 + 64);
}

// api: EntityEffectAddEvent, EntityEffectRemoveEvent
#[test]
fn effect_listeners_refuse_or_keep_effects_but_not_expired_ones() {
    let mut listeners = on(|e: &mut EntityEffectAdd, _: &mut Context<'_>| {
        if e.effect().effect() == effect::Type::Speed {
            e.cancel();
        }
    });
    let (tx, mut seen) = record();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityEffectRemove, _: &mut Context<'_>| {
            tx.try_send(e.effect().duration_ticks()).unwrap();
            e.cancel();
        },
    );
    let mut game = Game::new(1, listeners);
    let speed = effect::Instance::new(effect::Type::Speed, 100, 0, true, false);
    assert!(!add_effect(&mut game.state, sid(1), speed));
    assert!(!game.player(1).has_effect(effect::Type::Speed));
    let haste = effect::Instance::new(effect::Type::Haste, 2, 0, true, false);
    assert!(add_effect(&mut game.state, sid(1), haste));
    effects::remove(&mut game.state, sid(1), effect::Type::Haste);
    assert!(game.player(1).has_effect(effect::Type::Haste), "kept");
    game.idle(3);
    assert!(!game.player(1).has_effect(effect::Type::Haste), "ran out");
    assert_eq!(drain(&mut seen), [2, 0]);
}

// api: EntityEffectRemoveEvent
#[test]
fn an_effect_a_listener_gives_in_place_of_a_removed_one_stays() {
    let listeners = on(|_: &mut EntityEffectRemove, cx: &mut Context<'_>| {
        let given = effect::Type::Speed
            .instance()
            .with_duration_ticks(200)
            .with_amplifier(1);
        cx.player_mut(pid(1)).unwrap().add_effect(given);
    });
    let mut game = Game::new(1, listeners);
    effects::add(
        &mut game.state,
        sid(1),
        effect::Type::Speed,
        effects::Active::new(100, 0),
    );
    effects::remove(&mut game.state, sid(1), effect::Type::Speed);
    let kept = game
        .survival_mut(1)
        .effects
        .get(&effect::Type::Speed)
        .copied();
    assert_eq!(kept.map(|a| (a.duration, a.amplifier)), Some((200, 1)));
}

// api: EntityCombustEvent, EntityCombustByEntityEvent, EntityExtinguishEvent
#[test]
fn combust_listeners_change_the_burn_and_extinguish_listeners_hear_it_end() {
    let mut listeners = on(
        |e: &mut EntityCombust, _: &mut Context<'_>| match e.source() {
            CombustSource::Entity(_) => e.set_duration_secs(1),
            CombustSource::Block(_) => e.cancel(),
        },
    );
    let (tx, mut seen) = record();
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityExtinguish, _: &mut Context<'_>| {
            tx.try_send(e.cause()).unwrap();
        },
    );
    let mut game = Game::new(1, listeners);
    let fire = CombustSource::Block(BlockPos::new(0, -60, 0));
    set_on_fire_by(&mut game.state, sid(1), 8, fire);
    assert_eq!(game.survival_mut(1).fire_ticks, 0, "cancelled");
    let by = CombustSource::Entity(Actor::Player(pid(1)));
    set_on_fire_by(&mut game.state, sid(1), 8, by);
    assert_eq!(game.survival_mut(1).fire_ticks, 20);
    game.idle(20);
    assert_eq!(game.survival_mut(1).fire_ticks, 0);
    assert_eq!(drain(&mut seen), [ExtinguishCause::Ticking]);
}

/// A listener that sets the burn to 0 seconds leaves nothing burning: the monitors hear no
/// combustion.
#[test]
fn a_burn_of_no_seconds_is_not_heard_by_monitors() {
    let (tx, mut watched) = record();
    let mut listeners = on(|e: &mut EntityCombust, _: &mut Context<'_>| e.set_duration_secs(0));
    listeners.push_monitor(move |e: &EntityCombust, _: &Context<'_>| {
        tx.try_send(e.duration_secs()).unwrap();
    });
    let mut game = Game::new(1, listeners);
    let by = CombustSource::Entity(Actor::Player(pid(1)));
    set_on_fire_by(&mut game.state, sid(1), 8, by);
    assert_eq!(game.survival_mut(1).fire_ticks, 0);
    assert!(drain(&mut watched).is_empty());
}

/// A chat packet of session `s`.
fn chat(game: &mut Game, s: u64, message: &str) -> Input {
    let text = p::Text {
        text_type: TextType::Chat.into(),
        message: message.to_owned(),
        ..p::Text::default()
    };
    game.send(s, Packet::Text(Box::new(text)))
}

/// The texts of the `Text` packets in `packets`.
fn texts(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::Text(t) => Some(t.message.clone()),
            _ => None,
        })
        .collect()
}

// api: PlayerChatEvent, Player::chat
#[test]
fn chat_listeners_change_the_line_its_format_and_recipients() {
    let listeners = on(|e: &mut PlayerChat, _: &mut Context<'_>| {
        assert_eq!(e.recipients().len(), 2);
        match e.message() {
            "secret" => e.cancel(),
            "whisper" => {
                e.set_recipients(vec![e.player(), e.player()]);
            }
            _ => {
                let loud = e.message().to_uppercase();
                e.set_message(loud);
                e.set_format(Some("[{%0}] {%1} {%2".to_owned()));
            }
        }
    });
    let mut game = Game::new(2, listeners);
    let inputs = vec![chat(&mut game, 1, "secret"), chat(&mut game, 1, "hi")];
    game.run(inputs);
    assert_eq!(texts(&game.take(2)), ["[player1] HI {%2"]);
    game.take(1);
    let input = chat(&mut game, 1, "whisper");
    game.run(vec![input]);
    assert!(texts(&game.take(2)).is_empty());
    assert_eq!(texts(&game.take(1)), ["whisper"], "once, to the speaker");
    // A line starting with `/` is the player's command; its answer goes to it alone.
    game.perform(action::Chat::new(pid(1), "/nosuchcommand"));
    assert!(texts(&game.take(2)).is_empty());
    let own = texts(&game.take(1));
    assert_eq!(own, ["commands.generic.unknown"]);
}

// api: PlayerKickEvent, PlayerQuitEvent, Player::kick
#[test]
fn kick_listeners_keep_the_player_or_change_the_messages_and_quit_follows() {
    let mut listeners = on(|e: &mut PlayerKick, _: &mut Context<'_>| {
        if e.kick_reason() == "stay" {
            e.cancel();
        } else {
            e.set_quit_message("player1 was kicked".to_owned());
            e.set_kick_screen_message(Some("bye".to_owned()));
        }
    });
    let (tx, mut seen) = record();
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerQuit, _: &mut Context<'_>| {
            tx.try_send((e.player(), e.reason().clone(), e.quit_message().to_owned()))
                .unwrap();
        },
    );
    let mut game = Game::new(2, listeners);
    game.perform(action::Kick::new(pid(1), "stay"));
    assert!(game.player(1).is_online());
    game.perform(action::Kick::new(pid(1), "rules").with_quit_message("unused"));
    let (_, close) = game.closed(1);
    assert!(matches!(close, Some(Packet::Disconnect(d)) if d.message == "bye"));
    assert!(!game.state.players.contains_key(&sid(1)));
    assert_eq!(texts(&game.take(2)), ["player1 was kicked"]);
    assert_eq!(
        drain(&mut seen),
        [(
            pid(1),
            QuitReason::Kicked("rules".to_owned()),
            "player1 was kicked".to_owned()
        )]
    );
    // A direct kick raises no `PlayerKick`, but its leaving raises `PlayerQuit`.
    game.player_mut(2).kick("direct").unwrap();
    game.idle(1);
    assert_eq!(
        drain(&mut seen),
        [(
            pid(2),
            QuitReason::Kicked("direct".to_owned()),
            crate::tick::player::LEFT.to_owned()
        )]
    );
}

// api: InventoryCloseEvent
/// A menu an `InventoryClose` listener opens while a teleport to another world closes the
/// player's window does not follow the player: its blocks stand in the old world, so it closes
/// without another event before the player leaves.
#[test]
fn a_menu_a_close_listener_opens_does_not_follow_a_teleport_to_another_world() {
    let listeners = on(
        |e: &mut crate::event::InventoryClose, cx: &mut Context<'_>| {
            let menu = crate::menu::Menu::new(crate::menu::Kind::Chest);
            cx.open_menu(e.player(), menu, |_, _, _| crate::menu::Response::Keep)
                .expect("the menu opens");
        },
    );
    let mut game = Game::new(1, listeners);
    add_arena(&mut game);
    let barrel = BlockPos::new(2, -60, 2);
    game.state.worlds[0]
        .set_block(barrel, block::Kind::Barrel.default_state())
        .unwrap();
    assert!(inventory::open_block(&mut game.state, sid(1), 0, barrel));
    game.idle(3);
    let at = game.player(1).position;
    teleport(&mut game.state, sid(1), 1, at);
    let cx = Context {
        state: &mut game.state,
    };
    assert_eq!(cx.menu(pid(1)), None);
}

#[test]
fn the_teleport_action_names_its_cause_to_the_listeners() {
    let listeners = on(|e: &mut EntityTeleport, _: &mut Context<'_>| {
        assert_eq!(e.cause(), TeleportCause::Action);
        e.set_target(e.target_world(), Vec3::new(3.0, -60.0, 3.0));
    });
    let mut game = Game::new(1, listeners);
    let feet = Vec3::new(1.0, -60.0, 1.0);
    game.perform(action::Teleport::new(pid(1), world::Id::DEFAULT, feet));
    assert_eq!(game.player(1).position(), Vec3::new(3.0, -60.0, 3.0));
}

/// Listeners that record every [`PlayerWorldChange`] into `tx`: the player, the world it left,
/// the world it entered, and the world [`Player::world`] gives the listener.
fn world_changes(tx: mpsc::Sender<(Id, world::Id, world::Id, world::Id)>) -> Listeners {
    on(move |e: &mut PlayerWorldChange, cx: &mut Context<'_>| {
        let now = cx.player(e.player()).unwrap().world();
        tx.try_send((e.player(), e.old_world(), e.new_world(), now))
            .unwrap();
    })
}

/// A direct teleport into another world raises nothing at the call: the move, and its world
/// change, come at the tick's sync, once.
#[test]
fn a_direct_teleport_to_another_world_raises_its_world_change_once_at_the_sync() {
    let (tx, mut seen) = record();
    let mut game = Game::new(1, world_changes(tx));
    let arena = add_arena(&mut game);
    Context {
        state: &mut game.state,
    }
    .teleport(pid(1), arena, SPAWN_FEET)
    .unwrap();
    assert!(drain(&mut seen).is_empty(), "nothing at the call");
    game.idle(1);
    assert_eq!(
        drain(&mut seen),
        [(pid(1), world::Id::DEFAULT, arena, arena)]
    );
    game.idle(2);
    assert!(drain(&mut seen).is_empty(), "once");
}

#[test]
fn the_teleport_action_to_another_world_raises_its_world_change_once() {
    let (tx, mut seen) = record();
    let mut game = Game::new(1, world_changes(tx));
    let arena = add_arena(&mut game);
    game.perform(action::Teleport::new(pid(1), arena, SPAWN_FEET));
    game.idle(2);
    assert_eq!(
        drain(&mut seen),
        [(pid(1), world::Id::DEFAULT, arena, arena)]
    );
}

/// A teleport within the player's world, by the API or an action, raises no world change; one
/// into another world does.
#[test]
fn a_teleport_within_the_world_raises_no_world_change() {
    let (tx, mut seen) = record();
    let mut game = Game::new(1, world_changes(tx));
    let arena = add_arena(&mut game);
    let feet = Vec3::new(3.5, -60.0, 3.5);
    let mut cx = Context {
        state: &mut game.state,
    };
    cx.teleport(pid(1), world::Id::DEFAULT, feet).unwrap();
    game.idle(1);
    game.perform(action::Teleport::new(
        pid(1),
        world::Id::DEFAULT,
        SPAWN_FEET,
    ));
    assert_eq!(game.player(1).position(), SPAWN_FEET);
    assert!(drain(&mut seen).is_empty());
    game.perform(action::Teleport::new(pid(1), arena, SPAWN_FEET));
    assert_eq!(
        drain(&mut seen),
        [(pid(1), world::Id::DEFAULT, arena, arena)]
    );
}

/// An ender pearl an [`EntityTeleport`] listener sends into another world raises the world change
/// once, after its fall damage.
#[test]
fn an_ender_pearl_into_another_world_raises_its_world_change_after_the_damage() {
    let arena = world::Id::new(1).unwrap();
    let (tx, mut seen) = record();
    let mut listeners = on(move |e: &mut PlayerWorldChange, cx: &mut Context<'_>| {
        let health = cx.player(e.player()).unwrap().health();
        tx.try_send((e.player(), e.old_world(), e.new_world(), health))
            .unwrap();
    });
    listeners.push(
        Priority::Normal,
        move |e: &mut EntityTeleport, _: &mut Context<'_>| {
            assert_eq!(e.cause(), TeleportCause::EnderPearl);
            e.set_target(arena, SPAWN_FEET);
        },
    );
    let mut game = Game::new(1, listeners);
    add_arena(&mut game);
    let pearl = stack(ItemKind::EnderPearl, 1);
    assert!(crate::tick::entity::throw(&mut game.state, sid(1), &pearl));
    game.idle(60);
    assert_eq!(game.player(1).world(), arena, "it landed");
    assert_eq!(
        drain(&mut seen),
        [(pid(1), world::Id::DEFAULT, arena, 15.0)]
    );
}

/// A respawn a [`PlayerRespawn`] listener sends into another world raises the world change once,
/// after the reset: an effect its listener gives stays.
#[test]
fn a_respawn_in_another_world_raises_its_world_change_after_the_reset() {
    let arena = world::Id::new(1).unwrap();
    let speed = effect::Instance::new(effect::Type::Speed, 100, 0, true, false);
    let (tx, mut seen) = record();
    let mut listeners = on(move |e: &mut PlayerWorldChange, cx: &mut Context<'_>| {
        cx.player_mut(e.player()).unwrap().add_effect(speed);
        tx.try_send((e.player(), e.old_world(), e.new_world()))
            .unwrap();
    });
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerRespawn, _: &mut Context<'_>| {
            e.set_position(arena, SPAWN_FEET);
        },
    );
    let mut game = Game::new(1, listeners);
    add_arena(&mut game);
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    damage::respawn(&mut game.state, sid(1), Stay::Online);
    assert_eq!(drain(&mut seen), [(pid(1), world::Id::DEFAULT, arena)]);
    assert!(game.player(1).has_effect(effect::Type::Speed));
}

/// An unload sends each player of the world to the default world, raising the world change from
/// the world unloaded; a player elsewhere raises nothing.
#[test]
fn an_unload_raises_the_world_change_of_each_player_it_sends_away() {
    let (tx, mut seen) = record();
    let mut game = Game::new(2, world_changes(tx));
    let arena = add_arena(&mut game);
    let eye = SPAWN_FEET + Vec3::new(0.0, EYE_HEIGHT, 0.0);
    teleport(&mut game.state, sid(1), arena.index(), eye);
    assert!(
        drain(&mut seen).is_empty(),
        "the rule's teleport raises nothing itself"
    );
    Context {
        state: &mut game.state,
    }
    .force_unload_world(arena)
    .unwrap();
    game.idle(1);
    assert_eq!(
        drain(&mut seen),
        [(pid(1), arena, world::Id::DEFAULT, world::Id::DEFAULT)]
    );
}

/// A teleport applied as its player leaves raises no world change: the player has had its
/// [`PlayerQuit`]. Another player's teleport in the same tick does.
#[test]
fn a_teleport_applied_as_the_player_quits_raises_no_world_change() {
    let (tx, mut seen) = record();
    let mut game = Game::new(2, world_changes(tx));
    let arena = add_arena(&mut game);
    let mut cx = Context {
        state: &mut game.state,
    };
    cx.teleport(pid(1), arena, SPAWN_FEET).unwrap();
    cx.teleport(pid(2), arena, SPAWN_FEET).unwrap();
    cx.player_mut(pid(1)).unwrap().kick("bye").unwrap();
    game.idle(1);
    assert!(!game.state.players.contains_key(&sid(1)));
    assert_eq!(
        drain(&mut seen),
        [(pid(2), world::Id::DEFAULT, arena, arena)]
    );
}

/// A respawn into another world applied as its player leaves raises no world change: the player
/// has had its [`PlayerQuit`]. Another player's respawn there in the same tick does.
#[test]
fn a_respawn_applied_as_the_player_quits_raises_no_world_change() {
    let arena = world::Id::new(1).unwrap();
    let (tx, mut seen) = record();
    let (respawn_tx, mut respawned) = record();
    let mut listeners = world_changes(tx);
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerRespawn, _: &mut Context<'_>| {
            e.set_position(arena, SPAWN_FEET);
        },
    );
    listeners.push_monitor(move |e: &PlayerRespawn, _: &Context<'_>| {
        respawn_tx.try_send(e.player()).unwrap();
    });
    let mut game = Game::new(2, listeners);
    add_arena(&mut game);
    for s in 1..=2 {
        assert!(damage(&mut game.state, sid(s), 100.0, Damage::Magic));
        assert!(game.player_mut(s).respawn());
    }
    game.player_mut(1).kick("bye").unwrap();
    game.idle(1);
    assert!(!game.state.players.contains_key(&sid(1)));
    assert_eq!(drain(&mut respawned), [pid(1), pid(2)], "both respawned");
    assert_eq!(
        drain(&mut seen),
        [(pid(2), world::Id::DEFAULT, arena, arena)]
    );
}

/// A join placing the player in a default world other than the first raises no world change: it
/// is a first place. A move before the player spawns is one, raised before its [`PlayerJoin`].
#[test]
fn a_join_raises_no_world_change() {
    let (tx, mut seen) = record();
    let join_tx = tx.clone();
    let mut listeners = on(move |e: &mut PlayerWorldChange, _: &mut Context<'_>| {
        tx.try_send((e.player(), Some((e.old_world(), e.new_world()))))
            .unwrap();
    });
    listeners.push(
        Priority::Normal,
        move |e: &mut PlayerJoin, _: &mut Context<'_>| {
            join_tx.try_send((e.player(), None)).unwrap();
        },
    );
    let mut game = Game::new(0, listeners);
    let arena = add_arena(&mut game);
    let mut cx = Context {
        state: &mut game.state,
    };
    cx.set_default_world(arena).unwrap();
    let (joined, rx) = join(1, 0);
    game.receivers.push(rx);
    game.run(vec![joined, radius(1, 1, 8)]);
    assert_eq!(game.player(1).world(), arena);
    assert_ne!(game.player(1).phase, Phase::Spawned);
    assert!(drain(&mut seen).is_empty());
    let mut cx = Context {
        state: &mut game.state,
    };
    cx.teleport(pid(1), world::Id::DEFAULT, SPAWN_FEET).unwrap();
    game.idle(1);
    game.run(vec![initialised(1, 2, 1)]);
    assert_eq!(game.player(1).phase, Phase::Spawned);
    assert_eq!(
        drain(&mut seen),
        [(pid(1), Some((arena, world::Id::DEFAULT))), (pid(1), None)]
    );
}

// api: PlayerTransferEvent, Player::transfer
#[test]
fn transfer_listeners_keep_the_player_or_send_it_elsewhere() {
    let listeners = on(|e: &mut PlayerTransfer, _: &mut Context<'_>| {
        assert_eq!(e.message(), "Moved to another server");
        if e.port() == 1 {
            e.cancel();
        } else {
            e.set_address("b.example.com".to_owned());
            e.set_port(19_134);
        }
    });
    let mut game = Game::new(1, listeners);
    game.perform(action::Transfer::new(pid(1), "a.example.com", 1));
    assert!(game.player(1).is_online());
    game.perform(action::Transfer::new(pid(1), "a.example.com", 19_132));
    let (_, close) = game.closed(1);
    assert!(matches!(
        close,
        Some(Packet::Transfer(t)) if t.address == "b.example.com" && t.port == 19_134
    ));
}

// api: Entity::attack, Living::attack, Player::attack, EntityDamageEvent, PlayerDeathEvent
#[test]
fn the_damage_action_hurts_through_the_rules() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut PlayerDeath, _: &mut Context<'_>| {
        tx.try_send(e.player()).unwrap();
    });
    let mut game = Game::new(2, listeners);
    let by = DamageSource::Entity(Actor::Player(pid(2)));
    game.perform(action::Damage::new(pid(1), 6.0, DamageCause::Magic).with_source(by));
    assert_eq!(game.player(1).health(), 14.0);
    game.player_mut(1).set_game_mode(GameMode::Creative);
    game.perform(action::Damage::new(pid(1), 1000.0, DamageCause::Suicide));
    assert!(!game.player(1).is_alive());
    assert_eq!(drain(&mut seen), [pid(1)]);
}

/// The largest hit of a player with the strongest Strength kills through armor, and a totem
/// still saves from it, leaving one health.
#[test]
fn the_largest_hit_of_a_strong_player_kills_through_armor_and_a_totem_saves_from_it() {
    for amplifier in [0, 255] {
        for totem in [false, true] {
            let mut game = Game::new(2, Listeners::default());
            let target = game.player_mut(2);
            if totem {
                inventory::set_held(target, stack(ItemKind::Totem, 1));
            } else {
                inventory::set_armor(target, 1, stack(ItemKind::IronChestplate, 1));
            }
            for effect in [effect::Type::Strength, effect::Type::Weakness] {
                effects::add(
                    &mut game.state,
                    sid(1),
                    effect,
                    effects::Active::new(200, amplifier),
                );
            }
            let by = DamageSource::Entity(Actor::Player(pid(1)));
            // What `action::Damage` does, with no tick after it: the totem's Regeneration has not
            // healed yet.
            Context {
                state: &mut game.state,
            }
            .hurt_player(pid(2), MAX_DAMAGE, DamageCause::EntityAttack, by);
            let player = game.player(2);
            assert_eq!(player.is_alive(), totem, "amplifier {amplifier}");
            if totem {
                assert!((player.health() - 1.0).abs() < 1e-6, "{}", player.health());
                assert!(player.inventory.held().is_empty(), "the totem is used");
            }
        }
    }
}

/// A hit above the damage limit does nothing: past it the sums of a hit would lose the totem's
/// health − 1 in rounding, and the totem holder would die.
#[test]
fn a_hit_above_the_damage_limit_does_nothing() {
    let mut game = Game::new(2, Listeners::default());
    inventory::set_held(game.player_mut(2), stack(ItemKind::Totem, 1));
    effects::add(
        &mut game.state,
        sid(1),
        effect::Type::Strength,
        effects::Active::new(200, 0),
    );
    let by = DamageSource::Entity(Actor::Player(pid(1)));
    for amount in [f64::MAX, 1e18] {
        game.perform(
            action::Damage::new(pid(2), amount, DamageCause::EntityAttack).with_source(by),
        );
        let player = game.player(2);
        assert_eq!(player.health(), 20.0, "{amount}");
        assert!(!player.inventory.held().is_empty(), "{amount}");
    }
}

/// An emote packet of session `s`.
fn emote(game: &mut Game, s: u64, id: &str) -> Input {
    let e = p::Emote {
        entity_runtime_id: s,
        emote_length: 99,
        emote_id: id.to_owned(),
        xuid: "x".to_owned(),
        platform_id: String::new(),
        flags: 0,
    };
    game.send(s, Packet::Emote(e))
}

/// The emote IDs in `packets`.
fn emotes(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::Emote(e) => {
                assert_eq!((e.flags, e.emote_length, e.xuid.as_str()), (3, 0, ""));
                Some(e.emote_id.clone())
            }
            _ => None,
        })
        .collect()
}

// api: PlayerEmoteEvent
#[test]
fn emotes_reach_the_viewers_once_every_six_ticks_as_listeners_allow() {
    let listeners = on(
        |e: &mut PlayerEmote, _: &mut Context<'_>| match e.emote_id() {
            "no" => e.cancel(),
            "swap" => e.set_emote_id("other".to_owned()),
            _ => {}
        },
    );
    let mut game = Game::new(2, listeners);
    let first = emote(&mut game, 1, "wave");
    let again = emote(&mut game, 1, "wave");
    game.run(vec![first, again]);
    assert_eq!(emotes(&game.take(2)), ["wave"]);
    assert!(emotes(&game.take(1)).is_empty(), "not to the player itself");
    for (id, want) in [("no", None), ("swap", Some("other")), ("", None)] {
        game.idle(6);
        let input = emote(&mut game, 1, id);
        game.run(vec![input]);
        assert_eq!(emotes(&game.take(2)), want.into_iter().collect::<Vec<_>>());
    }
}

/// A valid skin with full ID `id`.
fn skin(id: &str) -> crate::protocol::Skin {
    crate::protocol::Skin {
        skin_image_width: 64,
        skin_image_height: 32,
        skin_data: vec![7; 64 * 32 * 4],
        skin_resource_patch: br#"{"geometry":{"default":"geometry.humanoid.custom"}}"#.to_vec(),
        skin_geometry: b"{}".to_vec(),
        full_id: id.to_owned(),
        ..crate::protocol::Skin::default()
    }
}

/// A skin packet of session `s`.
fn skin_input(game: &mut Game, s: u64, skin: crate::protocol::Skin) -> Input {
    let packet = p::PlayerSkin {
        uuid: crate::protocol::Uuid::default(),
        skin: std::sync::Arc::new(skin),
        new_skin_name: String::new(),
        old_skin_name: String::new(),
    };
    game.send(s, Packet::PlayerSkin(packet))
}

/// The full skin IDs of the `PlayerSkin` packets in `packets`.
fn skins(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::PlayerSkin(s) => Some(s.skin.full_id.clone()),
            _ => None,
        })
        .collect()
}

// api: PlayerChangeSkinEvent
#[test]
fn skin_changes_reach_everyone_or_go_back_to_the_player() {
    let listeners = on(|e: &mut PlayerChangeSkin, _: &mut Context<'_>| {
        if e.new_skin().full_id == "no" {
            e.cancel();
        }
    });
    let mut game = Game::new(2, listeners);
    let old = game.player(1).skin().full_id.clone();
    let input = skin_input(&mut game, 1, skin("no"));
    game.run(vec![input]);
    assert_eq!(skins(&game.take(1)), [old], "its own skin back");
    assert!(skins(&game.take(2)).is_empty());
    game.idle(20);
    let input = skin_input(&mut game, 1, skin("new"));
    game.run(vec![input]);
    assert_eq!(skins(&game.take(2)), ["new"]);
    assert_eq!(game.player(1).skin().full_id, "new");
    // Sent back, too fast, or with images that do not match their size: ignored.
    for s in [skin("new"), skin("fast")] {
        let input = skin_input(&mut game, 1, s);
        game.run(vec![input]);
    }
    game.idle(20);
    let mut bad = skin("bad");
    bad.skin_data.pop();
    let packet = p::PlayerSkin {
        uuid: crate::protocol::Uuid::default(),
        skin: std::sync::Arc::new(bad),
        new_skin_name: String::new(),
        old_skin_name: String::new(),
    };
    on_skin(&mut game.state, sid(1), &packet);
    game.idle(1);
    assert!(skins(&game.take(2)).is_empty());
    assert_eq!(game.player(1).skin().full_id, "new");
}

// api: PlayerChangeSkinEvent
#[test]
fn a_skin_a_listener_sets_through_the_api_is_not_overwritten_by_the_change() {
    let listeners = on(|_: &mut PlayerChangeSkin, cx: &mut Context<'_>| {
        cx.player_mut(pid(1))
            .unwrap()
            .set_skin(skin("api"))
            .unwrap();
    });
    let mut game = Game::new(2, listeners);
    let input = skin_input(&mut game, 1, skin("asked"));
    game.run(vec![input]);
    assert_eq!(game.player(1).skin().full_id, "api");
    assert_eq!(skins(&game.take(2)), ["api"]);
}

/// A valid skin of about 4 MB with full ID `id`: a 1000 × 1000 image (the budget counts bytes
/// before compression).
fn big_skin(id: &str) -> crate::protocol::Skin {
    crate::protocol::Skin {
        skin_image_width: 1000,
        skin_image_height: 1000,
        skin_data: vec![7; 1000 * 1000 * 4],
        ..skin(id)
    }
}

/// The bytes the skin budget counts for each `PlayerSkin` packet in `packets`, with its full ID.
fn skin_sizes(packets: Vec<Packet>) -> Vec<(String, usize)> {
    packets
        .into_iter()
        .filter_map(|p| match p {
            Packet::PlayerSkin(s) => Some((
                s.skin.full_id.clone(),
                crate::tick::queue::Skin::new(s.skin).size(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_skin_of_the_largest_size_reaches_everyone_in_its_tick() {
    let mut game = Game::new(2, Listeners::default());
    let mut largest = skin("largest");
    let size = crate::tick::queue::Skin::new(std::sync::Arc::new(largest.clone())).size();
    // `{}` padded with spaces to fill the bound.
    let mut geometry = vec![b' '; crate::entity::MAX_SKIN_BYTES - size + 2];
    geometry[0] = b'{';
    *geometry.last_mut().unwrap() = b'}';
    largest.skin_geometry = geometry;
    let input = skin_input(&mut game, 1, largest);
    game.run(vec![input]);
    assert_eq!(
        skin_sizes(game.take(2)),
        [("largest".to_owned(), crate::entity::MAX_SKIN_BYTES)]
    );
}

#[test]
fn skin_changes_every_second_reach_a_receiver_within_the_skin_budget() {
    let mut game = Game::new(2, Listeners::default());
    let mut sent = 0_usize;
    for t in 0..SECONDS * 20 {
        let inputs = if t % 20 == 0 {
            vec![skin_input(&mut game, 1, big_skin(&format!("big{t}")))]
        } else {
            Vec::new()
        };
        game.run(inputs);
        game.take(1);
        sent += skin_sizes(game.take(2))
            .into_iter()
            .map(|(_, size)| size)
            .sum::<usize>();
    }
    // One skin of the largest size at once, then 128 KiB a second.
    let budget = crate::entity::MAX_SKIN_BYTES + (128 << 10) * SECONDS;
    assert!(sent <= budget, "{sent} skin bytes in {SECONDS} s");
    // The latest skin waits rather than older ones going out.
    assert!(sent >= 2 * 4_000_000);
}

#[test]
fn players_changing_skins_together_each_get_a_turn() {
    let mut game = Game::new(3, Listeners::default());
    let mut senders = Vec::new();
    // Two skins of the largest size refill in 64 seconds; each player waits at most that.
    for t in 0..70 * 20 {
        let inputs = if t % 20 == 0 {
            [1, 2]
                .map(|s| skin_input(&mut game, s, big_skin(&format!("{s}/{t}"))))
                .into()
        } else {
            Vec::new()
        };
        game.run(inputs);
        game.take(1);
        game.take(2);
        senders.extend(
            skin_sizes(game.take(3))
                .into_iter()
                .map(|(id, _)| id.split_once('/').map(|(s, _)| s.to_owned()).unwrap()),
        );
    }
    assert!(senders.len() >= 3, "{senders:?}");
    assert!(
        senders.windows(2).all(|w| w[0] != w[1]),
        "turns alternate: {senders:?}"
    );
}

#[test]
fn rules_without_listeners_build_no_events_and_change_nothing() {
    // The same script with and without monitors gives the same state.
    let script = |listeners: Listeners| {
        let mut game = Game::new(2, listeners);
        inventory::give(game.player_mut(1), stack(ItemKind::Bread, 9));
        attack_player(&mut game.state, sid(2), sid(1));
        let input = game.flags(1, &[InputFlag::StartSprinting, InputFlag::StartJumping]);
        game.run(vec![input]);
        assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
        game.idle(2);
        damage::respawn(&mut game.state, sid(1), Stay::Online);
        game.idle(2);
        game.state.hash()
    };
    let mut watching = Listeners::default();
    watching.push_monitor(|_: &EntityDamage, _: &Context<'_>| {});
    watching.push_monitor(|_: &PlayerDeath, _: &Context<'_>| {});
    watching.push_monitor(|_: &PlayerExhaust, _: &Context<'_>| {});
    watching.push_monitor(|_: &PlayerRespawn, _: &Context<'_>| {});
    assert_eq!(script(Listeners::default()), script(watching));
}

// The settings that switch survival systems off: with the switch off the system's state stays as it
// was, its events are not raised and its packets are not sent.

/// The names of the attributes `packets` carry.
fn attribute_names(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::UpdateAttributes(a) => Some(a),
            _ => None,
        })
        .flat_map(|a| a.attributes.iter().map(|x| x.attribute_value.name.clone()))
        .collect()
}

/// The spawn window of the first player of a server with `edit`.
fn spawn_window(edit: impl FnOnce(&mut Config)) -> Vec<Packet> {
    let mut state = new_state(settings(edit), Listeners::default());
    let (joined, mut rx) = join(1, 0);
    crate::tick::tests::tick(
        &mut state,
        &mut vec![joined, radius(1, 1, 8)],
        &mut Output::default(),
    );
    received(&mut rx).0
}

/// The value of the game rule `name` in `packets`.
fn game_rule(packets: &[Packet], name: &str) -> Option<crate::protocol::GameRuleValue> {
    packets.iter().find_map(|p| match p {
        Packet::GameRulesChanged(g) => g
            .game_rules
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value),
        _ => None,
    })
}

/// Listeners that record a name for each event of a switchable system.
fn named_events(tx: &mpsc::Sender<&'static str>) -> Listeners {
    let mut l = Listeners::default();
    let t = tx.clone();
    l.push(
        Priority::Normal,
        move |_: &mut PlayerExhaust, _: &mut Context<'_>| {
            t.try_send("exhaust").unwrap();
        },
    );
    let t = tx.clone();
    l.push(
        Priority::Normal,
        move |e: &mut EntityRegainHealth, _: &mut Context<'_>| {
            if e.regain_reason() == RegainReason::Saturation {
                t.try_send("saturation").unwrap();
            }
        },
    );
    let t = tx.clone();
    l.push(
        Priority::Normal,
        move |_: &mut PlayerXpChange, _: &mut Context<'_>| {
            t.try_send("xp").unwrap();
        },
    );
    let t = tx.clone();
    l.push(
        Priority::Normal,
        move |e: &mut PlayerDeath, _: &mut Context<'_>| {
            if e.xp_drop_amount() > 0 {
                t.try_send("xp drop").unwrap();
            }
        },
    );
    let t = tx.clone();
    l.push(
        Priority::Normal,
        move |e: &mut EntityDamage, _: &mut Context<'_>| {
            t.try_send(match e.cause() {
                DamageCause::Fall => "fall",
                DamageCause::Drowning => "drowning",
                DamageCause::Suffocation => "suffocation",
                _ => "damage",
            })
            .unwrap();
        },
    );
    l
}

#[test]
fn food_off_spends_and_gains_no_food_raises_no_event_and_sends_no_food_attribute() {
    let window = attribute_names(&spawn_window(|c| c.general.food = false));
    assert!(window.iter().any(|n| n == "minecraft:health"));
    assert!(
        window
            .iter()
            .all(|n| !FOOD_ATTRIBUTES.contains(&n.as_str())),
        "{window:?}"
    );
    let (tx, mut seen) = record();
    let mut game = Game::with(1, named_events(&tx), |c| c.general.food = false);
    let p = game.player_mut(1);
    let off = Err(crate::player::Error::Disabled);
    assert_eq!(p.set_food(5.0), off);
    assert_eq!(p.add_food(1.0), off);
    assert_eq!(p.set_saturation(1.0), off);
    assert_eq!(p.add_saturation(1.0), off);
    assert_eq!(p.set_exhaustion(3.0), off);
    assert_eq!(p.exhaust(30.0), off);
    // A golden apple (eaten with a full bar), a sprint jump, a hit, the saturation effect.
    inventory::set_held(game.player_mut(1), stack(ItemKind::GoldenApple, 1));
    assert!(start_eating(&mut game.state, sid(1)));
    game.idle(32);
    assert!(finish_eating(&mut game.state, sid(1)));
    let input = game.flags(1, &[InputFlag::StartSprinting, InputFlag::StartJumping]);
    game.run(vec![input]);
    assert!(damage(&mut game.state, sid(1), 1.0, Damage::Contact));
    effects::add(
        &mut game.state,
        sid(1),
        effect::Type::Saturation,
        effects::Active::new(10, 0),
    );
    game.idle(200);
    let s = game.survival_mut(1);
    assert_eq!(
        (s.food, s.saturation, s.exhaustion, s.food_timer),
        (20.0, 5.0, 0.0, 0)
    );
    assert_eq!(drain(&mut seen), ["damage"]);
    let sent = attribute_names(&game.take(1));
    assert!(
        sent.iter().all(|n| !FOOD_ATTRIBUTES.contains(&n.as_str())),
        "{sent:?}"
    );
}

#[test]
fn xp_off_makes_no_points_levels_or_orbs_and_its_api_refuses() {
    let window = attribute_names(&spawn_window(|c| c.general.xp = false));
    assert!(window.iter().any(|n| n == "minecraft:health"));
    assert!(
        window.iter().all(|n| !XP_ATTRIBUTES.contains(&n.as_str())),
        "{window:?}"
    );
    let (tx, mut seen) = record();
    let mut game = Game::with(1, named_events(&tx), |c| c.general.xp = false);
    // A level saved while xp was on stays as it is.
    game.survival_mut(1).xp_level = 10;
    let p = game.player_mut(1);
    let off = Err(crate::player::Error::Disabled);
    assert_eq!(p.set_xp_level(5), off);
    assert_eq!(p.add_xp_levels(1), off);
    assert_eq!(p.set_xp_progress(0.5), off);
    assert_eq!(p.set_current_total_xp(9), off);
    assert_eq!(p.set_lifetime_total_xp(9), off);
    p.add_xp(100);
    p.subtract_xp(5);
    p.subtract_xp_levels(3);
    // A death drops no xp and keeps the level.
    assert!(damage(&mut game.state, sid(1), 100.0, Damage::Magic));
    game.idle(2);
    let s = game.survival_mut(1);
    assert_eq!(
        (s.xp_level, s.xp_progress, s.xp_lifetime, s.xp_cooldown),
        (10, 0.0, 0, 0)
    );
    assert_eq!(drain(&mut seen), ["damage"]);
    let sent = game.take(1);
    let names = attribute_names(&sent);
    assert!(
        names.iter().all(|n| !XP_ATTRIBUTES.contains(&n.as_str())),
        "{names:?}"
    );
    assert!(
        !sent.iter().any(|p| matches!(p, Packet::AddActor(_))),
        "no orb"
    );
}

/// An auth input of session `s` that moves it by `by`.
fn move_by(game: &mut Game, s: u64, by: Vec3) -> Input {
    let auth = p::PlayerAuthInput {
        position: (game.player(s).position + by).to_f32(),
        ..p::PlayerAuthInput::default()
    };
    game.send(s, Packet::PlayerAuthInput(Box::new(auth)))
}

/// The sounds among `packets`.
fn sounds(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::LevelSoundEvent(e) => Some(e.sound_type.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn fall_damage_off_lands_softly_and_spares_ender_pearl_falls() {
    let window = spawn_window(|c| c.general.fall_damage = false);
    assert_eq!(
        game_rule(&window, "fallDamage"),
        Some(crate::protocol::GameRuleValue::Bool(false))
    );
    let (tx, mut seen) = record();
    let mut game = Game::with(1, named_events(&tx), |c| c.general.fall_damage = false);
    game.survival_mut(1).fall_distance = 10.0;
    let input = move_by(&mut game, 1, Vec3::new(0.5, 0.0, 0.0));
    game.run(vec![input]);
    assert_eq!(game.survival_mut(1).health, 20.0);
    assert_eq!(game.survival_mut(1).fall_distance, 0.0, "landed");
    assert_eq!(
        sounds(&game.take(1)),
        [crate::protocol::consts::SoundEvent::Land.as_str()]
    );
    // An ender pearl's landing is fall damage.
    assert!(!damage(&mut game.state, sid(1), 5.0, Damage::Fall));
    assert_eq!(drain(&mut seen), ["exhaust"], "only the walk");
}

/// Whether the player's own entity data in `packets` shows it breathing (flag 35).
fn shows_breathing(packets: &[Packet]) -> bool {
    packets.iter().any(|p| {
        matches!(p, Packet::SetActorData(d)
        if d.entity_runtime_id == 1
            && matches!(
                d.entity_metadata.get(&0),
                Some(crate::protocol::MetadataValue::Int64(f)) if f & 1 << 35 != 0
            ))
    })
}

#[test]
fn drowning_off_keeps_the_air_full_the_player_breathing_and_unhurt() {
    let window = spawn_window(|c| c.general.drowning = false);
    assert_eq!(
        game_rule(&window, "drowningDamage"),
        Some(crate::protocol::GameRuleValue::Bool(false))
    );
    assert!(
        shows_breathing(&window),
        "breathing from the spawn window on"
    );
    let (tx, mut seen) = record();
    let mut game = Game::with(1, named_events(&tx), |c| c.general.drowning = false);
    let p = game.player_mut(1);
    let off = Err(crate::player::Error::Disabled);
    assert_eq!(p.set_air_supply_ticks(10), off);
    assert_eq!(p.set_max_air_supply_ticks(10), off);
    p.set_breathing(false);
    assert!(game.player(1).is_breathing());
    let eye = BlockPos::containing(game.player(1).position);
    game.state.worlds[0]
        .set_block(eye, block::Kind::Water.default_state())
        .unwrap();
    game.idle(400);
    game.state.worlds[0]
        .set_block(eye, block::Kind::Stone.default_state())
        .unwrap();
    game.idle(40);
    let s = game.survival_mut(1);
    assert_eq!((s.air, s.breathing, s.health), (300, true, 20.0));
    assert!(drain(&mut seen).is_empty());
    let data = game
        .take(1)
        .into_iter()
        .filter(|p| matches!(p, Packet::SetActorData(d) if d.entity_runtime_id == 1))
        .count();
    assert_eq!(data, 0, "no air or breathing change is sent");
}

/// The thorns answer counts the wearer's Strength, as any hit by it does.
#[test]
fn the_thorns_answer_counts_the_wearers_strength() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut EntityDamage, _: &mut Context<'_>| {
        tx.try_send(e.modifier(DamageModifier::Strength)).unwrap();
    });
    let mut game = Game::new(2, listeners);
    // Thorns above 10 always answers, dealing its level less 10.
    let chest = stack(ItemKind::DiamondChestplate, 1)
        .with_extra(Some(crate::item::Extra {
            enchantments: vec![(crate::item::Enchantment::Thorns, 11)],
            ..crate::item::Extra::default()
        }))
        .unwrap();
    inventory::set_armor(game.player_mut(1), 1, chest);
    let strength = effect::Instance::new(effect::Type::Strength, 200, 1, true, false);
    assert!(add_effect(&mut game.state, sid(1), strength));
    assert!(damage(&mut game.state, sid(1), 1.0, Damage::Attack(sid(2))));
    // Strength II: 1 + 1 × 0.3 × 2.
    assert_eq!(game.survival_mut(2).health, 20.0 - 1.6);
    assert_eq!(drain(&mut seen).last().copied(), Some(0.6));
}

#[test]
fn armor_off_neither_protects_nor_wears_nor_answers_with_thorns() {
    let (tx, mut seen) = record();
    let listeners = on(move |e: &mut EntityDamage, _: &mut Context<'_>| {
        let used = [
            DamageModifier::Armor,
            DamageModifier::ArmorEnchantments,
            DamageModifier::ArmorHelmet,
        ]
        .map(|m| e.is_applicable(m));
        tx.try_send(used).unwrap();
    });
    let mut game = Game::with(2, listeners, |c| c.general.armor = false);
    for (kind, slot) in [
        (ItemKind::DiamondHelmet, 0),
        (ItemKind::DiamondChestplate, 1),
    ] {
        let piece = stack(kind, 1)
            .with_extra(Some(crate::item::Extra {
                enchantments: vec![
                    (crate::item::Enchantment::Protection, 4),
                    (crate::item::Enchantment::Thorns, 10),
                ],
                ..crate::item::Extra::default()
            }))
            .unwrap();
        inventory::set_armor(game.player_mut(1), slot, piece);
    }
    let worn = game.player(1).inventory.armor().to_vec();
    assert!(damage(
        &mut game.state,
        sid(1),
        10.0,
        Damage::Attack(sid(2))
    ));
    game.idle(10);
    assert!(damage(&mut game.state, sid(1), 4.0, Damage::FallingBlock));
    assert_eq!(game.survival_mut(1).health, 6.0, "every hit landed whole");
    assert_eq!(game.player(1).inventory.armor(), worn.as_slice(), "no wear");
    assert_eq!(game.survival_mut(2).health, 20.0, "no thorns");
    assert_eq!(drain(&mut seen), [[false; 3]; 2]);
}

/// `daylight-cycle = false` holds the time; the client is told as the official server tells it
/// with `doDayLightCycle` false (P6 `rules`/`rulesjoin`): the rule, the time of day in
/// `StartGame`'s day cycle lock, the clock not paused and still corrected now and then, at the
/// held time.
#[test]
fn daylight_cycle_off_stops_the_time_and_tells_the_client_as_the_official_server() {
    let window = spawn_window(|c| c.general.daylight_cycle = false);
    assert_eq!(
        game_rule(&window, "doDayLightCycle"),
        Some(crate::protocol::GameRuleValue::Bool(false))
    );
    let lock = window.iter().find_map(|p| match p {
        Packet::StartGame(g) => Some((g.time, g.day_cycle_lock_time)),
        _ => None,
    });
    let Some((time, lock)) = lock else {
        panic!("no StartGame");
    };
    assert_eq!(i64::from(lock), time.rem_euclid(24_000));
    let paused: Vec<bool> = window
        .iter()
        .filter_map(|p| match p {
            Packet::SyncWorldClocks(c) => Some(
                c.sync_states
                    .iter()
                    .map(|s| s.paused)
                    .chain(c.clocks.iter().map(|c| c.paused))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        !paused.is_empty() && paused.iter().all(|&p| !p),
        "{paused:?}"
    );
    let mut game = Game::with(1, Listeners::default(), |c| {
        c.general.daylight_cycle = false;
    });
    let start = game.state.worlds[0].time_ticks();
    game.idle(600);
    assert_eq!(game.state.worlds[0].time_ticks(), start);
    let times: Vec<i32> = game
        .take(1)
        .iter()
        .filter_map(|p| match p {
            Packet::SyncWorldClocks(c) => c.sync_states.first().map(|s| s.time),
            _ => None,
        })
        .collect();
    assert!(!times.is_empty(), "the clock is still corrected");
    assert!(times.iter().all(|&t| i64::from(t) == start), "{times:?}");
}

/// `pvp = false`: no hit another player causes lands, by hand, by a projectile or by a blast,
/// while a player still hurts itself and a kill by another player's name goes through.
#[test]
fn pvp_off_spares_a_player_from_every_hit_another_player_causes() {
    let (tx, mut seen) = record();
    let listeners = on(move |_: &mut EntityDamage, _: &mut Context<'_>| {
        tx.try_send(()).unwrap();
    });
    let mut game = Game::with(2, listeners, |c| c.general.pvp = false);
    let base = game.player(1).position;
    game.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
    game.idle(1);
    let shot_by = |owner: u64| DamageSource::Child {
        child: crate::entity::Id::new(0, 0),
        owner: Some(Actor::Player(pid(owner))),
    };
    let by_hand = DamageSource::Entity(Actor::Player(pid(1)));
    attack_player(&mut game.state, sid(1), sid(2));
    let blows = [
        (Damage::Entity, shot_by(1)),
        (Damage::Explosion, shot_by(1)),
        (Damage::Magic, by_hand),
    ];
    for (cause, source) in blows {
        assert!(damage_by(&mut game.state, sid(2), 4.0, cause, source).is_none());
    }
    assert_eq!(game.survival_mut(2).health, 20.0);
    assert!(drain(&mut seen).is_empty());
    assert!(damage_by(&mut game.state, sid(2), 4.0, Damage::Entity, shot_by(2)).is_some());
    assert_eq!(game.survival_mut(2).health, 16.0);
    game.idle(10);
    assert!(damage_by(&mut game.state, sid(2), 30.0, Damage::Suicide, by_hand).is_some());
    assert!(game.survival_mut(2).is_dead());
}

#[test]
fn critical_hits_setting_decides_critical_hits_while_sprinting() {
    let taken = |critical_hits: CriticalHits, sprinting: bool| {
        let mut game = Game::with(2, Listeners::default(), |c| {
            c.general.critical_hits = critical_hits;
        });
        let base = game.player(1).position;
        game.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
        game.survival_mut(1).motion.yaw = 0.0;
        game.idle(1);
        let s = game.survival_mut(1);
        s.motion.sprinting = sprinting;
        s.fall_distance = 1.0;
        attack_player(&mut game.state, sid(1), sid(2));
        20.0 - game.survival_mut(2).health
    };
    assert_eq!(taken(CriticalHits::Off, false), 1.0);
    assert_eq!(taken(CriticalHits::Sprint, false), 1.5);
    assert_eq!(
        taken(CriticalHits::Sprint, true),
        1.0,
        "no critical hit while sprinting"
    );
    assert_eq!(taken(CriticalHits::Always, true), 1.5);
}

#[test]
fn show_coordinates_sets_the_rule_the_client_shows_them_by() {
    use crate::protocol::GameRuleValue::Bool;
    let shown = |on: bool| {
        game_rule(
            &spawn_window(|c| c.general.show_coordinates = on),
            "showCoordinates",
        )
    };
    assert_eq!(shown(false), Some(Bool(false)));
    assert_eq!(shown(true), Some(Bool(true)));
}

/// The client skips the death screen by the rule and asks to respawn at once; the server's
/// respawn flow stays the same.
#[test]
fn immediate_respawn_sets_the_rule_the_client_respawns_by() {
    use crate::protocol::GameRuleValue::Bool;
    let immediate = |on: bool| {
        game_rule(
            &spawn_window(|c| c.general.immediate_respawn = on),
            "doImmediateRespawn",
        )
    };
    assert_eq!(immediate(false), Some(Bool(false)));
    assert_eq!(immediate(true), Some(Bool(true)));
}

#[test]
fn movement_check_off_takes_moves_of_more_than_fifteen_blocks() {
    let moved = |on: bool| {
        let mut game = Game::with(1, Listeners::default(), |c| {
            c.general.movement_check = on;
        });
        let start = game.player(1).position;
        let input = move_by(&mut game, 1, Vec3::new(20.0, 0.0, 0.0));
        game.run(vec![input]);
        let reset = game.take(1).iter().any(|p| {
            matches!(p, Packet::MovePlayer(m)
                if m.mode == u8::from(crate::protocol::consts::MoveMode::Reset))
        });
        (game.player(1).position.x - start.x, reset)
    };
    assert_eq!(moved(true), (0.0, true));
    assert_eq!(moved(false), (20.0, false));
}
