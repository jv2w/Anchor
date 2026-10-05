//! The public player API on [`Player`]: who it is, where it is, the messages it is shown, and
//! leaving (its survival state is in `survival::api`); and the per-tick sync that does what the
//! setters marked (PLAN.md 10.8).
//!
//! A setter changes the value at once (after [`Player::teleport`], [`Player::position`] is the
//! new place) and marks what the world and the other players must see. [`sync`] runs after the
//! inputs, tasks and rules and before the views move: it disconnects the players that were kicked
//! or transferred, moves the teleported ones and sends what the survival setters changed. No call
//! raises an event; what follows from it at the end of the tick does (a kicked player's
//! [`PlayerQuit`]). [`Context::kick_player`], [`Context::transfer_player`] and
//! [`Context::teleport_player`] are the kick, the transfer and the teleport with their events
//! ([`crate::action::Kick`], [`crate::action::Transfer`], [`crate::action::Teleport`]).

use std::net::IpAddr;
use std::ops::Bound;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info};

use super::queue::SessionId;
use super::{Context, MAX_COORDINATE, Output, Phase, Player, State, dispatch, survival};
use crate::event::{Actor, EntityTeleport, PlayerKick, PlayerQuit, PlayerTransfer};
use crate::math::Vec3;
use crate::player::{Error, Id, MAX_TEXT, Message, Packets, Result};
use crate::protocol::Uuid;
use crate::protocol::consts::{DeviceOs, DisconnectReason, InputMode};
use crate::protocol::packet::{Disconnect, Packet, Transfer};
use crate::save::WorldIndex;
use crate::world;

/// The longest transfer address: the longest host name.
const MAX_ADDRESS: usize = 253;
/// The client's "disconnected" text, for a kick without a reason (PMMP
/// `disconnectionScreen_noReason`).
const NO_REASON: &str = "disconnectionScreen.noReason";
/// The log message of a transfer unless one is given.
const TRANSFERRED: &str = "Moved to another server";
/// The default quit message: BDS's yellow "left the game" line, which the client translates with
/// the player's name (PMMP `Player::getLeaveMessage`). A [`PlayerQuit`] message left at it is sent
/// so; any other is sent as it is.
pub(super) const LEFT: &str = "§e%multiplayer.player.left";
/// The lines a player's screen shows at once unless set (PMMP `Player::getScreenLineHeight`).
const DEFAULT_LINE_HEIGHT: u32 = 7;

/// What the API marked on a player for the end of the tick, and the chat limit.
#[derive(Debug, Default)]
pub(super) struct Marks {
    /// The name chat shows (PMMP `displayName`); `None` is the player's name. Not saved.
    display_name: Option<String>,
    /// Why the player leaves at the end of the tick.
    leave: Option<Leave>,
    /// Where the player moves at the end of the tick: the world and the eye position, checked.
    teleport: Option<(WorldIndex, Vec3)>,
    /// The tick of the player's last chat line and the lines it sent in that tick (PMMP
    /// `messageCounter`).
    pub(super) chat: (u64, u8),
    /// The lines the player's screen shows at once (PMMP `lineHeight`); `None` is 7. Not saved.
    line_height: Option<u32>,
}

/// How a player leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Leave {
    /// Disconnected with a reason (for the log and its screen).
    Kick {
        reason: String,
        /// What its screen shows instead of the reason.
        screen_message: Option<String>,
        /// What the other players see in chat; empty for nothing.
        quit_message: String,
    },
    /// Sent to another server.
    Transfer {
        address: String,
        port: u16,
        /// For the log.
        message: String,
    },
}

impl Leave {
    /// Why the player left, for the log and [`PlayerQuit`].
    fn reason(&self) -> &str {
        match self {
            Self::Kick { reason, .. } => reason,
            Self::Transfer { message, .. } => message,
        }
    }

    /// The last packet the player gets, and the message the other players see.
    fn into_parts(self) -> (Packet, String) {
        match self {
            Self::Kick {
                reason,
                screen_message,
                quit_message,
            } => {
                let message = screen_message.unwrap_or(reason);
                let packet = Packet::Disconnect(Disconnect {
                    reason: DisconnectReason::Kicked.into(),
                    hide_disconnection_screen: false,
                    message: if message.is_empty() {
                        NO_REASON.to_owned()
                    } else {
                        message
                    },
                    filtered_message: String::new(),
                });
                (packet, quit_message)
            }
            Self::Transfer { address, port, .. } => {
                let packet = Packet::Transfer(Box::new(Transfer {
                    address,
                    port,
                    reload_world: false,
                    gathering_join_info: None,
                }));
                (packet, LEFT.to_owned())
            }
        }
    }
}

super::checkpoint::fields!(Marks {
    display_name,
    leave,
    teleport,
    chat,
    line_height,
});
super::checkpoint::variants!(Leave {
    0 => Kick {
        reason,
        screen_message,
        quit_message
    },
    1 => Transfer {
        address,
        port,
        message
    },
});

/// What the player's connection and saved data tell besides its login: its latest ping and its
/// play history, in milliseconds since the Unix epoch.
#[derive(Debug, Default)]
pub(super) struct Presence {
    /// The latest round trip time in milliseconds ([`Measure::Ping`](super::queue::Measure));
    /// `None` until one arrived.
    ping_ms: Option<u32>,
    /// When the player first joined (`firstPlayed`).
    first_played: i64,
    /// When the player's data was saved before this join (`lastPlayed`).
    last_played: i64,
    /// The engine tick the saved data was placed at, which with the join time dates a save.
    placed_tick: u64,
}

super::checkpoint::fields!(Presence {
    ping_ms,
    first_played,
    last_played,
    placed_tick,
});

impl Presence {
    pub(super) fn set_ping(&mut self, millis: u32) {
        self.ping_ms = Some(millis);
    }

    /// Takes the play history of the saved data placed at engine tick `tick` (each `None` for a
    /// new player, which gets `join_ms`, as PMMP gives it the time of its first spawn).
    pub(super) fn place(
        &mut self,
        (first, last): (Option<i64>, Option<i64>),
        join_ms: i64,
        tick: u64,
    ) {
        self.first_played = first.unwrap_or(join_ms);
        self.last_played = last.unwrap_or(join_ms);
        self.placed_tick = tick;
    }

