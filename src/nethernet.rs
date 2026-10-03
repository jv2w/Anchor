//! NetherNet: HTTP signaling, WebRTC peers and their UDP driver (PLAN.md 2.1, 10.3).
//!
//! The signaling server hands each offer to an offer task, which parses it, verifies its
//! identity and asks the driver for a peer; the answer goes back signed with the server identity.
//! One driver task owns both UDP sockets (`nethernet-port`, IPv4 and IPv6) and every peer, so
//! peer state needs no locks, as in RakNet. Datagrams find their peer through two indexes: the
//! server ICE ufrag of a STUN request, then the source addresses that request proved. When both
//! data channels are open the driver hands a [`Connection`] to the session layer.

mod identity;
mod message;
mod peer;
mod sdp;
mod signaling;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;
use tracing::{debug, trace, warn};

use crate::auth::Auth;
use crate::clock;
use crate::crypto::{self, PublicKey};
use crate::login::{self, Mode};
use crate::raknet::ip_key;

use identity::ServerIdentity;
use peer::{Certificate, Peer};
use signaling::{CODE_BAD_OFFER, CODE_INTERNAL, Offer, Reply};

/// The identity key file in the data folder (PLAN.md 1).
const IDENTITY_FILE: &str = "identity.key";
/// Offers waiting for the offer task; the signaling server answers 503 when it is full.
const OFFER_QUEUE: usize = 64;
/// Messages from sessions and offer tasks to the driver. Sends wait when it is full, which slows
/// sessions to the driver's pace instead of buffering without bound.
const COMMAND_QUEUE: usize = 4096;
/// Messages received on one connection that its session has not taken yet; bytes are bounded
/// separately. A full queue disconnects (PLAN.md 10.3).
const INBOUND_QUEUE: usize = 1024;
/// Peers before login, in all and per IPv4 address or IPv6 /64 (PLAN.md 10.3).
const MAX_PRE_LOGIN: usize = 64;
const MAX_PRE_LOGIN_PER_IP: usize = 4;
/// Tries at a peer whose 24-bit ufrag no other peer uses; a fourth collision in a row means the
/// table is far beyond any real load.
const UFRAG_TRIES: usize = 3;
/// Source addresses one peer may be reached from: one per client candidate pair it checks, which
/// the offer's candidate limit already bounds.
const MAX_PEER_ADDRS: usize = 8;
/// Datagrams read from one socket before the task serves its other inputs.
const RECV_BATCH: usize = 256;
/// Commands handled before the task serves its other inputs.
const COMMAND_BATCH: usize = 256;
/// Kernel socket buffers, as for RakNet.
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;
/// How often timers run: str0m timeouts, deadlines and session flags.
const UPDATE_INTERVAL: Duration = Duration::from_millis(10);
/// How long a closed connection may take to deliver what it has sent, as RakNet allows.
const CLOSE_LINGER: Duration = Duration::from_secs(5);
/// Larger than any datagram a peer accepts, so that an oversized one is seen and dropped.
const RECV_BUFFER: usize = 2048;
/// Signaling answer code (go-nethernet `ErrorCodeIdentityVerificationFailed`) for an offer
/// identity that fails (PLAN.md 10.3).
const CODE_IDENTITY: u32 = 37;

/// Ports, addresses and limits for [`start`].
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// TCP signaling addresses (`server-port`).
    pub(crate) signaling_v4: Option<SocketAddrV4>,
    pub(crate) signaling_v6: Option<SocketAddrV6>,
    /// UDP game data addresses (`nethernet-port`).
    pub(crate) udp_v4: Option<SocketAddrV4>,
    pub(crate) udp_v6: Option<SocketAddrV6>,
    /// The address advertised to clients instead of the one they reached (`public-ip`).
    pub(crate) public_ip: Option<IpAddr>,
    /// Largest game message a client may send.
    pub(crate) max_message: usize,
    /// PEM certificate chain and key for HTTPS signaling.
    pub(crate) tls: Option<(PathBuf, PathBuf)>,
}

