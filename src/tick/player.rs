//! The public player API on [`Player`]: who it is, where it is, the messages it is shown, and
//! leaving (its survival state is in `survival::api`); and the per-tick sync that does what the
//! setters marked.
//!
//! A setter changes the value at once (after [`Context::teleport`], [`Player::position`] is the
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

use tracing::info;

use super::queue::SessionId;
use super::survival::Stay;
use super::{
    Context, Output, Phase, Player, State, checkpoint, dispatch, inside_border, inventory, survival,
};
use crate::event::{
    Actor, EntityTeleport, PlayerKick, PlayerQuit, PlayerTransfer, QuitReason, TeleportCause,
};
use crate::math::Vec3;
use crate::player::{Error, HudElement, Id, Message, Packets, Result};
use crate::protocol::Uuid;
use crate::protocol::consts::{self, DeviceOs, DisconnectReason, HudVisibility, InputMode};
use crate::protocol::packet::{
    Disconnect, Packet, PlaySound, SetHud, SpawnParticleEffect, Transfer,
};
use crate::save::WorldIndex;
use crate::world;

/// The lines a player's screen shows at once unless set.
const DEFAULT_LINE_HEIGHT: u32 = 7;
/// The default quit message: the yellow "left the game" line, which the client translates with
/// the player's display name. A [`PlayerQuit`] message left at it is sent so; any other is sent
/// as it is.
pub(super) const LEFT: &str = "§e%multiplayer.player.left";
/// The longest transfer address: the longest host name.
const MAX_ADDRESS: usize = 253;
/// The client's "disconnected" text, for a kick without a reason.
const NO_REASON: &str = "disconnectionScreen.noReason";
/// The log message of a transfer unless one is given.
const TRANSFERRED: &str = "Moved to another server";

/// What the API marked on a player for the end of the tick, and the chat limit.
#[derive(Debug, Default)]
pub(super) struct Marks {
    /// The name chat shows; `None` is the player's name. Not saved.
    display_name: Option<String>,
    /// Why the player leaves at the end of the tick.
    leave: Option<Leave>,
    /// Where the player moves at the end of the tick: the world and the eye position, checked.
    teleport: Option<(WorldIndex, Vec3)>,
    /// The tick of the player's last chat line and the lines it sent in that tick.
    pub(super) chat: (u64, u8),
    /// The lines the player's screen shows at once; `None` is 7. Not saved.
    line_height: Option<u32>,
    /// The screen elements the player's client hides, one [`HudElement::bit`] each. Not saved.
    hidden_hud: u16,
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
            hidden_hud,
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
        h.u64(u64::from(*hidden_hud));
    }
}

checkpoint::fields!(Marks {
    display_name,
    leave,
    teleport,
    chat,
    line_height,
    hidden_hud,
});

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
        /// For the log and [`PlayerQuit`].
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

checkpoint::variants!(Leave {
    0 => Kick {
        reason,
        screen_message,
        quit_message,
    },
    1 => Transfer {
        address,
        port,
        message,
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
    first_played_ms: i64,
    /// When the player's data was saved before this join (`lastPlayed`).
    last_played_ms: i64,
    /// The engine tick the saved data was placed at, which with the join time dates a save.
    placed_tick: u64,
}

impl Presence {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            ping_ms,
            first_played_ms,
            last_played_ms,
            placed_tick,
        } = self;
        h.u64(ping_ms.map_or(0, |p| u64::from(p) + 1));
        h.u64(first_played_ms.cast_unsigned());
        h.u64(last_played_ms.cast_unsigned());
        h.u64(*placed_tick);
    }

    /// What a save at engine tick `tick` keeps: the first join, and the time of the save (the tick
    /// reads no wall clock; it dates it from the join time and the ticks since, one
    /// [`TICK`](super::TICK) each).
    pub(super) fn saved(&self, join_ms: i64, tick: u64) -> (i64, i64) {
        let ticks = u128::from(tick.saturating_sub(self.placed_tick));
        let since =
            i64::try_from(ticks.saturating_mul(super::TICK.as_millis())).unwrap_or(i64::MAX);
        (self.first_played_ms, join_ms.saturating_add(since))
    }

    /// Takes the play history of the saved data placed at engine tick `tick` (each `None` for a
    /// new player, which gets `join_ms`).
    pub(super) fn place(
        &mut self,
        (first, last): (Option<i64>, Option<i64>),
        join_ms: i64,
        tick: u64,
    ) {
        self.first_played_ms = first.unwrap_or(join_ms);
        self.last_played_ms = last.unwrap_or(join_ms);
        self.placed_tick = tick;
    }

    /// Sets the latest round trip time to `millis` milliseconds.
    pub(super) fn set_ping(&mut self, millis: u32) {
        self.ping_ms = Some(millis);
    }
}

checkpoint::fields!(Presence {
    ping_ms,
    first_played_ms,
    last_played_ms,
    placed_tick,
});

impl Player {
    /// The client's random number, as its login tells (`getExtraData()["ClientRandomId"]`); 0
    /// when not sent.
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn client_random_id(&self) -> i64 {
        self.identity.client.random_id
    }

