//! One connection's session: the login sequence (network settings, login, handshake, resource
//! packs), then the relay between the connection and the tick.
//!
//! Inbound, a message is decrypted (RakNet after the handshake), decompressed and split into
//! packets. Before the player joins, packets outside the login sequence wait in a short queue
//! and reach the tick after the join, in order (gophertunnel defers them the same way, without
//! a bound). Outbound, the tick's packets are batched, compressed and encrypted here, off the
//! tick, and handed to the connection as it delivers them ([`SEND_LIMIT`]). NetherNet runs the
//! same handshake but never encrypts (DTLS protects it, as on BDS), and its Login must carry the
//! key its offer proved.

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tracing::{debug, warn};

use super::packs::{self, CHUNK_INTERVAL, MAX_UNDELIVERED, Reply, Transfer};
use super::transport::{self, Transport};
use super::{Event, Shared};
use crate::auth::Keys;
use crate::bucket::{Bucket, Rate};
use crate::clock;
use crate::crypto::encryption::{CHECKSUM, Decryptor, Encryptor, Handshake};
use crate::login::{self, Login, Mode};
use crate::protocol::InventoryTransactionData;
use crate::protocol::batch::{self, Header};
use crate::protocol::consts;
use crate::protocol::consts::InputMode;
use crate::protocol::packet::{
    self, Disconnect, NetworkSettings, PROTOCOL_VERSION, Packet, PlayStatus,
    RequestNetworkSettings, ResourcePackClientResponse, ServerToClientHandshake,
};
use crate::tick::queue::{
    self, Client, Identity, InputKind, Measure, Message, Outbound, SessionId, SessionInputs,
    SharedBatches,
};

/// From connection to verified login.
const LOGIN_DEADLINE: Duration = Duration::from_secs(10);
/// From connection to `SetLocalPlayerAsInitialised`, plus the allowance of a resource pack
/// download and of what the tick sends up to the spawn status ([`packs`]), the latter at most
/// [`Shared::max_send_extension`].
const SPAWN_DEADLINE: Duration = Duration::from_secs(60);
/// How often a joined session reports its ping to the tick when it changed: each
/// report is a logged input.
const PING_REPORT: Duration = Duration::from_secs(5);
/// The largest message before `NetworkSettings`.
const FIRST_MESSAGE_BYTES: usize = 1024;
/// Packets outside the login sequence held until the join. The official client's order is not
/// observable; 32 is far above the few packets (cache status, violation warnings) clients send.
const MAX_DEFERRED: usize = 32;
/// Reassembled plus decompressed bytes before login verification, in batch limits.
const PRE_AUTH_BATCHES: usize = 8;
/// Capacity the reused payload buffers keep between messages: an outbound batch cut (256 KiB)
/// plus a packet under it. A larger message (a Login or a skin of up to 4 MiB) would otherwise
/// pin its size until the session ends.
const KEPT_BUFFER_BYTES: usize = 512 * 1024;
/// The input budget refills this fraction of `max-batch-bytes` per second.
const BUDGET_REFILL_DIVISOR: usize = 4;
/// The input packet budget: at the default `max-players` the rates of all sessions fit in what
/// the tick applies (`network.md`). The burst is the session's share of the tick queue.
const PACKET_RATE: Rate = Rate {
    capacity: queue::SESSION_INPUTS,
    per_second: 512,
};
/// A copy of the held click ([`Session::held_click`]) within this long of the last one forwarded
/// is dropped: half a tick, so one still reaches every tick at any frame rate above 20.
const HELD_CLICK_REPEAT: Duration = Duration::from_millis(25);
/// Undelivered bytes ([`Transport::undelivered`]) at most before the session hands the
/// connection the next batch of the tick's output. It is above the most either transport keeps
/// in flight (RakNet 2048 datagrams of 1492 bytes and their frame cost, about 3.1 MiB; NetherNet
/// a 1 MiB SCTP buffer), so a healthy link always has bytes waiting to go, and low enough that
/// one more batch of the default 16 MiB `max-batch-bytes` (at most 18.7 MiB compressed, plus
/// about 4.7 MiB of RakNet frame cost at the smallest MTU) stays under the transports' 32 MiB
/// caps.
const SEND_LIMIT: usize = 4 * 1024 * 1024;
/// How long after the last handover the session still counts the bodies it handed over on top
/// of the connection's undelivered count: both transports take the session's messages as they
/// come and report the count every 10 ms update, so by then the count holds them.
const REPORT_WAIT: Duration = Duration::from_millis(30);
/// Bytes of the tick's output at most waiting for delivery, all of it but the spawn windows'
/// player lists ([`Pending::SpawnList`]): a link that falls this far behind ends, as the
/// transports end one with 32 MiB undelivered.
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
/// The shield's item network ID, which the item registry Anchor sends names.
const SHIELD_ID: i32 = crate::item::SHIELD_RUNTIME_ID;
/// What a player who refuses required packs sees.
const PACKS_REFUSED_MESSAGE: &str = "This server needs its resource packs: accept them to join";

/// Runs one connection until it closes, then tells the tick and the accept task.
pub(super) async fn run(id: SessionId, conn: Transport, shared: Arc<Shared>) {
    let mut session = Session::new(id, conn, shared);
    let end = session.serve().await;
    session.finish(end).await;
}

/// Where the connection is in the login sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for `RequestNetworkSettings`, the one uncompressed message.
    NetworkSettings,
    /// Waiting for `Login`, the first packet of the first compressed batch.
    Login,
    /// Waiting for `ClientToServerHandshake`, on RakNet the first encrypted packet.
    Handshake,
    /// Waiting for the resource pack responses.
    Packs,
    /// Joined: packets go to the tick.
    Joined,
}

struct Session {
    id: SessionId,
    conn: Transport,
    shared: Arc<Shared>,
    stage: Stage,
    started: Instant,
    /// When the spawn deadline passes: [`SPAWN_DEADLINE`] after the start, later by the
    /// allowance of a pack download and of each batch the tick sends up to the one with the
    /// spawn status, until `send_extension_left` is spent. After that batch the client must
    /// spawn in the time left, however much more it is sent.
    spawn_at: Instant,
    /// What the tick's batches may still add to `spawn_at`, from
    /// [`Shared::max_send_extension`]: a session the tick never sends the spawn status (its
    /// client never asks for a chunk radius, or a send hook drops the status) still times out.
    send_extension_left: Duration,
    /// The resource pack sequence.
    transfer: Transfer,
    encoder: Option<batch::Encoder>,
    decoder: Option<batch::Decoder>,
    encryptor: Option<Encryptor>,
    decryptor: Option<Decryptor>,
    /// Decompressed inbound payload, reused.
    payload: Vec<u8>,
    /// Outbound payload, reused.
    out_payload: Vec<u8>,
    /// Batch bodies of the first waiting output, in order and not encrypted yet, reused.
    out_bodies: VecDeque<Vec<u8>>,
    /// The tick's output after `out_bodies`, waiting for delivery ([`SEND_LIMIT`]), in order.
    pending: VecDeque<Pending>,
    /// Bytes of the [`Pending::Body`]s and [`Pending::Batches`] in `pending`.
    pending_bytes: usize,
    /// The batch with the spawn status went to the connection: later batches no longer move
    /// the spawn deadline.
    spawn_status_sent: bool,
    /// Bytes of the tick's output handed to the connection within [`REPORT_WAIT`] of the last
    /// handover, which its undelivered count may not hold yet.
    unreported_bytes: usize,
    /// Decompressed input bytes, on the time since `started`.
    budget: Bucket,
    budget_rate: Rate,
    /// Input packets, at [`PACKET_RATE`] on the time since `started`.
    packets: Bucket,
    /// The last `InventoryTransaction` forwarded, while it is a right click held on a block
    /// (`ClickBlock`, trigger `SimulationTick`), and when it went. The client sends that click
    /// again every frame while the button is down, so its byte-for-byte copies are dropped
    /// before the packet budget for [`HELD_CLICK_REPEAT`]: the tick ignores an identical click
    /// within two ticks of the first, so it would have ignored most of them (where it would not,
    /// `.claude/docs/network.md`). Any other use forwarded forgets it.
    held_click: Option<(Header, Vec<u8>, Duration)>,
    pre_auth_bytes: usize,
    identity: Option<Identity>,
    deferred: VecDeque<(Packet, usize)>,
    inputs: Option<SessionInputs>,
    outbound: Option<mpsc::Receiver<Outbound>>,
    /// The place among the sessions waiting to join, held from login to the join.
    waiting: Option<OwnedSemaphorePermit>,
    spawned: bool,
    logged_in: bool,
    /// The ping last reported to the tick, in milliseconds.
    reported_ping: Option<u32>,
}