    /// What a save at engine tick `tick` keeps: the first join, and the time of the save (PMMP
    /// writes the wall clock; the tick dates it from the join time and the ticks since, one
    /// [`super::TICK`] each).
    pub(super) fn saved(&self, join_ms: i64, tick: u64) -> (i64, i64) {
        let ticks = u128::from(tick.saturating_sub(self.placed_tick));
        let since =
            i64::try_from(ticks.saturating_mul(super::TICK.as_millis())).unwrap_or(i64::MAX);
        (self.first_played, join_ms.saturating_add(since))
    }

    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            ping_ms,
            first_played,
            last_played,
            placed_tick,
        } = self;
        h.u64(ping_ms.map_or(0, |p| u64::from(p) + 1));
        h.u64(first_played.cast_unsigned());
        h.u64(last_played.cast_unsigned());
        h.u64(*placed_tick);
    }
}

/// The wall-clock time `millis` after the Unix epoch.
fn unix_time(millis: i64) -> SystemTime {
    let offset = Duration::from_millis(millis.unsigned_abs());
    let time = if millis >= 0 {
        UNIX_EPOCH.checked_add(offset)
    } else {
        UNIX_EPOCH.checked_sub(offset)
    };
    time.unwrap_or(UNIX_EPOCH)
}

impl Marks {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            display_name,
            leave,
            teleport,
            chat,
            line_height,
        } = self;
        match display_name {
            Some(name) => h.bytes(name.as_bytes()),
            None => h.u64(u64::MAX),
        }
        match leave {
            Some(Leave::Kick {
                reason,
                screen_message,
                quit_message,
            }) => {
                h.bytes(reason.as_bytes());
                match screen_message {
                    Some(m) => h.bytes(m.as_bytes()),
                    None => h.u64(u64::MAX),
                }
                h.bytes(quit_message.as_bytes());
            }
            Some(Leave::Transfer {
                address,
                port,
                message,
            }) => {
                h.bytes(address.as_bytes());
                h.u64(u64::from(*port));
                h.bytes(message.as_bytes());
            }
            None => h.u64(u64::MAX),
        }
        match teleport {
            Some((world, eye)) => {
                h.usize(*world);
                for c in [eye.x, eye.y, eye.z] {
                    h.u64(c.to_bits());
                }
            }
            None => h.u64(u64::MAX),
        }
        h.u64(chat.0);
        h.u64(u64::from(chat.1));
        h.u64(line_height.map_or(0, u64::from));
    }
}

/// Does what the player setters marked this tick, after the inputs, tasks and rules and before
/// the views move and the output is built: kicked and transferred players get their last packet
/// and leave, teleported players move, and the survival marks are done.
pub(super) fn sync(state: &mut State, output: &mut Output) {
    // Most ticks mark no one: one pass over the players finds the marked ones, and the next one
    // is looked for after each is done, so marks it sets on later players are seen too.
    let mut after = Bound::Unbounded;
    let mut skin_budget = survival::SKIN_BYTES_PER_TICK;
    while let Some(session) = state
        .players
        .range((after, Bound::Unbounded))
        .find(|(_, p)| p.marks.leave.is_some() || p.marks.teleport.is_some() || survival::marked(p))
        .map(|(&s, _)| s)
    {
        after = Bound::Excluded(session);
        let Some(player) = state.players.get_mut(&session) else {
            continue;
        };
        if let Some(leave) = player.marks.leave.take() {
            let reason = leave.reason().to_owned();
            match &leave {
                Leave::Kick { reason, .. } => {
                    info!(session = session.get(), name = %player.identity.name, %reason, "player kicked");
                }
                Leave::Transfer { address, port, .. } => {
                    info!(session = session.get(), name = %player.identity.name, %address, port, "player transferred");
                }
            }
            let (packet, quit_message) = leave.into_parts();
            quit(state, session, &reason, quit_message);
            if let Some(p) = state.remove(session) {
                output.close(p.outbound, packet);
            }
            continue;
        }
        if let Some((world, eye)) = player.marks.teleport.take() {
            survival::teleport(state, session, world, eye);
        }
        survival::sync(state, session, &mut skin_budget);
    }
}

/// The player of `session` is about to leave for `reason` (PMMP `Player::onPostDisconnect`):
/// the [`PlayerQuit`] listeners may change `quit_message`, which every other online player then
/// sees in chat: [`LEFT`] as BDS's translated line with the player's name, an empty one not at
/// all, any other as it is. Call it just before the player is removed, in the tick's serial
/// step. A player still loading its data has not joined, so it raises nothing.
pub(super) fn quit(state: &mut State, session: SessionId, reason: &str, quit_message: String) {
    let Some(name) = state
        .players
        .get(&session)
        .filter(|p| p.phase != Phase::Loading)
        .map(|p| p.identity.name.clone())
    else {
        return;
    };
    let mut event = None;
    let message = if dispatch::heard::<PlayerQuit>(state) {
        let e = event.insert(PlayerQuit {
            player: Id::new(session),
            quit_message,
            quit_reason: reason.to_owned(),
        });
        dispatch::call(state, e);
        e.quit_message.clone()
    } else {
        quit_message
    };
    if !message.is_empty() {
        let message = if message == LEFT {
            Message::Translation {
                key: message,
                parameters: vec![name],
            }
        } else {
            Message::Raw(message)
        };
        match message.into_packets() {
            Ok(packets) => {
                for (&s, p) in &mut state.players {
                    if s != session && p.phase != Phase::Loading && p.is_online() {
                        for packet in packets.iter().flatten() {
                            p.send_packet(packet.clone());
                        }
                    }
                }
            }
            Err(e) => debug!(session = session.get(), error = %e, "quit message not sent"),
        }
    }
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
}

/// Sends `message` to every online player; returns how many got it. Fails, sending nothing, for a
/// message a client does not decode.
pub(super) fn broadcast(state: &mut State, message: Message) -> Result<usize> {
    Ok(broadcast_packets(state, &message.into_packets()?))
}

/// Sends the packets of a message to every online player; returns how many got them.
pub(super) fn broadcast_packets(state: &mut State, packets: &Packets) -> usize {
    let mut count = 0;
    for player in state.players.values_mut() {
        if player.phase == Phase::Loading || !player.is_online() {
            continue;
        }
        for packet in packets.iter().flatten() {
            player.send_packet(packet.clone());
        }
        count += 1;
    }
    count
}