/// Why NetherNet did not start.
#[derive(Debug)]
pub(crate) enum StartError {
    Identity(identity::Error),
    Tls(signaling::Error),
    Certificate(peer::Error),
    Bind(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Identity(_) => "identity key unusable",
            Self::Tls(_) => "tls certificate unusable",
            Self::Certificate(_) => "dtls certificate not created",
            Self::Bind(_) => "sockets not opened",
        })
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Identity(e) => Some(e),
            Self::Tls(e) => Some(e),
            Self::Certificate(e) => Some(e),
            Self::Bind(e) => Some(e),
        }
    }
}

impl From<StartError> for crate::Error {
    fn from(e: StartError) -> Self {
        match e {
            StartError::Identity(e) => Self::IdentityKey(Box::new(e)),
            StartError::Tls(e) => Self::TlsCertificate(Box::new(e)),
            StartError::Certificate(e) => Self::Start(io::Error::other(e)),
            StartError::Bind(e) => Self::Network(e),
        }
    }
}

/// A connection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The connection or the driver is closed.
    Closed,
}

/// A NetherNet result.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "connection closed",
        })
    }
}

impl std::error::Error for Error {}

/// Loads the identity key and the TLS certificate, binds the sockets and spawns the signaling
/// server, the offer task and the driver on the current runtime. `status` is the `GET /v1/join`
/// body; `auth` verifies online offers; `accepted` receives connections whose channels opened
/// (when it is full, the new connection is closed).
pub(crate) async fn start(
    settings: Settings,
    data_dir: &Path,
    status: watch::Receiver<Arc<[u8]>>,
    auth: Option<Auth>,
    accepted: mpsc::Sender<Connection>,
) -> std::result::Result<Handle, StartError> {
    let path = data_dir.join(IDENTITY_FILE);
    let identity = tokio::task::spawn_blocking(move || ServerIdentity::load_or_create(&path))
        .await
        .map_err(|e| StartError::Bind(io::Error::other(e)))?
        .map_err(StartError::Identity)?;
    let tls = match settings.tls {
        Some((cert, key)) => Some(
            tokio::task::spawn_blocking(move || signaling::load_tls(&cert, &key))
                .await
                .map_err(|e| StartError::Bind(io::Error::other(e)))?
                .map_err(StartError::Tls)?,
        ),
        None => None,
    };
    // One certificate for every peer, made now so that no join pays for it (PLAN.md 10.3).
    let certificate = Certificate::generate().map_err(StartError::Certificate)?;
    let v4 = settings.udp_v4.map(|a| bind(a.into())).transpose();
    let v6 = settings.udp_v6.map(|a| bind(a.into())).transpose();
    let (v4, v6) = (v4.map_err(StartError::Bind)?, v6.map_err(StartError::Bind)?);
    let udp_addrs = v4
        .iter()
        .chain(&v6)
        .map(UdpSocket::local_addr)
        .collect::<io::Result<Vec<_>>>()
        .map_err(StartError::Bind)?;
    let (offers_tx, offers_rx) = mpsc::channel(OFFER_QUEUE);
    let signaling = signaling::spawn(
        signaling::Settings {
            v4: settings.signaling_v4,
            v6: settings.signaling_v6,
            tls,
        },
        status,
        offers_tx,
    )
    .map_err(StartError::Bind)?;
    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_QUEUE);
    let (stop, stopped) = oneshot::channel();
    let driver = Driver {
        v4,
        v6,
        certificate,
        max_message: settings.max_message,
        peers: BTreeMap::new(),
        by_ufrag: BTreeMap::new(),
        by_addr: BTreeMap::new(),
        pre_login: BTreeMap::new(),
        pre_login_total: 0,
        next_id: 0,
        accepted,
        commands: commands_tx.clone(),
        buf: vec![0; RECV_BUFFER],
        doomed: Vec::new(),
    };
    let driver = tokio::spawn(driver.run(commands_rx, stopped));
    let offers = Offers {
        identity: Arc::new(identity),
        auth,
        public_ip: settings.public_ip,
        udp_addrs: udp_addrs.clone(),
        driver: commands_tx,
    };
    let offers = tokio::spawn(offers.run(offers_rx));
    Ok(Handle {
        signaling,
        udp_addrs,
        stop,
        driver,
        offers,
    })
}

fn bind(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        // Lets the IPv4 socket bind the same port on every OS.
        socket.set_only_v6(true)?;
    }
    socket.set_recv_buffer_size(SOCKET_BUFFER)?;
    socket.set_send_buffer_size(SOCKET_BUFFER)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