/// Tick output waiting for delivery.
enum Pending {
    /// A batch body of the session's own packets, compressed and not encrypted yet.
    Body(Vec<u8>),
    /// A packet of a spawn window's player list ([`Message::SpawnList`]), encoded into a batch
    /// of its own as it goes out: a list of every skin outgrows any bound on a copy per session,
    /// so [`MAX_PENDING_BYTES`] does not count it, and a session gets two lists at most.
    SpawnList(Arc<Packet>),
    /// Batches the tick shares among sessions, copied as they go out and counted by their bytes
    /// meanwhile.
    Batches(SharedBatches),
    /// The end of the batches with the spawn status.
    SpawnStatus,
}

/// Why a session ended.
#[derive(Debug)]
enum End {
    /// The peer or the transport closed the connection.
    Closed,
    /// The tick closed the session (kick, full server, shutdown).
    Kicked,
    /// The tick stopped.
    TickStopped,
    /// The server is stopping and the player had not joined.
    Shutdown,
    /// A deadline passed.
    Timeout(&'static str),
    /// The peer broke the protocol or a limit, or sending failed.
    Failed(Error),
}

/// A session failure; the connection closes.
#[derive(Debug)]
enum Error {
    Order(&'static str),
    Limit(&'static str),
    Protocol(i32),
    Batch(batch::Error),
    Packet(crate::protocol::Error),
    Login(login::Error),
    /// The Login key is not the key the NetherNet offer proved.
    Binding,
    Crypto(crate::crypto::Error),
    Transport(transport::Error),
    Queue(queue::Error),
    Packs(packs::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Order(what) => write!(f, "packet out of order: {what}"),
            Self::Limit(what) => write!(f, "limit exceeded: {what}"),
            Self::Protocol(version) => write!(f, "unsupported protocol version {version}"),
            Self::Batch(_) => f.write_str("malformed batch"),
            Self::Packet(_) => f.write_str("malformed packet"),
            Self::Login(_) => f.write_str("login rejected"),
            Self::Binding => f.write_str("login key differs from the nethernet offer key"),
            Self::Crypto(_) => f.write_str("encryption failed"),
            Self::Transport(_) => f.write_str("connection failed"),
            Self::Queue(_) => f.write_str("tick queue unavailable"),
            Self::Packs(_) => f.write_str("resource pack sequence failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Batch(e) => Some(e),
            Self::Packet(e) => Some(e),
            Self::Login(e) => Some(e),
            Self::Crypto(e) => Some(e),
            Self::Transport(e) => Some(e),
            Self::Queue(e) => Some(e),
            Self::Packs(e) => Some(e),
            Self::Order(_) | Self::Limit(_) | Self::Protocol(_) | Self::Binding => None,
        }
    }
}

impl Session {
    fn new(id: SessionId, conn: Transport, shared: Arc<Shared>) -> Self {
        let max = shared.batch.max_batch_bytes();
        let started = clock::now();
        let budget_rate = Rate {
            capacity: max,
            per_second: max / BUDGET_REFILL_DIVISOR,
        };
        Self {
            id,
            conn,
            stage: Stage::NetworkSettings,
            started,
            spawn_at: started + SPAWN_DEADLINE,
            send_extension_left: shared.max_send_extension,
            transfer: Transfer::default(),
            encoder: None,
            decoder: None,
            encryptor: None,
            decryptor: None,
            payload: Vec::new(),
            out_payload: Vec::new(),
            out_bodies: VecDeque::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            spawn_status_sent: false,
            unreported_bytes: 0,
            budget: Bucket::full(budget_rate, Duration::ZERO),
            budget_rate,
            packets: Bucket::full(PACKET_RATE, Duration::ZERO),
            held_click: None,
            pre_auth_bytes: 0,
            identity: None,
            deferred: VecDeque::new(),
            inputs: None,
            outbound: None,
            waiting: None,
            spawned: false,
            logged_in: false,
            reported_ping: None,
            shared,
        }
    }

    async fn serve(&mut self) -> End {
        let login_deadline = deadline(self.started + LOGIN_DEADLINE);
        let mut spawn_at = self.spawn_at;
        let spawn_deadline = deadline(spawn_at);
        // Already passed: the first pack chunk goes out as soon as it is asked for.
        let pace = deadline(self.started);
        let report_wait = tokio::time::sleep(Duration::ZERO);
        let mut undelivered = self.conn.undelivered();
        let mut ping_report = tokio::time::interval(PING_REPORT);
        ping_report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(login_deadline, spawn_deadline, pace, report_wait);
        let mut shutdown = self.shared.shutdown.clone();
        if *shutdown.borrow_and_update() {
            return End::Shutdown;
        }
        loop {
            let chunks_wait = self.stage == Stage::Packs && self.transfer.has_chunks();
            let undelivered_bytes = *undelivered.borrow();
            let delivered = undelivered_bytes <= MAX_UNDELIVERED;
            let output_waits = !self.out_bodies.is_empty() || !self.pending.is_empty();
            let output_fits = undelivered_bytes.saturating_add(self.unreported_bytes) <= SEND_LIMIT;
            tokio::select! {
                message = self.conn.recv() => {
                    let Some(message) = message else { return End::Closed };
                    if let Err(e) = self.on_message(message).await {
                        return End::Failed(e);
                    }
                }
                // A chunk goes out once the timer passed and the connection has at most about a
                // chunk undelivered. The count is read again: the last chunk may have raised it
                // since the loop read it, and then the passed timer waits for the branch below.
                () = &mut pace, if chunks_wait && delivered => {
                    if *undelivered.borrow() <= MAX_UNDELIVERED {
                        if let Err(e) = self.send_chunk().await {
                            return End::Failed(e);
                        }
                        pace.as_mut().reset(tokio::time::Instant::from_std(clock::now() + CHUNK_INTERVAL));
                    }
                }
                // The next chunk or batch of output waits until the connection has delivered
                // enough; the loop looks again at each new count.
                changed = undelivered.changed(),
                    if (chunks_wait && !delivered) || (output_waits && !output_fits) =>
                {
                    if changed.is_err() {
                        return End::Closed;
                    }
                }
                () = std::future::ready(()), if output_waits && output_fits => {
                    if let Err(e) = self.send_waiting().await {
                        return End::Failed(e);
                    }
                    report_wait.set(tokio::time::sleep(REPORT_WAIT));
                }
                () = &mut report_wait, if self.unreported_bytes > 0 => self.unreported_bytes = 0,
                // Taken while output waits too: a slow link ends on the bytes waiting
                // (`MAX_PENDING_BYTES`) or the spawn deadline, and the tick's queue fills only
                // when this task falls behind.
                outbound = next_outbound(self.outbound.as_mut()) => match outbound {
                    Some(Outbound::Send(messages)) => {
                        if let Err(e) = self.queue_messages(&messages) {
                            return End::Failed(e);
                        }
                    }
                    Some(Outbound::Close(packet)) => {
                        if let Err(e) = self.send(&[packet]).await {
                            return End::Failed(e);
                        }
                        return End::Kicked;
                    }
                    None => return End::TickStopped,
                },
                () = &mut login_deadline, if self.identity.is_none() && !self.logged_in => {
                    return End::Timeout("login");
                }
                () = &mut spawn_deadline, if !self.spawned => return End::Timeout("spawn"),
                _ = ping_report.tick(), if self.inputs.is_some() => self.report_ping(),
                // Joined sessions end when the stopping tick drops their outbound queue.
                _ = shutdown.changed(), if self.inputs.is_none() => return End::Shutdown,
            }
            // A pack response or a batch sent up to the spawn status moved the deadline.
            if self.spawn_at != spawn_at {
                spawn_at = self.spawn_at;
                spawn_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(spawn_at));
            }
        }
    }

    async fn finish(mut self, end: End) {
        match &end {
            End::Failed(e) => {
                debug!(session = self.id.get(), error = %e, cause = ?std::error::Error::source(e), "session failed");
            }
            End::Timeout(stage) => debug!(session = self.id.get(), stage, "session timed out"),
            _ => debug!(session = self.id.get(), ?end, "session ended"),
        }
        if let Some(identity) = &self.identity
            && self.inputs.is_some()
        {
            debug!(session = self.id.get(), name = %identity.name, "session of a player ended");
        }
        if let Some(inputs) = self.inputs.take() {
            inputs.leave().await;
        }
        // The tick tells the players it holds; these the server stopped before the tick did.
        if matches!(end, End::Shutdown | End::TickStopped) && self.encoder.is_some() {
            let disconnect = Packet::Disconnect(Disconnect {
                reason: i32::from(consts::DisconnectReason::Shutdown),
                message: self.shared.shutdown_message.clone(),
                ..Disconnect::default()
            });
            if let Err(e) = self.send(&[disconnect]).await {
                debug!(session = self.id.get(), error = %e, "shutdown message not sent");
            }
        }
        if self.logged_in && self.shared.events.send(Event::Ended).await.is_err() {
            debug!("accept task gone before the session ended");
        }
        self.conn.close();
    }

    async fn on_message(&mut self, mut message: Vec<u8>) -> Result<()> {
        if self.stage == Stage::NetworkSettings {
            return self.on_first_message(&message).await;
        }
        if let Some(decryptor) = &mut self.decryptor {
            let len = decryptor.decrypt(&mut message).map_err(Error::Crypto)?;
            message.truncate(len);
        }
        self.charge_pre_auth(message.len())?;
        let decoder = self
            .decoder
            .as_mut()
            .ok_or(Error::Order("batch before network settings"))?;
        let mut payload = mem::take(&mut self.payload);
        let result = match decoder.decode(&message, &mut payload) {
            Ok(()) => self.on_payload(&payload).await,
            Err(e) => Err(Error::Batch(e)),
        };
        payload.clear();
        payload.shrink_to(KEPT_BUFFER_BYTES);
        self.payload = payload;
        result
    }

    /// `RequestNetworkSettings`: the only message sent without a compression header.
    async fn on_first_message(&mut self, message: &[u8]) -> Result<()> {
        if message.len() > FIRST_MESSAGE_BYTES {
            return Err(Error::Limit("first message"));
        }
        let mut packets = batch::split(message).map_err(Error::Batch)?;
        let (header, bytes) = packets.next().ok_or(Error::Order("empty first message"))?;
        if packets.next().is_some() {
            return Err(Error::Order("more than one packet before network settings"));
        }
        let mut budget = FIRST_MESSAGE_BYTES;
        let Packet::RequestNetworkSettings(RequestNetworkSettings { client_protocol }) =
            Packet::decode(header.id, bytes, &mut budget, SHIELD_ID).map_err(Error::Packet)?
        else {
            return Err(Error::Order("first packet is not RequestNetworkSettings"));
        };
        self.check_protocol(client_protocol).await?;
        let settings = self.shared.batch;
        self.send(&[Packet::NetworkSettings(NetworkSettings {
            compression_threshold: settings.threshold(),
            compression_algorithm: settings.compression().network_id(),
            ..NetworkSettings::default()
        })])
        .await?;
        self.encoder = Some(batch::Encoder::new(settings));
        self.decoder = Some(batch::Decoder::new(settings));
        self.stage = Stage::Login;
        Ok(())
    }

    async fn on_payload(&mut self, payload: &[u8]) -> Result<()> {
        self.charge_pre_auth(payload.len())?;
        let now = clock::now().saturating_duration_since(self.started);
        if !self.budget.take(payload.len(), self.budget_rate, now) {
            return Err(Error::Limit("input bytes per second"));
        }
        let packets = batch::split(payload).map_err(Error::Batch)?;
        // The whole batch shares one allocation budget, as in `batch::decode_packets`.
        let mut budget = self.shared.batch.decode_budget();
        for (header, bytes) in packets {
            if self.held_click.as_ref().is_some_and(|(h, held, at)| {
                *h == header && held == bytes && now.saturating_sub(*at) < HELD_CLICK_REPEAT
            }) {
                continue;
            }
            // Counted before decoding, so packets dropped below count as their bytes do.
            if !self.packets.take(1, PACKET_RATE, now) {
                return Err(Error::Limit("input packets per second"));
            }
            let left = budget;
            match Packet::decode(header.id, bytes, &mut budget, SHIELD_ID) {
                // The input log holds the decoded packet and writes its bytes, so it counts the
                // larger of the two.
                Ok(packet) => {
                    let size = bytes.len().max(left - budget);
                    match &packet {
                        Packet::InventoryTransaction(t) if is_held_click(t) => {
                            self.held_click = Some((header, bytes.to_vec(), now));
                        }
                        Packet::InventoryTransaction(_) => self.held_click = None,
                        Packet::PlayerAuthInput(input) if input.item_interaction_data.is_some() => {
                            self.held_click = None;
                        }
                        _ => {}
                    }
                    self.on_packet(packet, size).await?;
                }
                // After the spawn one bad packet is dropped; the batch length
                // prefixes keep the other packets intact.
                Err(e) if self.spawned => {
                    warn!(session = self.id.get(), id = header.id, error = %e, "packet dropped");
                }
                Err(e) => return Err(Error::Packet(e)),
            }
        }
        Ok(())
    }

    /// Handles one decoded packet that the input log counts as `size` bytes.
    async fn on_packet(&mut self, packet: Packet, size: usize) -> Result<()> {
        match (self.stage, packet) {
            (Stage::Login, Packet::Login(login)) => self.on_login(login).await,
            (Stage::Login, _) => Err(Error::Order("first packet is not Login")),
            (Stage::Handshake, Packet::ClientToServerHandshake(_)) => self.login_success().await,
            (Stage::Packs, Packet::ResourcePackClientResponse(response)) => {
                self.on_pack_response(&response).await
            }
            (Stage::Packs, Packet::ResourcePackChunkRequest(request)) => self
                .transfer
                .on_chunk_request(&self.shared.packs, &request)
                .map_err(Error::Packs),
            // Chunks are sent without the blob cache, so the client's cache status needs no reply.
            (Stage::Handshake | Stage::Packs, Packet::ClientCacheStatus(_)) => Ok(()),
            (
                _,
                Packet::RequestNetworkSettings(_)
                | Packet::Login(_)
                | Packet::ClientToServerHandshake(_)
                | Packet::ResourcePackClientResponse(_)
                | Packet::ResourcePackChunkRequest(_),
            ) => Err(Error::Order("login sequence packet repeated or early")),
            (Stage::Joined, packet) => {
                if matches!(packet, Packet::SetLocalPlayerAsInitialised(_)) {
                    self.spawned = true;
                }
                self.forward(packet, size)
            }
            (_, packet) => {
                if self.deferred.len() >= MAX_DEFERRED {
                    return Err(Error::Limit("packets before the join"));
                }
                self.deferred.push_back((packet, size));
                Ok(())
            }
        }
    }

    /// Refuses a client of another protocol version, telling it which side is outdated. Both
    /// `RequestNetworkSettings` and `Login` carry the version.
    async fn check_protocol(&mut self, client_protocol: i32) -> Result<()> {
        if client_protocol == PROTOCOL_VERSION {
            return Ok(());
        }
        let status = if client_protocol > PROTOCOL_VERSION {
            consts::PlayStatus::LoginFailedServer
        } else {
            consts::PlayStatus::LoginFailedClient
        };
        self.send(&[Packet::PlayStatus(PlayStatus {
            status: i32::from(status),
        })])
        .await?;
        Err(Error::Protocol(client_protocol))
    }

    async fn on_login(&mut self, login: packet::Login) -> Result<()> {
        self.check_protocol(login.client_protocol).await?;
        let verified = match self.verify(&login.connection_request).await {
            Ok(verified)
                if self
                    .conn
                    .proven_key()
                    .is_some_and(|key| key.spki() != verified.client_key.spki()) =>
            {
                Err(Error::Binding)
            }
            verified => verified,
        };
        let verified = match verified {
            Ok(verified) => verified,
            Err(e) => {
                self.send(&[Packet::Disconnect(Disconnect {
                    reason: i32::from(consts::DisconnectReason::NotAuthenticated),
                    message: "Login failed".to_owned(),
                    ..Disconnect::default()
                })])
                .await?;
                return Err(e);
            }
        };
        let Ok(waiting) = Arc::clone(&self.shared.waiting).try_acquire_owned() else {
            // PMMP also turns players away as the server is full at login.
            self.send(&[queue::server_full()]).await?;
            return Err(Error::Limit("sessions waiting to join"));
        };
        self.waiting = Some(waiting);
        let Login {
            identity,
            client_key,
            client_data,
            skin,
        } = verified;
        let addr = self.conn.addr();
        let input_mode = u32::try_from(client_data.current_input_mode)
            .ok()
            .and_then(|mode| InputMode::try_from(mode).ok())
            .unwrap_or(InputMode::Undefined);
        self.identity = Some(Identity {
            name: identity.name,
            xuid: identity.xuid,
            uuid: identity.uuid,
            skin,
            device_os: client_data.device_os,
            address: Some(addr.ip()),
            client: Client {
                locale: client_data.language_code,
                input_mode,
                device_model: client_data.device_model,
                device_id: client_data.device_id,
                self_signed_id: client_data.self_signed_id,
                random_id: client_data.client_random_id,
                playfab_id: identity.playfab_id,
                game_version: client_data.game_version,
                server_address: client_data.server_address,
                port: addr.port(),
                // Stamped when the player is handed to the tick.
                join_time_ms: 0,
            },
        });
        self.logged_in = true;
        self.conn.logged_in();
        if self.shared.events.send(Event::LoggedIn).await.is_err() {
            debug!("accept task gone during login");
        }
        if self.conn.encrypts() && !self.shared.encryption {
            // `enable-encryption = false`: no handshake, plain RakNet traffic (PMMP
            // `NetworkSession::onServerLoginSuccess` without encryption).
            return self.login_success().await;
        }
        let handshake =
            Handshake::new(&self.shared.server_key, &client_key).map_err(Error::Crypto)?;
        // The handshake goes out before encryption starts; everything after it is encrypted.
        // BDS sends it on NetherNet too but never encrypts there (P6).
        self.send(&[Packet::ServerToClientHandshake(ServerToClientHandshake {
            jwt: handshake.jwt.into_bytes(),
        })])
        .await?;
        if self.conn.encrypts() {
            self.encryptor = Some(handshake.encryptor);
            self.decryptor = Some(handshake.decryptor);
        }
        self.stage = Stage::Handshake;
        Ok(())
    }

    /// The login succeeded (after the handshake, or at once without encryption): the client
    /// hears so and gets the resource packs on offer.
    async fn login_success(&mut self) -> Result<()> {
        self.stage = Stage::Packs;
        self.send(&[
            Packet::PlayStatus(PlayStatus {
                status: i32::from(consts::PlayStatus::LoginSuccess),
            }),
            Packet::ResourcePacksInfo(self.shared.packs.info().clone()),
        ])
        .await
    }

    /// Verifies the connection request: online against the authorization keys (one JWKS
    /// refresh for an unknown key ID, within the login deadline), offline without them.
    async fn verify(&self, request: &[u8]) -> Result<Login> {
        let Some(auth) = &self.shared.auth else {
            return login::verify(request, Mode::Offline, clock::token_now()).map_err(Error::Login);
        };
        let keys = auth.keys();
        match login::verify(request, Mode::Online(&keys), clock::token_now()) {
            Err(login::Error::UnknownKeyId) => {}
            result => return result.map_err(Error::Login),
        }
        let refreshed = tokio::time::timeout_at(
            tokio::time::Instant::from_std(self.started + LOGIN_DEADLINE),
            auth.refresh_keys(),
        )
        .await;
        let keys: Arc<Keys> = match refreshed {
            Ok(Some(keys)) => keys,
            Ok(None) | Err(_) => return Err(Error::Login(login::Error::UnknownKeyId)),
        };
        login::verify(request, Mode::Online(&keys), clock::token_now()).map_err(Error::Login)
    }

    async fn on_pack_response(&mut self, response: &ResourcePackClientResponse) -> Result<()> {
        let reply = self
            .transfer
            .on_response(&self.shared.packs, response)
            .map_err(Error::Packs)?;
        match reply {
            Reply::Send { packets, extend } => {
                self.spawn_at = self.spawn_at.checked_add(extend).unwrap_or(self.spawn_at);
                if packets.is_empty() {
                    return Ok(());
                }
                self.send(&packets).await
            }
            Reply::Join => self.join().await,
            Reply::Refused => {
                self.send(&[Packet::Disconnect(Disconnect {
                    reason: i32::from(consts::DisconnectReason::ResourcePackProblem),
                    message: PACKS_REFUSED_MESSAGE.to_owned(),
                    ..Disconnect::default()
                })])
                .await?;
                Err(Error::Order("resource packs refused"))
            }
        }
    }

    /// Sends the next resource pack chunk the client asked for.
    async fn send_chunk(&mut self) -> Result<()> {
        match self.transfer.next_chunk(&self.shared.packs) {
            Some(chunk) => self.send(&[chunk]).await,
            None => Ok(()),
        }
    }

    /// Hands the player to the tick, then the packets that arrived before the join. While every
    /// connection share of the tick queue is held, the player is turned away as from a full
    /// server.
    async fn join(&mut self) -> Result<()> {
        let mut identity = self
            .identity
            .clone()
            .ok_or(Error::Order("join before login"))?;
        identity.client.join_time_ms = clock::unix_millis();
        let (outbound, receiver) = queue::outbound_queue();
        let Some(mut inputs) = self.shared.inputs.connection(self.id) else {
            self.send(&[queue::server_full()]).await?;
            return Err(Error::Limit("sessions joined at once"));
        };
        inputs
            .try_send(InputKind::Join { identity, outbound })
            .map_err(Error::Queue)?;
        debug!(session = self.id.get(), addr = %self.conn.addr(), "player joining");
        // The tick counts the player from here.
        self.waiting = None;
        self.inputs = Some(inputs);
        self.outbound = Some(receiver);
        self.stage = Stage::Joined;
        while let Some((packet, size)) = self.deferred.pop_front() {
            if matches!(packet, Packet::SetLocalPlayerAsInitialised(_)) {
                self.spawned = true;
            }
            self.forward(packet, size)?;
        }
        Ok(())
    }

    /// Reports the connection's ping to the tick if it changed since the last report. A full
    /// share of the tick queue skips this report: the next packet finds it full and ends the
    /// session.
    fn report_ping(&mut self) {
        let Some(ping) = self.conn.ping() else {
            return;
        };
        let millis = u32::try_from(ping.as_millis()).unwrap_or(u32::MAX);
        if self.reported_ping == Some(millis) {
            return;
        }
        let Some(inputs) = self.inputs.as_mut() else {
            return;
        };
        match inputs.try_send(InputKind::Measure(Measure::Ping(millis))) {
            Ok(()) => self.reported_ping = Some(millis),
            Err(e) => debug!(session = self.id.get(), error = %e, "ping not reported"),
        }
    }

    /// Queues a packet for the tick. A full share of the tick queue ends the session (the tick
    /// never waits).
    fn forward(&mut self, packet: Packet, size: usize) -> Result<()> {
        let inputs = self
            .inputs
            .as_mut()
            .ok_or(Error::Order("packet before the join"))?;
        inputs
            .try_send(InputKind::Packet { packet, size })
            .map_err(Error::Queue)
    }

    /// Counts bytes against the pre-login total.
    fn charge_pre_auth(&mut self, bytes: usize) -> Result<()> {
        if self.logged_in {
            return Ok(());
        }
        let limit = self
            .shared
            .batch
            .max_batch_bytes()
            .saturating_mul(PRE_AUTH_BATCHES);
        self.pre_auth_bytes = self.pre_auth_bytes.saturating_add(bytes);
        if self.pre_auth_bytes > limit {
            return Err(Error::Limit("bytes before login"));
        }
        Ok(())
    }

    /// Batches, compresses and encrypts `packets` and hands the messages to the connection, after
    /// all the output still waiting, without waiting for delivery.
    async fn send<'p>(&mut self, packets: impl IntoIterator<Item = &'p Packet>) -> Result<()> {
        self.queue_packets(packets)?;
        while let Some(body) = self.next_body()? {
            self.send_body(body).await?;
        }
        Ok(())
    }