impl Player {
    /// The handle to keep beyond the tick.
    pub fn id(&self) -> Id {
        Id::new(self.session)
    }

    /// The player's name: its Xbox gamertag with Xbox sign-in.
    #[doc(alias = "Player::getName")]
    pub fn name(&self) -> &str {
        &self.identity.name
    }

    /// The name chat shows: the player's name unless
    /// [`set_display_name`](Self::set_display_name) changed it.
    #[doc(alias = "Player::getDisplayName")]
    pub fn display_name(&self) -> &str {
        self.marks
            .display_name
            .as_deref()
            .unwrap_or(&self.identity.name)
    }

    /// Sets the name chat shows. Not saved, as in PMMP. Fails for a name longer than 65,536
    /// bytes.
    #[doc(alias = "Player::setDisplayName")]
    pub fn set_display_name(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if name.len() > MAX_TEXT {
            return Err(Error::TooLong);
        }
        self.marks.display_name = Some(name);
        Ok(())
    }

    /// The Xbox user ID; `None` without Xbox sign-in.
    #[doc(alias = "Player::getXuid")]
    pub fn xuid(&self) -> Option<u64> {
        self.identity.xuid
    }

    /// Whether the player signed in with Xbox.
    #[doc(alias = "Player::isAuthenticated")]
    pub fn is_authenticated(&self) -> bool {
        self.identity.xuid.is_some()
    }

    /// The player's UUID, from its login.
    #[doc(alias = "Player::getUniqueId")]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// The skin the client sent at login.
    #[doc(alias = "Human::getSkin")]
    pub fn skin(&self) -> &crate::protocol::Skin {
        self.identity.skin.get()
    }

    /// The device the client runs on, as its login tells (`getExtraData()["DeviceOS"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_os(&self) -> DeviceOs {
        DeviceOs::try_from(self.identity.device_os).unwrap_or(DeviceOs::Unknown)
    }

    /// The IP address the client connected from, which IP bans match; `None` only for a player
    /// made without a connection (tests).
    #[doc(alias = "NetworkSession::getIp")]
    pub fn ip(&self) -> Option<IpAddr> {
        self.identity.address
    }

    /// The port the client connected from; 0 when unknown (a replay of an input log older than
    /// format 9).
    #[doc(alias = "NetworkSession::getPort")]
    pub fn port(&self) -> u16 {
        self.identity.client.port
    }

    /// The latest round trip time to the client: the RakNet ping, or on NetherNet the ICE
    /// consent check's; `None` until one was measured. The session reports it every 5 seconds
    /// when it changed.
    #[doc(alias = "NetworkSession::getPing")]
    pub fn ping(&self) -> Option<Duration> {
        self.presence
            .ping_ms
            .map(|ms| Duration::from_millis(u64::from(ms)))
    }

    /// The language the client runs in, such as `en_US`, as its login tells.
    #[doc(alias = "Player::getLocale")]
    #[doc(alias = "PlayerInfo::getLocale")]
    pub fn locale(&self) -> &str {
        &self.identity.client.locale
    }

    /// The input mode the client started in, as its login tells (PMMP's
    /// `getExtraData()["CurrentInputMode"]`); [`InputMode::Undefined`] for a value the protocol
    /// does not name.
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn input_mode(&self) -> InputMode {
        self.identity.client.input_mode
    }

    /// The device model, as its login tells (`getExtraData()["DeviceModel"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_model(&self) -> &str {
        &self.identity.client.device_model
    }

    /// The device ID, as its login tells (`getExtraData()["DeviceId"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_id(&self) -> &str {
        &self.identity.client.device_id
    }

    /// The client's version, such as `1.26.50`, as its login tells
    /// (`getExtraData()["GameVersion"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn game_version(&self) -> &str {
        &self.identity.client.game_version
    }

    /// The address the client was told to join, such as `play.example.com:19132`, as its login
    /// tells (`getExtraData()["ServerAddress"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn server_address(&self) -> &str {
        &self.identity.client.server_address
    }

    /// When the player first joined this server, by the wall clock (its saved `firstPlayed`, or
    /// this join for a new player).
    #[doc(alias = "Player::getFirstPlayed")]
    #[doc(alias = "IPlayer::getFirstPlayed")]
    pub fn first_played(&self) -> SystemTime {
        unix_time(self.presence.first_played)
    }

    /// When the player's data was last saved before this join, by the wall clock (its saved
    /// `lastPlayed`, or this join for a new player). It does not move while the player plays,
    /// as in PMMP.
    #[doc(alias = "Player::getLastPlayed")]
    #[doc(alias = "IPlayer::getLastPlayed")]
    pub fn last_played(&self) -> SystemTime {
        unix_time(self.presence.last_played)
    }

    /// Whether the player played here before this join: its data was saved more than a
    /// millisecond after its first join (PMMP's test).
    #[doc(alias = "Player::hasPlayedBefore")]
    #[doc(alias = "IPlayer::hasPlayedBefore")]
    pub fn has_played_before(&self) -> bool {
        self.presence
            .last_played
            .saturating_sub(self.presence.first_played)
            > 1
    }

    /// The chunk radius the player sees: what its client asked for, at most `view-distance`, or
    /// what [`Context::set_view_distance`](crate::Context::set_view_distance) set since; `None`
    /// until it asked.
    #[doc(alias = "Player::getViewDistance")]
    pub fn view_distance(&self) -> Option<u8> {
        self.chunk_radius
    }

    /// The lines the player's screen shows at once, which `/help` pages by: 7 unless
    /// [`set_screen_line_height`](Self::set_screen_line_height) changed it.
    #[doc(alias = "Player::getScreenLineHeight")]
    pub fn screen_line_height(&self) -> u32 {
        self.marks.line_height.unwrap_or(DEFAULT_LINE_HEIGHT)
    }

    /// Sets the lines the player's screen shows at once; `None` goes back to 7. Not saved, as in
    /// PMMP. Fails for 0.
    #[doc(alias = "Player::setScreenLineHeight")]
    pub fn set_screen_line_height(&mut self, height: Option<u32>) -> Result<()> {
        if height == Some(0) {
            return Err(Error::OutOfRange);
        }
        self.marks.line_height = height;
        Ok(())
    }

    /// Whether the player stays connected: false once it was kicked or transferred, until it
    /// leaves at the end of the tick.
    #[doc(alias = "Player::isOnline")]
    #[doc(alias = "Player::isConnected")]
    pub fn is_online(&self) -> bool {
        self.marks.leave.is_none()
    }

    /// The world the player is in.
    #[doc(alias = "Entity::getWorld")]
    pub fn world(&self) -> world::Id {
        world::Id::of(self.marks.teleport.map_or(self.world, |(world, _)| world))
    }

    /// Where the player's feet are; with [`world`](Self::world), [`yaw`](Self::yaw) and
    /// [`pitch`](Self::pitch) its PMMP location.
    #[doc(alias = "Entity::getPosition")]
    #[doc(alias = "Entity::getLocation")]
    pub fn position(&self) -> Vec3 {
        self.eye_position() - Vec3::new(0.0, survival::EYE_HEIGHT, 0.0)
    }

    /// Where the player's eyes are: 1.62 above its feet.
    #[doc(alias = "Entity::getEyePos")]
    pub fn eye_position(&self) -> Vec3 {
        self.marks.teleport.map_or(self.position, |(_, eye)| eye)
    }

    /// Moves the player's feet to `feet` in `world`: by the end of the tick its window closes,
    /// it wakes, its breaking stops, its fall ends, and its client and viewers see it there;
    /// another world sends its chunks again. [`position`](Self::position) is the new place at
    /// once; a second teleport in the tick wins. Fails for a position that is not finite or
    /// lies outside the world border. A world ID of another server is ignored at the end of the
    /// tick.
    #[doc(alias = "Player::teleport")]
    #[doc(alias = "Entity::teleport")]
    pub fn teleport(&mut self, world: world::Id, feet: Vec3) -> Result<()> {
        let eye = feet + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        let border = f64::from(MAX_COORDINATE);
        if !eye.is_finite() || [eye.x, eye.y, eye.z].iter().any(|c| c.abs() > border) {
            return Err(Error::OutsideWorld);
        }
        self.marks.teleport = Some((world.index(), eye));
        Ok(())
    }

    /// Shows the player `message` (PLAN.md 8.4: PMMP's message methods merged). The packets
    /// follow this tick's other packets. Fails, sending nothing, for a message a client does not
    /// decode ([`Message`] gives the bounds).
    #[doc(alias = "Player::sendMessage")]
    #[doc(alias = "Player::sendTip")]
    #[doc(alias = "Player::sendPopup")]
    #[doc(alias = "Player::sendJukeboxPopup")]
    #[doc(alias = "Player::sendActionBarMessage")]
    #[doc(alias = "Player::sendTitle")]
    #[doc(alias = "Player::sendSubTitle")]
    #[doc(alias = "Player::removeTitles")]
    #[doc(alias = "Player::resetTitles")]
    #[doc(alias = "Player::setTitleDuration")]
    #[doc(alias = "Player::sendToastNotification")]
    pub fn send(&mut self, message: Message) -> Result<()> {
        for packet in message.into_packets()?.into_iter().flatten() {
            self.send_packet(packet);
        }
        Ok(())
    }

    /// Disconnects the player at the end of the tick with `reason` on its screen (the client's
    /// "disconnected" text when empty); the other players see that it left the game. Fails if the player already leaves, or for a reason longer
    /// than 65,536 bytes.
    #[doc(alias = "Player::kick")]
    #[doc(alias = "Player::disconnect")]
    pub fn kick(&mut self, reason: impl Into<String>) -> Result<()> {
        if self.marks.leave.is_some() {
            return Err(Error::Leaving);
        }
        let reason = reason.into();
        if reason.len() > MAX_TEXT {
            return Err(Error::TooLong);
        }
        self.marks.leave = Some(Leave::Kick {
            reason,
            screen_message: None,
            quit_message: LEFT.to_owned(),
        });
        Ok(())
    }

    /// Sends the player to the server at `address` (a host name or IP address) and `port` at the
    /// end of the tick; it leaves this one, and the other players see that it left the game. Fails if the player already leaves, or for an empty
    /// address or one longer than 253 bytes.
    #[doc(alias = "Player::transfer")]
    pub fn transfer(&mut self, address: impl Into<String>, port: u16) -> Result<()> {
        if self.marks.leave.is_some() {
            return Err(Error::Leaving);
        }
        let address = address.into();
        if !is_valid_address(&address) {
            return Err(Error::InvalidAddress);
        }
        self.marks.leave = Some(Leave::Transfer {
            address,
            port,
            message: TRANSFERRED.to_owned(),
        });
        Ok(())
    }
}

