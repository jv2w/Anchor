//! One connection's session: the login sequence of PLAN.md 10.2, then the relay between the
//! connection and the tick.
//!
//! Inbound, a message is decrypted (RakNet after the handshake), decompressed and split into
//! packets. Before the player joins, packets outside the login sequence wait in a short queue
//! and reach the tick after the join, in order (gophertunnel defers them the same way, without
//! a bound). Outbound, the tick's packets are batched, compressed and encrypted here, off the
//! tick. NetherNet runs the same handshake but never encrypts (DTLS protects it, as on BDS), and
//! its Login must carry the key its offer proved (PLAN.md 2.1, 10.3).

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::transport::{self, Transport};
use super::{Event, Shared, unix_now};
use crate::auth::Keys;
use crate::clock;
use crate::crypto::encryption::{Decryptor, Encryptor, Handshake};
use crate::login::{self, Login, Mode};
use crate::protocol::batch::{self, Header};
use crate::protocol::packet::{
    self, Disconnect, NetworkSettings, PROTOCOL_VERSION, Packet, PlayStatus,
    RequestNetworkSettings, ResourcePackClientResponse, ResourcePackStack, ResourcePacksInfo,
    ServerToClientHandshake,
};
use crate::tick::queue::{self, Identity, InputKind, Message, Outbound, SessionId, SessionInputs};

/// From connection to verified login (PLAN.md 10.2).
const LOGIN_DEADLINE: Duration = Duration::from_secs(10);
/// From connection to `SetLocalPlayerAsInitialised` (PLAN.md 10.2).
const SPAWN_DEADLINE: Duration = Duration::from_secs(60);
/// The largest message before `NetworkSettings` (PLAN.md 3).
const FIRST_MESSAGE_BYTES: usize = 1024;
/// Packets outside the login sequence held until the join. The official client's order is not
/// observable; 32 is far above the few packets (cache status, violation warnings) clients send.
const MAX_DEFERRED: usize = 32;
/// Reassembled plus decompressed bytes before login verification, in batch limits (PLAN.md 3).
const PRE_AUTH_BATCHES: usize = 8;
/// The input budget refills this fraction of `max-batch-bytes` per second (PLAN.md 10.2).
const BUDGET_REFILL_DIVISOR: usize = 4;
/// The shield's item network ID, which the item registry Anchor sends names.
const SHIELD_ID: i32 = crate::item::SHIELD_RUNTIME_ID;
/// `PlayStatus` values (gophertunnel `PlayStatus*`).
const LOGIN_SUCCESS: i32 = 0;
const LOGIN_FAILED_CLIENT: i32 = 1;
const LOGIN_FAILED_SERVER: i32 = 2;
/// `ResourcePackClientResponse` values (gophertunnel `PackResponse*`).
const PACKS_REFUSED: u32 = 0;
const PACKS_SEND: u32 = 1;
const PACKS_ALL_DOWNLOADED: u32 = 2;
const PACKS_COMPLETED: u32 = 3;
/// `Disconnect` reason for a failed login (gophertunnel `DisconnectReasonNotAuthenticated`).
const REASON_NOT_AUTHENTICATED: i32 = 46;

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
    /// Waiting for `ClientToServerHandshake`, the first encrypted packet.
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
    encoder: Option<batch::Encoder>,
    decoder: Option<batch::Decoder>,
    encryptor: Option<Encryptor>,
    decryptor: Option<Decryptor>,
    /// Decompressed inbound payload, reused.
    payload: Vec<u8>,
    /// Outbound payload, reused.
    out_payload: Vec<u8>,
    budget: Budget,
    pre_auth_bytes: usize,
    identity: Option<Identity>,
    deferred: VecDeque<Packet>,
    inputs: Option<SessionInputs>,
    outbound: Option<mpsc::Receiver<Outbound>>,
    spawned: bool,
    logged_in: bool,
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
            Self::Order(_) | Self::Limit(_) | Self::Protocol(_) | Self::Binding => None,
        }
    }
}

impl Session {
    fn new(id: SessionId, conn: Transport, shared: Arc<Shared>) -> Self {
        let max = shared.batch.max_batch_bytes();
        let started = clock::now();
        Self {
            id,
            conn,
            stage: Stage::NetworkSettings,
            started,
            encoder: None,
            decoder: None,
            encryptor: None,
            decryptor: None,
            payload: Vec::new(),
            out_payload: Vec::new(),
            budget: Budget::new(max, max / BUDGET_REFILL_DIVISOR, started),
            pre_auth_bytes: 0,
            identity: None,
            deferred: VecDeque::new(),
            inputs: None,
            outbound: None,
            spawned: false,
            logged_in: false,
            shared,
        }
    }

