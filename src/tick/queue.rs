//! The queues between sessions and the tick: the input queue, its per-session stamping sender,
//! and the outbound queue of each session.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::protocol::consts;
use crate::protocol::packet::{Packet, PlayStatus};
use crate::protocol::{self, Uuid};

/// Inputs the tick queue holds. A session that finds it full disconnects.
pub(crate) const INPUT_CAPACITY: usize = 65_536;
/// Messages a session's outbound queue holds. The tick drops a session whose queue is full instead
/// of waiting.
const OUTBOUND_CAPACITY: usize = 1_024;
/// [`Control`]s one tick may ask for; a call past it changes nothing.
pub(crate) const MAX_CONTROLS_PER_TICK: usize = 256;

/// A session: one connection, numbered by the session layer in connection order and never
/// reused while the process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SessionId(u64);

impl SessionId {
    /// The console's lines (`console-input`): a session of its own, so its inputs are stamped
    /// and ordered like a connection's. Connections are numbered from 0 and would need 2^64 of
    /// them to reach it.
    pub(crate) const CONSOLE: Self = Self(u64::MAX);

    /// The results of blocking jobs ([`InputKind::Job`]): a session of its own whose sequence
    /// number is the job's id, so results applied in one tick are ordered by job, after every
    /// connection's inputs and before the console's.
    pub(crate) const JOBS: Self = Self(u64::MAX - 1);

    /// The networking's reports ([`Measure::Connections`]): a session of its own, ordered after
    /// every connection's inputs.
    pub(crate) const NETWORK: Self = Self(u64::MAX - 2);

    /// The tick loop's own reports ([`Measure::TickTime`]): a session of its own whose sequence
    /// number is the tick that reports.
    pub(crate) const ENGINE: Self = Self(u64::MAX - 3);

    /// The process measures ([`Measure::Process`]): a session of its own, ordered after every
    /// connection's inputs.
    pub(crate) const PROCESS: Self = Self(u64::MAX - 4);

    /// The session numbered `id`.
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The number.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Who a session's player is, from the verified login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The display name (Xbox gamertag online).
    pub(crate) name: String,
    /// The Xbox user ID; `None` offline.
    pub(crate) xuid: Option<u64>,
    /// The identity UUID.
    pub(crate) uuid: Uuid,
    /// The skin the client sent at login, as the player list shows it to everyone
    /// (`PlayerListEntry::skin`; `AddPlayer` has no skin: a client needs the list entry first).
    pub(crate) skin: Skin,
    /// The client's device (`DeviceOS` of the login's client data, 1 to 15; -1 unknown), the
    /// player list's `BuildPlatform` (BDS sends it, P6).
    pub(crate) device_os: i32,
    /// The address the client connected from, which IP bans match; `None` when unknown
    /// (tests).
    pub(crate) address: Option<IpAddr>,
    /// What else the login and the connection tell about the client.
    pub(crate) client: Client,
}

super::checkpoint::fields!(Identity {
    name,
    xuid,
    uuid,
    skin,
    device_os,
    address,
    client,
});

/// What the login's client data and identity token and the connection tell about the client
/// besides who it is (PMMP `PlayerInfo::getLocale`, `PlayerInfo::getExtraData`,
/// `NetworkSession::getPort`). Every client data text is the client's own claim; the login checked
/// the locale (a language tag of at most 64 bytes), the game version (digits and dots), the
/// self-signed ID (empty or a UUID) and the server address (a host and port). The token's PlayFab
/// ID is bounded by the token's own cap ([`MAX_TOKEN_PAYLOAD`](crate::login::MAX_TOKEN_PAYLOAD)).
/// The default, empty texts and zeros, is for a player made without a connection (tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client {
    /// The language code, such as `en_US` (`LanguageCode`).
    pub(crate) locale: String,
    /// The input mode the client started in (`CurrentInputMode`).
    pub(crate) input_mode: crate::protocol::consts::InputMode,
    /// The device model (`DeviceModel`).
    pub(crate) device_model: String,
    /// The device ID (`DeviceId`).
    pub(crate) device_id: String,
    /// The UUID the client made for itself (`SelfSignedId`); empty when not sent.
    pub(crate) self_signed_id: String,
    /// The client's random number (`ClientRandomId`); 0 when not sent.
    pub(crate) random_id: i64,
    /// The PlayFab master player ID of the identity token (`mid`), verified with Xbox sign-in
    /// and the client's claim without; empty when not sent.
    pub(crate) playfab_id: String,
    /// The client's version, such as `1.26.50` (`GameVersion`).
    pub(crate) game_version: String,
    /// The address the client was told to join (`ServerAddress`).
    pub(crate) server_address: String,
    /// The port the client connected from.
    pub(crate) port: u16,
    /// When the session handed the player to the tick, in milliseconds since the Unix epoch by
    /// the wall clock the session read.
    pub(crate) join_time_ms: i64,
}