impl Context<'_> {
    /// Kicks `player` as [`Player::kick`] does, after asking the [`PlayerKick`] listeners, which
    /// may keep it or change the reason, the screen message and the quit message the other
    /// players see (`quit_message` `None` is the usual "left the game" line, an empty one shows
    /// nothing). False if it was not kicked: it is gone or
    /// leaving, a listener cancelled it, or a text is longer than 65,536 bytes.
    pub(crate) fn kick_player(
        &mut self,
        player: Id,
        reason: String,
        screen_message: Option<String>,
        quit_message: Option<String>,
    ) -> bool {
        let session = player.session();
        let staying = |state: &State| state.players.get(&session).is_some_and(Player::is_online);
        if !staying(self.state) {
            return false;
        }
        let (mut reason, mut screen_message) = (reason, screen_message);
        let mut quit_message = quit_message.unwrap_or_else(|| LEFT.to_owned());
        let mut event = None;
        if dispatch::heard::<PlayerKick>(self.state) {
            let e = event.insert(PlayerKick {
                player,
                disconnect_reason: std::mem::take(&mut reason),
                screen_message: screen_message.take(),
                quit_message: std::mem::take(&mut quit_message),
                cancelled: false,
            });
            dispatch::call(self.state, e);
            if e.cancelled || (dispatch::listened::<PlayerKick>(self.state) && !staying(self.state))
            {
                return false;
            }
            reason.clone_from(&e.disconnect_reason);
            screen_message.clone_from(&e.screen_message);
            quit_message.clone_from(&e.quit_message);
        }
        let texts = [Some(&reason), screen_message.as_ref(), Some(&quit_message)];
        if texts.into_iter().flatten().any(|t| t.len() > MAX_TEXT) {
            return false;
        }
        if let Some(p) = self.state.players.get_mut(&session) {
            p.marks.leave = Some(Leave::Kick {
                reason,
                screen_message,
                quit_message,
            });
        }
        if let Some(e) = &event {
            dispatch::watch(self.state, e);
        }
        true
    }

    /// Sends `player` to the server at `address` and `port` as [`Player::transfer`] does, after
    /// asking the [`PlayerTransfer`] listeners, which may keep it or change the address, the port
    /// and the log `message` (`None`: "Moved to another server"). False if it was not sent:
    /// it is gone or leaving, a listener cancelled it, the address is empty or longer than 253
    /// bytes, or the message is longer than 65,536 bytes.
    pub(crate) fn transfer_player(
        &mut self,
        player: Id,
        address: String,
        port: u16,
        message: Option<String>,
    ) -> bool {
        let session = player.session();
        let staying = |state: &State| state.players.get(&session).is_some_and(Player::is_online);
        if !staying(self.state) {
            return false;
        }
        let mut leave = (
            address,
            port,
            message.unwrap_or_else(|| TRANSFERRED.to_owned()),
        );
        let mut event = None;
        if dispatch::heard::<PlayerTransfer>(self.state) {
            let e = event.insert(PlayerTransfer {
                player,
                address: std::mem::take(&mut leave.0),
                port: leave.1,
                message: std::mem::take(&mut leave.2),
                cancelled: false,
            });
            dispatch::call(self.state, e);
            if e.cancelled
                || (dispatch::listened::<PlayerTransfer>(self.state) && !staying(self.state))
            {
                return false;
            }
            leave = (e.address.clone(), e.port, e.message.clone());
        }
        let (address, port, message) = leave;
        if !is_valid_address(&address) || message.len() > MAX_TEXT {
            return false;
        }
        if let Some(p) = self.state.players.get_mut(&session) {
            p.marks.leave = Some(Leave::Transfer {
                address,
                port,
                message,
            });
        }
        if let Some(e) = &event {
            dispatch::watch(self.state, e);
        }
        true
    }

    /// Moves `player`'s feet to `feet` in `world` as [`Player::teleport`] does, after asking the
    /// [`EntityTeleport`] listeners, which may keep it where it is or send it elsewhere. False if
    /// it was not moved: it is gone, a listener cancelled it, or the place is not finite or lies
    /// outside the world border.
    pub(crate) fn teleport_player(&mut self, player: Id, world: world::Id, feet: Vec3) -> bool {
        let Some(origin) = self.player(player).map(|p| (p.world(), p.position())) else {
            return false;
        };
        let (mut world, mut feet) = (world, feet);
        let mut event = None;
        if dispatch::heard::<EntityTeleport>(self.state) {
            let e = event.insert(EntityTeleport {
                entity: Actor::Player(player),
                origin_world: origin.0,
                origin: origin.1,
                target_world: world,
                target: feet,
                cancelled: false,
            });
            dispatch::call(self.state, e);
            if e.cancelled {
                return false;
            }
            (world, feet) = (e.target_world, e.target);
        }
        let moved = self
            .player_mut(player)
            .is_some_and(|p| p.teleport(world, feet).is_ok());
        if moved && let Some(e) = &event {
            dispatch::watch(self.state, e);
        }
        moved
    }
}

