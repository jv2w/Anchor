//! The [`Context`] every task and listener gets: the root handle of the game (PMMP `Server`), and
//! the user's tasks, blocking jobs and data.
//!
//! The build-time tasks ([`ServerBuilder::every`](crate::ServerBuilder::every)) and the scheduled
//! tasks ([`schedule`]) run in the tick's task step, in the order [`crate::task`] tells; blocking
//! jobs ([`job`]) and offline player loads ([`offline`]) answer through the inputs; user data
//! ([`data`]) rides the save snapshots. Every one of them runs only in the tick's serial step, so
//! a replay of the input log runs them at the same points with the same state.
//!
//! The villager and game rule methods (`spawn_villager`, `set_random_tick_speed`) take the NPC
//! and the game rule packet from the rule modules that own them.

#[cfg(test)]
mod crash;
mod data;
mod job;
mod offline;
mod schedule;

pub(crate) use data::UserData;
pub(super) use data::{PlayerUser, placed, player_saved, save_player};
pub(super) use job::finish;
pub use offline::Offline;
pub(super) use offline::{
    answer_loads as answer_offline_loads, flush as flush_offline, joined as offline_joined,
    requests as offline_requests, take_loads as take_offline_loads,
};

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::time::Duration;

use super::checkpoint::{self, Reader, Writer};
use super::queue::{Control, InputSender, SessionId};
use super::{Phase, State, entity, player, survival, trade};
use crate::config::{Difficulty, GameMode, RANDOM_TICK_SPEED_RANGE};
use crate::math::Vec3;
use crate::player::{Id as PlayerId, Message, Player, Result as PlayerResult};
use crate::protocol::Uuid;
use crate::protocol::packet::GAME_VERSION;
use crate::world::World;
#[cfg(test)]
use data::registered;

/// What a task or a listener can see and change: the engine time, the server, the worlds, the
/// players and the entities (PMMP `Server`, without its singleton: a callback gets the context).
pub struct Context<'a> {
    pub(super) state: &'a mut State,
}