super::checkpoint::fields!(Client {
    locale,
    input_mode,
    device_model,
    device_id,
    self_signed_id,
    random_id,
    playfab_id,
    game_version,
    server_address,
    port,
    join_time_ms,
});

impl Default for Client {
    fn default() -> Self {
        Self {
            locale: String::new(),
            input_mode: crate::protocol::consts::InputMode::Undefined,
            device_model: String::new(),
            device_id: String::new(),
            self_signed_id: String::new(),
            random_id: 0,
            playfab_id: String::new(),
            game_version: String::new(),
            server_address: String::new(),
            port: 0,
            join_time_ms: 0,
        }
    }
}

impl Client {
    /// The bytes of its texts, which an input log entry of its join counts against the log's
    /// bound.
    pub(crate) fn size(&self) -> usize {
        [
            &self.locale,
            &self.device_model,
            &self.device_id,
            &self.self_signed_id,
            &self.playfab_id,
            &self.game_version,
            &self.server_address,
        ]
        .iter()
        .fold(0_usize, |sum, s| sum.saturating_add(s.len()))
    }

    /// Hashes every field (state hash).
    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            locale,
            input_mode,
            device_model,
            device_id,
            self_signed_id,
            random_id,
            playfab_id,
            game_version,
            server_address,
            port,
            join_time_ms,
        } = self;
        for text in [
            locale,
            device_model,
            device_id,
            self_signed_id,
            playfab_id,
            game_version,
            server_address,
        ] {
            h.bytes(text.as_bytes());
        }
        h.u64(u64::from(u32::from(*input_mode)));
        h.u64(random_id.cast_unsigned());
        h.u64(u64::from(*port));
        h.u64(join_time_ms.cast_unsigned());
    }
}

impl Identity {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            name,
            xuid,
            uuid,
            skin,
            device_os,
            address,
            client,
        } = self;
        h.bytes(name.as_bytes());
        h.u64(xuid.map_or(0, |x| x.wrapping_add(1)));
        h.bytes(uuid.as_bytes());
        skin.hash_into(h);
        h.u64(u64::from(device_os.cast_unsigned()));
        match address {
            None => h.u64(0),
            Some(IpAddr::V4(a)) => h.bytes(&a.octets()),
            Some(IpAddr::V6(a)) => h.bytes(&a.octets()),
        }
        client.hash_into(h);
    }

    /// The bytes of its skin and the client's texts: what an input log entry of its join counts
    /// against the log's bound.
    pub(crate) fn size(&self) -> usize {
        self.skin.size().saturating_add(self.client.size())
    }
}

/// A player's skin as the client sent it at login (bounded and checked there: image sizes match
/// their dimensions, at most 4 MiB in all, animation frame counts finite).
/// Every copy of the identity shares it. The default is a transparent 64 × 32 skin on the
/// standard model, for identities without a login (tests, replays of old logs).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Skin(Arc<protocol::Skin>);

// Equality is reflexive: a skin has no NaN (frame counts are checked finite at login).
impl Eq for Skin {}

impl Skin {
    /// Wraps a checked skin.
    pub(crate) fn new(skin: protocol::Skin) -> Self {
        Self(Arc::new(skin))
    }

    /// The skin as the protocol carries it.
    pub(crate) fn get(&self) -> &protocol::Skin {
        &self.0
    }

    /// Hashes the skin by its protocol bytes (state hash).
    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv) {
        let mut w = protocol::Writer::new(Vec::new(), 0);
        match protocol::Codec::encode(self.get(), &mut w) {
            Ok(()) => h.bytes(w.as_bytes()),
            Err(_) => h.u64(u64::MAX),
        }
    }

    /// The bytes the skin holds (images, geometry, patch, animation data): what an input log
    /// entry of its join counts against the log's bound.
    pub(crate) fn size(&self) -> usize {
        let s = &self.0;
        let animations: usize = s.animations.iter().map(|a| a.image_data.len()).sum();
        [
            s.skin_data.len(),
            s.cape_data.len(),
            s.skin_geometry.len(),
            s.skin_resource_patch.len(),
            s.animation_data.len(),
            s.geometry_data_engine_version.len(),
            animations,
        ]
        .iter()
        .fold(0_usize, |sum, n| sum.saturating_add(*n))
    }
}

