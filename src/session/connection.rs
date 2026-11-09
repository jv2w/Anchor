//! One connection's session: the login sequence (network settings, login, handshake, resource
//! packs), then the relay between the connection and the tick.
//!
//! Inbound, a message is decrypted (RakNet after the handshake), decompressed and split into
//! packets. Before the player joins, packets outside the login sequence wait in a short queue
//! and reach the tick after the join, in order (gophertunnel defers them the same way, without
//! a bound). Outbound, the tick's packets are batched, compressed and encrypted here, off the
//! tick, and handed to the connection as it delivers them ([`MAX_OUTPUT_UNDELIVERED_BYTES`]).
//! NetherNet runs the same handshake but never encrypts (DTLS protects it, as on the official server), and its
//! Login must carry the key its offer proved.

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::{pin, select};
use tracing::{debug, trace, warn};

use super::packs::{self, CHUNK_ALLOWANCE, CHUNK_INTERVAL, MAX_UNDELIVERED_BYTES, Reply, Transfer};
use super::transport::{self, Transport};
use super::{Event, Shared};
use crate::bucket::{Bucket, Rate};
use crate::clock;
use crate::crypto::encryption::{CHECKSUM_BYTES, Decryptor, Encryptor, Handshake};
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
    self, Client, Identity, InputKind, LeaveReason, Measure, Message, Outbound, SessionId,
    SessionInputs, SharedBatches, SharedPacket,
};

/// The input budget refills this fraction of `max-batch-bytes` per second.
const BUDGET_REFILL_DIVISOR: usize = 4;
/// A copy of the held click ([`Session::held_click`]) within this long of the last one forwarded
/// is dropped: half a tick, so one still reaches every tick at any frame rate above 20.
const HELD_CLICK_REPEAT: Duration =
    Duration::from_millis(1000 / 2 / crate::Context::TICKS_PER_SECOND as u64);
/// From connection to verified login.
const LOGIN_DEADLINE: Duration = Duration::from_secs(10);
/// Packets outside the login sequence held until the join. The official client's order is not
/// observable; 32 is far above the few packets (cache status, violation warnings) clients send.
const MAX_DEFERRED: usize = 32;
/// Bytes of the packets held until the join, counted as the input log counts them: 1 MiB is far
/// above the few small packets a client sends before it joins.
const MAX_DEFERRED_BYTES: usize = 1024 * 1024;
/// The largest message before `NetworkSettings`.
const MAX_FIRST_MESSAGE_BYTES: usize = 1024;
/// Capacity the reused payload buffers keep between messages: twice the size at which outbound
/// batches are cut ([`batch`]), so a cut batch and a packet under it fit. A larger message (a
/// Login or a skin of up to 4 MiB) would otherwise pin its size until the session ends.
const MAX_KEPT_BUFFER_BYTES: usize = 2 * batch::SPLIT_BYTES;
/// Undelivered bytes ([`Transport::undelivered`]) at most before the session hands the
/// connection the next batch of the tick's output: above what either transport keeps in flight
/// and low enough that one more batch stays under the transports' caps (`.claude/docs/network.md`).
const MAX_OUTPUT_UNDELIVERED_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of the tick's output at most waiting for delivery, all of it but the spawn windows'
/// player lists ([`Pending::SpawnList`]): a link that falls this far behind ends, as the
/// transports end one with 32 MiB undelivered.
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
/// Reassembled plus decompressed bytes before login verification, in batch limits.
const MAX_PRE_AUTH_BATCHES: usize = 8;
/// The input packet budget: at the default `max-players` the rates of all sessions fit in what
/// the tick applies (`.claude/docs/network.md`). The burst is the session's share of the tick
/// queue.
const PACKET_RATE: Rate = Rate {
    capacity: queue::MAX_SESSION_INPUTS,
    per_second: 512,
};
/// What a player who refuses required packs sees.
const PACKS_REFUSED_MESSAGE: &str = "This server needs its resource packs: accept them to join";
/// How often a joined session reports its ping to the tick when it changed: each
/// report is a logged input.
const PING_REPORT: Duration = Duration::from_secs(5);
/// How long after the last handover the session still counts the bodies it handed over on top
/// of the connection's undelivered count: both transports take the session's messages as they
/// come and report the count every 10 ms update, so by then the count holds them.
const REPORT_WAIT: Duration = Duration::from_millis(30);
/// From connection to `SetLocalPlayerAsInitialised`, plus the allowance of a resource pack
/// download and of what the tick sends up to the spawn status ([`packs`]), the latter at most
/// [`Shared::max_send_extension`].
const SPAWN_DEADLINE: Duration = Duration::from_secs(60);

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
    /// The tick's output after `out_bodies`, waiting for delivery
    /// ([`MAX_OUTPUT_UNDELIVERED_BYTES`]), in order.
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
    /// The verified player, from the login on.
    identity: Option<Identity>,
    deferred: VecDeque<(Packet, usize)>,
    /// The sizes of the packets in `deferred`, added.
    deferred_bytes: usize,
    inputs: Option<SessionInputs>,
    outbound: Option<mpsc::Receiver<Outbound>>,
    /// Where the tick sends to this session, from the login, which hands it to the tick, until
    /// the join hands it over again; then only the tick holds it, so the queue closes when the
    /// tick forgets the session.
    join_outbound: Option<mpsc::Sender<Outbound>>,
    /// The places of this session's IP among the sessions waiting to join, from the accept task.
    ip_waiting: Arc<Semaphore>,
    /// The places among the sessions waiting to join, in the IP's and in all, held from login to
    /// the join.
    waiting: Option<[OwnedSemaphorePermit; 2]>,
    spawned: bool,
    /// The ping last reported to the tick, in milliseconds.
    reported_ping: Option<u32>,
}

impl Session {
    fn new(
        id: SessionId,
        conn: Transport,
        shared: Arc<Shared>,
        ip_waiting: Arc<Semaphore>,
    ) -> Self {
        let max = shared.batch.max_batch_bytes();
        let send_extension_left = shared.max_send_extension;
        let started = clock::now();
        let budget_rate = Rate {
            capacity: max,
            per_second: max / BUDGET_REFILL_DIVISOR,
        };
        Self {
            id,
            conn,
            shared,
            stage: Stage::NetworkSettings,
            started,
            spawn_at: started + SPAWN_DEADLINE,
            send_extension_left,
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
            deferred_bytes: 0,
            inputs: None,
            outbound: None,
            join_outbound: None,
            ip_waiting,
            waiting: None,
            spawned: false,
            reported_ping: None,
        }
    }

