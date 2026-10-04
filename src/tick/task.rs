//! The [`Context`] every task and listener gets: the root handle of the game (PLAN.md 10.8, PMMP
//! `Server`), and the user's tasks, blocking jobs and data (PLAN.md 8.3, 10.6, 10.8).
//!
//! Build-time tasks ([`ServerBuilder::every`](crate::ServerBuilder::every)) run in registration
//! order after the tick's inputs, then the scheduled tasks due ([`schedule`]); blocking jobs
//! ([`job`]) answer through the inputs; user data ([`data`]) rides the save snapshots. Every one
//! of them runs only in the tick's serial step, so a replay of the input log runs them at the
//! same points with the same state.
//!
//! The villager and game rule methods (`spawn_villager`, `set_random_tick_speed`) are a thin
//! call into the rule module that owns the state.

#[cfg(test)]
mod crash;
mod data;
mod job;
mod schedule;

use std::fmt;
use std::num::NonZeroU32;

pub(crate) use self::data::UserData;
#[cfg(test)]
use self::data::registered;
pub(super) use self::data::{PlayerUser, placed, player_saved, save_player};
pub(super) use self::job::finish;
use super::queue::InputSender;
use super::{MAX_COORDINATE, Phase, PlayerKey, State, entity, player, trade};
use crate::config::{Difficulty, GameMode, RANDOM_TICK_SPEED};
use crate::math::Vec3;
use crate::player::{Id as PlayerId, Message, Player, Result as PlayerResult};
use crate::protocol::packet::{GAME_VERSION, GameRulesChanged, Packet};
use crate::protocol::{GameRule, GameRuleValue, Uuid};
use crate::world::World;

/// What a task or a listener can see and change: the engine time, the server, the worlds, the
/// players and the entities (PMMP `Server`, without its singleton: a callback gets the context).
pub struct Context<'a> {
    pub(super) state: &'a mut State,
}

/// Whether the player is online: its saved data has loaded, so it has a place in a world (PMMP
/// creates the `Player` then).
fn online(player: &Player) -> bool {
    player.phase != Phase::Loading
}

