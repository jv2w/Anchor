//! The queues between the session layer and the tick, and what travels on them: the input queue
//! with its per-session stamping senders, each session's outbound queue, and the queue of the
//! tick's controls to the networking; the verified player identities and skins, the measured
//! values, and the packet that turns a player away when the server is full.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::debug;

use crate::config::GameMode;
use crate::crypto;
use crate::entity::{MAX_SKIN_BYTES, MAX_SKIN_JSON_DEPTH};
use crate::protocol::consts;
use crate::protocol::packet::{Packet, PlayStatus};
use crate::protocol::{self, Uuid};
use crate::task::MAX_JOBS;

/// The same as [`SKIN_FIXED_BYTES`] for one animation: 2 image sizes (4 each), the data's length
/// prefix and 2 varints (at most 5 each) and the frame count (4).
const ANIMATION_FIXED_BYTES: usize = 27;
/// The ticks of [`Control`]s, at [`MAX_CONTROLS_PER_TICK`] each, that the queue to the networking
/// holds.
pub(super) const CONTROL_QUEUE_TICKS: usize = 4;
/// The sessions of the engine that queue inputs besides the connections: the console, the
/// networking's reports and the process measures.
const ENGINE_SESSIONS: usize = 3;
/// [`Control`]s one tick may ask for; a call past it changes nothing.
pub(crate) const MAX_CONTROLS_PER_TICK: usize = 256;
/// Inputs of one session the tick queue holds before the tick takes them: a session past its
/// share disconnects, and the queue keeps a share for every session, so one session filling the
/// queue never ends another. A client sends a `PlayerAuthInput` on each of its 20 ticks a second
/// and a few packets per action; the share holds more than a full batch
/// ([`MAX_PACKETS`](crate::protocol::batch::MAX_PACKETS)).
pub(crate) const MAX_SESSION_INPUTS: usize = 1024;
/// Messages a session's outbound queue holds. The tick drops a session whose queue is full instead
/// of waiting.
const OUTBOUND_CAPACITY: usize = 1024;
/// The same as [`SKIN_FIXED_BYTES`] for one persona piece: 2 length prefixes (at most 5 each), the
/// type (4), the pack UUID (16) and a bool.
const PIECE_FIXED_BYTES: usize = 31;
/// The most wire bytes of a skin's fields besides what its byte fields and texts hold: 11 length
/// prefixes of texts and byte fields and 3 of lists (at most 5 bytes each), 4 image sizes (4
/// each), the arm size (1), the colour (4), 5 bools and the trust text (`"false"`, 5).
const SKIN_FIXED_BYTES: usize = 101;
/// The same as [`SKIN_FIXED_BYTES`] for one tint colour: a length prefix (at most 5) and 4 colours
/// (4 each).
const TINT_FIXED_BYTES: usize = 21;

/// A session: one connection, numbered by the session layer in connection order and never
/// reused while the process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SessionId(u64);

impl SessionId {
    /// The console's lines (`console-input`): a session of its own, so its inputs are stamped
    /// and ordered like a connection's. Connections are numbered from 0 and would need 2^64 of
    /// them to reach it.
    pub(crate) const CONSOLE: Self = Self(u64::MAX);

    /// The tick loop's own reports ([`Measure::TickTime`]): a session of its own whose sequence
    /// number is the tick that reports.
    pub(crate) const ENGINE: Self = Self(u64::MAX - 3);

    /// The results of blocking jobs ([`InputKind::Job`]): a session of its own whose sequence
    /// number is the job's id, so results applied in one tick are ordered by job, after every
    /// connection's inputs and before the console's.
    pub(crate) const JOBS: Self = Self(u64::MAX - 1);

