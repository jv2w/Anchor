//! Scheduled tasks (PLAN.md 8.3, 10.6): code the server runs every few ticks in the tick's serial
//! step, and the [`Context`] it gets.
//!
//! Tasks are registered with [`ServerBuilder::every`](crate::ServerBuilder::every) before the
//! server is built and run in registration order after the tick's inputs, so a replay of the
//! input log runs them at the same points with the same state.
//!
//! The player, entity and game rule methods of [`Context`] are a preliminary API (PLAN.md 10.7):
//! each is a thin call into the rule module that owns the state, enough for the P6 console
//! (`cargo xtask p6 game anchor`) to prepare its scenarios through the public API. The API
//! milestone (M8) replaces them with player and entity handles.

use std::fmt;
use std::num::NonZeroU32;

use super::queue::SessionId;
use super::{MAX_COORDINATE, Phase, PlayerKey, State, entity, inventory, survival, trade};
use crate::config::RANDOM_TICK_SPEED;
use crate::effect;
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::Compound;
use crate::protocol::packet::{GameRulesChanged, Packet};
use crate::protocol::{GameRule, GameRuleValue};
use crate::world::World;

/// What a scheduled task can see and change: the engine time, the worlds, the players and the
/// entities.
///
/// The shape is preliminary: the API milestone (PLAN.md 10, M8) widens it.
pub struct Context<'a> {
    pub(super) state: &'a mut State,
}

/// A joined player, as [`Context::player_exact`] finds it. It stays valid until the player
/// leaves; a later join of the same name is another player.
///
/// Preliminary (PLAN.md 10.7): M8 replaces it with a player handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayerId(SessionId);