    /// Batches and compresses `packets` into bodies at the end of `out_bodies`.
    fn encode<'p>(&mut self, packets: impl IntoIterator<Item = &'p Packet>) -> Result<()> {
        let bodies = &mut self.out_bodies;
        let headers = packets.into_iter().map(|packet| {
            let header = Header {
                id: packet.id(),
                ..Header::default()
            };
            (header, packet)
        });
        let encoder = &mut self.encoder;
        let batched = batch::encode_packets(headers, SHIELD_ID, &mut self.out_payload, |payload| {
            let mut body = Vec::new();
            match encoder {
                Some(encoder) => encoder.encode(payload, &mut body)?,
                // Before compression is on, the payload goes as it is (no header byte).
                None => body.extend_from_slice(payload),
            }
            bodies.push_back(body);
            Ok(())
        });
        // Cleared first: `shrink_to` never goes below the length.
        self.out_payload.clear();
        self.out_payload.shrink_to(KEPT_BUFFER_BYTES);
        batched.map_err(Error::Batch)
    }

    /// Batches and compresses `packets` into bodies waiting after all the output before them.
    fn queue_packets<'p>(&mut self, packets: impl IntoIterator<Item = &'p Packet>) -> Result<()> {
        let start = self.out_bodies.len();
        self.encode(packets)?;
        for body in self.out_bodies.drain(start..) {
            self.pending_bytes = self.pending_bytes.saturating_add(body.len());
            self.pending.push_back(Pending::Body(body));
        }
        Ok(())
    }

    /// Queues the tick's messages in order: runs of packets batched here, a shared packet in a
    /// batch of its own, shared batches as they are. A spawn window's player list and shared
    /// batches wait as the shared copy, batched or copied as they go out. More than
    /// [`MAX_PENDING_BYTES`] waiting is a limit error.
    fn queue_messages(&mut self, messages: &[Message]) -> Result<()> {
        // A client answers each chunk sent in sub-chunk request mode with a request, which the
        // server asked for: it gives its packet back to the budget.
        let asked = messages
            .iter()
            .filter(|m| {
                matches!(m, Message::Packet(Packet::LevelChunk(c)) if c.sub_chunk_limit.is_some())
            })
            .count();
        self.packets.give(asked, PACKET_RATE);
        let same_run = |a: &Message, b: &Message| packet_of(a).is_some() && packet_of(b).is_some();
        for group in messages.chunk_by(same_run) {
            match group {
                [Message::SpawnList(packet)] => {
                    self.pending
                        .push_back(Pending::SpawnList(Arc::clone(packet)));
                }
                [Message::Shared(batches)] => {
                    self.pending_bytes = self.pending_bytes.saturating_add(bytes_of(batches));
                    self.pending
                        .push_back(Pending::Batches(Arc::clone(batches)));
                }
                [Message::SharedPacket(packet)] => self.queue_packets([&**packet])?,
                run => {
                    self.queue_packets(run.iter().filter_map(packet_of))?;
                    // The marker follows the run's last batch, so the few packets after the
                    // status in the same tick share its allowance.
                    if run.iter().filter_map(packet_of).any(is_spawn_status) {
                        self.pending.push_back(Pending::SpawnStatus);
                    }
                }
            }
        }
        if self.pending_bytes > MAX_PENDING_BYTES {
            return Err(Error::Limit("output waiting for delivery"));
        }
        Ok(())
    }

    /// The next batch body to go out, if any waits: the first output waiting is batched or
    /// copied once the bodies before it are gone.
    fn next_body(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(body) = self.out_bodies.pop_front() {
                return Ok(Some(body));
            }
            match self.pending.pop_front() {
                None => return Ok(None),
                Some(Pending::Body(body)) => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(body.len());
                    return Ok(Some(body));
                }
                Some(Pending::SpawnList(packet)) => self.encode([&*packet])?,
                Some(Pending::Batches(batches)) => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(bytes_of(&batches));
                    self.out_bodies
                        .extend(batches.iter().map(|body| owned_body(body)));
                }
                // The batch before the marker went out when the next one is asked for.
                Some(Pending::SpawnStatus) => self.spawn_status_sent = true,
            }
        }
    }

    /// Hands the next waiting body, if one waits, to the connection and counts it as not yet
    /// in the connection's undelivered count.
    async fn send_waiting(&mut self) -> Result<()> {
        let Some(body) = self.next_body()? else {
            return Ok(());
        };
        self.unreported_bytes = self.unreported_bytes.saturating_add(body.len());
        self.send_body(body).await
    }

    /// Encrypts a batch body when the session is encrypted and hands it to the connection. A
    /// batch of the tick up to the one with the spawn status moves the spawn deadline by its
    /// allowance, while `send_extension_left` lasts: the client must receive it before it can
    /// spawn, and the player list in it grows with the players.
    async fn send_body(&mut self, mut body: Vec<u8>) -> Result<()> {
        if let Some(encryptor) = &mut self.encryptor {
            encryptor.encrypt(&mut body).map_err(Error::Crypto)?;
        }
        if self.stage == Stage::Joined && !self.spawned && !self.spawn_status_sent {
            let extend = packs::send_allowance(body.len()).min(self.send_extension_left);
            self.send_extension_left = self.send_extension_left.saturating_sub(extend);
            self.spawn_at = self.spawn_at.checked_add(extend).unwrap_or(self.spawn_at);
        }
        self.conn.send(body).await.map_err(Error::Transport)
    }
}