impl Context<'_> {
    /// The engine time: ticks since the world was created (20 per second).
    #[doc(alias = "Server::getTick")]
    pub fn tick(&self) -> u64 {
        self.state.tick
    }

    /// The server software's name.
    #[doc(alias = "Server::getName")]
    pub const NAME: &'static str = "Anchor";

    /// The Minecraft: Bedrock Edition version the server speaks, such as `1.26.50`.
    #[doc(alias = "Server::getVersion")]
    pub const VERSION: &'static str = GAME_VERSION;

    /// Anchor's own version.
    #[doc(alias = "Server::getPocketMineVersion")]
    pub const ANCHOR_VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// The server name the settings give (`motd`). The server list shows the value the server
    /// started with.
    #[doc(alias = "Server::getMotd")]
    pub fn motd(&self) -> &str {
        &self.state.settings.motd
    }

    /// The most players online at once (`max-players`).
    #[doc(alias = "Server::getMaxPlayers")]
    pub fn max_players(&self) -> u32 {
        self.state.settings.max_players
    }

    /// Changes the most players online at once until the server stops. Players already online
    /// stay; joins beyond it are refused. The server list shows the value the server started
    /// with.
    pub fn set_max_players(&mut self, max: u32) {
        self.state.settings.max_players = max;
    }

    /// The difficulty (`difficulty`).
    #[doc(alias = "Server::getDifficulty")]
    pub fn difficulty(&self) -> Difficulty {
        self.state.settings.difficulty
    }

    /// The game mode new players get (`gamemode`).
    #[doc(alias = "Server::getGamemode")]
    pub fn default_game_mode(&self) -> GameMode {
        self.state.settings.game_mode
    }

    /// Whether every join gets the server's game mode instead of the saved one
    /// (`force-gamemode`).
    #[doc(alias = "Server::getForceGamemode")]
    pub fn is_game_mode_forced(&self) -> bool {
        self.state.settings.force_game_mode
    }

    /// Whether dead players come back as spectators (`hardcore`).
    #[doc(alias = "Server::isHardcore")]
    pub fn is_hardcore(&self) -> bool {
        self.state.settings.hardcore
    }

    /// The largest chunk radius a player gets (`view-distance`).
    #[doc(alias = "Server::getViewDistance")]
    pub fn view_distance(&self) -> u8 {
        self.state.settings.view_distance
    }

    /// Whether player data is loaded and saved (`save-player-data`).
    #[doc(alias = "Server::shouldSavePlayerData")]
    pub fn should_save_player_data(&self) -> bool {
        self.state.settings.save_player_data
    }

    /// The online players, in [`Id`](PlayerId) order. A player is online from the moment its
    /// saved data has loaded until it leaves.
    #[doc(alias = "Server::getOnlinePlayers")]
    pub fn players(&self) -> impl Iterator<Item = &Player> + '_ {
        self.state.players.values().filter(|p| online(p))
    }

    /// How many players are online.
    pub fn player_count(&self) -> usize {
        self.players().count()
    }

    /// The online player `id`; `None` once it has left.
    pub fn player(&self, id: PlayerId) -> Option<&Player> {
        self.state.players.get(&id.session()).filter(|p| online(p))
    }

    /// The online player `id`, to change it; `None` once it has left.
    pub fn player_mut(&mut self, id: PlayerId) -> Option<&mut Player> {
        self.state
            .players
            .get_mut(&id.session())
            .filter(|p| online(p))
    }

    /// The online player whose name is `name`, ignoring ASCII case (PMMP compares with
    /// `strtolower`). A player without Xbox sign-in is keyed by its lowercase name, so it is
    /// found by an index search; only the players signed in with Xbox (keyed by XUID, at most
    /// `max-players`) are compared one by one. The lookups return an [`Id`](PlayerId), so the
    /// context stays free for [`player_mut`](Self::player_mut).
    #[doc(alias = "Server::getPlayerExact")]
    pub fn player_exact(&self, name: &str) -> Option<PlayerId> {
        let index = &self.state.index;
        let key = PlayerKey::Name(name.to_ascii_lowercase());
        let found = index.get(&key).or_else(|| {
            // XUID keys sort before every name key.
            index
                .range(..PlayerKey::Name(String::new()))
                .map(|(_, session)| session)
                .find(|s| {
                    self.state
                        .players
                        .get(s)
                        .is_some_and(|p| p.identity.name.eq_ignore_ascii_case(name))
                })
        })?;
        self.state
            .players
            .get(found)
            .filter(|p| online(p))
            .map(Player::id)
    }

    /// The online player whose name starts with `prefix` (ignoring ASCII case) and is the
    /// shortest of those, the first in [`Id`](PlayerId) order on a tie (PMMP
    /// `getPlayerByPrefix`). The players (at most `max-players`) are compared one by one.
    #[doc(alias = "Server::getPlayerByPrefix")]
    pub fn player_by_prefix(&self, prefix: &str) -> Option<PlayerId> {
        let starts = |name: &str| {
            name.as_bytes()
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
        };
        let mut best: Option<&Player> = None;
        for p in self.players() {
            let name = &p.identity.name;
            if starts(name) && best.is_none_or(|b| name.len() < b.identity.name.len()) {
                best = Some(p);
                if name.len() == prefix.len() {
                    break;
                }
            }
        }
        best.map(Player::id)
    }

    /// The online player whose UUID is `uuid` (PMMP `getPlayerByUUID`). The players (at most
    /// `max-players`) are compared one by one.
    #[doc(alias = "Server::getPlayerByUUID")]
    pub fn player_by_uuid(&self, uuid: Uuid) -> Option<PlayerId> {
        self.players()
            .find(|p| p.identity.uuid == uuid)
            .map(Player::id)
    }

    /// Shows `message` to every online player (PMMP's broadcast methods merged, PLAN.md 8.4);
    /// returns how many got it. Fails, sending nothing, for a message a client does not decode.
    #[doc(alias = "Server::broadcastMessage")]
    #[doc(alias = "Server::broadcastTip")]
    #[doc(alias = "Server::broadcastPopup")]
    #[doc(alias = "Server::broadcastTitle")]
    pub fn broadcast(&mut self, message: Message) -> PlayerResult<usize> {
        player::broadcast(self.state, message)
    }

    /// The default world: the one the settings name (`level-name`), where players join.
    #[doc(alias = "WorldManager::getDefaultWorld")]
    pub fn default_world(&self) -> &World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a server always has its default world at index 0"
        )]
        &self.state.worlds[0]
    }

    /// The default world, to change it.
    pub fn default_world_mut(&mut self) -> &mut World {
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
    #[doc(alias = "World::save")]
    pub fn save(&mut self) {
        self.state.saving.requested = true;
    }

    /// Spawns a villager-type NPC (PLAN.md 8.6: no AI, network actor `minecraft:villager_v2`)
    /// at `pos` (its feet) in `world`, showing `profession` (the actor variant) and trading
    /// `offers` in screen order, like
    /// [`spawn_custom_entity`](Self::spawn_custom_entity). Fails with
    /// [`custom::Error::Offer`](crate::custom::Error::Offer) for an offer
    /// [`set_trade_offers`](Self::set_trade_offers) refuses, and with
    /// [`custom::Error::NotSpawned`](crate::custom::Error::NotSpawned) if the world does not
    /// exist, `pos` is not finite or outside the world border, its chunk is not loaded, or the
    /// world is at its entity limit.
    pub fn spawn_villager(
        &mut self,
        world: crate::world::Id,
        pos: Vec3,
        profession: u8,
        offers: &[crate::custom::Offer],
    ) -> std::result::Result<crate::entity::Id, crate::custom::Error> {
        let offers = trade::new_offers(offers)?;
        let border = f64::from(MAX_COORDINATE);
        let w = super::world_api::index(self.state, world)
            .filter(|_| pos.is_finite() && [pos.x, pos.y, pos.z].iter().all(|c| c.abs() <= border))
            .ok_or(crate::custom::Error::NotSpawned)?;
        let npc = entity::Npc::Villager { profession, offers };
        entity::spawn_npc(self.state, w, pos, npc).ok_or(crate::custom::Error::NotSpawned)
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
                p.send_packet(packet.clone());
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

/// A build-time task body.
type Body = Box<dyn FnMut(&mut Context<'_>) + Send>;

/// The user's tasks and blocking jobs.
#[derive(Default)]
pub(crate) struct Tasks {
    /// The build-time tasks ([`ServerBuilder::every`](crate::ServerBuilder::every)), in
    /// registration order.
    every: Vec<(NonZeroU32, Body)>,
    /// The tasks scheduled while the server runs.
    queue: schedule::Queue,
    /// The blocking jobs waiting for their result.
    jobs: job::Jobs,
}

impl Tasks {
    /// Adds `task`, run on every tick that is a multiple of `period`.
    pub(crate) fn push(
        &mut self,
        period: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) {
        self.every.push((period, Box::new(task)));
    }

    /// Runs blocking jobs from now on: on `handle`'s blocking pool, their results into the tick's
    /// input queue. Until then (and in a replay, which never starts it) jobs run no work and
    /// wait for results from the input log.
    pub(crate) fn start(&mut self, handle: tokio::runtime::Handle, inputs: &InputSender) {
        self.jobs.start(handle, inputs);
    }

    /// Runs the tasks due at the state's tick: the build-time tasks on the multiples of their
    /// period, in registration order, then the scheduled tasks due. The build-time tasks are
    /// taken out of the state while they run, so a task's [`Context`] can lend the whole state.
    pub(super) fn run(state: &mut State) {
        let mut every = std::mem::take(&mut state.tasks.every);
        let tick = state.tick;
        for (period, task) in &mut every {
            if tick.is_multiple_of(u64::from(period.get())) {
                task(&mut Context { state });
            }
        }
        state.tasks.every = every;
        schedule::run(state);
    }

    /// The ids of the blocking jobs waiting for their result, in order.
    #[cfg(test)]
    pub(super) fn waiting_jobs(&self) -> Vec<u64> {
        self.jobs.waiting()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self { every, queue, jobs } = self;
        h.usize(every.len());
        for (period, _) in every {
            h.u64(u64::from(period.get()));
        }
        queue.hash_into(h);
        jobs.hash_into(h);
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let every: Vec<NonZeroU32> = self.every.iter().map(|(period, _)| *period).collect();
        f.debug_struct("Tasks")
            .field("every", &every)
            .field("scheduled", &self.queue.len())
            .field("jobs", &self.jobs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::event::Listeners;
    use crate::item::{Stack, Type};
    use crate::nbt::Compound;
    use crate::protocol::Uuid;
    use crate::tick::queue::SessionId;
    use crate::tick::queue::{Identity, InputKind, Outbound, outbound_queue};
    use crate::tick::tests::{
        initialised, input, join, new_state, radius, received, settings, tick,
    };
    use crate::tick::{Output, Packet as P};
    use crate::world::Id as WorldId;

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
            address: None,
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

    // pmmp: Server::getPlayerExact
    #[test]
    fn players_are_found_by_name_ignoring_ascii_case_with_and_without_xbox() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        assert_eq!(
            ctx.player_exact("PLAYER1"),
            Some(PlayerId::new(SessionId::new(1)))
        );
        assert_eq!(
            ctx.player_exact("offline"),
            Some(PlayerId::new(SessionId::new(2)))
        );
        assert_eq!(ctx.player_exact("player"), None);
        assert_eq!(ctx.player_exact(""), None);
    }

    // pmmp: Server::getOnlinePlayers
    #[test]
    fn players_are_lent_in_id_order_while_online() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        let ids: Vec<PlayerId> = ctx.players().map(Player::id).collect();
        assert_eq!(ids, [1, 2].map(|s| PlayerId::new(SessionId::new(s))));
        assert_eq!(ctx.player_count(), 2);
        assert_eq!(ctx.player(ids[1]).map(Player::name), Some("Offline"));
        // A player whose data is still loading is not online yet.
        state.players.get_mut(&SessionId::new(2)).unwrap().phase = Phase::Loading;
        let mut ctx = Context { state: &mut state };
        assert_eq!(ctx.player_count(), 1);
        assert!(ctx.player(ids[1]).is_none());
        assert!(ctx.player_mut(ids[1]).is_none());
        assert_eq!(ctx.player_exact("offline"), None);
        assert!(ctx.player_mut(ids[0]).is_some());
    }

    // pmmp: Server::getPlayerByUUID
    #[test]
    fn players_are_found_by_uuid_while_online() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        let second = PlayerId::new(SessionId::new(2));
        assert_eq!(ctx.player_by_uuid(Uuid::from_bytes([9; 16])), Some(second));
        assert_eq!(ctx.player_by_uuid(Uuid::from_bytes([7; 16])), None);
        state.players.get_mut(&SessionId::new(2)).unwrap().phase = Phase::Loading;
        let ctx = Context { state: &mut state };
        assert_eq!(ctx.player_by_uuid(Uuid::from_bytes([9; 16])), None);
    }

    // pmmp: Server::getPlayerByPrefix
    #[test]
    fn the_shortest_name_with_the_prefix_wins() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        let first = Some(PlayerId::new(SessionId::new(1)));
        let second = Some(PlayerId::new(SessionId::new(2)));
        assert_eq!(ctx.player_by_prefix("PL"), first);
        assert_eq!(ctx.player_by_prefix("off"), second);
        assert_eq!(ctx.player_by_prefix("offline"), second);
        assert_eq!(ctx.player_by_prefix("offlinex"), None);
        // "Offline" and "player1" are as long: the first in ID order wins.
        assert_eq!(ctx.player_by_prefix(""), first);
    }

    // pmmp: Server::getTick, Server::getName, Server::getVersion, Server::getPocketMineVersion, Server::getMotd, Server::getMaxPlayers, Server::getDifficulty, Server::getGamemode, Server::getForceGamemode, Server::isHardcore, Server::getViewDistance, Server::shouldSavePlayerData
    #[test]
    fn the_server_reads_its_settings_and_a_lower_player_limit_refuses_joins() {
        let (mut state, _rx) = state();
        let ctx = Context { state: &mut state };
        assert_eq!(Context::NAME, "Anchor");
        assert_eq!(Context::VERSION, GAME_VERSION);
        assert_eq!(Context::ANCHOR_VERSION, env!("CARGO_PKG_VERSION"));
        let config = crate::config::Config::default();
        assert_eq!(ctx.motd(), config.general.motd);
        assert_eq!(ctx.max_players(), config.general.max_players);
        assert_eq!(ctx.difficulty(), config.general.difficulty);
        assert_eq!(ctx.default_game_mode(), config.general.gamemode);
        assert_eq!(ctx.is_game_mode_forced(), config.general.force_gamemode);
        assert_eq!(ctx.is_hardcore(), config.general.hardcore);
        assert_eq!(ctx.view_distance(), config.general.view_distance);
        assert!(!ctx.should_save_player_data());
        assert_eq!(ctx.tick(), state.tick);
        Context { state: &mut state }.set_max_players(2);
        let (third, mut rx3) = crate::tick::tests::join(3, 0);
        tick(&mut state, &mut vec![third], &mut Output::default());
        assert!(received(&mut rx3).1.is_some(), "refused");
        assert_eq!(state.players.len(), 2);
    }

    // pmmp: Server::broadcastMessage, Server::broadcastTip, Server::broadcastPopup, Server::broadcastTitle
    #[test]
    fn broadcasts_reach_every_online_player_once() {
        let (mut state, mut rx) = state();
        for r in &mut rx {
            received(r);
        }
        let mut ctx = Context { state: &mut state };
        let tip = crate::player::Message::Tip("hi".to_owned());
        assert_eq!(ctx.broadcast(tip), Ok(2));
        let long = crate::player::Message::Popup("x".repeat(70_000));
        assert_eq!(ctx.broadcast(long), Err(crate::player::Error::TooLong));
        tick(&mut state, &mut Vec::new(), &mut Output::default());
        for r in &mut rx {
            let tips = received(r)
                .0
                .into_iter()
                .filter(|p| matches!(p, P::Text(t) if t.message == "hi"))
                .count();
            assert_eq!(tips, 1);
        }
    }

    #[test]
    fn calls_for_a_player_who_left_change_nothing() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let gone = PlayerId::new(SessionId::new(99));
        let mut ctx = Context { state: &mut state };
        assert!(ctx.player(gone).is_none());
        assert!(ctx.player_mut(gone).is_none());
        assert_eq!(state.hash(), before);
    }

    #[test]
    fn villagers_outside_the_world_or_not_finite_are_refused() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let mut ctx = Context { state: &mut state };
        for feet in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
            Vec3::new(30_000_001.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -30_000_001.0),
        ] {
            assert_eq!(
                ctx.spawn_villager(WorldId::DEFAULT, feet, FARMER, &[]),
                Err(crate::custom::Error::NotSpawned)
            );
        }
        let elsewhere = WorldId::new(7).unwrap();
        let at = Vec3::new(0.5, -60.0, 0.5);
        assert_eq!(
            ctx.spawn_villager(elsewhere, at, FARMER, &[]),
            Err(crate::custom::Error::NotSpawned),
            "a world the server does not have"
        );
        assert_eq!(state.hash(), before);
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

    // pmmp: World::save, WorldManager::getDefaultWorld, WorldManager::getWorldByName
    #[test]
    fn save_marks_the_tick_for_a_commit_and_the_context_reads_the_state() {
        let (mut state, _rx) = state();
        let tick_now = state.tick;
        let mut ctx = Context { state: &mut state };
        assert_eq!(ctx.tick(), tick_now);
        assert_eq!(ctx.default_world().name(), "world");
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
            ctx.default_world_mut().set_block(at, stone).unwrap();
            ctx.save();
        });
        state.tasks = tasks;
        state.tick = 1;
        Tasks::run(&mut state);
        assert!(!state.saving.requested);
        state.tick = 2;
        Tasks::run(&mut state);
        assert!(state.saving.requested);
        assert!(format!("{:?}", state.tasks).contains("every: [2], scheduled: 0"));
    }

    #[test]
    fn villagers_trade_the_typed_offers_and_refuse_an_invalid_one() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let offer = crate::custom::Offer::new(stick(4), stick(1), 12).with_tier(2);
        let mut ctx = Context { state: &mut state };
        let at = Vec3::new(0.5, -60.0, 0.5);
        let free = crate::custom::Offer::new(Stack::EMPTY, stick(1), 1);
        assert!(matches!(
            ctx.spawn_villager(WorldId::DEFAULT, at, FARMER, &[offer.clone(), free]),
            Err(crate::custom::Error::Offer { .. })
        ));
        assert_eq!(state.hash(), before, "a refused offer spawns nothing");
        let mut ctx = Context { state: &mut state };
        let id = ctx
            .spawn_villager(WorldId::DEFAULT, at, FARMER, std::slice::from_ref(&offer))
            .unwrap();
        assert_eq!(ctx.trade_offers(id), Some(vec![offer]));
    }
}