    async fn serve(&mut self) -> End {
        let login_deadline = deadline(self.started + LOGIN_DEADLINE);
        let spawn_deadline = deadline(self.started + SPAWN_DEADLINE);
        tokio::pin!(login_deadline, spawn_deadline);
        let mut shutdown = self.shared.shutdown.clone();
        if *shutdown.borrow_and_update() {
            return End::Shutdown;
        }
        loop {
            tokio::select! {
                message = self.conn.recv() => {
                    let Some(message) = message else { return End::Closed };
                    if let Err(e) = self.on_message(message).await {
                        return End::Failed(e);
                    }
                }
                outbound = next_outbound(self.outbound.as_mut()) => match outbound {
                    Some(Outbound::Send(messages)) => {
                        if let Err(e) = self.send_messages(messages).await {
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
                // Joined sessions end when the stopping tick drops their outbound queue.
                _ = shutdown.changed(), if self.inputs.is_none() => return End::Shutdown,
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
            info!(session = self.id.get(), name = %identity.name, "player left");
        }
        if let Some(inputs) = self.inputs.take() {
            inputs.leave().await;
        }
        if self.logged_in
            && self
                .shared
                .events
                .send(Event::Ended { logged_in: true })
                .await
                .is_err()
        {
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
        if client_protocol != PROTOCOL_VERSION {
            let status = if client_protocol > PROTOCOL_VERSION {
                LOGIN_FAILED_SERVER
            } else {
                LOGIN_FAILED_CLIENT
            };
            self.send(&[Packet::PlayStatus(PlayStatus { status })])
                .await?;
            return Err(Error::Protocol(client_protocol));
        }
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
        self.budget
            .take(payload.len(), clock::now())
            .ok_or(Error::Limit("input bytes per second"))?;
        // The whole batch shares one allocation budget, as in `batch::decode_packets`.
        let mut budget = self.shared.batch.max_batch_bytes().saturating_mul(2);
        for (header, bytes) in batch::split(payload).map_err(Error::Batch)? {
            match Packet::decode(header.id, bytes, &mut budget, SHIELD_ID) {
                Ok(packet) => self.on_packet(packet).await?,
                // After the spawn one bad packet is dropped (PLAN.md 10.2); the batch length
                // prefixes keep the other packets intact.
                Err(e) if self.spawned => {
                    warn!(session = self.id.get(), id = header.id, error = %e, "packet dropped");
                }
                Err(e) => return Err(Error::Packet(e)),
            }
        }
        Ok(())
    }

    async fn on_packet(&mut self, packet: Packet) -> Result<()> {
        match (self.stage, packet) {
            (Stage::Login, Packet::Login(login)) => self.on_login(login).await,
            (Stage::Login, _) => Err(Error::Order("first packet is not Login")),
            (Stage::Handshake, Packet::ClientToServerHandshake(_)) => {
                self.stage = Stage::Packs;
                self.send(&[
                    Packet::PlayStatus(PlayStatus {
                        status: LOGIN_SUCCESS,
                    }),
                    Packet::ResourcePacksInfo(resource_packs_info()),
                ])
                .await
            }
            (Stage::Packs, Packet::ResourcePackClientResponse(response)) => {
                self.on_pack_response(response).await
            }
            // The blob cache is not used until chunks are (M6); the answer needs no reply.
            (Stage::Handshake | Stage::Packs, Packet::ClientCacheStatus(_)) => Ok(()),
            (
                _,
                Packet::RequestNetworkSettings(_)
                | Packet::Login(_)
                | Packet::ClientToServerHandshake(_)
                | Packet::ResourcePackClientResponse(_),
            ) => Err(Error::Order("login sequence packet repeated or early")),
            (Stage::Joined, packet) => {
                if matches!(packet, Packet::SetLocalPlayerAsInitialised(_)) {
                    self.spawned = true;
                }
                self.forward(packet)
            }
            (_, packet) => {
                if self.deferred.len() >= MAX_DEFERRED {
                    return Err(Error::Limit("packets before the join"));
                }
                self.deferred.push_back(packet);
                Ok(())
            }
        }
    }

    async fn on_login(&mut self, login: packet::Login) -> Result<()> {
        if login.client_protocol != PROTOCOL_VERSION {
            let status = if login.client_protocol > PROTOCOL_VERSION {
                LOGIN_FAILED_SERVER
            } else {
                LOGIN_FAILED_CLIENT
            };
            self.send(&[Packet::PlayStatus(PlayStatus { status })])
                .await?;
            return Err(Error::Protocol(login.client_protocol));
        }
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
                    reason: REASON_NOT_AUTHENTICATED,
                    message: "Login failed".to_owned(),
                    ..Disconnect::default()
                })])
                .await?;
                return Err(e);
            }
        };
        let Login {
            identity,
            client_key,
            client_data: _,
        } = verified;
        self.identity = Some(Identity {
            name: identity.name,
            xuid: identity.xuid,
            uuid: identity.uuid,
        });
        self.logged_in = true;
        self.conn.logged_in();
        if self.shared.events.send(Event::LoggedIn).await.is_err() {
            debug!("accept task gone during login");
        }
        let handshake =
            Handshake::new(&self.shared.server_key, &client_key).map_err(Error::Crypto)?;
        // The handshake goes out before encryption starts; everything after it is encrypted.
        // BDS sends it on NetherNet too but never encrypts there (P6, PLAN.md 10.3).
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