impl Default for Skin {
    fn default() -> Self {
        Self::new(protocol::Skin {
            skin_id: "Standard_Custom".to_owned(),
            full_id: "Standard_Custom".to_owned(),
            skin_resource_patch: br#"{"geometry":{"default":"geometry.humanoid.custom"}}"#.to_vec(),
            skin_image_width: 64,
            skin_image_height: 32,
            skin_data: vec![0; 64 * 32 * 4],
            geometry_data_engine_version: b"0.0.0".to_vec(),
            arm_size: 1,
            ..protocol::Skin::default()
        })
    }
}

/// One input for the tick, ordered by (`session`, `seq`) before it is applied.
#[derive(Debug, Clone)]
pub(crate) struct Input {
    /// The session it came from.
    pub(crate) session: SessionId,
    /// Its position among the session's inputs, from 0.
    pub(crate) seq: u64,
    /// What happened.
    pub(crate) kind: InputKind,
}

/// What a session reports to the tick.
#[derive(Debug, Clone)]
pub(crate) enum InputKind {
    /// The session finished the resource pack sequence and wants to play.
    Join {
        /// The verified player.
        identity: Identity,
        /// Where the tick sends this session's packets (from [`outbound_queue`]).
        outbound: mpsc::Sender<Outbound>,
    },
    /// An in-game packet (after `StartGame`) and its size on the wire, which the input log
    /// counts against its bound.
    Packet { packet: Packet, size: usize },
    /// The connection closed.
    Leave,
    /// A line typed on the console ([`SessionId::CONSOLE`]), at most
    /// [`MAX_LINE_BYTES`](crate::command::parse::MAX_LINE_BYTES) bytes, without its line end.
    Console(String),
    /// The result of the blocking job whose id is the input's sequence number, on
    /// [`SessionId::JOBS`].
    Job(JobOutcome),
    /// A value measured off the tick by a clock or the network, which the tick reads only as
    /// this logged input, so that a replay reads the same.
    Measure(Measure),
}

/// A value measured off the tick: what PMMP reads from the wall clock or the network when asked,
/// Anchor reads from the input log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Measure {
    /// How long the previous tick took, in microseconds (at most `u32::MAX`), on
    /// [`SessionId::ENGINE`].
    TickTime(u32),
    /// The session's latest round trip time, in milliseconds: the RakNet ping or the NetherNet
    /// ICE consent check.
    Ping(u32),
    /// The connections the networking holds, on [`SessionId::NETWORK`] (PMMP
    /// `Network::getConnectionCount`, `Network::getValidConnectionCount`).
    Connections {
        /// Every connection with a session, logged in or not.
        all: u32,
        /// The connections whose login was verified.
        logged_in: u32,
    },
    /// The process, every second, on [`SessionId::PROCESS`] (`/status`).
    Process {
        /// The resident memory in bytes (the working set on Windows); 0 if the OS did not tell.
        memory: u64,
        /// Wall-clock milliseconds since [`Server::run`](crate::Server::run) started.
        uptime_ms: u64,
    },
}

/// What the tick asks of the networking (PMMP `Network::setName`, `Network::blockAddress`,
/// `Network::unblockAddress`). The networking applies it when it reads it; nothing comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Control {
    /// The server list shows this name (a checked `motd`).
    Motd(String),
    /// Connections from `ip` are refused for `duration` from when the networking reads it;
    /// one that would end past what the clock holds lasts until it is unblocked.
    Block {
        ip: IpAddr,
        duration: std::time::Duration,
    },
    /// Connections from `ip` are accepted again.
    Unblock(IpAddr),
}

/// A blocking job's result as it travels to the tick and into the input log: the JSON of the
/// value its work returned, at most [`MAX_RESULT_BYTES`](crate::task::MAX_RESULT_BYTES), or why
/// there is none.
pub(crate) type JobOutcome = std::result::Result<Vec<u8>, crate::task::Error>;

/// Batches encoded and compressed once at start and sent to every player as they are: batch bodies,
/// each a compression header and the compressed packets, without the encryption a RakNet session
/// adds.
pub(crate) type SharedBatches = Arc<[Vec<u8>]>;

/// One thing for a session to send.
#[derive(Debug)]
pub(crate) enum Message {
    /// A packet the session encodes into its own batches.
    Packet(Packet),
    /// A packet several sessions send (a skin of up to 4 MiB), held once: each session encodes
    /// it into a batch of its own.
    SharedPacket(Arc<Packet>),
    /// Batches shared by every session, sent after the packets before them.
    Shared(SharedBatches),
}

/// A message from the tick to one session.
#[derive(Debug)]
pub(crate) enum Outbound {
    /// Messages to send, in order.
    Send(Vec<Message>),
    /// The last packet to send (a `Disconnect` or a failed `PlayStatus`) before closing the
    /// connection. The tick has already forgotten the session.
    Close(Packet),
}