    /// The device ID, as its login tells (`getExtraData()["DeviceId"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_id(&self) -> &str {
        &self.identity.client.device_id
    }

    /// The device model, as its login tells (`getExtraData()["DeviceModel"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_model(&self) -> &str {
        &self.identity.client.device_model
    }

    /// The device the client runs on, as its login tells (`getExtraData()["DeviceOS"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn device_os(&self) -> DeviceOs {
        DeviceOs::try_from(self.identity.device_os).unwrap_or(DeviceOs::Unknown)
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

    /// When the player first joined this server, by the wall clock (its saved `firstPlayed`, or
    /// this join for a new player).
    #[doc(alias = "Player::getFirstPlayed")]
    #[doc(alias = "IPlayer::getFirstPlayed")]
    pub fn first_played(&self) -> SystemTime {
        unix_time(self.presence.first_played_ms)
    }

    /// The client's version, such as `1.26.60`, as its login tells
    /// (`getExtraData()["GameVersion"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn game_version(&self) -> &str {
        &self.identity.client.game_version
    }

    /// Whether the player gets what goes to everyone in the game (chat, join and quit messages,
    /// skins): joined (its saved data has loaded) and not leaving.
    pub(crate) fn gets_broadcasts(&self) -> bool {
        self.phase != Phase::Loading && self.is_online()
    }

    /// Whether the player played here before this join: its data was saved more than a
    /// millisecond after its first join.
    #[doc(alias = "Player::hasPlayedBefore")]
    #[doc(alias = "IPlayer::hasPlayedBefore")]
    pub fn has_played_before(&self) -> bool {
        self.presence
            .last_played_ms
            .saturating_sub(self.presence.first_played_ms)
            > 1
    }

    /// The handle to keep beyond the tick.
    pub fn id(&self) -> Id {
        Id::new(self.session)
    }

    /// The input mode the client started in, as its login tells;
    /// [`InputMode::Undefined`] for a value the protocol
    /// does not name.
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn input_mode(&self) -> InputMode {
        self.identity.client.input_mode
    }

    /// The IP address the client connected from, which IP bans match; `None` only for a player
    /// made without a connection (tests).
    #[doc(alias = "NetworkSession::getIp")]
    pub fn ip(&self) -> Option<IpAddr> {
        self.identity.address
    }

    /// Whether the player signed in with Xbox.
    #[doc(alias = "Player::isAuthenticated")]
    pub fn is_authenticated(&self) -> bool {
        self.identity.xuid.is_some()
    }

    /// Whether the player stays connected: false once it was kicked or transferred, until it
    /// leaves at the end of the tick.
    #[doc(alias = "Player::isOnline")]
    #[doc(alias = "Player::isConnected")]
    pub fn is_online(&self) -> bool {
        self.marks.leave.is_none()
    }

    /// When the player's data was last saved before this join, by the wall clock (its saved
    /// `lastPlayed`, or this join for a new player). It does not move while the player plays.
    #[doc(alias = "Player::getLastPlayed")]
    #[doc(alias = "IPlayer::getLastPlayed")]
    pub fn last_played(&self) -> SystemTime {
        unix_time(self.presence.last_played_ms)
    }

    /// The language the client runs in, such as `en_US`, as its login tells.
    #[doc(alias = "Player::getLocale")]
    #[doc(alias = "PlayerInfo::getLocale")]
    pub fn locale(&self) -> &str {
        &self.identity.client.locale
    }

    /// The player's name: its Xbox gamertag with Xbox sign-in.
    #[doc(alias = "Player::getName")]
    pub fn name(&self) -> &str {
        &self.identity.name
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

    /// The PlayFab master player ID of the login's identity token (`mid`); empty when not sent,
    /// as with the legacy login chain. With Xbox sign-in (`xbox-auth`) the token is verified;
    /// without it, the value is the client's claim.
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn playfab_id(&self) -> &str {
        &self.identity.client.playfab_id
    }

    /// The port the client connected from; 0 only for a player made without a connection
    /// (tests).
    #[doc(alias = "NetworkSession::getPort")]
    pub fn port(&self) -> u16 {
        self.identity.client.port
    }

    /// Where the player's feet are: 1.62 below its eyes, rounded to 4 decimals,
    /// so the feet of a player standing on a block are on it (its client sends the eyes as
    /// floats, a hair off). With [`world`](Self::world), [`yaw`](Self::yaw) and
    /// [`pitch`](Self::pitch) its location.
    #[doc(alias = "Entity::getPosition")]
    #[doc(alias = "Entity::getLocation")]
    pub fn position(&self) -> Vec3 {
        survival::feet_at(self.eye_position())
    }

    /// Where the player's eyes are: 1.62 above its feet.
    #[doc(alias = "Entity::getEyePos")]
    pub fn eye_position(&self) -> Vec3 {
        self.marks.teleport.map_or(self.position, |(_, eye)| eye)
    }

    /// The lines the player's screen shows at once, for a server's own paged output (Anchor pages
    /// nothing by it): 7 unless [`set_screen_line_height`](Self::set_screen_line_height) changed
    /// it.
    #[doc(alias = "Player::getScreenLineHeight")]
    pub fn screen_line_height(&self) -> u32 {
        self.marks.line_height.unwrap_or(DEFAULT_LINE_HEIGHT)
    }

    /// The UUID the client made for itself, as its login tells (`getExtraData()["SelfSignedId"]`);
    /// empty when not sent.
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn self_signed_id(&self) -> &str {
        &self.identity.client.self_signed_id
    }

    /// The address the client was told to join, such as `play.example.com:19132`, as its login
    /// tells (`getExtraData()["ServerAddress"]`).
    #[doc(alias = "PlayerInfo::getExtraData")]
    pub fn server_address(&self) -> &str {
        &self.identity.client.server_address
    }

    /// The skin the client sent at login.
    #[doc(alias = "Human::getSkin")]
    pub fn skin(&self) -> &crate::protocol::Skin {
        self.identity.skin.get()
    }

    /// The player's UUID, from its login.
    #[doc(alias = "Player::getUniqueId")]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// The chunk radius the player sees: what its client asked for, at most `view-distance`, or
    /// what [`Context::set_view_distance`](crate::Context::set_view_distance) set since; `None`
    /// until it asked.
    #[doc(alias = "Player::getViewDistance")]
    pub fn view_distance(&self) -> Option<u8> {
        self.chunk_radius
    }

    /// The world the player is in.
    #[doc(alias = "Entity::getWorld")]
    pub fn world(&self) -> world::Id {
        world::Id::of(self.marks.teleport.map_or(self.world, |(world, _)| world))
    }

    /// The Xbox user ID; `None` without Xbox sign-in.
    #[doc(alias = "Player::getXuid")]
    pub fn xuid(&self) -> Option<u64> {
        self.identity.xuid
    }

    /// Plays `sound` as [`add_sound`](Self::add_sound) does, heard at full volume however far
    /// `pos` is, so a sound played where the player stands keeps up with it while it moves.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideWorld`] for a `pos` that is not finite or outside the world border.
    /// [`Error::NoFullVolume`] for a sound the client plays from a level event (a door's, a
    /// click), which has no full-volume form.
    pub fn add_full_volume_sound(&mut self, pos: Vec3, sound: world::Sound) -> Result<()> {
        check_position(pos)?;
        let packet = sound.full_volume_packet(pos).ok_or(Error::NoFullVolume)?;
        self.send_packet(packet);
        Ok(())
    }

    /// Plays one of the world's sounds to this player alone at `pos`, as [`Context::add_sound`]
    /// plays it to everyone who sees the place.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideWorld`] for a `pos` that is not finite or outside the world border.
    pub fn add_sound(&mut self, pos: Vec3, sound: world::Sound) -> Result<()> {
        check_position(pos)?;
        self.send_packet(sound.packet(pos));
        Ok(())
    }

    /// Hides `elements` of the player's screen. The packet follows this tick's other packets;
    /// an element already hidden is not sent again, so a repeated call changes nothing. The
    /// hidden elements are kept for the connection and not saved, and a new join shows
    /// everything.
    pub fn hide_hud(&mut self, elements: &[HudElement]) {
        self.change_hud(elements, HudVisibility::Hide);
    }

    /// Disconnects the player at the end of the tick with `reason` on its screen (the client's
    /// "disconnected" text when empty); the other players see that it left the game.
    ///
    /// # Errors
    ///
    /// [`Error::Leaving`] if the player already leaves.
    /// [`Error::TooLong`] for a reason longer than 65,536 bytes.
    #[doc(alias = "Player::kick")]
    #[doc(alias = "Player::disconnect")]
    pub fn kick(&mut self, reason: impl Into<String>) -> Result<()> {
        if self.marks.leave.is_some() {
            return Err(Error::Leaving);
        }
        let reason = reason.into();
        crate::player::check_text(&reason)?;
        self.marks.leave = Some(Leave::Kick {
            reason,
            screen_message: None,
            quit_message: LEFT.to_owned(),
        });
        Ok(())
    }

    /// Plays the sound named `name` (a vanilla or resource pack sound, such as `random.orb`) to
    /// this player alone at `pos`, once; a `volume` and `pitch` of 1 are the sound's own.
    /// [`Context::add_sound`] plays the world's sounds to everyone who sees a place.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a name longer than 65,536 bytes.
    /// [`Error::OutsideWorld`] for a `pos` that is not finite or outside the world border.
    /// [`Error::OutOfRange`] for a volume or pitch that is negative or not finite.
    pub fn play_sound(&mut self, name: &str, pos: Vec3, volume: f32, pitch: f32) -> Result<()> {
        crate::player::check_text(name)?;
        check_position(pos)?;
        if ![volume, pitch].iter().all(|v| v.is_finite() && *v >= 0.0) {
            return Err(Error::OutOfRange);
        }
        self.send_packet(Packet::PlaySound(PlaySound {
            sound_name: name.to_owned(),
            position: pos.to_f32(),
            volume,
            pitch,
            loop_count: 0,
            bypass_listener_range_check: false,
            handle: None,
            playback_position_seconds: None,
        }));
        Ok(())
    }

    /// Shows the player `message`. The packets follow this tick's
    /// other packets. More than 4,096 packets queued in one tick disconnect the player
    /// ([`send_packet`](Self::send_packet)). A message a client does not decode ([`Message`] gives
    /// the bounds) sends nothing.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a text or chat source name over its bound.
    /// [`Error::TooManyParameters`] for more than 4 translation parameters.
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

    /// Sets the name chat shows: chat lines and the join and quit lines name the player by it.
    /// Not saved.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a name longer than 65,536 bytes.
    #[doc(alias = "Player::setDisplayName")]
    pub fn set_display_name(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        crate::player::check_text(&name)?;
        self.marks.display_name = Some(name);
        Ok(())
    }

    /// Sets the lines the player's screen shows at once; `None` goes back to 7. Not saved.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for 0.
    #[doc(alias = "Player::setScreenLineHeight")]
    pub fn set_screen_line_height(&mut self, height: Option<u32>) -> Result<()> {
        if height == Some(0) {
            return Err(Error::OutOfRange);
        }
        self.marks.line_height = height;
        Ok(())
    }

    /// Shows `elements` of the player's screen again, as [`hide_hud`](Self::hide_hud) hides them;
    /// an element that is shown is not sent again.
    pub fn show_hud(&mut self, elements: &[HudElement]) {
        self.change_hud(elements, HudVisibility::Reset);
    }

    /// Shows the particle effect named `name` (a vanilla or resource pack effect, such as
    /// `minecraft:totem_particle`) to this player alone at `pos`. [`Context::add_particle`] shows
    /// the world's particles to everyone who sees a place.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a name longer than 65,536 bytes.
    /// [`Error::OutsideWorld`] for a `pos` that is not finite or outside the world border.
    pub fn show_particle(&mut self, name: &str, pos: Vec3) -> Result<()> {
        crate::player::check_text(name)?;
        check_position(pos)?;
        self.send_packet(Packet::SpawnParticleEffect(SpawnParticleEffect {
            // The overworld (`Dimension::Overworld`), every world's dimension.
            dimension: 0,
            // Not an entity's: shown at the position.
            entity_unique_id: -1,
            position: pos.to_f32(),
            particle_name: name.to_owned(),
            molang_variables: None,
        }));
        Ok(())
    }

    /// Sends the player to the server at `address` (a host name or IP address) and `port` at the
    /// end of the tick; it leaves this one, and the other players see that it left the game.
    ///
    /// # Errors
    ///
    /// [`Error::Leaving`] if the player already leaves.
    /// [`Error::InvalidAddress`] for an empty address or one longer than 253 bytes.
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

    /// Sends the elements whose visibility `visibility` changes, in one packet.
    fn change_hud(&mut self, elements: &[HudElement], visibility: HudVisibility) {
        let hiding = visibility == HudVisibility::Hide;
        let mut changed = Vec::new();
        for &element in elements {
            let bit = element.bit();
            if (self.marks.hidden_hud & bit != 0) == hiding {
                continue;
            }
            self.marks.hidden_hud ^= bit;
            changed.push(i32::from(consts::HudElement::from(element)));
        }
        if !changed.is_empty() {
            self.send_packet(Packet::SetHud(SetHud {
                elements: changed,
                visibility: visibility.into(),
            }));
        }
    }
}