    /// The networking's reports ([`Measure::Connections`]): a session of its own, ordered after
    /// every connection's inputs.
    pub(crate) const NETWORK: Self = Self(u64::MAX - 2);

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
    /// player list's `BuildPlatform` (the official server sends it, P6).
    pub(crate) device_os: i32,
    /// The address the client connected from, which IP bans match; `None` when unknown
    /// (tests).
    pub(crate) address: Option<IpAddr>,
    /// What else the login and the connection tell about the client.
    pub(crate) client: Client,
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
            Some(IpAddr::V4(a)) => h.bytes(&a.octets()),
            Some(IpAddr::V6(a)) => h.bytes(&a.octets()),
            None => h.u64(0),
        }
        client.hash_into(h);
    }

    /// The bytes of its skin and the client's texts: what an input log entry of its join counts
    /// against the log's bound.
    pub(crate) fn size(&self) -> usize {
        self.skin.size().saturating_add(self.client.size())
    }
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
/// besides who it is. Every client data text is the client's own claim; the login checked
/// the locale (a language tag of at most 64 bytes), the game version (digits and dots), the
/// self-signed ID (empty or a UUID) and the server address (a host and port). The token's PlayFab
/// ID is bounded by the token's own cap ([`MAX_TOKEN_PAYLOAD_BYTES`](crate::login::MAX_TOKEN_PAYLOAD_BYTES)).
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
    /// The client's version, such as `1.26.60` (`GameVersion`).
    pub(crate) game_version: String,
    /// The address the client was told to join (`ServerAddress`).
    pub(crate) server_address: String,
    /// The port the client connected from.
    pub(crate) port: u16,
    /// When the session handed the player to the tick, in milliseconds since the Unix epoch by
    /// the wall clock the session read.
    pub(crate) join_time_ms: i64,
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

#[cfg(test)]
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

/// A player's skin as the client sent it at login, where it is checked
/// ([`Skin::is_acceptable`]). Every copy of the identity shares it. The default is a transparent
/// 64 × 32 skin on the standard model, for identities without a login (tests).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Skin(Arc<protocol::Skin>);

impl Skin {
    /// A skin that wraps a checked one.
    pub(crate) fn new(skin: Arc<protocol::Skin>) -> Self {
        Self(skin)
    }

    /// The skin as the protocol carries it.
    pub(crate) fn get(&self) -> &protocol::Skin {
        &self.0
    }

    /// Whether a login accepts the skin, as an in-game change and the API do: image sizes that
    /// match their data, finite animation frame counts not below 0, at most [`MAX_SKIN_BYTES`] in
    /// all, and a geometry and a resource patch that are JSON nested at most
    /// [`MAX_SKIN_JSON_DEPTH`] deep, as the protocol requires of both (an empty geometry is not
    /// JSON: a skin without a custom geometry sends `{}`).
    pub(crate) fn is_acceptable(&self) -> bool {
        // The size first: it bounds the JSON the last checks read.
        protocol::skin_validate(&self.0).is_none()
            && self
                .0
                .animations
                .iter()
                .all(|a| a.frame_count.is_finite() && a.frame_count >= 0.0)
            && self.size() <= MAX_SKIN_BYTES
            && crypto::is_json_within_depth(&self.0.skin_geometry, MAX_SKIN_JSON_DEPTH)
            && crypto::is_json_within_depth(&self.0.skin_resource_patch, MAX_SKIN_JSON_DEPTH)
    }

    /// The skin as the packets that carry it share it, without a copy.
    pub(crate) fn shared(&self) -> &Arc<protocol::Skin> {
        &self.0
    }