/// Whether `address` can be a transfer address: a host name or IP address, not empty and at most
/// 253 bytes.
pub(super) fn is_valid_address(address: &str) -> bool {
    !address.is_empty() && address.len() <= MAX_ADDRESS
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::event::Listeners;
    use crate::nbt::Value;
    use crate::player::TitleTimes;
    use crate::tick::Context;
    use crate::tick::log::{EntryKind, InputLog};
    use crate::tick::queue::{Client, Identity, Input, InputKind, Measure, Outbound, SessionId};
    use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};

    /// Players 1 and 2 spawned next to each other, their queues emptied.
    pub(in crate::tick) fn two() -> (State, [mpsc::Receiver<Outbound>; 2]) {
        two_with(Listeners::default())
    }

    /// [`two`] with `listeners`.
    fn two_with(mut listeners: Listeners) -> (State, [mpsc::Receiver<Outbound>; 2]) {
        listeners.sort();
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = Output::default();
        let (j1, mut rx1) = join(1, 0);
        let (j2, mut rx2) = join(2, 0);
        tick(
            &mut state,
            &mut vec![j1, j2, radius(1, 1, 4), radius(2, 1, 4)],
            &mut output,
        );
        let ids = [1, 2].map(|s| state.players[&SessionId::new(s)].runtime_id);
        tick(
            &mut state,
            &mut vec![initialised(1, 2, ids[0]), initialised(2, 2, ids[1])],
            &mut output,
        );
        received(&mut rx1);
        received(&mut rx2);
        (state, [rx1, rx2])
    }

    /// Players 1 to `n` spawned, their queues emptied.
    fn many(n: u64) -> (State, Vec<mpsc::Receiver<Outbound>>) {
        let max = u32::try_from(n).unwrap();
        let mut state = new_state(
            settings(|c| c.general.max_players = max),
            Listeners::default(),
        );
        let mut output = Output::default();
        let mut inputs = Vec::new();
        let mut receivers = Vec::new();
        for s in 1..=n {
            let (j, rx) = join(s, 0);
            inputs.extend([j, radius(s, 1, 4)]);
            receivers.push(rx);
        }
        tick(&mut state, &mut inputs, &mut output);
        let mut inputs = (1..=n)
            .map(|s| initialised(s, 2, state.players[&SessionId::new(s)].runtime_id))
            .collect();
        tick(&mut state, &mut inputs, &mut output);
        for rx in &mut receivers {
            received(rx);
        }
        (state, receivers)
    }

    /// One tick with `inputs`.
    pub(in crate::tick) fn run(state: &mut State, inputs: Vec<Input>) {
        let mut inputs = inputs;
        tick(state, &mut inputs, &mut Output::default());
    }

    /// The player of session `s`.
    pub(in crate::tick) fn player_mut(state: &mut State, s: u64) -> &mut Player {
        state.players.get_mut(&SessionId::new(s)).unwrap()
    }

    #[test]
    fn a_skin_broadcast_is_one_shared_copy_within_the_tick_budget() {
        const N: u64 = 48;
        let (mut state, mut receivers) = many(N);
        // Two skins of 3 MiB: together over the tick's budget of 4 MiB.
        for s in [1, 2] {
            let p = player_mut(&mut state, s);
            let mut skin = p.skin().clone();
            skin.skin_id = format!("big{s}");
            skin.skin_geometry = vec![b' '; 3 << 20];
            p.set_skin(skin).unwrap();
        }
        sync(&mut state, &mut Output::default());
        let first = state.players[&SessionId::new(1)].shared_outbox.clone();
        assert_eq!(first.len(), 1);
        for p in state.players.values() {
            assert_eq!(p.shared_outbox.len(), 1, "the second skin waits");
            assert!(std::sync::Arc::ptr_eq(&p.shared_outbox[0], &first[0]));
            assert!(p.outbox.iter().all(|p| !matches!(p, Packet::PlayerSkin(_))));
        }
        // One copy held by every receiver (and this test): no copy per receiver.
        assert_eq!(
            std::sync::Arc::strong_count(&first[0]),
            usize::try_from(N).unwrap() + 1
        );
        drop(first);
        assert!(survival::marked(&state.players[&SessionId::new(2)]));
        state
            .players
            .values_mut()
            .for_each(|p| p.shared_outbox.clear());
        // The next tick sends the second skin to everyone.
        run(&mut state, Vec::new());
        for rx in &mut receivers {
            let skins: Vec<String> = received(rx)
                .0
                .into_iter()
                .filter_map(|p| match p {
                    Packet::PlayerSkin(s) => Some(s.skin.skin_id),
                    _ => None,
                })
                .collect();
            assert_eq!(skins, ["big2"]);
        }
        assert!(!survival::marked(&state.players[&SessionId::new(2)]));
    }

    // pmmp: Player::getName, Player::getDisplayName, Player::setDisplayName, Player::getXuid, Player::isAuthenticated, Player::getUniqueId, Human::getSkin
    #[test]
    fn the_identity_reads_the_login_and_the_display_name_changes_alone() {
        let (mut state, _rx) = two();
        let before = state.hash();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.id(), Id::new(SessionId::new(1)));
        assert_eq!(p.name(), "player1");
        assert_eq!(p.display_name(), "player1");
        assert_eq!(p.xuid(), Some(2_535_000_001));
        assert!(p.is_authenticated());
        assert_eq!(p.uuid(), Uuid::from_bytes([1; 16]));
        assert_eq!(p.skin().skin_image_width, 64);
        assert_eq!(p.device_os(), DeviceOs::Unknown);
        assert_eq!(
            p.set_display_name("x".repeat(MAX_TEXT + 1)),
            Err(Error::TooLong)
        );
        p.set_display_name("§aBoss").unwrap();
        assert_eq!(p.display_name(), "§aBoss");
        assert_eq!(p.name(), "player1");
        assert_ne!(state.hash(), before);
    }

    // pmmp: Player::teleport, Entity::teleport, Entity::getPosition, Entity::getLocation, Entity::getEyePos, Entity::getWorld
    #[test]
    fn a_teleport_moves_the_position_at_once_and_the_client_and_viewers_by_the_end_of_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        let start = p.position();
        for feet in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(30_000_001.0, -60.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
        ] {
            assert_eq!(
                p.teleport(world::Id::DEFAULT, feet),
                Err(Error::OutsideWorld)
            );
        }
        assert_eq!(p.position(), start);
        let to = Vec3::new(3.5, -60.0, 2.5);
        p.teleport(world::Id::DEFAULT, Vec3::new(9.5, -60.0, 9.5))
            .unwrap();
        p.teleport(world::Id::DEFAULT, to).unwrap();
        assert_eq!(p.position(), to);
        assert_eq!(
            p.eye_position(),
            to + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0)
        );
        assert_eq!(p.world(), world::Id::DEFAULT);
        run(&mut state, Vec::new());
        let moved = |packets: Vec<Packet>| {
            packets
                .into_iter()
                .filter(|p| matches!(p, Packet::MovePlayer(m) if m.mode == 2))
                .count()
        };
        // The last teleport of the tick is the one sent, once, to the player and its viewer.
        assert_eq!(moved(received(&mut rx1).0), 1);
        assert_eq!(moved(received(&mut rx2).0), 1);
        let p = &state.players[&SessionId::new(1)];
        assert_eq!(p.position(), to);
        assert!(p.marks.teleport.is_none());
    }

    // pmmp: Player::sendMessage, Player::sendTip, Player::sendPopup, Player::sendJukeboxPopup, Player::sendActionBarMessage, Player::sendTitle, Player::sendSubTitle, Player::removeTitles, Player::resetTitles, Player::setTitleDuration, Player::sendToastNotification, NetworkSession::sendDataPacket
    #[test]
    fn messages_reach_only_their_player_after_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        let times = |ms| TitleTimes {
            fade_in: std::time::Duration::from_millis(ms),
            stay: std::time::Duration::from_millis(ms),
            fade_out: std::time::Duration::from_millis(ms),
        };
        let messages = [
            Message::Raw("raw".to_owned()),
            Message::Tip("tip".to_owned()),
            Message::Popup("popup".to_owned()),
            Message::JukeboxPopup {
                key: "record.nowPlaying".to_owned(),
                parameters: vec!["cat".to_owned()],
            },
            Message::ActionBar("bar".to_owned()),
            Message::Title {
                title: "title".to_owned(),
                subtitle: "sub".to_owned(),
                times: Some(times(50)),
            },
            Message::Subtitle("sub".to_owned()),
            Message::ClearTitle,
            Message::ResetTitle,
            Message::TitleTimes(times(0)),
            Message::Toast {
                title: "t".to_owned(),
                body: "b".to_owned(),
            },
        ];
        for m in messages {
            p.send(m).unwrap();
        }
        assert_eq!(
            p.send(Message::Raw("x".repeat(MAX_TEXT + 1))),
            Err(Error::TooLong)
        );
        p.send_packet(Packet::Text(Box::new(crate::protocol::packet::Text {
            message: "own".to_owned(),
            ..Default::default()
        })));
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let count = |f: fn(&Packet) -> bool| got.iter().filter(|p| f(p)).count();
        assert_eq!(count(|p| matches!(p, Packet::Text(_))), 5);
        assert_eq!(count(|p| matches!(p, Packet::SetTitle(_))), 8);
        assert_eq!(count(|p| matches!(p, Packet::ToastRequest(_))), 1);
        assert!(received(&mut rx2).0.iter().all(|p| !matches!(
            p,
            Packet::Text(_) | Packet::SetTitle(_) | Packet::ToastRequest(_)
        )));
    }

    // pmmp: Player::kick, Player::disconnect, Player::isOnline, Player::isConnected
    #[test]
    fn a_kick_disconnects_once_with_its_reason_at_the_end_of_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert!(p.is_online());
        assert_eq!(p.kick("x".repeat(MAX_TEXT + 1)), Err(Error::TooLong));
        p.kick("bye").unwrap();
        assert!(!p.is_online());
        assert_eq!(p.kick("again"), Err(Error::Leaving));
        assert_eq!(p.transfer("example.com", 19132), Err(Error::Leaving));
        run(&mut state, Vec::new());
        let (_, close, _) = received(&mut rx1);
        assert!(matches!(
            close,
            Some(Packet::Disconnect(d)) if d.message == "bye" && d.reason == 55
        ));
        assert!(!state.players.contains_key(&SessionId::new(1)));
        assert!(state.index_matches_players());
        // The other player sees it leave the list.
        assert!(
            received(&mut rx2)
                .0
                .iter()
                .any(|p| matches!(p, Packet::PlayerList(l) if l.entries[0].action_type == 1))
        );
        // An empty reason shows the client's text.
        player_mut(&mut state, 2).kick("").unwrap();
        run(&mut state, Vec::new());
        let (_, close, _) = received(&mut rx2);
        assert!(matches!(close, Some(Packet::Disconnect(d)) if d.message == NO_REASON));
        assert!(state.players.is_empty());
    }

    // pmmp: PlayerQuitEvent
    #[test]
    fn the_others_see_bds_left_line_unless_a_listener_changes_it() {
        let (mut state, [mut rx1, mut rx2]) = two();
        player_mut(&mut state, 1).kick("bye").unwrap();
        run(&mut state, Vec::new());
        let texts = |packets: Vec<Packet>| -> Vec<(String, Vec<String>, bool)> {
            packets
                .into_iter()
                .filter_map(|p| match p {
                    Packet::Text(t) => Some((t.message, t.parameters, t.needs_translation)),
                    _ => None,
                })
                .collect()
        };
        assert!(
            texts(received(&mut rx1).0).is_empty(),
            "not the leaving one"
        );
        assert_eq!(
            texts(received(&mut rx2).0),
            [(LEFT.to_owned(), vec!["player1".to_owned()], true)]
        );
        // A listener's own message goes as it is; an empty one shows nothing.
        let listeners = || {
            let mut listeners = Listeners::default();
            listeners.push(
                crate::event::Priority::Normal,
                |e: &mut PlayerQuit, _: &mut Context<'_>| {
                    let own = if e.quit_reason() == "quiet" {
                        String::new()
                    } else {
                        format!("{} is gone", e.player().session().get())
                    };
                    e.set_quit_message(own);
                },
            );
            listeners
        };
        for (reason, seen) in [
            ("loud", vec![("1 is gone".to_owned(), Vec::new(), false)]),
            ("quiet", Vec::new()),
        ] {
            let (mut state, [_rx1, mut rx2]) = two_with(listeners());
            player_mut(&mut state, 1).kick(reason).unwrap();
            run(&mut state, Vec::new());
            assert_eq!(texts(received(&mut rx2).0), seen, "{reason}");
        }
    }

    // pmmp: NetworkSession::getIp, Player::getViewDistance, Player::getScreenLineHeight, Player::setScreenLineHeight, Player
    #[test]
    fn the_address_view_distance_and_line_height_read_the_session() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.ip(), None, "the test login has no address");
        let ip: IpAddr = [192, 0, 2, 7].into();
        p.identity.address = Some(ip);
        assert_eq!(p.ip(), Some(ip));
        // The client asked for 4 chunks.
        assert_eq!(p.view_distance(), Some(4));
        assert_eq!(p.screen_line_height(), DEFAULT_LINE_HEIGHT);
        assert_eq!(p.set_screen_line_height(Some(0)), Err(Error::OutOfRange));
        p.set_screen_line_height(Some(20)).unwrap();
        assert_eq!(p.screen_line_height(), 20);
        p.set_screen_line_height(None).unwrap();
        assert_eq!(p.screen_line_height(), DEFAULT_LINE_HEIGHT);
    }

    /// A login with every client value set, joining at `join_ms`.
    fn alex(join_ms: i64) -> Identity {
        Identity {
            name: "Alex".to_owned(),
            xuid: None,
            uuid: Uuid::from_bytes([9; 16]),
            skin: crate::tick::queue::Skin::default(),
            device_os: 2,
            address: Some(IpAddr::from([192, 0, 2, 9])),
            client: Client {
                locale: "ko_KR".to_owned(),
                input_mode: InputMode::Touch,
                device_model: "SM-S918N".to_owned(),
                device_id: "0123456789abcdef".to_owned(),
                game_version: "1.26.52".to_owned(),
                server_address: "play.example.com:19132".to_owned(),
                port: 50_123,
                join_time_ms: join_ms,
            },
        }
    }

    fn join_input(session: u64, identity: Identity) -> (Input, mpsc::Receiver<Outbound>) {
        let (outbound, rx) = crate::tick::queue::outbound_queue();
        let input = Input {
            session: SessionId::new(session),
            seq: 0,
            kind: InputKind::Join { identity, outbound },
        };
        (input, rx)
    }

    fn ping(session: u64, seq: u64, millis: u32) -> Input {
        Input {
            session: SessionId::new(session),
            seq,
            kind: InputKind::Measure(Measure::Ping(millis)),
        }
    }

    // pmmp: Player::getLocale, PlayerInfo::getLocale, PlayerInfo::getExtraData
    // pmmp: NetworkSession::getPort, NetworkSession::getPing
    /// What the login and the connection tell reaches the player's getters, and the logged join
    /// and pings replay to the same state at every tick.
    #[test]
    fn the_login_values_and_the_ping_reach_the_player_and_replay_the_same() {
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let apply = |state: &mut State, mut inputs: Vec<Input>, log: &mut InputLog| {
            let mut output = Output::default();
            crate::tick::step(state, &mut inputs, &mut Vec::new(), &pool, &mut output, log);
            crate::tick::deliver(state, &mut output, log);
            state.hash()
        };
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut log = InputLog::default();
        let (j, _rx) = join_input(1, alex(1_700_000_000_000));
        let mut live = vec![apply(&mut state, vec![j], &mut log)];
        let p = player_mut(&mut state, 1);
        assert_eq!(p.locale(), "ko_KR");
        assert_eq!(p.input_mode(), InputMode::Touch);
        assert_eq!(p.device_os(), DeviceOs::Ios);
        assert_eq!(p.device_model(), "SM-S918N");
        assert_eq!(p.device_id(), "0123456789abcdef");
        assert_eq!(p.game_version(), "1.26.52");
        assert_eq!(p.server_address(), "play.example.com:19132");
        assert_eq!(p.port(), 50_123);
        assert_eq!(p.ping(), None, "no ping measured yet");
        live.push(apply(&mut state, vec![ping(1, 1, 42)], &mut log));
        assert_eq!(
            player_mut(&mut state, 1).ping(),
            Some(Duration::from_millis(42))
        );
        live.push(apply(&mut state, vec![ping(1, 2, 35)], &mut log));
        assert_eq!(
            player_mut(&mut state, 1).ping(),
            Some(Duration::from_millis(35))
        );
        // The written log replays to the same states.
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        let mut replay = new_state(settings(|_| {}), Listeners::default());
        let mut again = InputLog::default();
        let mut keep = Vec::new();
        let mut replayed = Vec::new();
        for t in read.ticks() {
            let inputs = t
                .inputs
                .iter()
                .map(|e| match &e.kind {
                    EntryKind::Join(identity) => {
                        let (input, rx) = join_input(e.session.get(), identity.clone());
                        keep.push(rx);
                        input
                    }
                    EntryKind::Measure(m) => Input {
                        session: e.session,
                        seq: e.seq,
                        kind: InputKind::Measure(*m),
                    },
                    other => panic!("{other:?}"),
                })
                .collect();
            replayed.push(apply(&mut replay, inputs, &mut again));
        }
        assert_eq!(replayed, live);
        assert_eq!(again.encode().unwrap(), log.encode().unwrap());
        assert_eq!(player_mut(&mut replay, 1).port(), 50_123);
        assert!(pool.shutdown());
    }

    // pmmp: Player::hasPlayedBefore, Player::getFirstPlayed, Player::getLastPlayed
    // pmmp: IPlayer::hasPlayedBefore, IPlayer::getFirstPlayed, IPlayer::getLastPlayed
    /// A new player's first and last play are its join; the saved data keeps the first join and
    /// the time of the save under PMMP's keys, and the next run's join reads them back: the
    /// player has played before.
    #[test]
    fn the_play_history_is_saved_and_read_back_by_the_next_run() {
        let first_join = 1_700_000_000_000;
        let mut state = new_state(
            settings(|c| c.general.save_player_data = true),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, _rx) = join_input(1, alex(first_join));
        // The join asks for the saved data; the stand-in save thread has none.
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut Vec::new(), &mut output);
        let p = player_mut(&mut state, 1);
        assert_eq!(p.phase, Phase::Joined);
        let at = |ms: u64| UNIX_EPOCH + Duration::from_millis(ms);
        assert_eq!(p.first_played(), at(1_700_000_000_000));
        assert_eq!(p.last_played(), at(1_700_000_000_000));
        assert!(!p.has_played_before());
        // 100 ticks after the tick that placed the data, a save is dated 5 seconds after the
        // join.
        for _ in 0..99 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        let data = crate::tick::save::data_of(&state, &state.players[&SessionId::new(1)]);
        let saved = |key: &str| data.extra.get(key).cloned();
        assert_eq!(saved("firstPlayed"), Some(Value::Long(first_join)));
        assert_eq!(saved("lastPlayed"), Some(Value::Long(first_join + 5_000)));
        assert!(
            !player_mut(&mut state, 1).has_played_before(),
            "the last play does not move while the player plays, as in PMMP"
        );

        // The next run: an hour later, the saved data comes back.
        let mut next = new_state(
            settings(|c| c.general.save_player_data = true),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, _rx) = join_input(1, alex(first_join + 3_600_000));
        tick(&mut next, &mut vec![j], &mut output);
        for answer in &mut output.answers {
            if let crate::save::Response::Player { data: load, .. } = answer {
                *load = crate::save::PlayerLoad::Found(data.clone());
            }
        }
        tick(&mut next, &mut Vec::new(), &mut output);
        let p = player_mut(&mut next, 1);
        assert_eq!(p.phase, Phase::Joined);
        assert_eq!(p.first_played(), at(1_700_000_000_000));
        assert_eq!(p.last_played(), at(1_700_000_005_000));
        assert!(p.has_played_before());
    }

    // pmmp: Player::getLastPlayed
    /// Leaving dates the play history, so a player who changed nothing else is still saved
    /// when it leaves (PMMP saves every player that quits): its next join has played before.
    #[test]
    fn a_player_who_changed_nothing_else_is_saved_when_it_leaves() {
        let first_join = 1_700_000_000_000;
        let mut state = new_state(
            settings(|c| c.general.save_player_data = true),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, _rx) = join_input(1, alex(first_join));
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(!state.players[&SessionId::new(1)].data_dirty);
        for _ in 0..19 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        let leave = Input {
            session: SessionId::new(1),
            seq: 1,
            kind: InputKind::Leave,
        };
        tick(&mut state, &mut vec![leave], &mut output);
        let offline = state
            .saving
            .offline
            .values()
            .next()
            .expect("the data is kept");
        assert_eq!(offline.snapshot, None, "no commit holds it yet");
        let saved = |key: &str| offline.data.extra.get(key).cloned();
        assert_eq!(saved("firstPlayed"), Some(Value::Long(first_join)));
        assert_eq!(saved("lastPlayed"), Some(Value::Long(first_join + 1_000)));
    }

    // pmmp: Player::transfer
    #[test]
    fn a_transfer_sends_the_address_and_leaves() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.transfer("", 1), Err(Error::InvalidAddress));
        assert_eq!(p.transfer("a".repeat(254), 1), Err(Error::InvalidAddress));
        p.transfer("play.example.com", 19133).unwrap();
        // The leaving player gets no broadcast.
        let mut ctx = Context { state: &mut state };
        assert_eq!(ctx.broadcast(Message::Tip("t".to_owned())), Ok(1));
        run(&mut state, Vec::new());
        let (_, close, _) = received(&mut rx1);
        assert!(matches!(
            close,
            Some(Packet::Transfer(t)) if t.address == "play.example.com" && t.port == 19133
        ));
        assert_eq!(state.players.len(), 1);
    }
}