impl Context<'_> {
    /// Kicks `player` as [`Player::kick`] does, after asking the [`PlayerKick`] listeners, which
    /// may keep it or change the reason, the screen message and the quit message the other
    /// players see (`quit_message` `None` is the usual "left the game" line, an empty one shows
    /// nothing). Nothing happens if the player is gone or leaving or a text is longer than 65,536
    /// bytes, before the listeners are asked or after they changed it, or if a listener cancels
    /// it.
    pub(crate) fn kick_player(
        &mut self,
        player: Id,
        reason: String,
        screen_message: Option<String>,
        quit_message: Option<String>,
    ) {
        let session = player.session();
        let can_kick = |state: &State, reason: &str, screen: Option<&str>, quit: &str| {
            state.players.get(&session).is_some_and(Player::is_online)
                && [Some(reason), screen, Some(quit)]
                    .into_iter()
                    .flatten()
                    .all(|t| crate::player::check_text(t).is_ok())
        };
        let (mut reason, mut screen_message) = (reason, screen_message);
        let mut quit_message = quit_message.unwrap_or_else(|| LEFT.to_owned());
        if !can_kick(
            self.state,
            &reason,
            screen_message.as_deref(),
            &quit_message,
        ) {
            return;
        }
        let mut event = None;
        if dispatch::heard::<PlayerKick>(self.state) {
            let e = event.insert(PlayerKick {
                player,
                kick_reason: std::mem::take(&mut reason),
                screen_message: screen_message.take(),
                quit_message: std::mem::take(&mut quit_message),
                cancelled: false,
            });
            dispatch::call(self.state, e);
            if e.cancelled
                || (dispatch::listened::<PlayerKick>(self.state)
                    && !can_kick(
                        self.state,
                        &e.kick_reason,
                        e.screen_message.as_deref(),
                        &e.quit_message,
                    ))
            {
                return;
            }
            reason.clone_from(&e.kick_reason);
            screen_message.clone_from(&e.screen_message);
            quit_message.clone_from(&e.quit_message);
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
    }

    /// Moves `player`'s feet to `feet` in `world` as [`teleport`](Self::teleport) does, after
    /// asking the [`EntityTeleport`] listeners, which may keep it where it is or send it
    /// elsewhere. Nothing happens, and the monitors hear nothing, if the teleport fails before
    /// the listeners are asked or after they changed it, or if a listener cancels it.
    pub(crate) fn teleport_player(
        &mut self,
        player: Id,
        world: world::Id,
        feet: Vec3,
        cause: TeleportCause,
    ) {
        let Some(origin) = self.player(player).map(|p| (p.world(), p.position())) else {
            return;
        };
        let eye = feet + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        if survival::check_destination(self.state, world.index(), eye).is_err() {
            return;
        }
        let (mut world, mut feet) = (world, feet);
        let mut event = None;
        if dispatch::heard::<EntityTeleport>(self.state) {
            let e = event.insert(EntityTeleport {
                entity: Actor::Player(player),
                cause,
                origin_world: origin.0,
                origin: origin.1,
                target_world: world,
                target: feet,
                cancelled: false,
            });
            dispatch::call(self.state, e);
            if e.cancelled {
                return;
            }
            (world, feet) = (e.target_world, e.target);
        }
        let moved = self.teleport(player, world, feet).is_ok();
        if moved && let Some(e) = &event {
            dispatch::watch(self.state, e);
        }
    }

    /// Moves `player`'s feet to `feet` in `world`: by the end of the tick its window closes, its
    /// breaking stops, its fall ends, and its client and viewers see it there; another world
    /// sends its chunks again and raises
    /// [`PlayerWorldChange`](crate::event::PlayerWorldChange). [`Player::position`] is the new
    /// place at once; a second teleport in the tick wins, and a world that unloads before the
    /// player moves sends it to the default world's spawn as it sends the world's players. The
    /// player keeps its rotation unless [`Player::set_rotation`] turns it. A teleport that fails
    /// changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Offline`] if the player is not in the game.
    /// [`Error::WorldNotLoaded`] for a world that is not loaded (unloaded, or an ID of another
    /// server).
    /// [`Error::OutsideWorld`] for a position that is not finite or lies outside the world
    /// border.
    #[doc(alias = "Player::teleport")]
    #[doc(alias = "Entity::teleport")]
    pub fn teleport(&mut self, player: Id, world: world::Id, feet: Vec3) -> Result<()> {
        let eye = feet + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        let checked = survival::check_destination(self.state, world.index(), eye);
        let p = self.player_mut(player).ok_or(Error::Offline)?;
        checked?;
        p.marks.teleport = Some((world.index(), eye));
        Ok(())
    }

    /// Sends `player` to the server at `address` and `port` as [`Player::transfer`] does, after
    /// asking the [`PlayerTransfer`] listeners, which may keep it or change the address, the port
    /// and the `message` for the log and [`PlayerQuit`] (`None`: "Moved to another server").
    /// Nothing happens if the player is gone or leaving, the address is empty or longer than 253
    /// bytes or the message longer than 65,536 bytes, before the listeners are asked or after
    /// they changed it, or if a listener cancels it.
    pub(crate) fn transfer_player(
        &mut self,
        player: Id,
        address: String,
        port: u16,
        message: Option<String>,
    ) {
        let session = player.session();
        let can_send = |state: &State, address: &str, message: &str| {
            state.players.get(&session).is_some_and(Player::is_online)
                && is_valid_address(address)
                && crate::player::check_text(message).is_ok()
        };
        let mut leave = (
            address,
            port,
            message.unwrap_or_else(|| TRANSFERRED.to_owned()),
        );
        if !can_send(self.state, &leave.0, &leave.2) {
            return;
        }
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
                || (dispatch::listened::<PlayerTransfer>(self.state)
                    && !can_send(self.state, &e.address, &e.message))
            {
                return;
            }
            leave = (e.address.clone(), e.port, e.message.clone());
        }
        let (address, port, message) = leave;
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
    }
}