    /// The most bytes the skin takes on the wire: what it holds (images, geometry, patch,
    /// animation data and texts) and the fixed fields and length prefixes of the skin and of
    /// each animation, persona piece and tint colour. What [`MAX_SKIN_BYTES`] bounds, which
    /// bounds the number of animations, pieces and tints too; a player list entry counts it
    /// against `max-batch-bytes`, and an input log entry of its join against the log's bound.
    pub(crate) fn size(&self) -> usize {
        let s = &self.0;
        let animations = s
            .animations
            .iter()
            .map(|a| a.image_data.len().saturating_add(ANIMATION_FIXED_BYTES));
        let pieces = s.persona_pieces.iter().map(|p| {
            p.piece_id
                .len()
                .saturating_add(p.product_id.len())
                .saturating_add(PIECE_FIXED_BYTES)
        });
        // The wire name of a tint's piece type is never longer than the name it holds.
        let tints = s
            .piece_tint_colours
            .iter()
            .map(|t| t.piece_type.len().saturating_add(TINT_FIXED_BYTES));
        [
            SKIN_FIXED_BYTES,
            s.skin_data.len(),
            s.cape_data.len(),
            s.skin_geometry.len(),
            s.skin_resource_patch.len(),
            s.animation_data.len(),
            s.geometry_data_engine_version.len(),
            s.skin_id.len(),
            s.cape_id.len(),
            s.full_id.len(),
            s.profile_hash.len(),
        ]
        .into_iter()
        .chain(animations)
        .chain(pieces)
        .chain(tints)
        .fold(0_usize, usize::saturating_add)
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
}

#[cfg(test)]
impl Default for Skin {
    fn default() -> Self {
        Self::new(Arc::new(protocol::Skin {
            skin_id: "Standard_Custom".to_owned(),
            full_id: "Standard_Custom".to_owned(),
            skin_resource_patch: br#"{"geometry":{"default":"geometry.humanoid.custom"}}"#.to_vec(),
            skin_image_width: 64,
            skin_image_height: 32,
            skin_data: vec![0; 64 * 32 * 4],
            skin_geometry: b"{}".to_vec(),
            geometry_data_engine_version: b"0.0.0".to_vec(),
            arm_size: 1,
            ..protocol::Skin::default()
        }))
    }
}

// Equality is reflexive: a skin has no NaN (frame counts are checked finite at login).
impl Eq for Skin {}

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
    /// The session verified the login and asks whether the player may join: the tick answers
    /// [`Outbound::Admitted`] or closes the session before it offers the resource packs, so a
    /// refused player downloads none of them.
    Login {
        /// The verified player.
        identity: Identity,
        /// Where the tick answers (from [`outbound_queue`]); the session hands the same queue
        /// over again with its [`Join`](Self::Join).
        outbound: mpsc::Sender<Outbound>,
    },
    /// The session's login was admitted, the client finished the resource pack sequence and
    /// wants to play.
    Join {
        /// The verified player.
        identity: Identity,
        /// Where the tick sends this session's packets (from [`outbound_queue`]).
        outbound: mpsc::Sender<Outbound>,
    },
    /// An in-game packet (after `StartGame`) and the bytes the input log counts for it against
    /// its bound: the larger of its size on the wire and what decoding it allocated. The log does
    /// not keep the size, and no tick rule reads it.
    Packet { packet: Packet, size: usize },
    /// The connection closed, for the given reason.
    Leave(LeaveReason),
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

/// Why a connection ended, as far as the networking knows. A kick, a transfer and the like are
/// decided by the tick, not reported here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaveReason {
    /// The client closed the connection.
    Left,
    /// The connection closed without the client closing it: the transport, the session or the
    /// server ended it, or the transport cannot tell why.
    ConnectionClosed,
    /// The connection stayed silent until it timed out.
    TimedOut,
}

/// A value measured off the tick: what the wall clock or the network would tell,
/// read from the input log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Measure {
    /// How long the previous tick took, in microseconds (at most `u32::MAX`), on
    /// [`SessionId::ENGINE`].
    TickTime(u32),
    /// The session's latest round trip time, in milliseconds: the RakNet ping or the NetherNet
    /// ICE consent check.
    Ping(u32),
    /// The connections the networking holds, on [`SessionId::NETWORK`].
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

/// What the tick asks of the networking. The networking applies it when it reads it; nothing comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Control {
    /// The server list shows this name (a checked `motd`).
    Motd(String),
    /// The server list shows this player limit.
    MaxPlayers(u32),
    /// The server list shows this default game mode.
    GameMode(GameMode),
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
pub(crate) type JobOutcome = Result<Vec<u8>, crate::task::JobError>;

/// Batches encoded and compressed once at start and sent to every player as they are: batch bodies,
/// each a compression header and the compressed packets, without the encryption a RakNet session
/// adds.
pub(crate) type SharedBatches = Arc<[Vec<u8>]>;

/// A packet several sessions send (a skin of up to 4 MiB, a player list), held once and encoded
/// and compressed once: the first session that sends it makes its batch bodies, and every other
/// shares them as it shares [`SharedBatches`].
#[derive(Debug)]
pub(crate) struct SharedPacket {
    packet: Packet,
    /// The batch bodies, made by the first session that sends the packet. Every joined session
    /// encodes with the server's batch settings, so the bodies one makes are those every other
    /// would make. An async cell: a session that finds them being made waits without holding
    /// its thread.
    batches: OnceCell<SharedBatches>,
}

impl SharedPacket {
    /// A packet held for the sessions that send it.
    pub(crate) fn new(packet: Packet) -> Self {
        Self {
            packet,
            batches: OnceCell::new(),
        }
    }

    /// The packet.
    pub(crate) fn get(&self) -> &Packet {
        &self.packet
    }