/// Controls the NetherNet tasks.
#[derive(Debug)]
pub(crate) struct Handle {
    signaling: signaling::Handle,
    udp_addrs: Vec<SocketAddr>,
    stop: oneshot::Sender<()>,
    driver: JoinHandle<()>,
    offers: JoinHandle<()>,
}

impl Handle {
    /// The bound TCP signaling addresses (IPv4 first).
    pub(crate) fn signaling_addrs(&self) -> &[SocketAddr] {
        self.signaling.local_addrs()
    }

    /// The bound UDP addresses (IPv4 first).
    pub(crate) fn udp_addrs(&self) -> &[SocketAddr] {
        &self.udp_addrs
    }

    /// Stops signaling, closes every peer and waits for the tasks to end.
    pub(crate) async fn shutdown(self) {
        self.signaling.shutdown().await;
        self.offers.abort();
        if self.stop.send(()).is_err() {
            debug!("nethernet driver already stopped");
        }
        if let Err(error) = self.driver.await {
            warn!(%error, "nethernet driver failed");
        }
    }
}

/// What a connection tells the driver through its watch channel.
#[derive(Clone, Copy, Debug, Default)]
struct Flags {
    logged_in: bool,
    closed: bool,
}

/// A request to the driver.
#[derive(Debug)]
enum Command {
    /// Make a peer for a verified offer.
    Open {
        remote: peer::Remote,
        /// The advertised host candidate.
        local: SocketAddr,
        client_key: Option<PublicKey>,
        /// The signaling client's IP, for the pre-login limits.
        ip: IpAddr,
        reply: oneshot::Sender<std::result::Result<peer::Local, Reply>>,
    },
    /// A game message from a session.
    Send { id: u64, message: Vec<u8> },
}

/// A received game message and its share of the connection's inbound byte budget.
#[derive(Debug)]
struct Inbound {
    message: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

/// One NetherNet connection, held by its session. Dropping it closes the connection.
#[derive(Debug)]
pub(crate) struct Connection {
    id: u64,
    addr: SocketAddr,
    client_key: Option<PublicKey>,
    inbound: mpsc::Receiver<Inbound>,
    commands: mpsc::Sender<Command>,
    signal: watch::Sender<Flags>,
}

impl Connection {
    /// The client address the data channels run on.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The client key the offer's identity proved; `None` for an offline offer without one.
    pub(crate) fn client_key(&self) -> Option<&PublicKey> {
        self.client_key.as_ref()
    }

    /// The next game message, or `None` once the connection is closed.
    pub(crate) async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inbound.recv().await.map(|inbound| inbound.message)
    }

    /// Queues a game message. Waits while the driver is busy. An error means the connection is
    /// closed.
    pub(crate) async fn send(&self, message: Vec<u8>) -> Result<()> {
        if self.inbound.is_closed() {
            return Err(Error::Closed);
        }
        let id = self.id;
        self.commands
            .send(Command::Send { id, message })
            .await
            .map_err(|_| Error::Closed)
    }

    /// The session logged the player in: the connection leaves the pre-login limits.
    pub(crate) fn logged_in(&self) {
        self.signal.send_modify(|flags| flags.logged_in = true);
    }

    /// Closes the connection after what is queued has been delivered.
    pub(crate) fn close(&self) {
        self.signal.send_modify(|flags| flags.closed = true);
    }
}

/// The offer task: one negotiation task per offer. The signaling server's connection limit
/// bounds how many run at once.
struct Offers {
    identity: Arc<ServerIdentity>,
    auth: Option<Auth>,
    public_ip: Option<IpAddr>,
    udp_addrs: Vec<SocketAddr>,
    driver: mpsc::Sender<Command>,
}