/// Does what the player setters marked this tick, after the inputs, tasks and rules and before
/// the views move and the output is built: kicked and transferred players get their last packet
/// and leave, refused hotbar selections are answered, teleported players move, the survival
/// marks are done, and changed skins go out within the server's skin budget.
pub(super) fn sync(state: &mut State, output: &mut Output) {
    // Most ticks mark no one: one pass over the players finds the marked ones, and the next one
    // is looked for after each is done, so marks it sets on later players are seen too.
    let mut after = Bound::Unbounded;
    // Changed skins go out in turn once the pass is done, on the ticks it met one.
    let mut skins_wait = false;
    loop {
        let marked = state
            .players
            .range_mut((after, Bound::Unbounded))
            .find(|(_, p)| {
                p.marks.leave.is_some()
                    || inventory::has_unanswered_hold(p)
                    || p.marks.teleport.is_some()
                    || survival::marked(p)
            });
        let Some((&session, player)) = marked else {
            break;
        };
        after = Bound::Excluded(session);
        if let Some(leave) = player.marks.leave.take() {
            let reason = leave.reason().to_owned();
            let quit_reason = match &leave {
                Leave::Kick { .. } => {
                    info!(session = session.get(), player = %player.identity.name, %reason, "player kicked");
                    QuitReason::Kicked(reason)
                }
                Leave::Transfer { address, port, .. } => {
                    info!(session = session.get(), player = %player.identity.name, %address, port, %reason, "player transferred");
                    QuitReason::Transferred(reason)
                }
            };
            let (packet, quit_message) = leave.into_parts();
            state.send_off(session, quit_reason, quit_message, packet, output);
            continue;
        }
        inventory::answer_held(player);
        skins_wait |= survival::skin_waits(player);
        let from = player.world;
        teleport_marked(state, session);
        // Here and not in `quit`: a player that leaves has had its `PlayerQuit`. Before the
        // survival marks, so a respawn into a third world comes after it.
        survival::world_changed(state, session, from);
        survival::sync(state, session, Stay::Online);
    }
    if skins_wait {
        survival::send_skins(state);
    }
}