    /// The packet's batch bodies, which `encode` makes on the first call; later calls
    /// share them, and a call while they are being made waits for them.
    pub(crate) async fn batches<E>(
        &self,
        encode: impl FnOnce(&Packet) -> Result<SharedBatches, E>,
    ) -> Result<SharedBatches, E> {
        let batches = self
            .batches
            .get_or_try_init(|| async { encode(&self.packet) })
            .await?;
        Ok(Arc::clone(batches))
    }

    /// The packet, to change it, forgetting any batch bodies made from it.
    pub(crate) fn get_mut(&mut self) -> &mut Packet {
        self.batches = OnceCell::new();
        &mut self.packet
    }
}

impl Clone for SharedPacket {
    /// Copies the packet without its batch bodies: a copy is made to be changed.
    fn clone(&self) -> Self {
        Self::new(self.packet.clone())
    }
}

/// One thing for a session to send.
#[derive(Debug)]
pub(crate) enum Message {
    /// A packet the session encodes into its own batches.
    Packet(Packet),
    /// A packet several sessions send, in batches of its own encoded once for all of them.
    SharedPacket(Arc<SharedPacket>),
    /// A packet of a spawn window's player list, which holds every placed player's skin and so
    /// outgrows any bound on a session's output: sent as a [`Message::SharedPacket`], but the
    /// session holds its batches as the shared copy without counting them. A session gets two
    /// lists at most, one with the start game and one in the spawn window.
    SpawnList(Arc<SharedPacket>),
    /// Batches shared by every session, sent after the packets before them.
    Shared(SharedBatches),
}

/// A message from the tick to one session.
#[derive(Debug)]
pub(crate) enum Outbound {
    /// The tick admitted the session's [`InputKind::Login`]: the session offers the resource
    /// packs.
    Admitted,
    /// Messages to send, in order.
    Send(Vec<Message>),
    /// The last packet to send (a `Disconnect`, a `Transfer` or a failed `PlayStatus`) before
    /// closing the connection. The tick has already forgotten the session.
    Close(Packet),
}