impl Offers {
    async fn run(self, mut offers: mpsc::Receiver<Offer>) {
        let this = Arc::new(self);
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                offer = offers.recv() => {
                    let Some(offer) = offer else { break };
                    tasks.spawn(Arc::clone(&this).answer(offer));
                }
                Some(ended) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(e) = ended {
                        warn!(error = %e, "nethernet offer task failed");
                    }
                }
            }
        }
        tasks.shutdown().await;
    }

    async fn answer(self: Arc<Self>, offer: Offer) {
        let Offer {
            network_id,
            sdp,
            peer,
            local,
            reply,
        } = offer;
        let answer = match self.negotiate(&sdp, peer.ip(), local.ip()).await {
            Ok(answer) => Reply::Answer(answer),
            Err(refusal) => {
                debug!(network_id, %peer, ?refusal, "nethernet offer refused");
                refusal
            }
        };
        if reply.send(answer).is_err() {
            trace!(network_id, "signaling connection gone before the answer");
        }
    }

    /// Verifies `text` and makes a peer for it; the answer SDP, or the reply that refuses it.
    async fn negotiate(
        &self,
        text: &str,
        client_ip: IpAddr,
        local_ip: IpAddr,
    ) -> std::result::Result<String, Reply> {
        let offer = sdp::parse_offer(text).map_err(|_| Reply::Code(CODE_BAD_OFFER))?;
        let client_key = match &offer.identity {
            Some(assertion) => Some(self.verify(assertion, &offer.fingerprint).await?),
            None if self.auth.is_some() => return Err(Reply::Code(CODE_IDENTITY)),
            None => None,
        };
        let advertised = self.advertised(local_ip)?;
        let remote = peer::Remote {
            ice_ufrag: offer.ice_ufrag,
            ice_pwd: offer.ice_pwd,
            fingerprint: offer.fingerprint.bytes,
            candidates: offer.candidates.iter().map(|c| c.addr).collect(),
            max_message_size: offer.max_message_size,
        };
        let (reply, answer) = oneshot::channel();
        let open = Command::Open {
            remote,
            local: advertised,
            client_key,
            ip: client_ip,
            reply,
        };
        self.driver
            .send(open)
            .await
            .map_err(|_| Reply::Code(CODE_INTERNAL))?;
        let local = answer.await.map_err(|_| Reply::Code(CODE_INTERNAL))??;
        let fingerprint = sdp::fingerprint_text(&local.fingerprint);
        let assertion = self
            .identity
            .assertion(&fingerprint, unix_now())
            .map_err(|_| Reply::Code(CODE_INTERNAL))?;
        let mut session_id = [0; 8];
        crypto::fill_random(&mut session_id).map_err(|_| Reply::Code(CODE_INTERNAL))?;
        Ok(sdp::write_answer(&sdp::Answer {
            session_id: u64::from_le_bytes(session_id),
            identity: &assertion,
            addr: advertised,
            ice_ufrag: &local.ice_ufrag,
            ice_pwd: &local.ice_pwd,
            fingerprint: &fingerprint,
        }))
    }

    /// Verifies the offer identity: online against the authorization keys with one JWKS refresh
    /// for an unknown key ID, as the Login check does.
    async fn verify(
        &self,
        assertion: &sdp::Assertion,
        fingerprint: &sdp::Fingerprint,
    ) -> std::result::Result<PublicKey, Reply> {
        let refused = |_| Reply::Code(CODE_IDENTITY);
        let Some(auth) = &self.auth else {
            return identity::verify_offer(assertion, fingerprint, Mode::Offline, unix_now())
                .map_err(refused);
        };
        let keys = auth.keys();
        match identity::verify_offer(assertion, fingerprint, Mode::Online(&keys), unix_now()) {
            Err(identity::Error::Token(login::Error::UnknownKeyId)) => {}
            result => return result.map_err(refused),
        }
        let keys = auth
            .refresh_keys()
            .await
            .ok_or(Reply::Code(CODE_IDENTITY))?;
        identity::verify_offer(assertion, fingerprint, Mode::Online(&keys), unix_now())
            .map_err(refused)
    }

    /// The host candidate to advertise: `public-ip`, or the address the client reached the
    /// signaling server on, with the UDP port of that address family.
    fn advertised(&self, local_ip: IpAddr) -> std::result::Result<SocketAddr, Reply> {
        let ip = self.public_ip.unwrap_or(local_ip).to_canonical();
        self.udp_addrs
            .iter()
            .find(|a| a.is_ipv4() == ip.is_ipv4())
            .map(|a| SocketAddr::new(ip, a.port()))
            .ok_or(Reply::Code(CODE_INTERNAL))
    }
}

/// Wall-clock Unix seconds for token checks; before 1970 reads as 0 and fails every check.
fn unix_now() -> i64 {
    clock::unix_now()
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(0)
}