/// Sends the players of the world `from`, which unloads, to `eye` in the world `to`: a teleport
/// into `from` that waits for the end of a tick (marked by a listener of the view phase) goes
/// there too, so a teleport that succeeded is never dropped.
pub(super) fn redirect_teleports(state: &mut State, from: WorldIndex, to: WorldIndex, eye: Vec3) {
    for player in state.players.values_mut() {
        if let Some(mark) = &mut player.marks.teleport
            && mark.0 == from
        {
            *mark = (to, eye);
        }
    }
}

/// Raises [`PlayerQuit`] for the player of `session`, who is about to leave for `reason`:
/// its listeners may change `quit_message`, then what the API marked on the
/// player, the listeners' marks included, takes effect (the save follows the event, so a teleport
/// is the saved place and a kill dies with its drops), then every other online player sees the
/// message in chat: [`LEFT`] as the translated line with the player's display name, an empty
/// one not at all, any other as it is. Call it just before the player is removed, in the tick's
/// serial step. A player still loading its data has not joined, so it raises nothing.
pub(super) fn quit(
    state: &mut State,
    session: SessionId,
    reason: QuitReason,
    quit_message: String,
) {
    let Some(name) = state
        .players
        .get(&session)
        .filter(|p| p.phase != Phase::Loading)
        .map(|p| p.display_name().to_owned())
    else {
        return;
    };
    let mut event = None;
    let message = if dispatch::heard::<PlayerQuit>(state) {
        let e = event.insert(PlayerQuit {
            player: Id::new(session),
            quit_message,
            reason,
        });
        dispatch::call(state, e);
        e.quit_message.clone()
    } else {
        quit_message
    };
    teleport_marked(state, session);
    survival::sync(state, session, Stay::Leaving);
    if !message.is_empty() {
        let message = if message == LEFT {
            Message::Translation {
                key: message,
                parameters: vec![name],
            }
        } else {
            Message::Raw(message)
        };
        super::spawn::tell_others(state, session, message);
    }
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
}

/// Moves the player of `session` if it was teleported.
fn teleport_marked(state: &mut State, session: SessionId) {
    if let Some((world, eye)) = state
        .players
        .get_mut(&session)
        .and_then(|p| p.marks.teleport.take())
    {
        survival::teleport(state, session, world, eye);
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
        if !player.gets_broadcasts() {
            continue;
        }
        for packet in packets.iter().flatten() {
            player.send_packet(packet.clone());
        }
        count += 1;
    }
    count
}

/// Whether `address` can be a transfer address: a host name or IP address, not empty and at most
/// 253 bytes.
fn is_valid_address(address: &str) -> bool {
    !address.is_empty() && address.len() <= MAX_ADDRESS
}