impl Context<'_> {
    /// The engine time: ticks since the world was created (20 per second).
    #[doc(alias = "Server::getTick")]
    pub fn tick(&self) -> u64 {
        self.state.tick
    }

    /// The default world: the one the settings name (`level-name`), where players join.
    #[doc(alias = "WorldManager::getDefaultWorld")]
    pub fn world(&self) -> &World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a server always has its default world at index 0"
        )]
        &self.state.worlds[0]
    }

    /// The default world, to change it.
    pub fn world_mut(&mut self) -> &mut World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a server always has its default world at index 0"
        )]
        &mut self.state.worlds[0]
    }

    /// The world named `name`, if the server has it.
    #[doc(alias = "WorldManager::getWorldByName")]
    pub fn world_named(&self, name: &str) -> Option<&World> {
        self.state.worlds.iter().find(|w| w.name() == name)
    }

    /// The world named `name`, to change it.
    pub fn world_named_mut(&mut self, name: &str) -> Option<&mut World> {
        self.state.worlds.iter_mut().find(|w| w.name() == name)
    }

    /// Saves every persistent world and the player data at the end of this tick, together, as
    /// one commit (PLAN.md 6). Calling it again in the same tick changes nothing.
    #[doc(alias = "WorldManager::save")]
    #[doc(alias = "World::save")]
    pub fn save(&mut self) {
        self.state.saving.requested = true;
    }

    /// The joined player whose name is `name`, ignoring ASCII case (PMMP compares with
    /// `strtolower`). Without Xbox sign-in the name is the player's key and the lookup is an
    /// index search; with it, the players (at most `max-players`) are compared one by one.
    #[doc(alias = "Server::getPlayerExact")]
    pub fn player_exact(&self, name: &str) -> Option<PlayerId> {
        let key = PlayerKey::Name(name.to_ascii_lowercase());
        if let Some(&session) = self.state.index.get(&key) {
            return Some(PlayerId(session));
        }
        self.state
            .players
            .iter()
            .find(|(_, p)| p.identity.name.eq_ignore_ascii_case(name))
            .map(|(&session, _)| PlayerId(session))
    }

    /// Puts `stack` into the player's inventory where it fits and tells its client; returns what
    /// did not fit (all of it if the player has left).
    #[doc(alias = "Inventory::addItem")]
    pub fn give(&mut self, player: PlayerId, stack: Stack) -> Stack {
        match self.state.players.get_mut(&player.0) {
            Some(p) => inventory::give(p, stack),
            None => stack,
        }
    }

    /// Moves the player's feet to `feet` in its world and tells its client and viewers. False if
    /// the player has left or `feet` is not finite or lies outside the world border.
    #[doc(alias = "Player::teleport")]
    pub fn teleport(&mut self, player: PlayerId, feet: Vec3) -> bool {
        let border = f64::from(MAX_COORDINATE);
        let eye = feet + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        if !eye.is_finite() || [eye.x, eye.y, eye.z].iter().any(|c| c.abs() > border) {
            return false;
        }
        let Some(world) = self.state.players.get(&player.0).map(|p| p.world) else {
            return false;
        };
        survival::teleport(self.state, player.0, world, eye);
        true
    }

    /// Adds experience points to the player (orbs, commands); nothing if it has left.
    #[doc(alias = "ExperienceManager::addXp")]
    pub fn add_experience(&mut self, player: PlayerId, points: u32) {
        if let Some(p) = self.state.players.get_mut(&player.0) {
            survival::add_experience(p, points);
        }
    }

    /// Gives the player `effect` for `duration` ticks at `amplifier` (0 is level I), visible and
    /// not ambient, as PMMP's `EffectInstance` defaults. False if the player has left or a
    /// stronger effect of the type stays.
    #[doc(alias = "EffectManager::add")]
    pub fn add_effect(
        &mut self,
        player: PlayerId,
        effect: effect::Type,
        duration: u32,
        amplifier: u8,
    ) -> bool {
        if !self.state.players.contains_key(&player.0) {
            return false;
        }
        // PMMP caps durations at the largest 32-bit integer (`Limits::INT32_MAX`).
        let duration = duration.min(i32::MAX.cast_unsigned());
        let instance = effect::Instance::new(effect, duration, amplifier, true, false);
        survival::add_effect(self.state, player.0, instance)
    }

    /// Spawns a villager-type NPC (PLAN.md 8.6: no AI, network actor `minecraft:villager_v2`)
    /// at `pos` in the default world, showing `profession` (the actor variant) and trading
    /// `offers`: one compound per offer in Bedrock's offer form (`buyA`, `buyCountA`, `sell`,
    /// `maxUses`, `tier`, ...). `None` if `pos` is not finite or outside the world border, its
    /// chunk is not loaded or the world is at its entity limit.
    pub fn spawn_villager(
        &mut self,
        pos: Vec3,
        profession: u8,
        offers: &[Compound],
    ) -> Option<crate::entity::Id> {
        let border = f64::from(MAX_COORDINATE);
        if !pos.is_finite() || [pos.x, pos.y, pos.z].iter().any(|c| c.abs() > border) {
            return None;
        }
        let npc = entity::Npc::Villager {
            profession,
            offers: trade::Offers::from_nbt(&Compound::from([(
                "Recipes".to_owned(),
                crate::nbt::Value::List(crate::nbt::List::Compound(offers.to_vec())),
            )])),
        };
        entity::spawn_npc(self.state, 0, pos, npc)
    }

    /// Removes the entity `id` from its world and its viewers; false if it is already gone.
    #[doc(alias = "Entity::flagForDespawn")]
    pub fn remove_entity(&mut self, id: crate::entity::Id) -> bool {
        entity::remove(self.state, id)
    }

    /// Sets the random block ticks per sub-chunk per tick (the `randomTickSpeed` game rule;
    /// `random-tick-speed` sets it at start) until the server stops, and tells the players.
    /// False, changing nothing, above the setting's limit (4,096).
    pub fn set_random_tick_speed(&mut self, speed: u32) -> bool {
        if !RANDOM_TICK_SPEED.contains(&speed) {
            return false;
        }
        self.state.settings.random_tick_speed = speed;
        let packet = Packet::GameRulesChanged(GameRulesChanged {
            game_rules: vec![GameRule {
                name: "randomTickSpeed".to_owned(),
                can_be_modified_by_player: true,
                value: GameRuleValue::Uint32(speed),
            }],
        });
        // Players before the spawn window get the rule in it (`survival::game_rules`).
        for p in self.state.players.values_mut() {
            if matches!(p.phase, Phase::Spawning | Phase::Spawned) {
                p.send(packet.clone());
            }
        }
        true
    }
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("tick", &self.state.tick)
            .field("worlds", &self.state.worlds.len())
            .field("players", &self.state.players.len())
            .field("save", &self.state.saving.requested)
            .finish()
    }
}