/// The driver's side of an accepted connection.
#[derive(Debug)]
struct Link {
    inbound: mpsc::Sender<Inbound>,
    budget: Arc<Semaphore>,
    signal: watch::Receiver<Flags>,
}

/// One peer in the driver.
struct Entry {
    peer: Peer,
    ufrag: String,
    /// Source addresses a STUN request with this peer's ufrag came from.
    addrs: Vec<SocketAddr>,
    /// The signaling client's per-IP key while the peer counts against the pre-login limits.
    pre_login: Option<u128>,
    client_key: Option<PublicKey>,
    /// Set once both channels are open and the session has the connection.
    link: Option<Link>,
    /// When the peer next needs a poll without new input. The peer keeps its own deadlines
    /// (establishment and silence, peer.rs).
    timeout: Instant,
    /// Set when the session closed the connection: the peer is removed once everything sent is
    /// acknowledged, or at this time.
    closing: Option<Instant>,
}

/// The driver task: both sockets, every peer and their indexes.
struct Driver {
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    certificate: Certificate,
    max_message: usize,
    peers: BTreeMap<u64, Entry>,
    by_ufrag: BTreeMap<String, u64>,
    by_addr: BTreeMap<SocketAddr, u64>,
    /// Peers before login per IP key.
    pre_login: BTreeMap<u128, usize>,
    pre_login_total: usize,
    next_id: u64,
    accepted: mpsc::Sender<Connection>,
    commands: mpsc::Sender<Command>,
    buf: Vec<u8>,
    doomed: Vec<u64>,
}