impl Context<'_> {
    /// The fixed tick rate: the ticks the server runs a second while no tick runs late.
    #[doc(alias = "Server::TARGET_TICKS_PER_SECOND")]
    pub const TICKS_PER_SECOND: u32 = 20;

    /// The server software's name.
    #[doc(alias = "Server::getName")]
    pub const NAME: &'static str = "Anchor";

    /// The Minecraft: Bedrock Edition version the server speaks, such as `1.26.60`.
    #[doc(alias = "Server::getVersion")]
    pub const VERSION: &'static str = GAME_VERSION;

    /// Anchor's own version.
    #[doc(alias = "Server::getPocketMineVersion")]
    pub const ANCHOR_VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// The engine time: ticks since the world was created
    /// ([`TICKS_PER_SECOND`](Self::TICKS_PER_SECOND) a second).
    #[doc(alias = "Server::getTick")]
    pub fn tick(&self) -> u64 {
        self.state.tick
    }

    /// The server name the server list shows: `motd`, or what
    /// [`set_motd`](Self::set_motd) set since.
    #[doc(alias = "Server::getMotd")]
    #[doc(alias = "Network::getName")]
    pub fn motd(&self) -> &str {
        &self.state.settings.motd
    }

    /// The connections the networking holds, logged in or not, as it last reported (every
    /// second when the count changed).
    #[doc(alias = "Network::getConnectionCount")]
    pub fn connection_count(&self) -> usize {
        usize::try_from(self.state.network.connections().0).unwrap_or(usize::MAX)
    }

    /// The connections whose login was verified, as the networking last reported.
    #[doc(alias = "Network::getValidConnectionCount")]
    pub fn valid_connection_count(&self) -> usize {
        usize::try_from(self.state.network.connections().1).unwrap_or(usize::MAX)
    }

    /// The ticks a second the latest measured tick allows:
    /// [`TICKS_PER_SECOND`](Self::TICKS_PER_SECOND) unless it took longer than one tick at that
    /// rate (PMMP's formula). Every tick's time is measured off the game logic and read as a logged
    /// input by the next tick, so a replay reads the same.
    #[doc(alias = "Server::getTicksPerSecond")]
    pub fn tps(&self) -> f64 {
        self.state.network.tps()
    }

    /// The mean of [`tps`](Self::tps) over the last 20 measured ticks.
    #[doc(alias = "Server::getTicksPerSecondAverage")]
    pub fn average_tps(&self) -> f64 {
        self.state.network.average_tps()
    }

    /// How long the latest measured tick took (PMMP gives it as a percentage of 50 ms).
    #[doc(alias = "Server::getTickUsage")]
    #[doc(alias = "World::getTickRateTime")]
    pub fn mspt(&self) -> Duration {
        self.state.network.mspt()
    }

    /// The mean of [`mspt`](Self::mspt) over the last 20 measured ticks.
    #[doc(alias = "Server::getTickUsageAverage")]
    pub fn average_mspt(&self) -> Duration {
        self.state.network.average_mspt()
    }

    /// The most players at once (`max-players`), those whose saved data is still loading
    /// included.
    #[doc(alias = "Server::getMaxPlayers")]
    pub fn max_players(&self) -> u32 {
        self.state.settings.max_players
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

    /// The largest chunk radius a player gets (`view-distance`), in a world without its own
    /// (`[worlds.<name>]`).
    #[doc(alias = "Server::getViewDistance")]
    pub fn view_distance(&self) -> u8 {
        self.state.settings.view.view_distance
    }

    /// Whether player data is loaded and saved (`save-player-data`).
    #[doc(alias = "Server::shouldSavePlayerData")]
    pub fn should_save_player_data(&self) -> bool {
        self.state.settings.save_player_data
    }

    /// The players in the game, in [`Id`](PlayerId) order. A player is in the game from the
    /// moment its saved data has loaded until it leaves; a player kicked or transferred leaves
    /// at the end of the tick, and until then is here with
    /// [`Player::is_online`](crate::player::Player::is_online) false.
    #[doc(alias = "Server::getOnlinePlayers")]
    pub fn players(&self) -> impl Iterator<Item = &Player> + '_ {
        self.state.players.values().filter(|p| in_game(p))
    }

    /// The number of players in the game ([`players`](Self::players)).
    pub fn player_count(&self) -> usize {
        self.players().count()
    }

    /// The player `id` while it is in the game ([`players`](Self::players)); `None` once it has
    /// left.
    pub fn player(&self, id: PlayerId) -> Option<&Player> {
        self.state.players.get(&id.session()).filter(|p| in_game(p))
    }

    /// The player in the game whose name is `name`, ignoring ASCII case (PMMP compares with
    /// `strtolower`), the first in [`Id`](PlayerId) order if players signed in with Xbox share
    /// it. An index search. The lookups return an [`Id`](PlayerId), so the context stays free for
    /// [`player_mut`](Self::player_mut).
    #[doc(alias = "Server::getPlayerExact")]
    pub fn player_exact(&self, name: &str) -> Option<PlayerId> {
        self.state
            .names
            .range((name.to_ascii_lowercase(), SessionId::new(0))..)
            .take_while(|(n, _)| n.eq_ignore_ascii_case(name))
            .find_map(|&(_, session)| self.player(PlayerId::new(session)))
            .map(Player::id)
    }

    /// The player in the game whose name starts with `prefix` (ignoring ASCII case) and is the
    /// shortest of those, the first in [`Id`](PlayerId) order on a tie (PMMP
    /// `getPlayerByPrefix`). Only the names with the prefix are looked at.
    #[doc(alias = "Server::getPlayerByPrefix")]
    pub fn player_by_prefix(&self, prefix: &str) -> Option<PlayerId> {
        let starts = |name: &str| {
            name.as_bytes()
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
        };
        // Lowercase names with the prefix follow it in order: the scan stops at the first other.
        self.state
            .names
            .range((prefix.to_ascii_lowercase(), SessionId::new(0))..)
            .take_while(|(name, _)| starts(name))
            .filter(|&&(_, session)| self.player(PlayerId::new(session)).is_some())
            .min_by_key(|(name, session)| (name.len(), *session))
            .map(|&(_, session)| PlayerId::new(session))
    }

    /// The player in the game whose UUID is `uuid` (PMMP `getPlayerByUUID`), the first in
    /// [`Id`](PlayerId) order if logins share it. An index search.
    #[doc(alias = "Server::getPlayerByUUID")]
    pub fn player_by_uuid(&self, uuid: Uuid) -> Option<PlayerId> {
        self.state
            .uuids
            .range((uuid, SessionId::new(0))..)
            .take_while(|(u, _)| *u == uuid)
            .find_map(|&(_, session)| self.player(PlayerId::new(session)))
            .map(Player::id)
    }

    /// The default world, where players join: the one the settings name (`level-name`) unless
    /// [`set_default_world`](Self::set_default_world) named another.
    #[doc(alias = "WorldManager::getDefaultWorld")]
    pub fn default_world(&self) -> &World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the default world is always one of the server's, and loaded"
        )]
        &self.state.worlds[self.state.catalog.default_world()]
    }

    /// The loaded world named `name`, if any.
    #[doc(alias = "WorldManager::getWorldByName")]
    pub fn world_named(&self, name: &str) -> Option<&World> {
        let index = self.state.catalog.named(name)?;
        self.world(crate::world::Id::of(index))
    }

    /// Changes the server name the server list shows (the RakNet pong and NetherNet's
    /// `GET /v1/join`) until the server stops; the networking shows it once this tick ends.
    /// False, changing nothing, for a name the `motd` setting refuses (empty, over 256 bytes, or
    /// with `;` or control characters), or past 256 network changes in one tick.
    #[doc(alias = "Network::setName")]
    #[doc(alias = "Network::updateName")]
    pub fn set_motd(&mut self, motd: impl Into<String>) -> bool {
        let motd = motd.into();
        if !crate::config::is_motd(&motd)
            || !self.state.network.control(Control::Motd(motd.clone()))
        {
            return false;
        }
        self.state.settings.motd = motd;
        true
    }

    /// Refuses new connections from `ip` for `duration` from when the networking reads it, at
    /// the end of this tick; [`Duration::MAX`] lasts until
    /// [`unblock_address`](Self::unblock_address). A block of an address already blocked
    /// replaces its end, as PMMP `Network::blockAddress` does. Players already connected from it
    /// stay ([`Player::kick`](crate::player::Player::kick) them). False, changing nothing, past
    /// 256 network changes in one tick; at most 4,096 addresses are blocked at once, and the
    /// networking refuses a block past that with a warning.
    #[doc(alias = "Network::blockAddress")]
    pub fn block_address(&mut self, ip: IpAddr, duration: Duration) -> bool {
        self.state.network.control(Control::Block { ip, duration })
    }

    /// Accepts connections from `ip` again, at the end of this tick. False, changing nothing,
    /// past 256 network changes in one tick.
    #[doc(alias = "Network::unblockAddress")]
    pub fn unblock_address(&mut self, ip: IpAddr) -> bool {
        self.state.network.control(Control::Unblock(ip))
    }

    /// Changes the most players at once until the server stops. Players already in stay; joins
    /// beyond it are refused. The server list shows the value the server started with, and the
    /// networking holds at most twice that value of joined and joining players (each takes a
    /// share of the tick's input queue, sized at start), so a value above it lets no more
    /// players in.
    pub fn set_max_players(&mut self, max: u32) {
        self.state.settings.max_players = max;
    }

    /// Lends the player `id` to change while it is in the game ([`players`](Self::players));
    /// `None` once it has left.
    pub fn player_mut(&mut self, id: PlayerId) -> Option<&mut Player> {
        self.state
            .players
            .get_mut(&id.session())
            .filter(|p| in_game(p))
    }

    /// Shows `message` to every player in the game that is not leaving (PMMP's broadcast methods
    /// merged); returns how many got it. Fails, sending nothing, for a message a client does not
    /// decode.
    #[doc(alias = "Server::broadcastMessage")]
    #[doc(alias = "Server::broadcastTip")]
    #[doc(alias = "Server::broadcastPopup")]
    #[doc(alias = "Server::broadcastTitle")]
    pub fn broadcast(&mut self, message: Message) -> PlayerResult<usize> {
        player::broadcast(self.state, message)
    }

    /// Lends the default world to change.
    pub fn default_world_mut(&mut self) -> &mut World {
        let index = self.state.catalog.default_world();
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the default world is always one of the server's, and loaded"
        )]
        &mut self.state.worlds[index]
    }

    /// Lends the loaded world named `name` to change, if any.
    pub fn world_named_mut(&mut self, name: &str) -> Option<&mut World> {
        let index = self.state.catalog.named(name)?;
        self.world_mut(crate::world::Id::of(index))
    }

    /// Asks for a save: the next snapshot commits every persistent world and, with
    /// `save-player-data`, the player data, together, as one commit. It is taken at the end of
    /// this tick, or, while the save before is still being committed, at the first tick boundary
    /// after that commit, with the changes of the ticks between. Calling it again before that
    /// snapshot changes nothing.
    #[doc(alias = "World::save")]
    pub fn save(&mut self) {
        self.state.saving.requested = true;
    }

    /// Saves as [`save`](Self::save) does, then copies every open data file, as that save left
    /// it, into its own backup folder (`data/backups/<file>/<unix seconds>.redb`, a world's in
    /// `data/backups/worlds/<name>/`) while the server runs, keeping each file's 3 newest backups
    /// (those made at start included). The save thread copies it in steps between its
    /// other work, so players do not wait for it; what came of it is in the log, not here: a
    /// backup asked for while another is being made is not made, and a failed one leaves no
    /// file. A stop waits for the backup being made. Calling it again before the save changes
    /// nothing. PMMP has no backup.
    ///
    /// Returns `false`, asking for nothing, when backups are off (`backups = false`).
    pub fn backup(&mut self) -> bool {
        if !self.state.settings.backups {
            return false;
        }
        self.state.saving.requested = true;
        self.state.saving.backup = true;
        true
    }

    /// Spawns a villager-type NPC (no AI, network actor `minecraft:villager_v2`) at `pos` (its
    /// feet) in `world`, showing `profession` (the actor variant) and trading `offers` in screen
    /// order, like [`spawn_custom_entity`](Self::spawn_custom_entity). Fails with
    /// [`custom::Error::Offer`](crate::custom::Error::Offer) for offers
    /// [`custom::check_offers`](crate::custom::check_offers) refuses, and with
    /// [`custom::Error::NotSpawned`](crate::custom::Error::NotSpawned) where no villager can be
    /// spawned (that variant tells where).
    pub fn spawn_villager(
        &mut self,
        world: crate::world::Id,
        pos: Vec3,
        profession: u8,
        offers: &[crate::custom::Offer],
    ) -> std::result::Result<crate::entity::Id, crate::custom::Error> {
        let offers = trade::new_offers(offers)?;
        let w =
            super::world_api::index(self.state, world).ok_or(crate::custom::Error::NotSpawned)?;
        let npc = entity::Npc::Villager { profession, offers };
        entity::spawn_npc(self.state, w, pos, npc).ok_or(crate::custom::Error::NotSpawned)
    }

    /// Sets the random block ticks per sub-chunk per tick (the `randomTickSpeed` game rule;
    /// `random-tick-speed` sets it at start) until the server stops, and tells the players.
    /// False, changing nothing, above the setting's limit (4,096).
    pub fn set_random_tick_speed(&mut self, speed: u32) -> bool {
        if !RANDOM_TICK_SPEED_RANGE.contains(&speed) {
            return false;
        }
        self.state.settings.random_tick_speed = speed;
        let packet = survival::random_tick_speed_changed(speed);
        // A player still loading gets the rule in its spawn window (`survival::game_rules`); a
        // joined one whose window has not gone out yet gets it twice, which does no harm.
        for p in self.state.players.values_mut() {
            if in_game(p) {
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

/// The user's tasks, blocking jobs and offline player holds.
#[derive(Default)]
pub(crate) struct Tasks {
    /// The build-time tasks ([`ServerBuilder::every`](crate::ServerBuilder::every)), in
    /// registration order.
    every: Vec<(NonZeroU32, Body)>,
    /// The tasks scheduled while the server runs.
    queue: schedule::Queue,
    /// The blocking jobs waiting for their result.
    jobs: job::Jobs,
    /// The offline players' records held and the loads waiting for the save thread.
    offline: offline::Holds,
}

impl Tasks {
    /// Writes what changes while the server runs: the build-time tasks are code, fixed at build
    /// (their periods are in the registration fingerprint).
    pub(super) fn checkpoint(&self, w: &mut Writer) {
        let Self {
            every: _,
            queue,
            jobs,
            offline,
        } = self;
        queue.checkpoint(w);
        jobs.checkpoint(w);
        offline.checkpoint(w);
    }

    /// The ids of the blocking jobs waiting for their result, in order.
    #[cfg(test)]
    pub(super) fn waiting_jobs(&self) -> Vec<u64> {
        self.jobs.waiting()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, data: &UserData, h: &mut super::Fnv) {
        let Self {
            every,
            queue,
            jobs,
            offline,
        } = self;
        h.usize(every.len());
        for (period, _) in every {
            h.u64(u64::from(period.get()));
        }
        queue.hash_into(h);
        jobs.hash_into(h);
        offline.hash_into(data, h);
    }

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

    /// Reads what [`Self::checkpoint`] wrote into the tasks registered at build.
    pub(super) fn restore(&mut self, r: &mut Reader<'_>) -> checkpoint::Result<()> {
        self.queue.restore(r)?;
        self.jobs.restore(r)?;
        self.offline.restore(r)
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let every: Vec<NonZeroU32> = self.every.iter().map(|(period, _)| *period).collect();
        f.debug_struct("Tasks")
            .field("every", &every)
            .field("scheduled", &self.queue.len())
            .field("jobs", &self.jobs)
            .field("offline", &self.offline)
            .finish()
    }
}

/// A build-time task body.
type Body = Box<dyn FnMut(&mut Context<'_>) + Send>;

/// Tells whether the player is in the game: its saved data has loaded, so it has a place in a
/// world (PMMP creates the `Player` then).
fn in_game(player: &Player) -> bool {
    player.phase != Phase::Loading
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use crate::event::Listeners;
    use crate::item::{Stack, Type};
    use crate::nbt::Compound;
    use crate::protocol::GameRuleValue;
    use crate::tick::queue::{Client, Identity, InputKind, Outbound, Skin, outbound_queue};
    use crate::tick::tests::{
        initialised, input, join, new_state, radius, received, settings, tick,
    };
    use crate::tick::{Output, Packet as P};
    use crate::world::Id as WorldId;

    /// The network actor variant of a farmer villager (`EntityVariant` of
    /// `minecraft:villager_v2`).
    const FARMER: u8 = 1;

    /// Makes a state with session 1 (`player1`, signed in with Xbox) spawned and session 2
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
            skin: Skin::default(),
            device_os: -1,
            address: None,
            client: Client::default(),
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

    // api: Server::getPlayerExact
    #[test]
    fn players_are_found_by_name_ignoring_ascii_case_with_and_without_xbox() {
        let (mut state, _rx) = state();
        let cx = Context { state: &mut state };
        assert_eq!(
            cx.player_exact("PLAYER1"),
            Some(PlayerId::new(SessionId::new(1)))
        );
        assert_eq!(
            cx.player_exact("offline"),
            Some(PlayerId::new(SessionId::new(2)))
        );
        assert_eq!(cx.player_exact("player"), None);
        assert_eq!(cx.player_exact(""), None);
    }

    // api: Server::getOnlinePlayers
    #[test]
    fn players_are_lent_in_id_order_while_in_the_game() {
        let (mut state, _rx) = state();
        let cx = Context { state: &mut state };
        let ids: Vec<PlayerId> = cx.players().map(Player::id).collect();
        assert_eq!(ids, [1, 2].map(|s| PlayerId::new(SessionId::new(s))));
        assert_eq!(cx.player_count(), 2);
        assert_eq!(cx.player(ids[1]).map(Player::name), Some("Offline"));
        // A player whose data is still loading is not in the game yet.
        state.players.get_mut(&SessionId::new(2)).unwrap().phase = Phase::Loading;
        let mut cx = Context { state: &mut state };
        assert_eq!(cx.player_count(), 1);
        assert!(cx.player(ids[1]).is_none());
        assert!(cx.player_mut(ids[1]).is_none());
        assert_eq!(cx.player_exact("offline"), None);
        assert!(cx.player_mut(ids[0]).is_some());
    }

    // api: Server::getPlayerByUUID
    #[test]
    fn players_are_found_by_uuid_while_in_the_game() {
        let (mut state, _rx) = state();
        let cx = Context { state: &mut state };
        let second = PlayerId::new(SessionId::new(2));
        assert_eq!(cx.player_by_uuid(Uuid::from_bytes([9; 16])), Some(second));
        assert_eq!(cx.player_by_uuid(Uuid::from_bytes([7; 16])), None);
        state.players.get_mut(&SessionId::new(2)).unwrap().phase = Phase::Loading;
        let cx = Context { state: &mut state };
        assert_eq!(cx.player_by_uuid(Uuid::from_bytes([9; 16])), None);
    }

    // api: Server::getPlayerByPrefix
    #[test]
    fn the_shortest_name_with_the_prefix_wins() {
        let (mut state, _rx) = state();
        let cx = Context { state: &mut state };
        let first = Some(PlayerId::new(SessionId::new(1)));
        let second = Some(PlayerId::new(SessionId::new(2)));
        assert_eq!(cx.player_by_prefix("PL"), first);
        assert_eq!(cx.player_by_prefix("off"), second);
        assert_eq!(cx.player_by_prefix("offline"), second);
        assert_eq!(cx.player_by_prefix("offlinex"), None);
        // "Offline" and "player1" are as long: the first in ID order wins.
        assert_eq!(cx.player_by_prefix(""), first);
    }

    // api: Server::getTick, Server::getName, Server::getVersion, Server::getPocketMineVersion
    // api: Server::getMotd, Server::getMaxPlayers, Server::getDifficulty, Server::getGamemode
    // api: Server::getForceGamemode, Server::isHardcore, Server::getViewDistance
    // api: Server::shouldSavePlayerData
    #[test]
    fn the_server_reads_its_settings_and_a_lower_player_limit_refuses_joins() {
        let (mut state, _rx) = state();
        let cx = Context { state: &mut state };
        assert_eq!(Context::NAME, "Anchor");
        assert_eq!(Context::VERSION, GAME_VERSION);
        assert_eq!(Context::ANCHOR_VERSION, env!("CARGO_PKG_VERSION"));
        let config = crate::config::Config::default();
        assert_eq!(cx.motd(), config.general.motd);
        assert_eq!(cx.max_players(), config.general.max_players);
        assert_eq!(cx.difficulty(), config.general.difficulty);
        assert_eq!(cx.default_game_mode(), config.general.gamemode);
        assert_eq!(cx.is_game_mode_forced(), config.general.force_gamemode);
        assert_eq!(cx.is_hardcore(), config.general.hardcore);
        assert_eq!(cx.view_distance(), config.general.view_distance);
        assert!(!cx.should_save_player_data());
        assert_eq!(cx.tick(), state.tick);
        Context { state: &mut state }.set_max_players(2);
        let (third, mut rx3) = join(3, 0);
        tick(&mut state, &mut vec![third], &mut Output::default());
        assert!(received(&mut rx3).1.is_some(), "refused");
        assert_eq!(state.players.len(), 2);
    }

    // api: Server::broadcastMessage, Server::broadcastTip, Server::broadcastPopup
    // api: Server::broadcastTitle
    #[test]
    fn broadcasts_reach_every_player_in_the_game_once() {
        let (mut state, mut rx) = state();
        for r in &mut rx {
            received(r);
        }
        let mut cx = Context { state: &mut state };
        let tip = Message::Tip("hi".to_owned());
        assert_eq!(cx.broadcast(tip), Ok(2));
        let long = Message::Popup("x".repeat(70_000));
        assert_eq!(cx.broadcast(long), Err(crate::player::Error::TooLong));
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
        let mut cx = Context { state: &mut state };
        assert!(cx.player(gone).is_none());
        assert!(cx.player_mut(gone).is_none());
        assert_eq!(state.hash(), before);
    }

    #[test]
    fn villagers_outside_the_world_or_not_finite_are_refused() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let mut cx = Context { state: &mut state };
        for feet in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
            Vec3::new(30_000_001.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -30_000_001.0),
            Vec3::new(0.0, 30_000_001.0, 0.0),
            Vec3::new(0.0, -30_000_001.0, 0.0),
        ] {
            assert_eq!(
                cx.spawn_villager(WorldId::DEFAULT, feet, FARMER, &[]),
                Err(crate::custom::Error::NotSpawned)
            );
        }
        let elsewhere = WorldId::new(7).unwrap();
        let at = Vec3::new(0.5, -60.0, 0.5);
        assert_eq!(
            cx.spawn_villager(elsewhere, at, FARMER, &[]),
            Err(crate::custom::Error::NotSpawned),
            "a world the server does not have"
        );
        assert_eq!(state.hash(), before);
    }

    #[test]
    fn the_random_tick_speed_changes_up_to_the_setting_limit_and_joined_players_hear_it() {
        let (mut state, [mut rx1, mut rx2]) = state();
        let mut cx = Context { state: &mut state };
        assert!(!cx.set_random_tick_speed(4097));
        assert!(cx.set_random_tick_speed(0));
        assert!(cx.set_random_tick_speed(4096));
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
        // Session 2 has not asked for its chunks: it hears the rule now, and its spawn window
        // carries the new value again.
        assert_eq!(rules(&mut rx2), spawned);
    }

    // api: World::save, WorldManager::getDefaultWorld, WorldManager::getWorldByName
    #[test]
    fn save_marks_the_tick_for_a_commit_and_the_context_reads_the_state() {
        let (mut state, _rx) = state();
        let tick_now = state.tick;
        let mut cx = Context { state: &mut state };
        assert_eq!(cx.tick(), tick_now);
        assert_eq!(cx.default_world().name(), "world");
        assert!(cx.world_named("world").is_some());
        assert!(cx.world_named_mut("nether").is_none());
        cx.save();
        assert!(format!("{cx:?}").contains("save: true"));
        assert!(state.saving.requested);
    }

    #[test]
    fn tasks_run_on_their_period_with_the_whole_state_and_stay_registered() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut tasks = Tasks::default();
        tasks.push(NonZeroU32::new(2).unwrap(), |cx| {
            let at = crate::world::BlockPos::new(0, -50, 0);
            let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
            cx.default_world_mut().set_block(at, stone).unwrap();
            cx.save();
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
    fn shutdown_runs_deferred_callbacks_before_the_final_snapshot() {
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            |_: &mut crate::event::PlayerQuit, cx: &mut Context<'_>| {
                cx.defer(|cx| *cx.data_mut::<u32>().expect("registered") += 1)
                    .expect("room");
            },
        );
        let mut state = new_state(settings(|_| {}), listeners);
        state.data = registered::<u32, u32>("visits", "quits");
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut Output::default());
        state.close_all(&mut Output::default());
        let snapshot =
            crate::tick::save::snapshot(&mut state, &mut crate::tick::InputLog::default(), true);
        let Some(crate::save::Request::Commit(snapshot)) = snapshot else {
            panic!("the final snapshot is always taken");
        };
        assert_eq!(snapshot.user, [("quits".to_owned(), b"1".to_vec())]);
    }

    fn stick(count: u8) -> Stack {
        let stick = Type::from_name("minecraft:stick", 0, None, &Compound::new()).unwrap();
        Stack::new(stick, count)
    }

    #[test]
    fn villagers_trade_the_typed_offers_and_refuse_an_invalid_one() {
        let (mut state, _rx) = state();
        let before = state.hash();
        let offer = crate::custom::Offer::new(stick(4), stick(1), 12).with_tier(2);
        let mut cx = Context { state: &mut state };
        let at = Vec3::new(0.5, -60.0, 0.5);
        let free = crate::custom::Offer::new(Stack::EMPTY, stick(1), 1);
        assert!(matches!(
            cx.spawn_villager(WorldId::DEFAULT, at, FARMER, &[offer.clone(), free]),
            Err(crate::custom::Error::Offer { .. })
        ));
        assert_eq!(state.hash(), before, "a refused offer spawns nothing");
        let mut cx = Context { state: &mut state };
        let id = cx
            .spawn_villager(WorldId::DEFAULT, at, FARMER, std::slice::from_ref(&offer))
            .unwrap();
        assert_eq!(cx.trade_offers(id), Some(vec![offer]));
    }
}