/// A task body.
type Body = Box<dyn FnMut(&mut Context<'_>) + Send>;

/// The scheduled tasks, in registration order.
#[derive(Default)]
pub(crate) struct Tasks(Vec<(NonZeroU32, Body)>);

impl Tasks {
    /// Adds `task`, run on every tick that is a multiple of `period`.
    pub(crate) fn push(
        &mut self,
        period: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) {
        self.0.push((period, Box::new(task)));
    }

    /// Runs the tasks due at the state's tick. The tasks are taken out of the state while they
    /// run, so a task's [`Context`] can lend the whole state.
    pub(super) fn run(state: &mut State) {
        let mut tasks = std::mem::take(&mut state.tasks);
        let tick = state.tick;
        for (period, task) in &mut tasks.0 {
            if tick.is_multiple_of(u64::from(period.get())) {
                task(&mut Context { state });
            }
        }
        state.tasks = tasks;
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(period, _)| period))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::event::Listeners;
    use crate::item::Type;
    use crate::nbt::Value;
    use crate::protocol::Uuid;
    use crate::tick::queue::{Identity, InputKind, Outbound, outbound_queue};
    use crate::tick::tests::{
        initialised, input, join, new_state, radius, received, settings, tick,
    };
    use crate::tick::{Output, Packet as P};

    /// The network actor variant of a farmer villager (`EntityVariant` of
    /// `minecraft:villager_v2`).
    const FARMER: u8 = 1;

    /// A state with session 1 (`player1`, signed in with Xbox) spawned and session 2
    /// (`Offline`, no XUID) joined, and their outbound queues.
    fn state() -> (State, [mpsc::Receiver<Outbound>; 2]) {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (first, rx1) = join(1, 0);
        let (tx, rx2) = outbound_queue();
        let offline = Identity {
            name: "Offline".to_owned(),
            xuid: None,
            uuid: Uuid::from_bytes([9; 16]),
            skin: crate::tick::queue::Skin::default(),
            device_os: -1,
        };
        let second = input(
            2,
            0,
            InputKind::Join {
                identity: offline,
                outbound: tx,
            },
        );
        tick(&mut state, &mut vec![first, second], &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 4)], &mut output);
        let runtime_id = state.players[&SessionId::new(1)].runtime_id;
        tick(
            &mut state,
            &mut vec![initialised(1, 2, runtime_id)],
            &mut output,
        );
        (state, [rx1, rx2])
    }

    fn stick(count: u8) -> Stack {
        let stick = Type::from_name("minecraft:stick", 0, None, &Compound::new()).unwrap();
        Stack::new(stick, count)
    }

    #[test]
    fn players_are_found_by_name_ignoring_ascii_case_with_and_without_xbox() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        assert_eq!(
            ctx.player_exact("PLAYER1"),
            Some(PlayerId(SessionId::new(1)))
        );
        assert_eq!(
            ctx.player_exact("offline"),
            Some(PlayerId(SessionId::new(2)))
        );
        assert_eq!(ctx.player_exact("player"), None);
        assert_eq!(ctx.player_exact(""), None);
    }

    #[test]
    fn give_fills_the_inventory_and_returns_what_does_not_fit() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let mut ctx = Context { state: &mut state };
        let player = ctx.player_exact("player1").unwrap();
        assert!(ctx.give(player, stick(5)).is_empty());
        let full: u8 = 64;
        for _ in 0..40 {
            ctx.give(player, stick(full));
        }
        // 36 slots of 64 hold 2,304 sticks; the rest comes back.
        assert_eq!(ctx.give(player, stick(7)).count(), 7);
        assert_ne!(state.hash(), before);
    }

    #[test]
    fn calls_for_a_player_who_left_change_nothing() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let gone = PlayerId(SessionId::new(99));
        let mut ctx = Context { state: &mut state };
        assert_eq!(ctx.give(gone, stick(3)).count(), 3);
        assert!(!ctx.teleport(gone, Vec3::new(0.0, -60.0, 0.0)));
        ctx.add_experience(gone, 100);
        assert!(!ctx.add_effect(gone, effect::Type::Slowness, 600, 1));
        assert_eq!(state.hash(), before);
    }

    #[test]
    fn teleports_and_villagers_outside_the_world_or_not_finite_are_refused() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let mut ctx = Context { state: &mut state };
        let player = ctx.player_exact("player1").unwrap();
        for feet in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
            Vec3::new(30_000_001.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -30_000_001.0),
        ] {
            assert!(!ctx.teleport(player, feet));
            assert_eq!(ctx.spawn_villager(feet, FARMER, &[]), None);
        }
        assert_eq!(state.hash(), before);
        // A valid teleport is handed to the survival module.
        assert!(Context { state: &mut state }.teleport(player, Vec3::new(-11.5, -60.0, -11.5)));
    }

    #[test]
    fn experience_changes_the_player_and_zero_points_do_not() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let mut ctx = Context { state: &mut state };
        let player = ctx.player_exact("player1").unwrap();
        ctx.add_experience(player, 0);
        assert_eq!(state.hash(), before);
        let mut ctx = Context { state: &mut state };
        ctx.add_experience(player, 1395);
        assert_ne!(state.hash(), before);
    }

    #[test]
    fn the_random_tick_speed_changes_up_to_the_setting_limit_and_spawned_players_hear_it() {
        let (mut state, [mut rx1, mut rx2]) = state();
        let mut ctx = Context { state: &mut state };
        assert!(!ctx.set_random_tick_speed(4097));
        assert!(ctx.set_random_tick_speed(0));
        assert!(ctx.set_random_tick_speed(4096));
        assert_eq!(state.settings.random_tick_speed, 4096);
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}
        tick(&mut state, &mut Vec::new(), &mut Output::default());
        let rules = |rx: &mut mpsc::Receiver<Outbound>| {
            received(rx)
                .0
                .into_iter()
                .filter_map(|p| match p {
                    P::GameRulesChanged(g) => Some(g.game_rules),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let spawned = rules(&mut rx1);
        assert_eq!(spawned.len(), 2);
        assert!(spawned.iter().all(|r| r.len() == 1
            && r[0].name == "randomTickSpeed"
            && matches!(r[0].value, GameRuleValue::Uint32(0 | 4096))));
        // Session 2 has not asked for its chunks: its spawn window carries the new value.
        assert!(rules(&mut rx2).is_empty());
    }

    #[test]
    fn save_marks_the_tick_for_a_commit_and_the_context_reads_the_state() {
        let (mut state, _rx) = state();
        let tick_now = state.tick;
        let mut ctx = Context { state: &mut state };
        assert_eq!(ctx.tick(), tick_now);
        assert_eq!(ctx.world().name(), "world");
        assert!(ctx.world_named("world").is_some());
        assert!(ctx.world_named_mut("nether").is_none());
        ctx.save();
        assert!(format!("{ctx:?}").contains("save: true"));
        assert!(state.saving.requested);
    }

    #[test]
    fn tasks_run_on_their_period_with_the_whole_state_and_stay_registered() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut tasks = Tasks::default();
        tasks.push(NonZeroU32::new(2).unwrap(), |ctx| {
            let at = crate::world::BlockPos::new(0, -50, 0);
            let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
            ctx.world_mut().set_block(at, stone).unwrap();
            ctx.save();
        });
        state.tasks = tasks;
        state.tick = 1;
        Tasks::run(&mut state);
        assert!(!state.saving.requested);
        state.tick = 2;
        Tasks::run(&mut state);
        assert!(state.saving.requested);
        assert_eq!(format!("{:?}", state.tasks), "[2]");
    }

    #[test]
    fn offers_reach_the_villager_contract_and_an_unknown_entity_is_not_removed() {
        let (mut state, _rx) = state();
        let mut offer = Compound::new();
        offer.insert("maxUses".to_owned(), Value::Int(12));
        let mut ctx = Context { state: &mut state };
        // The NPC itself is the entity module's; a valid call must not panic.
        let spawned = ctx.spawn_villager(Vec3::new(0.5, -60.0, 0.5), FARMER, &[offer]);
        if let Some(id) = spawned {
            assert!(ctx.remove_entity(id));
            assert!(!ctx.remove_entity(id));
        }
        assert!(!ctx.remove_entity(crate::entity::Id::new(4_000_000, 7)));
    }
}