impl Driver {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(UPDATE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = readable(self.v4.as_ref()) => self.receive(false),
                _ = readable(self.v6.as_ref()) => self.receive(true),
                Some(command) = commands.recv() => {
                    self.command(command);
                    for _ in 1..COMMAND_BATCH {
                        let Ok(command) = commands.try_recv() else { break };
                        self.command(command);
                    }
                }
                _ = interval.tick() => {
                    // Commands queued before a close flag (the last packets) are applied first,
                    // so a closing peer is not judged drained before it has them.
                    for _ in 0..COMMAND_QUEUE {
                        let Ok(command) = commands.try_recv() else { break };
                        self.command(command);
                    }
                    self.update();
                }
                _ = &mut stopped => break,
            }
        }
        let ids: Vec<u64> = self.peers.keys().copied().collect();
        for id in ids {
            self.remove(id, "server stopping");
        }
        debug!("nethernet driver stopped");
    }

    fn receive(&mut self, v6: bool) {
        let now = clock::now();
        for _ in 0..RECV_BATCH {
            let socket = if v6 { &self.v6 } else { &self.v4 };
            let Some(socket) = socket else { return };
            let (len, from) = match socket.try_recv_from(&mut self.buf) {
                Ok(received) => received,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                // Windows reports ICMP port unreachable as WSAECONNRESET on the next receive
                // (tokio-rs/tokio#2017); that and other per-datagram errors are skipped.
                Err(error) => {
                    trace!(%error, "nethernet receive error skipped");
                    continue;
                }
            };
            let from = SocketAddr::new(from.ip().to_canonical(), from.port());
            let Some(data) = self.buf.get(..len).filter(|_| len < RECV_BUFFER) else {
                trace!(%from, len, "nethernet oversized datagram dropped");
                continue;
            };
            let id = match peer::stun_server_ufrag(data) {
                Some(ufrag) => self.by_ufrag.get(ufrag).copied(),
                None => self.by_addr.get(&from).copied(),
            };
            let Some(id) = id else {
                trace!(%from, "nethernet datagram for no peer dropped");
                continue;
            };
            let Some(entry) = self.peers.get_mut(&id) else {
                continue;
            };
            if !entry.addrs.contains(&from) {
                // Only a STUN request with the peer's ufrag reaches here from a new address;
                // str0m checks its integrity before the address is used for anything else.
                if entry.addrs.len() >= MAX_PEER_ADDRS {
                    trace!(%from, "nethernet peer address limit reached");
                    continue;
                }
                entry.addrs.push(from);
                self.by_addr.insert(from, id);
            }
            // The peer must be polled before the next datagram reaches it (peer.rs).
            entry.peer.receive(now, from, data);
            self.drive(id, now);
        }
        self.reap();
    }

    fn command(&mut self, command: Command) {
        let now = clock::now();
        match command {
            Command::Open {
                remote,
                local,
                client_key,
                ip,
                reply,
            } => {
                let result = self.open(&remote, local, client_key, ip, now);
                if reply.send(result).is_err() {
                    // The offer task is gone; an unanswered peer would only wait for its
                    // deadline.
                    trace!("nethernet offer withdrawn");
                }
            }
            Command::Send { id, message } => {
                let Some(entry) = self.peers.get_mut(&id) else {
                    return;
                };
                if let Err(e) = entry.peer.send(&message) {
                    debug!(peer = id, error = %e, "nethernet send failed");
                    self.doomed.push(id);
                }
                self.drive(id, now);
                self.reap();
            }
        }
    }

    fn open(
        &mut self,
        remote: &peer::Remote,
        local: SocketAddr,
        client_key: Option<PublicKey>,
        ip: IpAddr,
        now: Instant,
    ) -> std::result::Result<peer::Local, Reply> {
        let key = ip_key(ip);
        let per_ip = self.pre_login.get(&key).copied().unwrap_or(0);
        if self.pre_login_total >= MAX_PRE_LOGIN || per_ip >= MAX_PRE_LOGIN_PER_IP {
            return Err(Reply::Busy);
        }
        let mut made = None;
        for _ in 0..UFRAG_TRIES {
            let peer = Peer::new(
                remote.clone(),
                local,
                &self.certificate,
                self.max_message,
                now,
            )
            .map_err(|_| Reply::Code(CODE_INTERNAL))?;
            if !self.by_ufrag.contains_key(&peer.local().ice_ufrag) {
                made = Some(peer);
                break;
            }
        }
        let peer = made.ok_or(Reply::Code(CODE_INTERNAL))?;
        let answer = peer.local().clone();
        let id = self.next_id;
        // A u64 counter does not wrap in a process lifetime; saturating keeps it panic-free.
        self.next_id = self.next_id.saturating_add(1);
        self.by_ufrag.insert(answer.ice_ufrag.clone(), id);
        *self.pre_login.entry(key).or_default() += 1;
        self.pre_login_total += 1;
        self.peers.insert(
            id,
            Entry {
                peer,
                ufrag: answer.ice_ufrag.clone(),
                addrs: Vec::new(),
                pre_login: Some(key),
                client_key,
                link: None,
                timeout: now,
                closing: None,
            },
        );
        self.drive(id, now);
        self.reap();
        Ok(answer)
    }

    /// Runs one peer's outputs until it waits for time or input.
    fn drive(&mut self, id: u64, now: Instant) {
        let Some(entry) = self.peers.get_mut(&id) else {
            return;
        };
        loop {
            match entry.peer.poll(now) {
                peer::Output::Transmit { to, data } => {
                    let socket = if to.is_ipv4() { &self.v4 } else { &self.v6 };
                    // A full send buffer drops the datagram; SCTP resends what matters.
                    if let Some(socket) = socket
                        && let Err(error) = socket.try_send_to(&data, to)
                    {
                        trace!(%to, %error, "nethernet datagram not sent");
                    }
                }
                peer::Output::Timeout(at) => {
                    entry.timeout = at;
                    return;
                }
                peer::Output::Open => {
                    let Some(addr) = entry.addrs.last().copied() else {
                        self.doomed.push(id);
                        return;
                    };
                    let link = accept(
                        &self.accepted,
                        &self.commands,
                        id,
                        addr,
                        entry,
                        self.max_message,
                    );
                    if link.is_none() {
                        self.doomed.push(id);
                        return;
                    }
                    entry.link = link;
                }
                peer::Output::Message(message) => {
                    if !deliver(entry.link.as_ref(), message) {
                        debug!(peer = id, "nethernet session not keeping up; closing");
                        self.doomed.push(id);
                        return;
                    }
                }
                peer::Output::Closed(reason) => {
                    debug!(peer = id, %reason, "nethernet peer closed");
                    self.doomed.push(id);
                    return;
                }
            }
        }
    }

    fn update(&mut self) {
        let now = clock::now();
        let mut due = Vec::new();
        for (&id, entry) in &mut self.peers {
            if let Some(link) = &mut entry.link {
                let flags = match link.signal.has_changed() {
                    Ok(false) => None,
                    Ok(true) => Some(*link.signal.borrow_and_update()),
                    // The connection was dropped.
                    Err(_) => Some(Flags {
                        logged_in: false,
                        closed: true,
                    }),
                };
                if let Some(flags) = flags {
                    if flags.logged_in
                        && let Some(key) = entry.pre_login.take()
                    {
                        release(&mut self.pre_login, &mut self.pre_login_total, key);
                    }
                    if flags.closed && entry.closing.is_none() {
                        entry.closing = Some(now.checked_add(CLOSE_LINGER).unwrap_or(now));
                    }
                }
            }
            // A closing peer stays until what was sent is acknowledged, or the linger ends.
            if let Some(until) = entry.closing
                && (now >= until || entry.peer.is_drained())
            {
                self.doomed.push(id);
                continue;
            }
            if now >= entry.timeout {
                due.push(id);
            }
        }
        for id in due {
            self.drive(id, now);
        }
        self.reap();
    }

    /// Removes the peers marked for closing.
    fn reap(&mut self) {
        while let Some(id) = self.doomed.pop() {
            self.remove(id, "closed");
        }
    }

    fn remove(&mut self, id: u64, why: &'static str) {
        let Some(mut entry) = self.peers.remove(&id) else {
            return;
        };
        debug!(peer = id, why, "nethernet peer removed");
        entry.peer.close();
        let now = clock::now();
        // Flush what closing produced (SCTP abort, DTLS close notify) without keeping the peer.
        while let peer::Output::Transmit { to, data } = entry.peer.poll(now) {
            let socket = if to.is_ipv4() { &self.v4 } else { &self.v6 };
            if let Some(socket) = socket
                && let Err(error) = socket.try_send_to(&data, to)
            {
                trace!(%to, %error, "nethernet datagram not sent");
            }
        }
        self.by_ufrag.remove(&entry.ufrag);
        for addr in &entry.addrs {
            if self.by_addr.get(addr) == Some(&id) {
                self.by_addr.remove(addr);
            }
        }
        if let Some(key) = entry.pre_login {
            release(&mut self.pre_login, &mut self.pre_login_total, key);
        }
    }
}