/// The packet that turns a player away because the server is full: `PlayStatus` with
/// `LoginFailedServerFull`, whose text the client shows itself. The session layer sends it when
/// too many sessions wait to join, the tick when `max-players` players are online or no entity
/// runtime ID is left.
pub(crate) fn server_full() -> Packet {
    Packet::PlayStatus(PlayStatus {
        status: i32::from(consts::PlayStatus::LoginFailedServerFull),
    })
}

/// Why an input was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The queue is full; the session should disconnect.
    Full,
    /// The tick has stopped.
    Closed,
}

/// A result with the queue [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "the tick input queue is full",
            Self::Closed => "the tick has stopped",
        })
    }
}

impl std::error::Error for Error {}

/// The tick input queue: the sending side for the session layer and the tick's receiver.
pub(crate) fn input_queue() -> (InputSender, mpsc::Receiver<Input>) {
    let (tx, rx) = mpsc::channel(INPUT_CAPACITY);
    (InputSender(tx), rx)
}

/// A session's outbound queue; the sender goes to the tick in [`InputKind::Join`].
pub(crate) fn outbound_queue() -> (mpsc::Sender<Outbound>, mpsc::Receiver<Outbound>) {
    mpsc::channel(OUTBOUND_CAPACITY)
}

/// The queue of [`Control`]s from the tick to the networking: four ticks at the tick's bound.
pub(crate) fn control_queue() -> (mpsc::Sender<Control>, mpsc::Receiver<Control>) {
    mpsc::channel(4 * MAX_CONTROLS_PER_TICK)
}

/// The sending side of the tick input queue, shared by every session.
#[derive(Debug, Clone)]
pub(crate) struct InputSender(mpsc::Sender<Input>);

impl InputSender {
    /// Queues the result of job `id`, waiting for room so that the job's callback always runs.
    /// Fails only if the tick has stopped.
    pub(crate) async fn job(&self, id: u64, outcome: JobOutcome) -> Result<()> {
        let input = Input {
            session: SessionId::JOBS,
            seq: id,
            kind: InputKind::Job(outcome),
        };
        self.0.send(input).await.map_err(|_| Error::Closed)
    }

    /// A sender that stamps the inputs of session `id` with its sequence numbers.
    pub(crate) fn session(&self, id: SessionId) -> SessionInputs {
        SessionInputs {
            session: id,
            next_seq: 0,
            queue: self.0.clone(),
        }
    }
}

/// The inputs of one session, stamped in order. Owned by that session's task.
#[derive(Debug)]
pub(crate) struct SessionInputs {
    session: SessionId,
    next_seq: u64,
    queue: mpsc::Sender<Input>,
}

impl SessionInputs {
    /// Queues `kind` without waiting. On [`Error::Full`] the session should disconnect; it may
    /// still call [`leave`](Self::leave).
    pub(crate) fn try_send(&mut self, kind: InputKind) -> Result<()> {
        let input = Input {
            session: self.session,
            seq: self.next_seq,
            kind,
        };
        match self.queue.try_send(input) {
            Ok(()) => {
                // 2^64 inputs never arrive, so the order never wraps.
                self.next_seq = self.next_seq.saturating_add(1);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(Error::Full),
            Err(TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }

    /// Queues [`InputKind::Leave`], waiting for room so that the tick always learns the session
    /// ended. Returns at once if the tick has stopped.
    pub(crate) async fn leave(self) {
        let input = Input {
            session: self.session,
            seq: self.next_seq,
            kind: InputKind::Leave,
        };
        // A closed queue means the tick has stopped and forgets every session anyway.
        if self.queue.send(input).await.is_err() {
            tracing::debug!(session = self.session.get(), "tick stopped before leave");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_stamped_in_order_per_session() {
        let (sender, mut rx) = input_queue();
        let mut a = sender.session(SessionId::new(7));
        let mut b = sender.session(SessionId::new(3));
        a.try_send(InputKind::Leave).unwrap();
        b.try_send(InputKind::Leave).unwrap();
        a.try_send(InputKind::Leave).unwrap();
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|i| (i.session.get(), i.seq))
            .collect();
        assert_eq!(got, [(7, 0), (3, 0), (7, 1)]);
    }

    #[test]
    fn a_full_queue_and_a_stopped_tick_are_reported() {
        let (tx, rx) = mpsc::channel(1);
        let mut s = InputSender(tx).session(SessionId::new(1));
        s.try_send(InputKind::Leave).unwrap();
        assert_eq!(s.try_send(InputKind::Leave), Err(Error::Full));
        drop(rx);
        assert_eq!(s.try_send(InputKind::Leave), Err(Error::Closed));
    }
}