/// The packet a [`Message::Packet`] carries.
fn packet_of(message: &Message) -> Option<&Packet> {
    match message {
        Message::Packet(packet) => Some(packet),
        Message::SharedPacket(_) | Message::SpawnList(_) | Message::Shared(_) => None,
    }
}

/// The bytes of the bodies of `batches`.
fn bytes_of(batches: &SharedBatches) -> usize {
    batches.iter().map(Vec::len).sum()
}

/// Whether `transaction` is a right click held on a block (`facts.md`): a `ClickBlock` the
/// client's simulation tick sends, not a press.
fn is_held_click(transaction: &packet::InventoryTransaction) -> bool {
    matches!(&transaction.transaction_data, InventoryTransactionData::UseItem(data)
        if consts::UseItemAction::try_from(data.action_type) == Ok(consts::UseItemAction::ClickBlock)
            && consts::TriggerType::try_from(data.trigger_type)
                == Ok(consts::TriggerType::SimulationTick))
}

/// Whether `packet` is the tick's `PlayStatus(PlayerSpawn)`.
fn is_spawn_status(packet: &Packet) -> bool {
    matches!(packet, Packet::PlayStatus(PlayStatus { status })
        if *status == i32::from(consts::PlayStatus::PlayerSpawn))
}

/// A session's copy of a shared batch body, with room for the encryption checksum.
fn owned_body(body: &[u8]) -> Vec<u8> {
    let mut owned = Vec::with_capacity(body.len() + CHECKSUM);
    owned.extend_from_slice(body);
    owned
}