/// Takes one peer off the pre-login counts.
fn release(per_ip: &mut BTreeMap<u128, usize>, total: &mut usize, key: u128) {
    *total = total.saturating_sub(1);
    if let Some(count) = per_ip.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            per_ip.remove(&key);
        }
    }
}

/// Hands a newly open peer to the session layer; `None` when the accept queue is full.
fn accept(
    accepted: &mpsc::Sender<Connection>,
    commands: &mpsc::Sender<Command>,
    id: u64,
    addr: SocketAddr,
    entry: &mut Entry,
    max_message: usize,
) -> Option<Link> {
    let (inbound, receiver) = mpsc::channel(INBOUND_QUEUE);
    let (signal, watcher) = watch::channel(Flags::default());
    let connection = Connection {
        id,
        addr,
        client_key: entry.client_key.take(),
        inbound: receiver,
        commands: commands.clone(),
        signal,
    };
    if let Err(error) = accepted.try_send(connection) {
        debug!(%addr, %error, "nethernet connection not accepted");
        return None;
    }
    // Two largest messages may wait for the session, as on RakNet.
    let bytes = max_message.saturating_mul(2).min(Semaphore::MAX_PERMITS);
    Some(Link {
        inbound,
        budget: Arc::new(Semaphore::new(bytes)),
        signal: watcher,
    })
}

/// Passes a message to the session; `false` when its queue or byte budget is full.
fn deliver(link: Option<&Link>, message: Vec<u8>) -> bool {
    let Some(link) = link else {
        // Data before both channels opened is not game data yet.
        return false;
    };
    let Ok(len) = u32::try_from(message.len()) else {
        return false;
    };
    let Ok(budget) = Arc::clone(&link.budget).try_acquire_many_owned(len) else {
        return false;
    };
    link.inbound
        .try_send(Inbound {
            message,
            _budget: budget,
        })
        .is_ok()
}

async fn readable(socket: Option<&UdpSocket>) -> io::Result<()> {
    match socket {
        Some(socket) => socket.readable().await,
        None => std::future::pending().await,
    }
}