    /// Verifies the connection request: online against the authorization keys (one JWKS
    /// refresh for an unknown key ID, within the login deadline), offline without them.
    async fn verify(&self, request: &[u8]) -> Result<Login> {
        let Some(auth) = &self.shared.auth else {
            return login::verify(request, Mode::Offline, clock::token_now()).map_err(Error::Login);
        };
        let verified = tokio::time::timeout_at(
            tokio::time::Instant::from_std(self.started + LOGIN_DEADLINE),
            auth.verify_with_refresh(
                |keys| login::verify(request, Mode::Online(keys), clock::token_now()),
                |e| matches!(e, login::Error::UnknownKeyId),
            ),
        )
        .await;
        match verified {
            Ok(Some(result)) => result.map_err(Error::Login),
            Ok(None) | Err(_) => Err(Error::Login(login::Error::UnknownKeyId)),
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
        pin!(login_deadline, spawn_deadline, pace, report_wait);
        let mut shutdown = self.shared.shutdown.clone();
        if *shutdown.borrow_and_update() {
            return End::Shutdown;
        }
        loop {
            let chunks_wait = self.stage == Stage::Packs && self.transfer.has_chunks();
            let undelivered_bytes = *undelivered.borrow();
            let delivered = undelivered_bytes <= MAX_UNDELIVERED_BYTES;
            let output_waits = !self.pending.is_empty();
            let output_fits = undelivered_bytes.saturating_add(self.unreported_bytes)
                <= MAX_OUTPUT_UNDELIVERED_BYTES;
            select! {
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
                    if *undelivered.borrow() <= MAX_UNDELIVERED_BYTES {
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
                    Some(Outbound::Admitted) => {
                        if let Err(e) = self.offer_packs().await {
                            return End::Failed(e);
                        }
                    }
                    Some(Outbound::Send(messages)) => {
                        if let Err(e) = self.queue_messages(&messages).await {
                            return End::Failed(e);
                        }
                    }
                    Some(Outbound::Close(packet)) => {
                        if let Err(e) = self.send(&[packet]).await {
                            return End::Failed(e);
                        }
                        return End::Kicked;
                    }
                    // The tick closes its input queue before it drops the outbound queues of
                    // the sessions it still holds; any other closed queue is one the running
                    // tick forgot (it was full).
                    None if self.inputs.as_ref().is_some_and(SessionInputs::is_tick_stopped) => {
                        return End::TickStopped;
                    }
                    None => return End::Forgotten,
                },
                () = &mut login_deadline, if self.identity.is_none() => {
                    return End::Timeout("login");
                }
                () = &mut spawn_deadline, if !self.spawned => return End::Timeout("spawn"),
                _ = ping_report.tick(), if self.stage == Stage::Joined => self.report_ping(),
                // Joined sessions end when the stopping tick drops their outbound queue.
                _ = shutdown.changed(), if self.stage != Stage::Joined => return End::Shutdown,
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
            End::Timeout(stage) => debug!(session = self.id.get(), stage, "session timed out"),
            End::Failed(e) => {
                debug!(
                    session = self.id.get(),
                    error = e as &(dyn std::error::Error + 'static),
                    "session failed"
                );
            }
            _ => debug!(session = self.id.get(), ?end, "session ended"),
        }
        if let Some(identity) = &self.identity
            && self.inputs.is_some()
        {
            debug!(session = self.id.get(), player = %identity.name, "session of a player ended");
        }
        if let Some(inputs) = self.inputs.take() {
            // Only the transport knows why a connection it closed ended; any other end is the
            // session's or the server's own.
            let reason = if matches!(end, End::Closed) {
                self.conn.leave_reason()
            } else {
                LeaveReason::ConnectionClosed
            };
            inputs.leave(reason).await;
        }
        // The tick tells the players it holds; these the server stopped before the tick did.
        if matches!(end, End::TickStopped | End::Shutdown) && self.encoder.is_some() {
            let disconnect = Packet::Disconnect(Disconnect {
                reason: i32::from(consts::DisconnectReason::Shutdown),
                message: self.shared.shutdown_message.clone(),
                ..Disconnect::default()
            });
            if let Err(e) = self.send(&[disconnect]).await {
                debug!(
                    session = self.id.get(),
                    error = &e as &(dyn std::error::Error + 'static),
                    "shutdown message not sent"
                );
            }
        }
        if self.identity.is_some() && self.shared.events.send(Event::Ended).await.is_err() {
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
        payload.shrink_to(MAX_KEPT_BUFFER_BYTES);
        self.payload = payload;
        result
    }

    /// Handles `RequestNetworkSettings`, the only message sent without a compression header.
    async fn on_first_message(&mut self, message: &[u8]) -> Result<()> {
        if message.len() > MAX_FIRST_MESSAGE_BYTES {
            return Err(Error::Limit("first message"));
        }
        let mut packets = batch::split(message).map_err(Error::Batch)?;
        let (header, bytes) = packets.next().ok_or(Error::Order("empty first message"))?;
        if packets.next().is_some() {
            return Err(Error::Order("more than one packet before network settings"));
        }
        let mut budget = MAX_FIRST_MESSAGE_BYTES;
        let Packet::RequestNetworkSettings(RequestNetworkSettings { client_protocol }) =
            Packet::decode(header.id, bytes, &mut budget).map_err(Error::Packet)?
        else {
            return Err(Error::Order("first packet is not RequestNetworkSettings"));
        };
        self.check_protocol(client_protocol).await?;
        let settings = self.shared.batch;
        self.send(&[Packet::NetworkSettings(NetworkSettings {
            compression_threshold: settings.threshold_bytes(),
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
        let encrypted = self.decryptor.is_some();
        for (header, bytes) in packets {
            // The Login turns encryption on for the next message: a packet after it in this
            // plain payload would skip the proof that the client holds its key.
            if !encrypted && self.decryptor.is_some() {
                return Err(Error::Order("packet after the login in its batch"));
            }
            if self.held_click.as_ref().is_some_and(|(h, held, at)| {
                *h == header && held == bytes && now.saturating_sub(*at) < HELD_CLICK_REPEAT
            }) {
                continue;
            }
            // Counted before decoding, so packets dropped below count as their bytes do.
            if !self.packets.take(1, PACKET_RATE, now) {
                return Err(Error::Limit("input packets per second"));
            }
            // Split screen is not supported: a sub-client's packet would act as the main
            // player's. After the join it is dropped; before, it breaks the login sequence.
            if header.sender_sub_client != 0 {
                if self.stage != Stage::Joined {
                    return Err(Error::Order("sub-client packet before the join"));
                }
                trace!(
                    session = self.id.get(),
                    id = header.id,
                    "sub-client packet dropped"
                );
                continue;
            }
            let left = budget;
            // Only what clients send decodes (gophertunnel's client packet pool): a packet only
            // servers send fails as one that does not decode.
            let decoded = if Packet::is_from_client(header.id) {
                Packet::decode(header.id, bytes, &mut budget)
            } else {
                Err(crate::protocol::Error::InvalidValue {
                    field: "packet id",
                    reason: "only servers send it",
                })
            };
            match decoded {
                // The input log holds the decoded packet and writes its bytes, so it counts the
                // larger of the two.
                Ok(packet) => {
                    let size = bytes.len().max(left - budget);
                    // The batch paid for its bytes; what decoding allocated beyond them is
                    // charged now, so few bytes cannot fill the tick queue with memory.
                    let extra = size - bytes.len();
                    if !self.budget.take(extra, self.budget_rate, now) {
                        return Err(Error::Limit("input bytes per second"));
                    }
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
                    warn!(
                        session = self.id.get(),
                        id = header.id,
                        error = &e as &(dyn std::error::Error + 'static),
                        "packet dropped"
                    );
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
            (Stage::Handshake, Packet::ClientToServerHandshake(_)) => self.log_in().await,
            (Stage::Packs, Packet::ResourcePackClientResponse(response)) => {
                self.on_pack_response(&response).await
            }
            (Stage::Packs, Packet::ResourcePackChunkRequest(request)) => self
                .transfer
                .on_chunk_request(&self.shared.packs, &request)
                .map_err(Error::Packs),
            // Chunks are sent without the blob cache, so the client's cache status needs no reply.
            (Stage::Handshake | Stage::Admission | Stage::Packs, Packet::ClientCacheStatus(_)) => {
                Ok(())
            }
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
                let bytes = self.deferred_bytes.saturating_add(size);
                if self.deferred.len() >= MAX_DEFERRED || bytes > MAX_DEFERRED_BYTES {
                    return Err(Error::Limit("packets before the join"));
                }
                self.deferred_bytes = bytes;
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
        let (Ok(ip_place), Ok(place)) = (
            Arc::clone(&self.ip_waiting).try_acquire_owned(),
            Arc::clone(&self.shared.waiting).try_acquire_owned(),
        ) else {
            // Players are turned away as the server is full at login.
            self.send(&[queue::server_full()]).await?;
            return Err(Error::Limit("sessions waiting to join"));
        };
        self.waiting = Some([ip_place, place]);
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
            address: Some(addr.ip().to_canonical()),
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
        self.conn.logged_in();
        if self.shared.events.send(Event::LoggedIn).await.is_err() {
            debug!("accept task gone during login");
        }
        if self.conn.can_encrypt() && !self.shared.encryption {
            // `enable-encryption = false`: no handshake, plain RakNet traffic.
            return self.log_in().await;
        }
        let handshake =
            Handshake::new(&self.shared.server_key, &client_key).map_err(Error::Crypto)?;
        // The handshake goes out before encryption starts; everything after it is encrypted.
        // The official server sends it on NetherNet too but never encrypts there (P6).
        self.send(&[Packet::ServerToClientHandshake(ServerToClientHandshake {
            jwt: handshake.jwt.into_bytes(),
        })])
        .await?;
        if self.conn.can_encrypt() {
            self.encryptor = Some(handshake.encryptor);
            self.decryptor = Some(handshake.decryptor);
        }
        self.stage = Stage::Handshake;
        Ok(())
    }

    /// Hands the verified login to the tick (after the handshake, or at once without encryption),
    /// which admits it ([`Outbound::Admitted`]) or refuses it ([`Outbound::Close`]) before the
    /// client learns anything of the resource packs: a player the whitelist, a ban or a
    /// `PlayerLogin` listener keeps out downloads none of them. While every connection share of
    /// the tick queue is held, the player is turned away as from a full server.
    async fn log_in(&mut self) -> Result<()> {
        let identity = self
            .identity
            .clone()
            .ok_or(Error::Order("handshake before login"))?;
        let Some(mut inputs) = self.shared.inputs.connection(self.id) else {
            self.send(&[queue::server_full()]).await?;
            return Err(Error::Limit("sessions logged in at once"));
        };
        let (outbound, receiver) = queue::outbound_queue();
        inputs
            .try_send(InputKind::Login {
                identity,
                outbound: outbound.clone(),
            })
            .map_err(Error::Queue)?;
        self.inputs = Some(inputs);
        self.outbound = Some(receiver);
        self.join_outbound = Some(outbound);
        self.stage = Stage::Admission;
        Ok(())
    }

    /// Tells the client the login succeeded, once the tick admitted it, and offers it the
    /// resource packs.
    async fn offer_packs(&mut self) -> Result<()> {
        self.stage = Stage::Packs;
        self.send(&[
            Packet::PlayStatus(PlayStatus {
                status: i32::from(consts::PlayStatus::LoginSuccess),
            }),
            Packet::ResourcePacksInfo(self.shared.packs.info().clone()),
        ])
        .await
    }

    async fn on_pack_response(&mut self, response: &ResourcePackClientResponse) -> Result<()> {
        let reply = self
            .transfer
            .on_response(&self.shared.packs, response)
            .map_err(Error::Packs)?;
        match reply {
            Reply::Send { packets, extend } => {
                self.extend_spawn_deadline(extend);
                if packets.is_empty() {
                    return Ok(());
                }
                self.send(&packets).await
            }
            Reply::Join => self.join(),
            Reply::Refused => {
                self.send(&[Packet::Disconnect(Disconnect {
                    reason: i32::from(consts::DisconnectReason::ResourcePackProblem),
                    message: PACKS_REFUSED_MESSAGE.to_owned(),
                    ..Disconnect::default()
                })])
                .await?;
                Err(Error::PacksRefused)
            }
        }
    }

    /// Sends the next resource pack chunk the client asked for, and moves the spawn deadline by
    /// its allowance.
    async fn send_chunk(&mut self) -> Result<()> {
        let Some(chunk) = self.transfer.next_chunk(&self.shared.packs) else {
            return Ok(());
        };
        self.send(&[chunk]).await?;
        self.extend_spawn_deadline(CHUNK_ALLOWANCE);
        Ok(())
    }

    /// Hands the player to the tick, then the packets that arrived before the join.
    fn join(&mut self) -> Result<()> {
        let mut identity = self
            .identity
            .clone()
            .ok_or(Error::Order("join before login"))?;
        identity.client.join_time_ms = clock::unix_millis();
        let (Some(inputs), Some(outbound)) = (self.inputs.as_mut(), self.join_outbound.take())
        else {
            return Err(Error::Order("join before the login was admitted"));
        };
        inputs
            .try_send(InputKind::Join { identity, outbound })
            .map_err(Error::Queue)?;
        debug!(session = self.id.get(), addr = %self.conn.addr(), "player joining");
        // The tick counts the player from here.
        self.waiting = None;
        self.stage = Stage::Joined;
        self.deferred_bytes = 0;
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
            Err(e) => debug!(
                session = self.id.get(),
                error = &e as &(dyn std::error::Error + 'static),
                "ping not reported"
            ),
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
        if self.identity.is_some() {
            return Ok(());
        }
        let limit = self
            .shared
            .batch
            .max_batch_bytes()
            .saturating_mul(MAX_PRE_AUTH_BATCHES);
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
        while let Some(body) = self.next_body() {
            self.send_body(body).await?;
        }
        Ok(())
    }

    /// Returns the batch bodies of a packet the tick shares among sessions, batched and
    /// compressed by the first session that sends it: every joined session's encoder has the
    /// server's batch settings.
    async fn shared_batches(&mut self, packet: &SharedPacket) -> Result<SharedBatches> {
        packet
            .batches(|packet| {
                let start = self.out_bodies.len();
                self.encode([packet])?;
                Ok(self.out_bodies.drain(start..).collect())
            })
            .await
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

    /// Batches and compresses `packets` into bodies at the end of `out_bodies`.
    fn encode<'p>(&mut self, packets: impl IntoIterator<Item = &'p Packet>) -> Result<()> {
        let batched = batch::encode_bodies(
            packets,
            self.encoder.as_mut(),
            &mut self.out_payload,
            &mut self.out_bodies,
        );
        // Cleared first: `shrink_to` never goes below the length.
        self.out_payload.clear();
        self.out_payload.shrink_to(MAX_KEPT_BUFFER_BYTES);
        batched.map_err(Error::Encode)
    }

    /// Queues the tick's messages in order: runs of packets batched here, a shared packet in
    /// batches of its own made once for every session, shared batches as they are. Shared
    /// batches wait as the shared copy, copied as they go out. More than [`MAX_PENDING_BYTES`]
    /// waiting is a limit error.
    async fn queue_messages(&mut self, messages: &[Message]) -> Result<()> {
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
                [Message::SharedPacket(packet)] => {
                    let batches = self.shared_batches(packet).await?;
                    self.queue_batches(batches);
                }
                [Message::SpawnList(packet)] => {
                    let batches = self.shared_batches(packet).await?;
                    self.pending.push_back(Pending::SpawnList(batches, 0));
                }
                [Message::Shared(batches)] => self.queue_batches(Arc::clone(batches)),
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

    /// Queues shared batches after all the output before them, counted by their bytes.
    fn queue_batches(&mut self, batches: SharedBatches) {
        self.pending_bytes = self.pending_bytes.saturating_add(bytes_of(&batches));
        self.pending.push_back(Pending::Batches(batches, 0));
    }

    /// Takes the next batch body to go out, if any waits. Shared batches are copied one body at
    /// a time, so a session never holds a copy of a whole list.
    fn next_body(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.pending.pop_front()? {
                Pending::Body(body) => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(body.len());
                    return Some(body);
                }
                Pending::SpawnList(batches, next) => {
                    let Some(body) = batches.get(next).map(|body| owned_body(body)) else {
                        continue;
                    };
                    self.pending
                        .push_front(Pending::SpawnList(batches, next + 1));
                    return Some(body);
                }
                Pending::Batches(batches, next) => {
                    let Some(body) = batches.get(next).map(|body| owned_body(body)) else {
                        continue;
                    };
                    self.pending_bytes = self.pending_bytes.saturating_sub(body.len());
                    self.pending.push_front(Pending::Batches(batches, next + 1));
                    return Some(body);
                }
                // The batch before the marker went out when the next one is asked for.
                Pending::SpawnStatus => self.spawn_status_sent = true,
            }
        }
    }

    /// Hands the next waiting body, if one waits, to the connection and counts it as not yet
    /// in the connection's undelivered count.
    async fn send_waiting(&mut self) -> Result<()> {
        let Some(body) = self.next_body() else {
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
            self.extend_spawn_deadline(extend);
        }
        self.conn.send(body).await.map_err(Error::Transport)
    }

    /// Moves the spawn deadline `extend` later; one past what [`Instant`] holds stays put.
    fn extend_spawn_deadline(&mut self, extend: Duration) {
        self.spawn_at = self.spawn_at.checked_add(extend).unwrap_or(self.spawn_at);
    }
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
    /// Waiting for the tick to admit or refuse the login.
    Admission,
    /// Waiting for the resource pack responses.
    Packs,
    /// Joined: packets go to the tick.
    Joined,
}

/// Tick output waiting for delivery.
enum Pending {
    /// A batch body of the session's own packets, compressed and not encrypted yet.
    Body(Vec<u8>),
    /// The batches of a spawn window's player list ([`Message::SpawnList`]) and the index of the
    /// next one to copy: a list of every skin outgrows any bound on a session's output, so
    /// [`MAX_PENDING_BYTES`] does not count it, and a session gets two lists at most.
    SpawnList(SharedBatches, usize),
    /// Batches the tick shares among sessions and the index of the next one to copy, counted by
    /// their bytes meanwhile.
    Batches(SharedBatches, usize),
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
    /// The running tick forgot the session without a last packet (its outbound queue was full).
    Forgotten,
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
    /// The Login key is not the key the NetherNet offer proved.
    Binding,
    /// A batch the client sent is malformed.
    Batch(batch::Error),
    /// The session's own packets did not encode into batches.
    Encode(batch::Error),
    Packet(crate::protocol::Error),
    Login(login::Error),
    Crypto(crate::crypto::Error),
    Transport(transport::Error),
    Queue(queue::Error),
    Packs(packs::Error),
    /// The client refused the resource packs.
    PacksRefused,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Order(what) => write!(f, "packet out of order: {what}"),
            Self::Limit(what) => write!(f, "limit exceeded: {what}"),
            Self::Protocol(version) => write!(f, "unsupported protocol version {version}"),
            Self::Binding => f.write_str("login key differs from the nethernet offer key"),
            Self::Batch(_) => f.write_str("malformed batch"),
            Self::Encode(_) => f.write_str("server packets not encoded"),
            Self::Packet(_) => f.write_str("malformed packet"),
            Self::Login(_) => f.write_str("login rejected"),
            Self::Crypto(_) => f.write_str("encryption failed"),
            Self::Transport(_) => f.write_str("connection failed"),
            Self::Queue(_) => f.write_str("tick queue unavailable"),
            Self::Packs(_) => f.write_str("resource pack sequence failed"),
            Self::PacksRefused => f.write_str("client refused the resource packs"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Order(_)
            | Self::Limit(_)
            | Self::Protocol(_)
            | Self::Binding
            | Self::PacksRefused => None,
            Self::Batch(e) | Self::Encode(e) => Some(e),
            Self::Packet(e) => Some(e),
            Self::Login(e) => Some(e),
            Self::Crypto(e) => Some(e),
            Self::Transport(e) => Some(e),
            Self::Queue(e) => Some(e),
            Self::Packs(e) => Some(e),
        }
    }
}

/// A result with the module's [`enum@Error`].
type Result<T, E = Error> = std::result::Result<T, E>;

/// Runs one connection until it closes, then tells the tick and the accept task.
pub(super) async fn run(
    id: SessionId,
    conn: Transport,
    shared: Arc<Shared>,
    ip_waiting: Arc<Semaphore>,
) {
    let mut session = Session::new(id, conn, shared, ip_waiting);
    let end = session.serve().await;
    session.finish(end).await;
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

/// Whether `transaction` is a right click held on a block (`xtask/data/p6/facts.md`): a `ClickBlock` the
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

/// Copies a shared batch body for the session, with room for the encryption checksum.
fn owned_body(body: &[u8]) -> Vec<u8> {
    let mut owned = Vec::with_capacity(body.len() + CHECKSUM_BYTES);
    owned.extend_from_slice(body);
    owned
}

/// Waits for the next tick message, forever before the login is handed to the tick.
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
    use super::*;

    use crate::crypto::ServerKey;
    use crate::pack::tests::UUID;
    use crate::pack::{Acceptance, Offer};
    use crate::protocol::batch::MAX_PACKETS;
    use crate::tick::TICK;
    use crate::tick::queue::{
        Input, InputReceiver, InputSender, MAX_SESSION_INPUTS, Skin, input_queue,
    };

    /// Batches of 1 MiB the tick sends in the tests below.
    const BATCH_BYTES: usize = 1024 * 1024;
    /// The [`BATCH_BYTES`] batches of a spawn window's player list, more than
    /// [`MAX_PENDING_BYTES`].
    const BATCHES: u8 = 40;
    /// The sessions that use up their packet budget between two ticks.
    const FLOODERS: u64 = 80;
    /// The top of `chunks-per-tick`: the chunks sent at once in sub-chunk request mode.
    const MAX_CHUNKS_PER_TICK: i32 = 256;
    /// Ticks of output at most before a link that delivers nothing has ended its session.
    const MAX_OUTPUT_TICKS: usize = 2000;
    /// [`STEP`]s at most before a session past its deadline has ended.
    const MAX_STEPS: usize = 3000;
    /// 200 seconds, past the spawn deadline and the most the output may add to it.
    const MAX_TICKS: usize = 4000;
    /// The small shared packets queued behind output that waits.
    const SMALL_PACKETS: usize = 2000;
    /// How long the link waits between two deliveries in the paced tests.
    const STEP: Duration = Duration::from_millis(100);
    /// What the link delivers each [`STEP`] the session sends nothing: 640 KiB/s.
    const STEP_BYTES: usize = 64 * 1024;

    /// Makes what the sessions share: offline, unencrypted, no packs, inputs on `inputs`.
    fn shared(inputs: InputSender) -> Arc<Shared> {
        sharing(
            inputs,
            Offer::new(Vec::new(), Acceptance::Optional).unwrap(),
        )
        .0
    }

    /// Makes what the sessions share: offline, unencrypted, `packs` on offer, inputs on
    /// `inputs`; and the shutdown signal, which stops sessions before the join once dropped.
    fn sharing(
        inputs: InputSender,
        packs: Offer,
    ) -> (Arc<Shared>, tokio::sync::watch::Sender<bool>) {
        let (events, _) = mpsc::channel(1);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
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
            waiting: Arc::new(tokio::sync::Semaphore::new(1)),
            max_send_extension: crate::session::max_send_extension(1),
        });
        (shared, stop)
    }

    /// Makes the identity of player `id`, as its login gives it.
    fn identity(id: u64) -> Identity {
        Identity {
            name: format!("Player{id}"),
            xuid: None,
            uuid: crate::protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: -1,
            address: None,
            client: Client::default(),
        }
    }

    /// Makes the places of one IP for sessions waiting to join, as the accept task gives them.
    fn ip_waiting() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(crate::session::MAX_WAITING_PER_IP))
    }

    /// Makes session `id` as any client gets it after the login and the resource packs: joined,
    /// with its Login and Join in the tick queue.
    async fn joined(id: u64, shared: &Arc<Shared>) -> Session {
        let conn = Transport::Raknet(crate::raknet::tests::detached_connection());
        joined_on(id, shared, conn).await
    }

    /// Makes [`joined`]'s session on `conn`.
    async fn joined_on(id: u64, shared: &Arc<Shared>, conn: Transport) -> Session {
        let mut session = Session::new(SessionId::new(id), conn, Arc::clone(shared), ip_waiting());
        session.decoder = Some(batch::Decoder::new(shared.batch));
        session.identity = Some(identity(id));
        session.log_in().await.unwrap();
        session.join().unwrap();
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
            // Uncompressed batches of SetLocalPlayerAsInitialised(0), 3 bytes a packet; the tick
            // drains nothing meanwhile, as between two ticks.
            let mut flood = vec![0xff];
            flood.extend([0x02, 0x71, 0x00].repeat(MAX_PACKETS));
            let mut ended = None;
            for _ in 0..=MAX_SESSION_INPUTS.div_ceil(MAX_PACKETS) {
                if let Err(e) = flooder.on_message(flood.clone()).await {
                    ended = Some(e);
                    break;
                }
            }
            player
                .on_message(vec![0xff, 0x02, 0x71, 0x00])
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

    /// Packets after the login and before the join are held up to `MAX_DEFERRED`; one more ends
    /// the session, and the join hands the held ones to the tick and empties the queue.
    #[test]
    fn packets_before_the_join_are_held_up_to_a_bound_and_handed_over_by_the_join() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let conn = Transport::Raknet(crate::raknet::tests::detached_connection());
            let mut session =
                Session::new(SessionId::new(0), conn, Arc::clone(&shared), ip_waiting());
            session.decoder = Some(batch::Decoder::new(shared.batch));
            session.identity = Some(identity(0));
            session.log_in().await.unwrap();
            session.stage = Stage::Packs;
            let packet = || {
                Packet::SetLocalPlayerAsInitialised(packet::SetLocalPlayerAsInitialised::default())
            };
            for _ in 0..MAX_DEFERRED {
                session.on_packet(packet(), 3).await.unwrap();
            }
            assert!(matches!(
                session.on_packet(packet(), 3).await,
                Err(Error::Limit(_))
            ));
            assert_eq!(session.deferred.len(), MAX_DEFERRED);
            session.join().unwrap();
            assert!(session.deferred.is_empty());
            let mut packets = 0;
            while let Ok(input) = queue.try_recv() {
                packets += usize::from(matches!(input.kind, InputKind::Packet { .. }));
            }
            assert_eq!(packets, MAX_DEFERRED);
        });
    }

    /// However many sessions send all their packet budget allows between two ticks, another
    /// client's packet still reaches the tick: each fills only its own share of the queue.
    #[test]
    fn many_sessions_at_their_budget_leave_room_for_another() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(u32::try_from(FLOODERS).unwrap());
            let shared = shared(inputs);
            let mut player = joined(FLOODERS, &shared).await;
            let mut flooders = Vec::new();
            for id in 0..FLOODERS {
                flooders.push(joined(id, &shared).await);
            }
            // Uncompressed batches of SetLocalPlayerAsInitialised(0), 3 bytes a packet: the
            // whole burst.
            let batch = |packets: usize| {
                let mut b = vec![0xff];
                b.extend([0x02, 0x71, 0x00].repeat(packets));
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
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            let chunks = || {
                (0..MAX_CHUNKS_PER_TICK)
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
            let requests_sent =
                std::iter::repeat_n(&request, usize::try_from(MAX_CHUNKS_PER_TICK).unwrap());
            let mut requests = vec![0xff];
            batch::encode_packets(requests_sent, &mut Vec::new(), |payload| {
                requests.extend_from_slice(payload);
                Ok(())
            })
            .unwrap();
            // A second of ticks; the tick takes every input between two.
            for _ in 0..20 {
                player
                    .queue_messages(&chunks())
                    .await
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
            let packet = Packet::InventoryTransaction(Box::new(packet::InventoryTransaction {
                actions: vec![
                    crate::protocol::InventoryAction::default();
                    crate::protocol::MAX_LIST_LEN
                ],
                ..packet::InventoryTransaction::default()
            }));
            let mut body = Vec::new();
            packet.encode(&mut body).unwrap();
            let mut budget = usize::MAX;
            Packet::decode(packet.id(), &body, &mut budget).unwrap();
            let allocated = usize::MAX - budget;
            let mut message = vec![0xff];
            batch::encode_packets([&packet], &mut Vec::new(), |p| {
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

    /// Packets that decode into far more than their bytes are charged to the input byte budget
    /// by what decoding allocated, so a client cannot hold the tick's memory with few bytes.
    #[test]
    fn packets_are_charged_to_the_byte_budget_by_what_decoding_allocated() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            let packet = Packet::InventoryTransaction(Box::new(packet::InventoryTransaction {
                actions: vec![
                    crate::protocol::InventoryAction::default();
                    crate::protocol::MAX_LIST_LEN
                ],
                ..packet::InventoryTransaction::default()
            }));
            let mut body = Vec::new();
            packet.encode(&mut body).unwrap();
            let mut budget = usize::MAX;
            Packet::decode(packet.id(), &body, &mut budget).unwrap();
            let allocated = usize::MAX - budget;
            let mut message = vec![0xff];
            batch::encode_packets([&packet], &mut Vec::new(), |p| {
                message.extend_from_slice(p);
                Ok(())
            })
            .unwrap();
            // No refill, so a slow machine cannot earn the budget back while the test runs.
            player.budget_rate.per_second = 0;
            let messages = player.budget_rate.capacity.div_ceil(allocated) + 1;
            assert!(
                messages < PACKET_RATE.capacity,
                "the packet budget must not end it first"
            );
            let mut ended = None;
            for _ in 0..messages {
                if let Err(e) = player.on_message(message.clone()).await {
                    ended = Some(e);
                    break;
                }
            }
            assert!(
                matches!(ended, Some(Error::Limit("input bytes per second"))),
                "{ended:?}"
            );
        });
    }

    /// The packets held before the join are bounded in bytes too, not only in number.
    #[test]
    fn packets_before_the_join_are_held_up_to_a_byte_bound() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let shared = shared(inputs);
            let conn = Transport::Raknet(crate::raknet::tests::detached_connection());
            let mut session =
                Session::new(SessionId::new(0), conn, Arc::clone(&shared), ip_waiting());
            session.identity = Some(identity(0));
            session.stage = Stage::Packs;
            let packet =
                Packet::SetLocalPlayerAsInitialised(packet::SetLocalPlayerAsInitialised::default());
            let result = session.on_packet(packet, 16 * 1024 * 1024).await;
            assert!(matches!(result, Err(Error::Limit(_))), "{result:?}");
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
                let packet = Packet::InventoryTransaction(Box::new(packet::InventoryTransaction {
                    transaction_data: InventoryTransactionData::UseItem(Box::new(
                        crate::protocol::UseItemTransactionData {
                            action_type: u32::from(consts::UseItemAction::ClickBlock),
                            trigger_type: u32::from(trigger),
                            ..crate::protocol::UseItemTransactionData::default()
                        },
                    )),
                    ..packet::InventoryTransaction::default()
                }));
                let mut message = vec![0xff];
                batch::encode_packets([&packet], &mut Vec::new(), |p| {
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
            player.started = player.started.checked_sub(TICK).unwrap();
            player.on_message(held.clone()).await.unwrap();
            assert_eq!(forwarded(), 1, "a copy a tick later");
            let press = click(consts::TriggerType::PlayerInput);
            player.on_message(press.clone()).await.unwrap();
            player.on_message(press).await.unwrap();
            player.on_message(held).await.unwrap();
            assert_eq!(forwarded(), 3, "presses, and the held click after them");
        });
    }

    /// A packet only servers send never reaches the tick: after the spawn it is dropped as a
    /// packet that does not decode, and before the spawn it ends the session.
    #[test]
    fn a_packet_only_servers_send_never_reaches_the_tick() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            // An uncompressed batch of SetTime(0).
            let set_time = vec![0xff, 0x02, 0x0a, 0x00];
            player.spawned = true;
            player
                .on_message(set_time.clone())
                .await
                .expect("the packet is dropped");
            let mut forwarded = 0;
            while let Ok(input) = queue.try_recv() {
                forwarded += usize::from(matches!(input.kind, InputKind::Packet { .. }));
            }
            assert_eq!(forwarded, 0);
            player.spawned = false;
            let ended = player.on_message(set_time).await;
            assert!(matches!(ended, Err(Error::Packet(_))), "{ended:?}");
        });
    }

    /// A split-screen sub-client's packet never reaches the tick, where it would act as the main
    /// player's: after the join it is dropped, before the join it ends the session.
    #[test]
    fn a_sub_client_packet_never_reaches_the_tick() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let mut player = joined(0, &shared).await;
            // An uncompressed batch of SetLocalPlayerAsInitialised(0) from sub-client 1: the
            // header 0x71 | 1 << 10 as a varuint32.
            let from_sub_client = vec![0xff, 0x03, 0xf1, 0x08, 0x00];
            player
                .on_message(from_sub_client.clone())
                .await
                .expect("the packet is dropped");
            let mut forwarded = 0;
            while let Ok(input) = queue.try_recv() {
                forwarded += usize::from(matches!(input.kind, InputKind::Packet { .. }));
            }
            assert_eq!(forwarded, 0);
            player.stage = Stage::Packs;
            let ended = player.on_message(from_sub_client).await;
            assert!(matches!(ended, Err(Error::Order(_))), "{ended:?}");
        });
    }

    /// Makes the connection request of an offline Login whose token and client data `key`
    /// signs.
    fn login_request(key: &ServerKey) -> Vec<u8> {
        let token = crate::crypto::sign_jwt(
            key,
            &serde_json::json!({"cpk": key.public_key_base64(), "xname": "Steve"}),
        )
        .unwrap();
        let chain = serde_json::json!({
            "AuthenticationType": 0,
            "Certificate": serde_json::json!({"chain": []}).to_string(),
            "Token": token,
        })
        .to_string();
        let patch = br#"{"geometry":{"default":"geometry.humanoid"}}"#;
        let client_data = crate::crypto::sign_jwt(
            key,
            &serde_json::json!({
                "DeviceOS": 7,
                "GameVersion": "1.26.60",
                "LanguageCode": "en_US",
                "ServerAddress": "127.0.0.1:19132",
                "SkinData": crate::crypto::STD.encode(&[0; 8]),
                "SkinImageHeight": 1,
                "SkinImageWidth": 2,
                "SkinId": "skin",
                "SkinResourcePatch": crate::crypto::STD.encode(patch),
            }),
        )
        .unwrap();
        let mut request = Vec::new();
        for part in [chain, client_data] {
            request.extend_from_slice(&i32::try_from(part.len()).unwrap().to_le_bytes());
            request.extend_from_slice(part.as_bytes());
        }
        request
    }

    /// On an encrypted RakNet connection, the packets after the Login in its plain batch end the
    /// session: a client that replays another player's Login cannot finish the handshake
    /// without the key it proves.
    #[test]
    fn packets_after_the_login_that_turns_encryption_on_end_the_session() {
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let mut shared = shared(inputs);
            Arc::get_mut(&mut shared)
                .expect("the test holds the only reference")
                .encryption = true;
            let (conn, _far) = crate::raknet::tests::linked_connection();
            let mut session = Session::new(
                SessionId::new(0),
                Transport::Raknet(conn),
                Arc::clone(&shared),
                ip_waiting(),
            );
            session.encoder = Some(batch::Encoder::new(shared.batch));
            session.decoder = Some(batch::Decoder::new(shared.batch));
            session.stage = Stage::Login;
            let key = ServerKey::generate().unwrap();
            let packets = [
                Packet::Login(packet::Login {
                    client_protocol: PROTOCOL_VERSION,
                    connection_request: login_request(&key),
                }),
                Packet::ClientToServerHandshake(packet::ClientToServerHandshake::default()),
            ];
            let mut message = vec![0xff];
            batch::encode_packets(&packets, &mut Vec::new(), |payload| {
                message.extend_from_slice(payload);
                Ok(())
            })
            .unwrap();
            let ended = session.on_message(message).await;
            assert!(matches!(ended, Err(Error::Order(_))), "{ended:?}");
            assert_eq!(session.stage, Stage::Handshake);
            assert!(queue.try_recv().is_err(), "nothing reached the tick");
        });
    }

    /// Sessions of one IP hold at most their share of the places waiting to join: a login past it
    /// is turned away as from a full server, and a login from another IP still gets a place.
    #[test]
    fn one_ip_cannot_fill_the_places_waiting_to_join() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(3);
            let mut shared = shared(inputs);
            Arc::get_mut(&mut shared)
                .expect("the test holds the only reference")
                .waiting = Arc::new(Semaphore::new(3));
            let key = ServerKey::generate().unwrap();
            let one_ip = Arc::new(Semaphore::new(1));
            let mut logins = Vec::new();
            let mut sessions = Vec::new();
            for (id, ip) in (0..).zip([Arc::clone(&one_ip), one_ip, ip_waiting()]) {
                let (conn, far) = crate::raknet::tests::linked_connection();
                let mut session = Session::new(
                    SessionId::new(id),
                    Transport::Raknet(conn),
                    Arc::clone(&shared),
                    ip,
                );
                session.encoder = Some(batch::Encoder::new(shared.batch));
                let login = packet::Login {
                    client_protocol: PROTOCOL_VERSION,
                    connection_request: login_request(&key),
                };
                logins.push(session.on_login(login).await.is_ok());
                sessions.push((session, far));
            }
            assert_eq!(logins, [true, false, true]);
        });
    }

    /// The session offers the resource packs only once the tick admitted the login: a login the
    /// tick refuses gets its last packet and nothing of the packs, and an admitted one gets
    /// `LoginSuccess` and the offer. A pack answer before the tick's answer breaks the sequence.
    #[test]
    fn the_packs_are_offered_only_after_the_tick_admits_the_login() {
        for admitted in [false, true] {
            let sent = crate::raknet::tests::run_paused(async {
                let (inputs, mut queue) = input_queue(1);
                let pack = crate::pack::tests::pack(UUID, "1.0.0", 1024);
                let (shared, _stop) = sharing(
                    inputs,
                    Offer::new(vec![pack], Acceptance::Required).unwrap(),
                );
                let (conn, mut far) = crate::raknet::tests::linked_connection();
                let mut session = Session::new(
                    SessionId::new(0),
                    Transport::Raknet(conn),
                    Arc::clone(&shared),
                    ip_waiting(),
                );
                session.encoder = Some(batch::Encoder::new(shared.batch));
                session.identity = Some(identity(0));
                session.log_in().await.unwrap();
                let early = Packet::ResourcePackClientResponse(ResourcePackClientResponse {
                    response: u32::from(consts::PackResponse::Completed),
                    ..ResourcePackClientResponse::default()
                });
                assert!(matches!(
                    session.on_packet(early, 3).await,
                    Err(Error::Order(_))
                ));
                let Ok(Input {
                    kind: InputKind::Login { outbound, .. },
                    ..
                }) = queue.try_recv()
                else {
                    panic!("the login is queued");
                };
                if admitted {
                    outbound.send(Outbound::Admitted).await.unwrap();
                } else {
                    let refusal = Packet::Disconnect(Disconnect {
                        reason: i32::from(consts::DisconnectReason::NotAllowed),
                        ..Disconnect::default()
                    });
                    outbound.send(Outbound::Close(refusal)).await.unwrap();
                }
                drop(outbound);
                let end = tokio::time::timeout(Duration::from_secs(1), session.serve()).await;
                assert_eq!(matches!(end, Ok(End::Kicked)), !admitted, "{end:?}");
                drop(session);
                let mut decoder = batch::Decoder::new(shared.batch);
                let mut sent = Vec::new();
                while let Some(message) = far.sent().await {
                    let mut payload = Vec::new();
                    decoder.decode(&message, &mut payload).unwrap();
                    let mut packets = Vec::new();
                    batch::decode_packets(&payload, &shared.batch, &mut packets).unwrap();
                    sent.extend(packets.into_iter().map(|(_, packet)| packet));
                }
                sent
            });
            if admitted {
                assert!(
                    matches!(
                        sent.as_slice(),
                        [Packet::PlayStatus(s), Packet::ResourcePacksInfo(_)]
                            if s.status == i32::from(consts::PlayStatus::LoginSuccess)
                    ),
                    "{sent:?}"
                );
            } else {
                assert!(
                    matches!(sent.as_slice(), [Packet::Disconnect(d)] if d.message.is_empty()),
                    "{sent:?}"
                );
            }
        }
    }

    /// Asking for packs moves the spawn deadline by the base only and each chunk sent by its
    /// allowance, so a client that asks and never downloads gains little.
    #[test]
    fn the_spawn_deadline_moves_by_the_chunks_sent_not_the_chunks_asked_for() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let big = crate::pack::tests::pack(UUID, "1.0.0", 7 * 128 * 1024);
            let (shared, _stop) =
                sharing(inputs, Offer::new(vec![big], Acceptance::Required).unwrap());
            let (conn, _far) = crate::raknet::tests::linked_connection();
            let mut session = Session::new(
                SessionId::new(0),
                Transport::Raknet(conn),
                Arc::clone(&shared),
                ip_waiting(),
            );
            session.identity = Some(identity(0));
            session.stage = Stage::Packs;
            let id = shared.packs.id(0).to_owned();
            let send_packs = ResourcePackClientResponse {
                response: u32::from(consts::PackResponse::SendPacks),
                packs_to_download: vec![id.clone()],
            };
            session.on_pack_response(&send_packs).await.unwrap();
            assert_eq!(
                session.spawn_at,
                session.started + SPAWN_DEADLINE + packs::DOWNLOAD_BASE
            );
            let request = packet::ResourcePackChunkRequest {
                uuid: id,
                chunk_index: 0,
            };
            session
                .transfer
                .on_chunk_request(&shared.packs, &request)
                .unwrap();
            let asked = session.spawn_at;
            session.send_chunk().await.unwrap();
            assert_eq!(session.spawn_at, asked + CHUNK_ALLOWANCE);
        });
    }

    /// A client that asks for every chunk at once on a link that delivers nothing gets one
    /// chunk, not one every [`CHUNK_INTERVAL`] until the transport's outbound cap, and the rest
    /// as the connection delivers them.
    #[test]
    fn pack_chunks_wait_for_the_connection_to_deliver() {
        crate::raknet::tests::run_paused(async {
            let (inputs, _queue) = input_queue(1);
            let big = crate::pack::tests::pack(UUID, "1.0.0", 7 * 128 * 1024);
            let (shared, _stop) =
                sharing(inputs, Offer::new(vec![big], Acceptance::Required).unwrap());
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = Session::new(
                SessionId::new(0),
                Transport::Raknet(conn),
                Arc::clone(&shared),
                ip_waiting(),
            );
            session.identity = Some(identity(0));
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
                .size_bytes()
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
            select! {
                end = session.serve() => panic!("the session ended: {end:?}"),
                () = client => {}
            }
        });
    }

    /// Takes the Login and the Join next in `queue` and returns where the tick sends their
    /// session.
    fn joined_outbound(queue: &mut InputReceiver) -> mpsc::Sender<Outbound> {
        let Ok(Input {
            kind: InputKind::Login { .. },
            ..
        }) = queue.try_recv()
        else {
            panic!("the login is queued");
        };
        let Ok(Input {
            kind: InputKind::Join { outbound, .. },
            ..
        }) = queue.try_recv()
        else {
            panic!("the join is queued");
        };
        outbound
    }

    /// A client that refuses the resource packs ends its session with that cause, not an
    /// ordering failure.
    #[test]
    fn a_pack_refusal_ends_the_session_as_a_refusal() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, _far) = crate::raknet::tests::linked_connection();
            let mut session = Session::new(
                SessionId::new(0),
                Transport::Raknet(conn),
                Arc::clone(&shared),
                ip_waiting(),
            );
            session.encoder = Some(batch::Encoder::new(shared.batch));
            session.identity = Some(identity(0));
            session.log_in().await.unwrap();
            session.offer_packs().await.unwrap();
            let refusal = Packet::ResourcePackClientResponse(ResourcePackClientResponse {
                response: u32::from(consts::PackResponse::Refused),
                ..ResourcePackClientResponse::default()
            });
            let ended = session.on_packet(refusal, 3).await;
            assert_eq!(
                ended.map_err(|e| e.to_string()),
                Err("client refused the resource packs".to_owned())
            );
        });
    }

    /// A joined session the running tick forgets (its outbound queue was full) closes without
    /// the shutdown message, which only a session whose tick stopped gets.
    #[test]
    fn only_a_stopped_tick_gives_a_joined_session_the_shutdown_message() {
        for tick_stops in [false, true] {
            let sent = crate::raknet::tests::run(async {
                let (inputs, mut queue) = input_queue(1);
                let shared = shared(inputs);
                let (conn, mut far) = crate::raknet::tests::linked_connection();
                let mut session = Session::new(
                    SessionId::new(0),
                    Transport::Raknet(conn),
                    Arc::clone(&shared),
                    ip_waiting(),
                );
                session.encoder = Some(batch::Encoder::new(shared.batch));
                session.decoder = Some(batch::Decoder::new(shared.batch));
                session.identity = Some(identity(0));
                session.log_in().await.unwrap();
                session.join().unwrap();
                let outbound = joined_outbound(&mut queue);
                let queue = (!tick_stops).then_some(queue);
                drop(outbound);
                let end = session.serve().await;
                session.finish(end).await;
                drop(queue);
                let mut decoder = batch::Decoder::new(shared.batch);
                let mut sent = Vec::new();
                while let Some(message) = far.sent().await {
                    let mut payload = Vec::new();
                    decoder.decode(&message, &mut payload).unwrap();
                    let mut packets = Vec::new();
                    batch::decode_packets(&payload, &shared.batch, &mut packets).unwrap();
                    sent.extend(packets.into_iter().map(|(_, packet)| packet));
                }
                sent
            });
            let shutdown = sent.iter().any(|packet| {
                matches!(packet, Packet::Disconnect(d)
                    if d.reason == i32::from(consts::DisconnectReason::Shutdown))
            });
            assert_eq!(shutdown, tick_stops, "{sent:?}");
        }
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
            let waited = crate::raknet::tests::run_paused(async {
                let (inputs, mut queue) = input_queue(1);
                let shared = shared(inputs);
                let (conn, _far) = crate::raknet::tests::linked_connection();
                let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
                let outbound = joined_outbound(&mut queue);
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

    /// Makes `count` packets of a spawn window's player list, standing in for entries of large
    /// skins (the session never looks inside): each [`BATCH_BYTES`] of data filled with its
    /// index.
    fn numbered_list(count: u8) -> impl Iterator<Item = Message> {
        (0..count).map(|i| {
            Message::SpawnList(Arc::new(SharedPacket::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![i; BATCH_BYTES],
                    ..packet::ResourcePackChunkData::default()
                },
            ))))
        })
    }

    /// A spawn window's player list larger than the transports' 32 MiB caps and
    /// [`MAX_PENDING_BYTES`] (one of large skins) goes out before the spawn as a slow link
    /// delivers it, while the tick sends a message every tick for longer than its queue of 1,024
    /// messages lasts: the tick never finds the queue full, the connection never holds more than
    /// the send limit before another batch, and every batch arrives, in order.
    #[test]
    fn a_spawn_player_list_over_the_transport_caps_goes_out_as_the_link_delivers() {
        assert!(usize::from(BATCHES) * BATCH_BYTES > MAX_PENDING_BYTES);
        crate::raknet::tests::run_paused(async {
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
                for _ in 0..MAX_TICKS {
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
                            assert!(
                                held <= MAX_OUTPUT_UNDELIVERED_BYTES,
                                "{held} bytes held before another batch"
                            );
                            far.undelivered.send_modify(|bytes| *bytes += batch.len());
                        }
                        Ok(None) => panic!("the session dropped the connection"),
                        Err(_) => far
                            .undelivered
                            .send_modify(|bytes| *bytes = bytes.saturating_sub(STEP_BYTES)),
                    }
                }
            };
            select! {
                end = session.serve() => panic!("the session ended: {end:?}"),
                () = tick => panic!("the batches took more than {MAX_TICKS} ticks"),
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
        crate::raknet::tests::run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            session.spawned = true;
            let outbound = joined_outbound(&mut queue);
            let tick = async {
                let mut ticks = tokio::time::interval(TICK);
                for _ in 0..MAX_OUTPUT_TICKS {
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
                    assert!(
                        held <= MAX_OUTPUT_UNDELIVERED_BYTES,
                        "{held} bytes held before another batch"
                    );
                    far.undelivered.send_modify(|bytes| *bytes += batch.len());
                }
            };
            let end = select! {
                end = session.serve() => end,
                () = tick => panic!("the session took {MAX_OUTPUT_TICKS} ticks of output"),
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

    /// Makes the tick's spawn status, which the client needs before it can spawn.
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
        // 128 KiB each step, 1.25 MiB/s: 20 seconds of allowance a second if it counted.
        let waited = crate::raknet::tests::run_paused(async {
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
                for _ in 0..MAX_STEPS {
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
            let end = select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MAX_STEPS} steps"),
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
        // 128 KiB each step, 1.25 MiB/s: 20 seconds of allowance a second.
        let (waited, most) = crate::raknet::tests::run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            let broadcast = Arc::new(SharedPacket::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![0; 128 * 1024],
                    ..packet::ResourcePackChunkData::default()
                },
            )));
            let start = tokio::time::Instant::now();
            let tick = async {
                let mut steps = tokio::time::interval(STEP);
                for _ in 0..MAX_STEPS {
                    steps.tick().await;
                    let message = Message::SharedPacket(Arc::clone(&broadcast));
                    outbound.send(Outbound::Send(vec![message])).await.unwrap();
                }
            };
            // The link delivers everything at once.
            let link = async { while far.sent().await.is_some() {} };
            let end = select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MAX_STEPS} steps"),
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
        crate::raknet::tests::run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            let skin = Arc::new(SharedPacket::new(Packet::ResourcePackChunkData(
                packet::ResourcePackChunkData {
                    data: vec![0; BATCH_BYTES],
                    ..packet::ResourcePackChunkData::default()
                },
            )));
            let tick = async {
                let mut ticks = tokio::time::interval(TICK);
                for _ in 0..MAX_TICKS {
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
            let end = select! {
                end = session.serve() => end,
                () = tick => panic!("the session went on for {MAX_TICKS} ticks"),
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
        crate::raknet::tests::run_paused(async {
            let (inputs, mut queue) = input_queue(1);
            let shared = shared(inputs);
            let (conn, mut far) = crate::raknet::tests::linked_connection();
            let mut session = joined_on(0, &shared, Transport::Raknet(conn)).await;
            let outbound = joined_outbound(&mut queue);
            far.undelivered
                .send_replace(MAX_OUTPUT_UNDELIVERED_BYTES + 1);
            let soft_enum = Arc::new(SharedPacket::new(Packet::UpdateSoftEnum(
                packet::UpdateSoftEnum {
                    enum_type: "maps".to_owned(),
                    options: vec!["lobby".to_owned()],
                    ..packet::UpdateSoftEnum::default()
                },
            )));
            let small = (0..SMALL_PACKETS)
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
            let end = select! {
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

    /// Shared batches are copied one body at a time as they go out, so a session never holds a
    /// copy of a whole player list.
    #[test]
    fn shared_batches_are_copied_one_body_at_a_time() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let shared = shared(inputs);
            let mut session = joined(0, &shared).await;
            let batches: SharedBatches = (0..3).map(|n| vec![n; BATCH_BYTES]).collect();
            session
                .queue_messages(&[Message::Shared(Arc::clone(&batches))])
                .await
                .unwrap();
            assert_eq!(session.next_body().as_deref(), Some(&batches[0][..]));
            assert!(session.out_bodies.is_empty());
            assert!(!session.pending.is_empty());
            let rest: Vec<Vec<u8>> = std::iter::from_fn(|| session.next_body()).collect();
            assert_eq!(rest, batches[1..]);
            assert_eq!(session.pending_bytes, 0);
        });
    }

    /// A packet the tick shares among sessions is batched and compressed once: every session
    /// waits with the same bodies, which are those a session batches for the packet itself.
    #[test]
    fn a_shared_packet_is_compressed_once_for_every_session() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(2);
            let shared = shared(inputs);
            let skin = Packet::ResourcePackChunkData(packet::ResourcePackChunkData {
                data: (0..=u8::MAX).cycle().take(BATCH_BYTES).collect(),
                ..packet::ResourcePackChunkData::default()
            });
            let message = Message::SharedPacket(Arc::new(SharedPacket::new(skin.clone())));
            let mut waiting = Vec::new();
            for id in 0..2 {
                let mut session = joined(id, &shared).await;
                session.encoder = Some(batch::Encoder::new(shared.batch));
                session
                    .queue_messages(std::slice::from_ref(&message))
                    .await
                    .unwrap();
                let Some(Pending::Batches(batches, _)) = session.pending.pop_front() else {
                    panic!("session {id} waits with shared batches");
                };
                session.queue_packets([&skin]).unwrap();
                let own: Vec<Vec<u8>> = std::iter::from_fn(|| session.next_body()).collect();
                assert_eq!(*batches, *own, "session {id}");
                waiting.push(batches);
            }
            assert!(Arc::ptr_eq(&waiting[0], &waiting[1]));
        });
    }

    /// A send hook's copy of a shared packet goes out in batches of the changed copy, and the
    /// other sessions keep the original's.
    #[test]
    fn a_changed_copy_of_a_shared_packet_is_compressed_again() {
        crate::raknet::tests::run(async {
            let (inputs, _queue) = input_queue(1);
            let shared = shared(inputs);
            let mut session = joined(0, &shared).await;
            let time = |time| Packet::SetTime(packet::SetTime { time });
            let original = Arc::new(SharedPacket::new(time(1)));
            let first = session.shared_batches(&original).await.unwrap();
            let mut copy = Arc::clone(&original);
            *Arc::make_mut(&mut copy).get_mut() = time(2);
            let changed = session.shared_batches(&copy).await.unwrap();
            session.queue_packets([&time(2)]).unwrap();
            let own: Vec<Vec<u8>> = std::iter::from_fn(|| session.next_body()).collect();
            assert_eq!(*changed, *own);
            let kept = session.shared_batches(&original).await.unwrap();
            assert!(Arc::ptr_eq(&kept, &first));
        });
    }
}