/// The next tick message, or never when the player has not joined.
async fn next_outbound(outbound: Option<&mut mpsc::Receiver<Outbound>>) -> Option<Outbound> {
    match outbound {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn deadline(at: Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(tokio::time::Instant::from_std(at))
}

#[cfg(test)]
mod tests {
    use tokio::sync::{Semaphore, watch};

    use super::*;
    use crate::crypto::ServerKey;
    use crate::pack::Offer;
    use crate::pack::tests::UUID;
    use crate::protocol::batch::MAX_PACKETS;
    use crate::tick::queue::{
        Input, InputReceiver, InputSender, SESSION_INPUTS, Skin, input_queue,
    };

    /// What the sessions share: offline, unencrypted, no packs, inputs on `inputs`.
    fn shared(inputs: InputSender) -> Arc<Shared> {
        sharing(inputs, Offer::new(Vec::new(), false).unwrap()).0
    }

    /// What the sessions share: offline, unencrypted, `packs` on offer, inputs on `inputs`; and
    /// the shutdown signal, which stops sessions before the join once dropped.
    fn sharing(inputs: InputSender, packs: Offer) -> (Arc<Shared>, watch::Sender<bool>) {
        let (events, _) = mpsc::channel(1);
        let (stop, shutdown) = watch::channel(false);
        let shared = Arc::new(Shared {
            batch: batch::Settings::default(),
            auth: None,
            server_key: ServerKey::generate().unwrap(),
            packs,
            inputs,
            events,
            shutdown,
            shutdown_message: String::new(),
            encryption: false,
            waiting: Arc::new(Semaphore::new(1)),
            max_send_extension: crate::session::max_send_extension(1),
        });
        (shared, stop)
    }

    /// Runs a test on its own single-thread runtime with time paused: it jumps ahead whenever
    /// every task waits.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test drives a session on a runtime of its own"
    )]
    fn run_paused<F: Future>(test: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(test)
    }

    /// Session `id` as any client gets it after the login and the resource packs: joined, with
    /// its Join in the tick queue.
    async fn joined(id: u64, shared: &Arc<Shared>) -> Session {
        let conn = Transport::Raknet(crate::raknet::tests::detached_connection());
        joined_on(id, shared, conn).await
    }

    /// [`joined`] on `conn`.
    async fn joined_on(id: u64, shared: &Arc<Shared>, conn: Transport) -> Session {
        let mut session = Session::new(SessionId::new(id), conn, Arc::clone(shared));
        session.decoder = Some(batch::Decoder::new(shared.batch));
        session.logged_in = true;
        session.identity = Some(Identity {
            name: format!("Player{id}"),
            xuid: None,
            uuid: crate::protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: -1,
            address: None,
            client: Client::default(),
        });
        session.join().await.unwrap();
        session
    }

    /// One client sending more small packets in one tick than its share of the tick queue ends
    /// its own session, and another client's next packet still reaches the tick.
    #[test]
    fn a_packet_flood_ends_only_the_flooding_session() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut flooder = joined(0, &shared).await;
            let mut player = joined(1, &shared).await;
            // Uncompressed batches of SetTime(0), 3 bytes a packet; the tick drains nothing
            // meanwhile, as between two ticks.
            let mut flood = vec![0xff];
            flood.extend([0x02, 0x0a, 0x00].repeat(MAX_PACKETS));
            let mut ended = None;
            for _ in 0..=SESSION_INPUTS.div_ceil(MAX_PACKETS) {
                if let Err(e) = flooder.on_message(flood.clone()).await {
                    ended = Some(e);
                    break;
                }
            }
            player
                .on_message(vec![0xff, 0x02, 0x0a, 0x00])
                .await
                .expect("the other player's packet is queued");
            assert!(matches!(ended, Some(Error::Queue(_))), "{ended:?}");
            let mut reached = false;
            while let Ok(input) = queue.try_recv() {
                reached |= input.session == SessionId::new(1)
                    && matches!(input.kind, InputKind::Packet { .. });
            }
            assert!(reached, "the other player's packet reached the tick");
        });
    }

    /// However many sessions send all their packet budget allows between two ticks, another
    /// client's packet still reaches the tick: each fills only its own share of the queue.
    #[test]
    fn many_sessions_at_their_budget_leave_room_for_another() {
        const FLOODERS: u64 = 80;
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(u32::try_from(FLOODERS).unwrap());
            let shared = shared(inputs);
            let mut player = joined(FLOODERS, &shared).await;
            let mut flooders = Vec::new();
            for id in 0..FLOODERS {
                flooders.push(joined(id, &shared).await);
            }
            // Uncompressed batches of SetTime(0), 3 bytes a packet: the whole burst.
            let batch = |packets: usize| {
                let mut b = vec![0xff];
                b.extend([0x02, 0x0a, 0x00].repeat(packets));
                b
            };
            let burst = [
                batch(MAX_PACKETS),
                batch(PACKET_RATE.capacity - MAX_PACKETS),
            ];
            for flooder in &mut flooders {
                for message in &burst {
                    if flooder.on_message(message.clone()).await.is_err() {
                        break;
                    }
                }
            }
            player
                .on_message(batch(1))
                .await
                .expect("the other player's packet is queued");
        });
    }

    /// A client that answers each chunk sent in sub-chunk request mode with a request keeps its
    /// packet budget, at the most `chunks-per-tick` for as long as the chunks come.
    #[test]
    fn sub_chunk_requests_for_the_chunks_sent_take_no_packet_budget() {
        const MOST: i32 = 256;
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            let chunks = || {
                (0..MOST)
                    .map(|x| {
                        Message::Packet(Packet::LevelChunk(packet::LevelChunk {
                            position: [x, 0],
                            sub_chunk_limit: Some(0),
                            ..packet::LevelChunk::default()
                        }))
                    })
                    .collect::<Vec<_>>()
            };
            let request = Packet::SubChunkRequest(packet::SubChunkRequest::default());
            let headers = std::iter::repeat_n(&request, usize::try_from(MOST).unwrap()).map(|p| {
                (
                    Header {
                        id: p.id(),
                        ..Header::default()
                    },
                    p,
                )
            });
            let mut requests = vec![0xff];
            batch::encode_packets(headers, SHIELD_ID, &mut Vec::new(), |payload| {
                requests.extend_from_slice(payload);
                Ok(())
            })
            .unwrap();
            // A second of ticks; the tick takes every input between two.
            for _ in 0..20 {
                player
                    .queue_messages(&chunks())
                    .expect("the chunks are batched");
                player
                    .on_message(requests.clone())
                    .await
                    .expect("the requests are queued");
                while queue.try_recv().is_ok() {}
            }
        });
    }

    /// A packet that decodes into far more than its bytes reaches the tick with what decoding
    /// it allocated, which the input log counts.
    #[test]
    fn a_packet_reaches_the_tick_with_what_decoding_it_allocated() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            let packet = Packet::InventoryTransaction(packet::InventoryTransaction {
                actions: vec![
                    crate::protocol::InventoryAction::default();
                    crate::protocol::MAX_LIST_LEN
                ],
                ..packet::InventoryTransaction::default()
            });
            let mut body = Vec::new();
            packet.encode(&mut body, SHIELD_ID).unwrap();
            let mut budget = usize::MAX;
            Packet::decode(packet.id(), &body, &mut budget, SHIELD_ID).unwrap();
            let allocated = usize::MAX - budget;
            let mut message = vec![0xff];
            let header = Header {
                id: packet.id(),
                ..Header::default()
            };
            batch::encode_packets([(header, &packet)], SHIELD_ID, &mut Vec::new(), |p| {
                message.extend_from_slice(p);
                Ok(())
            })
            .unwrap();
            player.on_message(message).await.unwrap();
            let mut sizes = Vec::new();
            while let Ok(input) = queue.try_recv() {
                if let InputKind::Packet { size, .. } = input.kind {
                    sizes.push(size);
                }
            }
            assert!(allocated > body.len());
            assert_eq!(sizes, [allocated]);
        });
    }

    /// A client holding use on a block sends its click again every frame, however high its
    /// frame rate: the copies take no packet budget and reach the tick at most once in a while,
    /// and a click of its own (a press) always reaches it.
    #[test]
    fn a_held_click_sent_every_frame_takes_no_packet_budget() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            player.spawned = true;
            let click = |trigger: consts::TriggerType| {
                let packet = Packet::InventoryTransaction(packet::InventoryTransaction {
                    transaction_data: InventoryTransactionData::UseItem(Box::new(
                        crate::protocol::UseItemTransactionData {
                            action_type: u32::from(consts::UseItemAction::ClickBlock),
                            trigger_type: u32::from(trigger),
                            ..crate::protocol::UseItemTransactionData::default()
                        },
                    )),
                    ..packet::InventoryTransaction::default()
                });
                let mut message = vec![0xff];
                let header = Header {
                    id: packet.id(),
                    ..Header::default()
                };
                batch::encode_packets([(header, &packet)], SHIELD_ID, &mut Vec::new(), |p| {
                    message.extend_from_slice(p);
                    Ok(())
                })
                .unwrap();
                message
            };
            let mut forwarded = || {
                let mut count = 0;
                while let Ok(input) = queue.try_recv() {
                    count += usize::from(matches!(input.kind, InputKind::Packet { .. }));
                }
                count
            };
            let held = click(consts::TriggerType::SimulationTick);
            for _ in 0..2 * PACKET_RATE.capacity {
                player
                    .on_message(held.clone())
                    .await
                    .expect("the copies take no packet budget");
            }
            assert_eq!(forwarded(), 1);
            player.started = player.started.checked_sub(crate::tick::TICK).unwrap();
            player.on_message(held.clone()).await.unwrap();
            assert_eq!(forwarded(), 1, "a copy a tick later");
            let press = click(consts::TriggerType::PlayerInput);
            player.on_message(press.clone()).await.unwrap();
            player.on_message(press).await.unwrap();
            player.on_message(held).await.unwrap();
            assert_eq!(forwarded(), 3, "presses, and the held click after them");
        });
    }

    /// A client that asks for every chunk at once on a link that delivers nothing gets one
    /// chunk, not one every [`CHUNK_INTERVAL`] until the transport's outbound cap, and the rest
    /// as the connection delivers them.
    #[test]
    fn pack_chunks_wait_for_the_connection_to_deliver() {
        run_paused(async {
            let (inputs, _queue) = input_queue(1);
            let big = crate::pack::tests::pack(UUID, "1.0.0", 7 * 128 * 1024);
            let (shared, _stop) = sharing(inputs, Offer::new(vec![big], true).unwrap());
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = Session::new(
                SessionId::new(0),
                Transport::Raknet(conn),
                Arc::clone(&shared),
            );
            session.logged_in = true;
            session.stage = Stage::Packs;
            let id = shared.packs.id(0).to_owned();
            let send_packs = ResourcePackClientResponse {
                response: u32::from(consts::PackResponse::SendPacks),
                packs_to_download: vec![id.clone()],
            };
            session
                .transfer
                .on_response(&shared.packs, &send_packs)
                .unwrap();
            let chunks = shared.packs.packs()[0]
                .size()
                .div_ceil(u64::from(packs::CHUNK_BYTES));
            assert_eq!(chunks, 8);
            for chunk_index in 0..i32::try_from(chunks).unwrap() {
                let request = packet::ResourcePackChunkRequest {
                    uuid: id.clone(),
                    chunk_index,
                };
                session
                    .transfer
                    .on_chunk_request(&shared.packs, &request)
                    .unwrap();
            }
            let client = async {
                let mut sent = 0;
                // For half a minute nothing sent is delivered.
                while let Ok(Some(chunk)) =
                    tokio::time::timeout(Duration::from_secs(30), far.sent()).await
                {
                    sent += 1;
                    far.undelivered.send_modify(|bytes| *bytes += chunk.len());
                }
                assert_eq!(sent, 1, "{} bytes undelivered", *far.undelivered.borrow());
                // Then each chunk is delivered as it comes.
                for _ in 1..chunks {
                    far.undelivered.send_replace(0);
                    far.sent().await.unwrap();
                }
            };
            tokio::select! {
                end = session.serve() => panic!("the session ended: {end:?}"),
                () = client => {}
            }
        });
    }

    /// Before the spawn, each batch from the tick moves the spawn deadline by its time at the
    /// download floor rate (64 KiB/s): a client that must first receive 8 MiB (a player list of
    /// large skins) has 128 seconds more, and one sent nothing still times out after the 60.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test reads the paused runtime's clock, which only timers move"
    )]
    fn the_spawn_deadline_grows_with_what_is_sent_before_the_spawn() {
        for (packets, allowance_secs) in [(0, 0), (64, 128)] {
            let waited = run_paused(async {
                let (inputs, mut queue) = input_queue(1);
                let shared = shared(inputs);
                let (conn, _far) = crate::raknet::tests::linked_connection();
                let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
                let Ok(Input {
                    kind: InputKind::Join { outbound, .. },
                    ..
                }) = queue.try_recv()
                else {
                    panic!("the join is queued");
                };
                let messages = (0..packets)
                    .map(|_| {
                        Message::Packet(Packet::ResourcePackChunkData(
                            packet::ResourcePackChunkData {
                                data: vec![0; 128 * 1024],
                                ..packet::ResourcePackChunkData::default()
                            },
                        ))
                    })
                    .collect();
                outbound.send(Outbound::Send(messages)).await.unwrap();
                let start = tokio::time::Instant::now();
                let end = session.serve().await;
                assert!(matches!(end, End::Timeout("spawn")), "{end:?}");
                start.elapsed()
            });
            let expected = SPAWN_DEADLINE + Duration::from_secs(allowance_secs);
            assert!(
                waited >= expected && waited < expected + Duration::from_secs(1),
                "timed out after {waited:?} with {packets} packets sent"
            );
        }
    }

    /// Batches of 1 MiB the tick sends in the tests below.
    const BATCH_BYTES: usize = 1024 * 1024;

    /// Where the tick sends the session whose Join is next in `queue`.
    fn joined_outbound(queue: &mut InputReceiver) -> mpsc::Sender<Outbound> {
        let Ok(Input {
            kind: InputKind::Join { outbound, .. },
            ..
        }) = queue.try_recv()
        else {
            panic!("the join is queued");
        };
        outbound
    }

    /// `count` packets of a spawn window's player list, standing in for entries of large skins
    /// (the session never looks inside): each [`BATCH_BYTES`] of data filled with its index.
    fn numbered_list(count: u8) -> impl Iterator<Item = Message> {
        (0..count).map(|i| {
            Message::SpawnList(Arc::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![i; BATCH_BYTES],
                    ..packet::ResourcePackChunkData::default()
                },
            )))
        })
    }

    /// A spawn window's player list larger than the transports' 32 MiB caps and
    /// [`MAX_PENDING_BYTES`] (one of large skins) goes out before the spawn as a slow link
    /// delivers it, while the tick sends a message every tick for longer than its queue of 1,024
    /// messages lasts: the tick never finds the queue full, the connection never holds more than
    /// the send limit before another batch, and every batch arrives, in order.
    #[test]
    fn a_spawn_player_list_over_the_transport_caps_goes_out_as_the_link_delivers() {
        const BATCHES: u8 = 40;
        /// What the link delivers each [`STEP`] the session sends nothing: 640 KiB/s.
        const STEP_BYTES: usize = 64 * 1024;
        const STEP: Duration = Duration::from_millis(100);
        const TICK: Duration = Duration::from_millis(50);
        const MOST_TICKS: usize = 4_000;
        assert!(usize::from(BATCHES) * BATCH_BYTES > MAX_PENDING_BYTES);
        run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            outbound
                .send(Outbound::Send(numbered_list(BATCHES).collect()))
                .await
                .unwrap();
            let ticks_sent = std::cell::Cell::new(0);
            let tick = async {
                let mut ticks = tokio::time::interval(TICK);
                for _ in 0..MOST_TICKS {
                    ticks.tick().await;
                    let time = Message::Packet(Packet::SetTime(packet::SetTime::default()));
                    if let Err(e) = outbound.try_send(Outbound::Send(vec![time])) {
                        panic!("the tick could not send: {e}");
                    }
                    ticks_sent.set(ticks_sent.get() + 1);
                }
            };
            let link = async {
                let mut received = 0;
                while received < BATCHES {
                    match tokio::time::timeout(STEP, far.sent()).await {
                        Ok(Some(batch)) => {
                            // The data, filled with the packet's index, ends the batch.
                            assert_eq!(batch.last(), Some(&received), "batch out of order");
                            received += 1;
                            let held = *far.undelivered.borrow();
                            assert!(held <= SEND_LIMIT, "{held} bytes held before another batch");
                            far.undelivered.send_modify(|bytes| *bytes += batch.len());
                        }
                        Ok(None) => panic!("the session dropped the connection"),
                        Err(_) => far
                            .undelivered
                            .send_modify(|bytes| *bytes = bytes.saturating_sub(STEP_BYTES)),
                    }
                }
            };
            tokio::select! {
                end = session.serve() => panic!("the session ended: {end:?}"),
                () = tick => panic!("the batches took more than {MOST_TICKS} ticks"),
                () = link => {}
            }
            assert!(ticks_sent.get() > 1024, "{} ticks", ticks_sent.get());
        });
    }

    /// On a link that delivers nothing, the session keeps taking the tick's messages and ends
    /// once more than [`MAX_PENDING_BYTES`] of its own output waits, not when the tick's queue
    /// fills, and the connection never holds more than the send limit and one batch.
    #[test]
    fn a_link_that_delivers_nothing_ends_on_the_output_waiting_not_the_tick_queue() {
        const TICK: Duration = Duration::from_millis(50);
        const MOST_TICKS: usize = 2_000;
        run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            session.spawned = true;
            let outbound = joined_outbound(&mut queue);
            let tick = async {
                let mut ticks = tokio::time::interval(TICK);
                for _ in 0..MOST_TICKS {
                    ticks.tick().await;
                    let chunk = Message::Packet(Packet::ResourcePackChunkData(
                        packet::ResourcePackChunkData {
                            data: vec![0; BATCH_BYTES],
                            ..packet::ResourcePackChunkData::default()
                        },
                    ));
                    if let Err(e) = outbound.try_send(Outbound::Send(vec![chunk])) {
                        panic!("the tick could not send: {e}");
                    }
                }
            };
            let link = async {
                while let Some(batch) = far.sent().await {
                    let held = *far.undelivered.borrow();
                    assert!(held <= SEND_LIMIT, "{held} bytes held before another batch");
                    far.undelivered.send_modify(|bytes| *bytes += batch.len());
                }
            };
            let end = tokio::select! {
                end = session.serve() => end,
                () = tick => panic!("the session took {MOST_TICKS} ticks of output"),
                () = link => panic!("the connection closed before the session ended"),
            };
            assert!(
                matches!(
                    end,
                    End::Failed(Error::Limit("output waiting for delivery"))
                ),
                "{end:?}"
            );
        });
    }

    /// The tick's spawn status, which the client needs before it can spawn.
    fn spawn_status() -> Message {
        Message::Packet(Packet::PlayStatus(PlayStatus {
            status: i32::from(consts::PlayStatus::PlayerSpawn),
        }))
    }

    /// Once the batch with the spawn status went out, later batches no longer move the spawn
    /// deadline: a client that keeps receiving the tick's output but never says it spawned
    /// times out after the 60 seconds and the spawn batch's small allowance.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test reads the paused runtime's clock, which only timers move"
    )]
    fn output_after_the_spawn_status_does_not_move_the_spawn_deadline() {
        /// 128 KiB each step, 1.25 MiB/s: 20 seconds of allowance a second if it counted.
        const STEP: Duration = Duration::from_millis(100);
        const MOST_STEPS: usize = 3_000;
        let waited = run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            outbound
                .send(Outbound::Send(vec![spawn_status()]))
                .await
                .unwrap();
            let start = tokio::time::Instant::now();
            let tick = async {
                let mut steps = tokio::time::interval(STEP);
                for _ in 0..MOST_STEPS {
                    steps.tick().await;
                    let chunk = Message::Packet(Packet::ResourcePackChunkData(
                        packet::ResourcePackChunkData {
                            data: vec![0; 128 * 1024],
                            ..packet::ResourcePackChunkData::default()
                        },
                    ));
                    outbound.send(Outbound::Send(vec![chunk])).await.unwrap();
                }
            };
            // The link delivers everything at once.
            let link = async { while far.sent().await.is_some() {} };
            let end = tokio::select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MOST_STEPS} steps"),
                () = link => panic!("the connection closed before the session ended"),
            };
            assert!(matches!(end, End::Timeout("spawn")), "{end:?}");
            start.elapsed()
        });
        assert!(
            waited < SPAWN_DEADLINE + Duration::from_secs(1),
            "timed out after {waited:?}"
        );
    }

    /// The tick never sends the spawn status (the client never asks for a chunk radius), and the
    /// session receives every shared broadcast as fast as it comes: the broadcasts move the spawn
    /// deadline by [`Shared::max_send_extension`] at most, and the session times out.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test reads the paused runtime's clock, which only timers move"
    )]
    fn output_without_a_spawn_status_moves_the_spawn_deadline_only_so_far() {
        /// 128 KiB each step, 1.25 MiB/s: 20 seconds of allowance a second.
        const STEP: Duration = Duration::from_millis(100);
        const MOST_STEPS: usize = 3_000;
        let (waited, most) = run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            let broadcast = Arc::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![0; 128 * 1024],
                    ..packet::ResourcePackChunkData::default()
                },
            ));
            let start = tokio::time::Instant::now();
            let tick = async {
                let mut steps = tokio::time::interval(STEP);
                for _ in 0..MOST_STEPS {
                    steps.tick().await;
                    let message = Message::SharedPacket(Arc::clone(&broadcast));
                    outbound.send(Outbound::Send(vec![message])).await.unwrap();
                }
            };
            // The link delivers everything at once.
            let link = async { while far.sent().await.is_some() {} };
            let end = tokio::select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MOST_STEPS} steps"),
                () = link => panic!("the connection closed before the session ended"),
            };
            assert!(matches!(end, End::Timeout("spawn")), "{end:?}");
            (start.elapsed(), shared.max_send_extension)
        });
        let expected = SPAWN_DEADLINE + most;
        assert!(
            waited >= expected && waited < expected + Duration::from_secs(1),
            "timed out after {waited:?}"
        );
    }

    /// A joined session the tick never sends the spawn status (its client never asks for a chunk
    /// radius) keeps receiving shared broadcasts (skins of other players) faster than its slow
    /// link delivers them: their bytes count against [`MAX_PENDING_BYTES`], and the session ends
    /// on that bound instead of holding them until its spawn deadline.
    #[test]
    fn shared_broadcasts_before_the_spawn_status_count_against_the_byte_bound() {
        /// What the link delivers each [`STEP`] the session sends nothing: 640 KiB/s.
        const STEP_BYTES: usize = 64 * 1024;
        const STEP: Duration = Duration::from_millis(100);
        const TICK: Duration = Duration::from_millis(50);
        /// 200 seconds, past the spawn deadline and the most the output may add to it.
        const MOST_TICKS: usize = 4_000;
        run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            let skin = Arc::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![0; BATCH_BYTES],
                    ..packet::ResourcePackChunkData::default()
                },
            ));
            let tick = async {
                let mut ticks = tokio::time::interval(TICK);
                for _ in 0..MOST_TICKS {
                    ticks.tick().await;
                    let message = Message::SharedPacket(Arc::clone(&skin));
                    if let Err(e) = outbound.try_send(Outbound::Send(vec![message])) {
                        panic!("the tick could not send: {e}");
                    }
                }
            };
            let link = async {
                loop {
                    match tokio::time::timeout(STEP, far.sent()).await {
                        Ok(Some(batch)) => {
                            far.undelivered.send_modify(|bytes| *bytes += batch.len());
                        }
                        Ok(None) => return,
                        Err(_) => far
                            .undelivered
                            .send_modify(|bytes| *bytes = bytes.saturating_sub(STEP_BYTES)),
                    }
                }
            };
            let end = tokio::select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MOST_TICKS} ticks"),
                () = link => panic!("the connection closed before the session ended"),
            };
            assert!(
                matches!(
                    end,
                    End::Failed(Error::Limit("output waiting for delivery"))
                ),
                "{end:?}"
            );
        });
    }

    /// Shared output waiting for a link that delivers nothing counts by its bytes, before the
    /// spawn status as after it: 2,000 small shared packets (a soft enum changed every tick for
    /// 100 seconds) leave the session running, and shared batches past [`MAX_PENDING_BYTES`]
    /// end it.
    #[test]
    fn shared_output_waits_within_the_byte_bound() {
        const SMALL: usize = 2_000;
        run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            far.undelivered.send_replace(SEND_LIMIT + 1);
            let soft_enum = Arc::new(Packet::UpdateSoftEnum(packet::UpdateSoftEnum {
                enum_type: "maps".to_owned(),
                options: vec!["lobby".to_owned()],
                ..packet::UpdateSoftEnum::default()
            }));
            let small = (0..SMALL)
                .map(|_| Message::SharedPacket(Arc::clone(&soft_enum)))
                .collect();
            let large = (0..=MAX_PENDING_BYTES / BATCH_BYTES)
                .map(|_| vec![0; BATCH_BYTES])
                .collect();
            let large = vec![Message::Shared(large)];
            let small_waited = std::cell::Cell::new(false);
            let tick = async {
                outbound.send(Outbound::Send(small)).await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
                small_waited.set(true);
                outbound.send(Outbound::Send(large)).await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            let link = async { while far.sent().await.is_some() {} };
            let end = tokio::select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on past its bound"),
                () = link => panic!("the connection closed before the session ended"),
            };
            assert!(
                small_waited.get(),
                "ended on the small shared packets: {end:?}"
            );
            assert!(
                matches!(
                    end,
                    End::Failed(Error::Limit("output waiting for delivery"))
                ),
                "{end:?}"
            );
        });
    }
}