    /// Verifies the connection request: online against the authorization keys (one JWKS
    /// refresh for an unknown key ID, within the login deadline), offline without them.
    async fn verify(&self, request: &[u8]) -> Result<Login> {
        let Some(auth) = &self.shared.auth else {
            return login::verify(request, Mode::Offline, unix_now()).map_err(Error::Login);
        };
        let keys = auth.keys();
        match login::verify(request, Mode::Online(&keys), unix_now()) {
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
        login::verify(request, Mode::Online(&keys), unix_now()).map_err(Error::Login)
    }

    async fn on_pack_response(&mut self, response: ResourcePackClientResponse) -> Result<()> {
        match response.response {
            PACKS_ALL_DOWNLOADED => {
                self.send(&[Packet::ResourcePackStack(resource_pack_stack())])
                    .await
            }
            PACKS_COMPLETED => self.join(),
            // Anchor offers no packs until M8, so there is nothing to send.
            PACKS_SEND if response.packs_to_download.is_empty() => Ok(()),
            PACKS_SEND => Err(Error::Order("requested a pack that was not offered")),
            PACKS_REFUSED => Err(Error::Order("resource packs refused")),
            _ => Err(Error::Order("unknown resource pack response")),
        }
    }

    /// Hands the player to the tick, then the packets that arrived before the join.
    fn join(&mut self) -> Result<()> {
        let identity = self
            .identity
            .clone()
            .ok_or(Error::Order("join before login"))?;
        let (outbound, receiver) = queue::outbound_queue();
        let mut inputs = self.shared.inputs.session(self.id);
        inputs
            .try_send(InputKind::Join { identity, outbound })
            .map_err(Error::Queue)?;
        info!(session = self.id.get(), addr = %self.conn.addr(), "player joining");
        self.inputs = Some(inputs);
        self.outbound = Some(receiver);
        self.stage = Stage::Joined;
        while let Some(packet) = self.deferred.pop_front() {
            if matches!(packet, Packet::SetLocalPlayerAsInitialised(_)) {
                self.spawned = true;
            }
            self.forward(packet)?;
        }
        Ok(())
    }

    /// Queues a packet for the tick. A full tick queue ends the session (the tick never waits).
    fn forward(&mut self, packet: Packet) -> Result<()> {
        let inputs = self
            .inputs
            .as_mut()
            .ok_or(Error::Order("packet before the join"))?;
        inputs
            .try_send(InputKind::Packet(packet))
            .map_err(Error::Queue)
    }

    /// Counts bytes against the pre-login total (PLAN.md 3).
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

    /// Batches, compresses and encrypts `packets` and hands the messages to the connection.
    async fn send(&mut self, packets: &[Packet]) -> Result<()> {
        let mut bodies = Vec::new();
        let headers = packets.iter().map(|packet| {
            let header = Header {
                id: packet.id(),
                ..Header::default()
            };
            (header, packet)
        });
        let encoder = &mut self.encoder;
        batch::encode_packets(headers, SHIELD_ID, &mut self.out_payload, |payload| {
            let mut body = Vec::new();
            match encoder {
                Some(encoder) => encoder.encode(payload, &mut body)?,
                // Before compression is on, the payload goes as it is (no header byte).
                None => body.extend_from_slice(payload),
            }
            bodies.push(body);
            Ok(())
        })
        .map_err(Error::Batch)?;
        for body in bodies {
            self.send_body(body).await?;
        }
        Ok(())
    }

    /// Sends the tick's messages in order: runs of packets batched here, shared batches as
    /// they are.
    async fn send_messages(&mut self, messages: Vec<Message>) -> Result<()> {
        let mut run = Vec::new();
        for message in messages {
            match message {
                Message::Packet(packet) => run.push(packet),
                Message::Shared(batches) => {
                    if !run.is_empty() {
                        self.send(&run).await?;
                        run.clear();
                    }
                    for body in batches.iter() {
                        self.send_body(body.clone()).await?;
                    }
                }
            }
        }
        if !run.is_empty() {
            self.send(&run).await?;
        }
        Ok(())
    }

    /// Encrypts a batch body when the session is encrypted and hands it to the connection.
    async fn send_body(&mut self, mut body: Vec<u8>) -> Result<()> {
        if let Some(encryptor) = &mut self.encryptor {
            encryptor.encrypt(&mut body).map_err(Error::Crypto)?;
        }
        self.conn.send(body).await.map_err(Error::Transport)
    }
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

/// `ResourcePacksInfo` with no packs, as BDS sends it (P6).
fn resource_packs_info() -> ResourcePacksInfo {
    ResourcePacksInfo {
        world_template_version: "0.0.0".to_owned(),
        ..ResourcePacksInfo::default()
    }
}

/// `ResourcePackStack` with no packs, as BDS sends it (P6).
fn resource_pack_stack() -> ResourcePackStack {
    ResourcePackStack {
        base_game_version: "*".to_owned(),
        ..ResourcePackStack::default()
    }
}

/// A token bucket over decompressed input bytes (PLAN.md 10.2).
#[derive(Debug)]
struct Budget {
    tokens: usize,
    capacity: usize,
    per_second: usize,
    refilled: Instant,
}

impl Budget {
    fn new(capacity: usize, per_second: usize, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            per_second,
            refilled: now,
        }
    }

    /// Takes `bytes`, or `None` when the bucket holds fewer. The time a refill pays for is
    /// moved forward exactly, so frequent calls lose no fraction of the rate.
    fn take(&mut self, bytes: usize, now: Instant) -> Option<()> {
        const NANOS: u128 = 1_000_000_000;
        let rate = u128::try_from(self.per_second).unwrap_or(u128::MAX).max(1);
        let elapsed = now.saturating_duration_since(self.refilled).as_nanos();
        let earned = rate.saturating_mul(elapsed) / NANOS;
        if self.tokens >= self.capacity
            || earned >= u128::try_from(self.capacity).unwrap_or(u128::MAX)
        {
            self.tokens = self.capacity;
            self.refilled = now;
        } else if earned > 0 {
            let paid =
                Duration::from_nanos(u64::try_from(earned * NANOS / rate).unwrap_or(u64::MAX));
            self.tokens = self
                .tokens
                .saturating_add(usize::try_from(earned).unwrap_or(usize::MAX))
                .min(self.capacity);
            self.refilled = self.refilled.checked_add(paid).unwrap_or(now);
        }
        self.tokens = self.tokens.checked_sub(bytes)?;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_refills_at_its_rate_up_to_its_capacity() {
        let start = clock::now();
        let mut budget = Budget::new(1000, 250, start);
        assert_eq!(budget.take(1000, start), Some(()));
        assert_eq!(budget.take(1, start), None);
        assert_eq!(budget.take(250, start + Duration::from_secs(1)), Some(()));
        assert_eq!(budget.take(1, start + Duration::from_secs(1)), None);
        assert_eq!(
            budget.take(1000, start + Duration::from_secs(100)),
            Some(())
        );
        assert_eq!(budget.take(1, start + Duration::from_secs(100)), None);
    }

    #[test]
    fn empty_pack_stacks_match_bds() {
        assert_eq!(resource_pack_stack().base_game_version, "*");
        assert!(resource_pack_stack().texture_packs.is_empty());
        assert_eq!(resource_packs_info().world_template_version, "0.0.0");
    }
}