/// A position finite and inside the world border.
fn check_position(pos: Vec3) -> Result<()> {
    if !inside_border(pos) {
        return Err(Error::OutsideWorld);
    }
    Ok(())
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use crate::event::Listeners;
    use crate::nbt::Value;
    use crate::player::{MAX_TEXT_BYTES, TitleTimes};
    use crate::tick::Context;
    use crate::tick::log::{EntryKind, InputLog};
    use crate::tick::queue::{
        Client, Identity, Input, InputKind, LeaveReason, Measure, Outbound, SessionId,
    };
    use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};

    /// The number of players in the skin broadcast test.
    const PLAYERS: u64 = 48;

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
    fn a_skin_broadcast_is_one_shared_copy_within_the_skin_budget() {
        let (mut state, mut receivers) = many(PLAYERS);
        // Two skins of 3 MiB: together over the budget's one skin of 4 MiB.
        for s in [1, 2] {
            let p = player_mut(&mut state, s);
            let mut skin = p.skin().clone();
            skin.skin_id = format!("big{s}");
            let mut geometry = vec![b' '; 3 << 20];
            geometry[0] = b'{';
            *geometry.last_mut().unwrap() = b'}';
            skin.skin_geometry = geometry;
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
            usize::try_from(PLAYERS).unwrap() + 1
        );
        drop(first);
        assert!(survival::marked(&state.players[&SessionId::new(2)]));
        for p in state.players.values_mut() {
            p.shared_outbox.clear();
        }
        // The second skin goes to everyone once the budget has refilled what it lacks: 2 MiB at
        // 128 KiB a second, 16 seconds.
        let mut skins = vec![Vec::new(); receivers.len()];
        for t in 0..20 * 20 {
            run(&mut state, Vec::new());
            for (rx, skins) in receivers.iter_mut().zip(&mut skins) {
                skins.extend(received(rx).0.into_iter().filter_map(|p| match p {
                    Packet::PlayerSkin(s) => Some(s.skin.skin_id.clone()),
                    _ => None,
                }));
            }
            let marked = survival::marked(&state.players[&SessionId::new(2)]);
            assert_eq!(marked, skins[0].is_empty());
            if !marked {
                assert!(t > 15 * 20, "not before the budget refilled");
                break;
            }
        }
        assert!(skins.iter().all(|s| s == &["big2"]));
    }

    /// Players 1 and 2 spawned next to each other, their queues emptied.
    pub(in crate::tick) fn two() -> (State, [mpsc::Receiver<Outbound>; 2]) {
        two_with(Listeners::default())
    }

    /// [`two`] with `listeners`.
    fn two_with(listeners: Listeners) -> (State, [mpsc::Receiver<Outbound>; 2]) {
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

    // api: Player::getName, Player::getDisplayName, Player::setDisplayName, Player::getXuid
    // api: Player::isAuthenticated, Player::getUniqueId, Human::getSkin
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
            p.set_display_name("x".repeat(MAX_TEXT_BYTES + 1)),
            Err(Error::TooLong)
        );
        p.set_display_name("§aBoss").unwrap();
        assert_eq!(p.display_name(), "§aBoss");
        assert_eq!(p.name(), "player1");
        assert_ne!(state.hash(), before);
    }

    // api: Player::teleport, Entity::teleport, Entity::getPosition, Entity::getLocation
    // api: Entity::getEyePos, Entity::getWorld
    #[test]
    fn a_teleport_moves_the_position_at_once_and_the_client_and_viewers_by_the_end_of_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let id = Id::new(SessionId::new(1));
        let start = player_mut(&mut state, 1).position();
        let mut cx = Context { state: &mut state };
        for feet in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(30_000_001.0, -60.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
        ] {
            assert_eq!(
                cx.teleport(id, world::Id::DEFAULT, feet),
                Err(Error::OutsideWorld)
            );
        }
        let gone = Id::new(SessionId::new(9));
        assert_eq!(
            cx.teleport(gone, world::Id::DEFAULT, start),
            Err(Error::Offline)
        );
        let elsewhere = world::Id::of(7);
        assert_eq!(
            cx.teleport(id, elsewhere, start),
            Err(Error::WorldNotLoaded)
        );
        assert_eq!(player_mut(&mut state, 1).position(), start);
        let to = Vec3::new(3.5, -60.0, 2.5);
        let mut cx = Context { state: &mut state };
        cx.teleport(id, world::Id::DEFAULT, Vec3::new(9.5, -60.0, 9.5))
            .unwrap();
        cx.teleport(id, world::Id::DEFAULT, to).unwrap();
        let p = player_mut(&mut state, 1);
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

    // api: Entity::getPosition
    /// The feet of a player standing on a block are on it: its client sends the eyes as floats,
    /// a hair below 1.62 above the block, and the feet are rounded as the survival rules round
    /// them.
    #[test]
    fn the_feet_of_a_player_on_a_block_are_on_that_block() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        for y in [-60, 31, 32, 64] {
            let eye = Vec3::new(0.5, f64::from(y) + survival::EYE_HEIGHT, 0.5);
            p.position = Vec3::from_f32(eye.to_f32());
            assert_eq!(p.position().y, f64::from(y));
            assert_eq!(crate::world::BlockPos::containing(p.position()).y, y);
        }
    }

    // api: Player::sendMessage, Player::sendTip, Player::sendPopup, Player::sendJukeboxPopup
    // api: Player::sendActionBarMessage, Player::sendTitle, Player::sendSubTitle
    // api: Player::removeTitles, Player::resetTitles, Player::setTitleDuration
    // api: Player::sendToastNotification, NetworkSession::sendDataPacket
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
            p.send(Message::Raw("x".repeat(MAX_TEXT_BYTES + 1))),
            Err(Error::TooLong)
        );
        p.send_packet(Packet::Text(Box::new(crate::protocol::packet::Text {
            message: "own".to_owned(),
            ..crate::protocol::packet::Text::default()
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

    // api: Player::kick, Player::disconnect, Player::isOnline, Player::isConnected
    #[test]
    fn a_kick_disconnects_once_with_its_reason_at_the_end_of_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert!(p.is_online());
        assert_eq!(p.kick("x".repeat(MAX_TEXT_BYTES + 1)), Err(Error::TooLong));
        p.kick("bye").unwrap();
        assert!(!p.is_online());
        assert_eq!(p.kick("again"), Err(Error::Leaving));
        assert_eq!(p.transfer("example.com", 19_132), Err(Error::Leaving));
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

    // api: PlayerQuitEvent
    #[test]
    fn the_others_see_the_translated_left_line_unless_a_listener_changes_it() {
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
                    let own = if *e.reason() == QuitReason::Kicked("quiet".to_owned()) {
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

    // api: Player::setDisplayName
    /// The left line names the player by its display name, as the join line and chat do.
    #[test]
    fn the_left_line_names_the_player_by_its_display_name() {
        let (mut state, [_rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        p.set_display_name("§cBoss").unwrap();
        p.kick("bye").unwrap();
        run(&mut state, Vec::new());
        let left = received(&mut rx2).0.into_iter().find_map(|p| match p {
            Packet::Text(t) if t.message == LEFT => Some(t.parameters),
            _ => None,
        });
        assert_eq!(left, Some(vec!["§cBoss".to_owned()]));
    }

    // api: PlayerQuitEvent
    /// What a [`PlayerQuit`] listener does to the quitting player takes effect before it is
    /// saved, which follows the event: a teleport is the saved position, and a kill drops
    /// what the player carries with [`crate::event::PlayerDeath`].
    #[test]
    fn a_quit_listener_s_teleport_and_kill_are_saved_with_the_quitting_player() {
        let to = Vec3::new(3.5, -60.0, 2.5);
        let (deaths, mut died) = mpsc::channel(4);
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            move |e: &mut PlayerQuit, cx: &mut Context<'_>| {
                cx.teleport(e.player(), world::Id::DEFAULT, to).unwrap();
                assert!(cx.player_mut(e.player()).unwrap().kill());
            },
        );
        listeners.push_monitor(move |e: &crate::event::PlayerDeath, _: &Context<'_>| {
            deaths.try_send(e.drops().len()).unwrap();
        });
        let (mut state, _rx) = two_with(listeners);
        // The players joined without loading data; a leaving one is kept for the next commit.
        state.saving.players = true;
        let bread = crate::item::Stack::new(crate::item::Kind::Bread.default_type(), 5);
        assert!(inventory::give(player_mut(&mut state, 1), bread).is_empty());
        player_mut(&mut state, 1).kick("bye").unwrap();
        run(&mut state, Vec::new());
        assert_eq!(died.try_recv(), Ok(1), "one death, dropping the bread");
        let saved = &state.saving.offline.values().next().unwrap().data;
        let eye = to + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        assert_eq!(saved.position, [eye.x, eye.y, eye.z]);
        assert!(saved.extra.contains_key("DeathPositionX"));
        assert!(
            matches!(saved.extra.get("Inventory"), Some(Value::List(l)) if l.is_empty()),
            "{:?}",
            saved.extra.get("Inventory")
        );
    }

    /// Only the quits of the server's stop are shutdown quits.
    #[test]
    fn only_the_stop_s_quits_are_shutdown_quits() {
        let (quits, mut quit) = mpsc::channel(4);
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            move |e: &mut PlayerQuit, _: &mut Context<'_>| {
                quits.try_send(*e.reason() == QuitReason::Shutdown).unwrap();
            },
        );
        let (mut state, _rx) = two_with(listeners);
        player_mut(&mut state, 1).kick("bye").unwrap();
        run(&mut state, Vec::new());
        assert_eq!(quit.try_recv(), Ok(false), "a kick");
        state.close_all(&mut Output::default());
        assert_eq!(quit.try_recv(), Ok(true), "the stop");
        assert!(state.players.is_empty());
    }

    /// A sound or a particle played to one player reaches it alone: a named sound with its
    /// volume and pitch, one of the world's sounds, a named particle. A position outside the
    /// world, a volume or pitch that is not a finite amount, or a name too long sends nothing.
    #[test]
    fn sounds_and_particles_played_to_one_player_reach_it_alone() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let at = Vec3::new(1.5, -59.0, 2.5);
        let p = player_mut(&mut state, 1);
        p.play_sound("random.orb", at, 0.5, 1.5).unwrap();
        p.add_sound(at, crate::world::Sound::ChestOpen).unwrap();
        p.show_particle("minecraft:totem_particle", at).unwrap();
        let nowhere = Vec3::new(f64::NAN, 0.0, 0.0);
        assert_eq!(
            p.play_sound("random.orb", nowhere, 1.0, 1.0),
            Err(Error::OutsideWorld)
        );
        assert_eq!(
            p.add_sound(nowhere, crate::world::Sound::ChestOpen),
            Err(Error::OutsideWorld)
        );
        assert_eq!(
            p.show_particle("minecraft:totem_particle", nowhere),
            Err(Error::OutsideWorld)
        );
        assert_eq!(
            p.play_sound("random.orb", at, f32::NAN, 1.0),
            Err(Error::OutOfRange)
        );
        assert_eq!(
            p.play_sound("random.orb", at, 1.0, -1.0),
            Err(Error::OutOfRange)
        );
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(p.play_sound(&long, at, 1.0, 1.0), Err(Error::TooLong));
        assert_eq!(p.show_particle(&long, at), Err(Error::TooLong));
        run(&mut state, Vec::new());
        let effects = |packets: Vec<Packet>| -> Vec<Packet> {
            packets
                .into_iter()
                .filter(|p| {
                    matches!(
                        p,
                        Packet::PlaySound(_)
                            | Packet::LevelSoundEvent(_)
                            | Packet::SpawnParticleEffect(_)
                    )
                })
                .collect()
        };
        let got = effects(received(&mut rx1).0);
        assert!(
            matches!(&got[..], [
            Packet::PlaySound(s),
            Packet::LevelSoundEvent(chest),
            Packet::SpawnParticleEffect(e),
        ] if s.sound_name == "random.orb"
            && s.position == at.to_f32()
            && (s.volume, s.pitch) == (0.5, 1.5)
            && chest.sound_type == crate::protocol::consts::SoundEvent::ChestOpen.as_str()
            && e.particle_name == "minecraft:totem_particle"
            && e.position == at.to_f32()
            && e.entity_unique_id == -1),
            "{got:?}"
        );
        assert!(effects(received(&mut rx2).0).is_empty(), "the other player");
    }

    /// A full-volume sound reaches one player as the world's sound with the global flag; a sound
    /// sent as a level event, which has no such flag, and a position outside the world send
    /// nothing.
    #[test]
    fn a_full_volume_sound_reaches_one_player_with_the_global_flag() {
        use crate::world::{NoteInstrument, Sound};
        let (mut state, [mut rx1, mut rx2]) = two();
        let at = Vec3::new(1.5, -59.0, 2.5);
        let note = Sound::Note {
            instrument: NoteInstrument::Pling,
            note: 12,
        };
        let p = player_mut(&mut state, 1);
        p.add_full_volume_sound(at, note).unwrap();
        assert_eq!(
            p.add_full_volume_sound(at, Sound::XpCollect),
            Err(Error::NoFullVolume)
        );
        assert_eq!(
            p.add_full_volume_sound(Vec3::new(f64::NAN, 0.0, 0.0), note),
            Err(Error::OutsideWorld)
        );
        run(&mut state, Vec::new());
        let sounds = |packets: Vec<Packet>| -> Vec<Packet> {
            packets
                .into_iter()
                .filter(|p| matches!(p, Packet::LevelSoundEvent(_) | Packet::LevelEvent(_)))
                .collect()
        };
        let got = sounds(received(&mut rx1).0);
        let Packet::LevelSoundEvent(plain) = note.packet(at) else {
            panic!("a note is a level sound event");
        };
        assert!(
            matches!(&got[..], [Packet::LevelSoundEvent(s)]
            if *s == crate::protocol::packet::LevelSoundEvent {
                disable_relative_volume: true,
                ..plain.clone()
            }),
            "{got:?}"
        );
        assert!(sounds(received(&mut rx2).0).is_empty(), "the other player");
    }

    // api: Player::kick
    /// What a player is sent in the step that removes it, before the kick or by a quit listener
    /// (the stop's included), reaches it before the final `Disconnect`: the
    /// queued packets go ahead of the disconnect packet.
    #[test]
    fn messages_sent_in_the_leaving_step_reach_the_player_before_its_disconnect() {
        let texts = |packets: Vec<Packet>| -> Vec<String> {
            packets
                .into_iter()
                .filter_map(|p| match p {
                    Packet::Text(t) => Some(t.message),
                    _ => None,
                })
                .collect()
        };
        let listeners = || {
            let mut listeners = Listeners::default();
            listeners.push(
                crate::event::Priority::Normal,
                |e: &mut PlayerQuit, cx: &mut Context<'_>| {
                    let p = cx.player_mut(e.player()).unwrap();
                    p.send(Message::Raw("quit".to_owned())).unwrap();
                },
            );
            listeners
        };
        let (mut state, [mut rx1, _rx2]) = two_with(listeners());
        let p = player_mut(&mut state, 1);
        p.send(Message::Raw("before".to_owned())).unwrap();
        p.kick("bye").unwrap();
        run(&mut state, Vec::new());
        let (packets, close, _) = received(&mut rx1);
        assert_eq!(texts(packets), ["before", "quit"]);
        assert!(matches!(close, Some(Packet::Disconnect(_))));
        let (mut state, [mut rx1, _rx2]) = two_with(listeners());
        let mut output = Output::default();
        state.close_all(&mut output);
        crate::tick::deliver(&mut state, &mut output, &mut InputLog::default());
        let (packets, close, _) = received(&mut rx1);
        assert_eq!(texts(packets), ["quit"], "the stop");
        assert!(matches!(close, Some(Packet::Disconnect(_))));
    }

    /// An outbox that overflowed lost what came after it filled, so none of it goes, kicked or
    /// not: the player gets the `Disconnect` alone.
    #[test]
    fn a_player_kicked_with_an_overflowed_outbox_gets_only_its_disconnect() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        for _ in 0..=crate::tick::MAX_OUTBOX {
            p.send(Message::Raw("spam".to_owned())).unwrap();
        }
        p.kick("bye").unwrap();
        run(&mut state, Vec::new());
        let (packets, close, _) = received(&mut rx1);
        assert!(packets.is_empty(), "{} packets", packets.len());
        assert!(matches!(close, Some(Packet::Disconnect(_))));
    }

    // api: NetworkSession::getIp, Player::getViewDistance, Player::getScreenLineHeight
    // api: Player::setScreenLineHeight, Player
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

    /// The `SetHud` packets in `packets`, as (elements, visibility).
    fn hud_packets(packets: Vec<Packet>) -> Vec<(Vec<i32>, i32)> {
        packets
            .into_iter()
            .filter_map(|p| match p {
                Packet::SetHud(h) => Some((h.elements, h.visibility)),
                _ => None,
            })
            .collect()
    }

    /// Hiding screen elements sends the changed ones to that player alone, a repeat sends
    /// nothing and leaves the state hash as it was, and showing them again sends them back.
    #[test]
    fn hiding_and_showing_the_hud_sends_the_changes_to_that_player_alone() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let hide = i32::from(HudVisibility::Hide);
        let reset = i32::from(HudVisibility::Reset);
        let health = i32::from(consts::HudElement::Health);
        let food = i32::from(consts::HudElement::Hunger);
        let hot_bar = i32::from(consts::HudElement::HotBar);
        let both = [HudElement::Health, HudElement::Food];

        player_mut(&mut state, 1).hide_hud(&both);
        run(&mut state, Vec::new());
        assert_eq!(
            hud_packets(received(&mut rx1).0),
            [(vec![health, food], hide)]
        );
        assert!(hud_packets(received(&mut rx2).0).is_empty());

        let hash = state.hash();
        player_mut(&mut state, 1).hide_hud(&both);
        player_mut(&mut state, 1).hide_hud(&[HudElement::Health, HudElement::Health]);
        assert_eq!(state.hash(), hash, "the second call changes nothing");
        run(&mut state, Vec::new());
        assert!(hud_packets(received(&mut rx1).0).is_empty());

        player_mut(&mut state, 1).hide_hud(&[HudElement::HotBar, HudElement::Food]);
        player_mut(&mut state, 1).show_hud(&[HudElement::Health, HudElement::PaperDoll]);
        run(&mut state, Vec::new());
        assert_eq!(
            hud_packets(received(&mut rx1).0),
            [(vec![hot_bar], hide), (vec![health], reset)]
        );

        player_mut(&mut state, 1).show_hud(&[HudElement::Food, HudElement::HotBar]);
        run(&mut state, Vec::new());
        assert_eq!(
            hud_packets(received(&mut rx1).0),
            [(vec![food, hot_bar], reset)]
        );
        assert_eq!(player_mut(&mut state, 1).marks.hidden_hud, 0);
        assert!(hud_packets(received(&mut rx2).0).is_empty());
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
                self_signed_id: "8f3c1a2e-5b7d-4e6f-9a0b-1c2d3e4f5a6b".to_owned(),
                random_id: -4_242_424_242,
                playfab_id: "ABCDEF0123456789".to_owned(),
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

    // api: Player::getLocale, PlayerInfo::getLocale, PlayerInfo::getExtraData
    // api: NetworkSession::getPort, NetworkSession::getPing
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
        assert_eq!(p.self_signed_id(), "8f3c1a2e-5b7d-4e6f-9a0b-1c2d3e4f5a6b");
        assert_eq!(p.client_random_id(), -4_242_424_242);
        assert_eq!(p.playfab_id(), "ABCDEF0123456789");
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

    // api: Player::hasPlayedBefore, Player::getFirstPlayed, Player::getLastPlayed
    // api: IPlayer::hasPlayedBefore, IPlayer::getFirstPlayed, IPlayer::getLastPlayed
    /// A new player's first and last play are its join; the saved data keeps the first join and
    /// the time of the save, and the next run's join reads them back: the
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
        assert_eq!(saved("lastPlayed"), Some(Value::Long(first_join + 5000)));
        assert!(
            !player_mut(&mut state, 1).has_played_before(),
            "the last play does not move while the player plays"
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

    // api: Player::getLastPlayed
    /// Leaving dates the play history, so a player who changed nothing else is still saved
    /// when it leaves: its next join has played before.
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
            kind: InputKind::Leave(LeaveReason::Left),
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
        assert_eq!(saved("lastPlayed"), Some(Value::Long(first_join + 1000)));
    }

    // api: Player::transfer
    #[test]
    fn a_transfer_sends_the_address_and_leaves() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.transfer("", 1), Err(Error::InvalidAddress));
        assert_eq!(p.transfer("a".repeat(254), 1), Err(Error::InvalidAddress));
        p.transfer("play.example.com", 19_133).unwrap();
        // The leaving player gets no broadcast.
        let mut cx = Context { state: &mut state };
        assert_eq!(cx.broadcast(Message::Tip("t".to_owned())), Ok(1));
        run(&mut state, Vec::new());
        let (_, close, _) = received(&mut rx1);
        assert!(matches!(
            close,
            Some(Packet::Transfer(t)) if t.address == "play.example.com" && t.port == 19_133
        ));
        assert_eq!(state.players.len(), 1);
    }

    /// A kick, a transfer or a teleport that cannot happen raises no event: the listeners are
    /// asked only about one that can, and the player stays.
    #[test]
    fn an_impossible_kick_transfer_or_teleport_asks_no_listener() {
        let (asked, mut events) = mpsc::channel(8);
        let mut listeners = Listeners::default();
        let tx = asked.clone();
        listeners.push(
            crate::event::Priority::Normal,
            move |_: &mut PlayerKick, _: &mut Context<'_>| tx.try_send("kick").unwrap(),
        );
        let tx = asked.clone();
        listeners.push(
            crate::event::Priority::Normal,
            move |_: &mut PlayerTransfer, _: &mut Context<'_>| tx.try_send("transfer").unwrap(),
        );
        listeners.push(
            crate::event::Priority::Normal,
            move |_: &mut EntityTeleport, _: &mut Context<'_>| asked.try_send("teleport").unwrap(),
        );
        let (mut state, _rx) = two_with(listeners);
        let id = Id::new(SessionId::new(1));
        let mut cx = Context { state: &mut state };
        cx.kick_player(id, "x".repeat(MAX_TEXT_BYTES + 1), None, None);
        cx.transfer_player(id, String::new(), 19_132, None);
        let nowhere = Vec3::new(f64::NAN, 0.0, 0.0);
        cx.teleport_player(id, world::Id::DEFAULT, nowhere, TeleportCause::Action);
        let unloaded = world::Id::of(7);
        let feet = Vec3::new(0.5, -60.0, 0.5);
        cx.teleport_player(id, unloaded, feet, TeleportCause::Action);
        assert!(events.try_recv().is_err(), "no listener was asked");
        let p = &state.players[&SessionId::new(1)];
        assert!(p.is_online() && p.marks.teleport.is_none());
    }

    /// A teleport a listener sends to a world that is not loaded does not happen: the monitors
    /// hear nothing, and the player stays.
    #[test]
    fn a_teleport_a_listener_sends_to_an_unloaded_world_does_not_happen() {
        let (heard, mut monitors) = mpsc::channel(8);
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            |e: &mut EntityTeleport, _: &mut Context<'_>| {
                e.set_target(world::Id::of(7), e.target());
            },
        );
        listeners.push_monitor(move |_: &EntityTeleport, _: &Context<'_>| {
            heard.try_send(()).unwrap();
        });
        let (mut state, _rx) = two_with(listeners);
        let id = Id::new(SessionId::new(1));
        let feet = Vec3::new(3.5, -60.0, 2.5);
        Context { state: &mut state }.teleport_player(
            id,
            world::Id::DEFAULT,
            feet,
            TeleportCause::Action,
        );
        assert!(monitors.try_recv().is_err(), "no monitor");
        assert!(state.players[&SessionId::new(1)].marks.teleport.is_none());
    }
}