/// Why an input was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The session's share of the queue is full; the session should disconnect.
    Full,
    /// The tick has stopped.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("the session's share of the tick input queue is full"),
            Self::Closed => f.write_str("the tick has stopped"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// An input in the queue with the place it takes in its session's share, which the tick gives
/// back by taking it; a job's result takes none. A connection's leave also carries its connection
/// share: the queue is in order, so when the tick takes the leave it has taken every input of the
/// session, and a new connection's share never counts beside them.
#[derive(Debug)]
struct Queued {
    input: Input,
    _share: Option<OwnedSemaphorePermit>,
    _connection: Option<OwnedSemaphorePermit>,
}

/// The sending side of the tick input queue, shared by every session.
#[derive(Debug, Clone)]
pub(crate) struct InputSender {
    queue: mpsc::Sender<Queued>,
    /// One permit per connection session that may hold a share at once.
    connections: Arc<Semaphore>,
}

impl InputSender {
    /// Takes one of the queue's connection shares for connection session `id` and returns the
    /// session's sender, which holds the share until the tick takes its leave; `None` while every
    /// share is held.
    pub(crate) fn connection(&self, id: SessionId) -> Option<SessionInputs> {
        let connection = Arc::clone(&self.connections).try_acquire_owned().ok()?;
        Some(SessionInputs {
            connection: Some(connection),
            ..self.session(id)
        })
    }

    /// A sender that stamps the inputs of engine session `id` with its sequence numbers.
    pub(crate) fn session(&self, id: SessionId) -> SessionInputs {
        SessionInputs {
            session: id,
            next_seq: 0,
            queue: self.queue.clone(),
            share: Arc::new(Semaphore::new(MAX_SESSION_INPUTS)),
            connection: None,
        }
    }

    /// Queues the result of job `id`, waiting for room so that the job's callback always runs.
    /// Fails only if the tick has stopped.
    pub(crate) async fn job(&self, id: u64, outcome: JobOutcome) -> Result<()> {
        let input = Input {
            session: SessionId::JOBS,
            seq: id,
            kind: InputKind::Job(outcome),
        };
        let queued = Queued {
            input,
            _share: None,
            _connection: None,
        };
        self.queue.send(queued).await.map_err(|_| Error::Closed)
    }
}

/// The inputs of one session, stamped in order. Owned by that session's task.
#[derive(Debug)]
pub(crate) struct SessionInputs {
    session: SessionId,
    next_seq: u64,
    queue: mpsc::Sender<Queued>,
    /// [`MAX_SESSION_INPUTS`] permits, one taken by each input the tick has not taken yet.
    share: Arc<Semaphore>,
    /// The connection share of a connection session, which its leave carries to the tick.
    connection: Option<OwnedSemaphorePermit>,
}

impl SessionInputs {
    /// Whether the tick has stopped: it drops its end of the queue then, and only then.
    pub(crate) fn is_tick_stopped(&self) -> bool {
        self.queue.is_closed()
    }

    /// Queues `kind` without waiting. On [`Error::Full`] the session should disconnect; it may
    /// still call [`leave`](Self::leave).
    pub(crate) fn try_send(&mut self, kind: InputKind) -> Result<()> {
        let Ok(share) = Arc::clone(&self.share).try_acquire_owned() else {
            return Err(Error::Full);
        };
        let queued = Queued {
            input: Input {
                session: self.session,
                seq: self.next_seq,
                kind,
            },
            _share: Some(share),
            _connection: None,
        };
        match self.queue.try_send(queued) {
            Ok(()) => {
                // 2^64 inputs never arrive, so the order never wraps.
                self.next_seq = self.next_seq.saturating_add(1);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(Error::Full),
            Err(TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }

    /// Queues [`InputKind::Leave`] for `reason` with the connection share, waiting for room in the
    /// session's share so that the tick always learns the session ended. Returns at once if the
    /// tick has stopped.
    pub(crate) async fn leave(mut self, reason: LeaveReason) {
        #[expect(
            clippy::expect_used,
            reason = "invariant: nothing closes the share, so acquiring only waits for the tick to \
                      take inputs"
        )]
        let share = Arc::clone(&self.share)
            .acquire_owned()
            .await
            .expect("the share stays open");
        let queued = Queued {
            input: Input {
                session: self.session,
                seq: self.next_seq,
                kind: InputKind::Leave(reason),
            },
            _share: Some(share),
            _connection: self.connection.take(),
        };
        // A closed queue means the tick has stopped and forgets every session anyway.
        if self.queue.send(queued).await.is_err() {
            debug!(session = self.session.get(), "tick stopped before leave");
        }
    }
}

/// The tick's end of the input queue. Taking an input gives its place back to its session's
/// share.
#[derive(Debug)]
pub(crate) struct InputReceiver(mpsc::Receiver<Queued>);

impl InputReceiver {
    /// The inputs waiting.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Waits for the next input and takes it; `None` once every sender is gone.
    #[cfg(test)]
    pub(crate) async fn recv(&mut self) -> Option<Input> {
        self.0.recv().await.map(|queued| queued.input)
    }

    /// Takes the next input, without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Input, TryRecvError> {
        self.0.try_recv().map(|queued| queued.input)
    }
}

/// Makes the tick input queue of a server of `max_players`: the sending side for the session
/// layer and the tick's receiver.
///
/// The queue holds a [`MAX_SESSION_INPUTS`] share for every session that can hold one at once
/// and the results of every job, so no session or job ever finds it full. Connection sessions
/// hold a share from their login until the tick takes their leave ([`InputSender::connection`]):
/// `max_players` online and as many between their login and their join (the sessions waiting
/// to join are as many at most) or that the tick turned away and whose leave it has not taken.
pub(crate) fn input_queue(max_players: u32) -> (InputSender, InputReceiver) {
    let connections = usize::try_from(max_players)
        .unwrap_or(usize::MAX)
        .saturating_mul(2);
    let capacity = connections
        .saturating_add(ENGINE_SESSIONS)
        .saturating_mul(MAX_SESSION_INPUTS)
        .saturating_add(MAX_JOBS);
    let (tx, rx) = mpsc::channel(capacity);
    let sender = InputSender {
        queue: tx,
        connections: Arc::new(Semaphore::new(connections)),
    };
    (sender, InputReceiver(rx))
}

/// Makes a session's outbound queue; the sender goes to the tick in [`InputKind::Login`] and
/// [`InputKind::Join`].
pub(crate) fn outbound_queue() -> (mpsc::Sender<Outbound>, mpsc::Receiver<Outbound>) {
    mpsc::channel(OUTBOUND_CAPACITY)
}

/// Makes the queue of [`Control`]s from the tick to the networking: [`CONTROL_QUEUE_TICKS`] ticks
/// at the tick's bound.
pub(crate) fn control_queue() -> (mpsc::Sender<Control>, mpsc::Receiver<Control>) {
    mpsc::channel(CONTROL_QUEUE_TICKS * MAX_CONTROLS_PER_TICK)
}

/// Builds the packet that turns a player away because the server is full: `PlayStatus` with
/// `LoginFailedServerFull`, whose text the client shows itself. The session layer sends it when
/// too many sessions wait to join, the tick when `max-players` players are online or no entity
/// runtime ID is left.
pub(crate) fn server_full() -> Packet {
    Packet::PlayStatus(PlayStatus {
        status: i32::from(consts::PlayStatus::LoginFailedServerFull),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skins_need_shallow_json_geometry_and_patch() {
        let with = |geometry: &[u8], patch: &[u8]| {
            Skin::new(Arc::new(protocol::Skin {
                skin_geometry: geometry.to_vec(),
                skin_resource_patch: patch.to_vec(),
                ..Skin::default().get().clone()
            }))
            .is_acceptable()
        };
        let patch = Skin::default().get().skin_resource_patch.clone();
        let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n)).into_bytes();
        assert!(with(b"{}", &patch));
        assert!(with(b"null", &patch));
        assert!(with(&nested(MAX_SKIN_JSON_DEPTH), &patch));
        for geometry in [&b""[..], b"{", b"   ", &nested(MAX_SKIN_JSON_DEPTH + 1)] {
            assert!(!with(geometry, &patch), "{:?}", geometry.get(..8));
        }
        assert!(!with(b"{}", b""));
        assert!(!with(b"{}", &nested(MAX_SKIN_JSON_DEPTH + 1)));
    }

    #[test]
    fn inputs_are_stamped_in_order_per_session() {
        let (sender, mut rx) = input_queue(1);
        let mut a = sender.session(SessionId::new(7));
        let mut b = sender.session(SessionId::new(3));
        a.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        b.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        a.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|i| (i.session.get(), i.seq))
            .collect();
        assert_eq!(got, [(7, 0), (3, 0), (7, 1)]);
    }

    /// A session past its share is told so while another still has room, the tick taking an
    /// input gives its place back, and a stopped tick is reported.
    #[test]
    fn a_full_share_and_a_stopped_tick_are_reported() {
        let (sender, mut rx) = input_queue(1);
        let mut a = sender.connection(SessionId::new(1)).unwrap();
        let mut b = sender.connection(SessionId::new(2)).unwrap();
        assert!(
            sender.connection(SessionId::new(3)).is_none(),
            "two connections at most"
        );
        for _ in 0..MAX_SESSION_INPUTS {
            a.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        }
        assert_eq!(
            a.try_send(InputKind::Leave(LeaveReason::Left)),
            Err(Error::Full)
        );
        b.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        rx.try_recv().unwrap();
        a.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        drop(b);
        assert!(
            sender.connection(SessionId::new(3)).is_some(),
            "b's share is free again"
        );
        drop(rx);
        assert_eq!(
            a.try_send(InputKind::Leave(LeaveReason::Left)),
            Err(Error::Closed)
        );
    }

    /// A connection that left keeps its connection share until the tick takes its leave, so the
    /// inputs it queued before never count beside a new connection's share.
    #[test]
    fn a_connection_share_is_held_until_the_tick_takes_the_leave() {
        let (sender, mut rx) = input_queue(1);
        let _b = sender.connection(SessionId::new(1)).unwrap();
        let mut a = sender.connection(SessionId::new(2)).unwrap();
        for _ in 1..MAX_SESSION_INPUTS {
            a.try_send(InputKind::Leave(LeaveReason::Left)).unwrap();
        }
        let leaving = std::pin::pin!(a.leave(LeaveReason::Left));
        let polled = leaving.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        assert_eq!(polled, std::task::Poll::Ready(()));
        for _ in 1..MAX_SESSION_INPUTS {
            rx.try_recv().unwrap();
            assert!(
                sender.connection(SessionId::new(3)).is_none(),
                "a's inputs are still queued"
            );
        }
        rx.try_recv().unwrap();
        assert!(
            sender.connection(SessionId::new(3)).is_some(),
            "the tick took a's leave"
        );
    }
}
