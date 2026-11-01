//! NetherNet: HTTP signaling, WebRTC peers and their UDP driver.
//!
//! The signaling server hands each offer to an offer task, which parses it, verifies its
//! identity and asks the driver for a peer; the answer goes back signed with the server identity.
//! The driver holds every peer and runs in the UDP task (`udp`) beside RakNet's, on the same
//! `server-port` sockets, so peer state needs no locks. Datagrams find their peer through two
//! indexes: the server ICE ufrag of a STUN request, then the source addresses that request
//! proved. When both data channels are open the driver hands a [`Connection`] to the session
//! layer.

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

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, trace, warn};

use crate::auth::Auth;
use crate::clock;
use crate::crypto::{self, PublicKey};
use crate::login::{self, Mode};
use crate::raknet::{Flags, Inbound, Inbox, ip_key, report_undelivered};
use crate::udp::Sockets;

use identity::ServerIdentity;
use peer::{Certificate, Peer};
use signaling::{CODE_BAD_OFFER, CODE_INTERNAL, Offer, Reply};

pub(crate) use peer::MAX_DATAGRAM;

/// The identity key file in the data folder.
const IDENTITY_FILE: &str = "identity.key";
/// Offers waiting for the offer task; the signaling server answers 503 when it is full.
const OFFER_QUEUE: usize = 64;
/// Messages from sessions and offer tasks to the driver. Sends wait when it is full, which slows
/// sessions to the driver's pace instead of buffering without bound.
const COMMAND_QUEUE: usize = 4096;
/// Peers before login, in all and per IPv4 address or IPv6 /64.
const MAX_PRE_LOGIN: usize = 64;
const MAX_PRE_LOGIN_PER_IP: usize = 4;
/// Tries at a peer whose 24-bit ufrag no other peer uses; a third collision in a row means the
/// table is far beyond any real load.
const UFRAG_TRIES: usize = 3;
/// Source addresses one peer may be reached from: one per client candidate pair it checks, which
/// the offer's candidate limit already bounds.
const MAX_PEER_ADDRS: usize = 8;
/// Commands handled before the UDP task serves its other inputs.
const COMMAND_BATCH: usize = 256;
/// How long a closed connection may take to deliver what it has sent, as RakNet allows.
const CLOSE_LINGER: Duration = Duration::from_secs(5);
/// The STUN magic cookie (RFC 5389), bytes 4 to 7 of every STUN message.
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
/// Signaling answer code (go-nethernet `ErrorCodeIdentityVerificationFailed`) for an offer
/// identity that fails.
const CODE_IDENTITY: u32 = 37;

/// Addresses and limits for [`start`].
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// TCP signaling addresses (`server-port`).
    pub(crate) signaling_v4: Option<SocketAddrV4>,
    pub(crate) signaling_v6: Option<SocketAddrV6>,
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

/// Loads the identity key and the TLS certificate and spawns the signaling server and the offer
/// task on the current runtime; the driver they return sends on `sockets` and is the UDP task's
/// to run. `status` is the `GET /v1/join` body; `auth` verifies online offers; `accepted`
/// receives connections whose channels opened (when it is full, the new connection is closed).
pub(crate) async fn start(
    settings: Settings,
    data_dir: &Path,
    status: watch::Receiver<Arc<[u8]>>,
    auth: Option<Auth>,
    accepted: mpsc::Sender<Connection>,
    sockets: Arc<Sockets>,
) -> std::result::Result<(Handle, Driver), StartError> {
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
    // One certificate for every peer, made now so that no join pays for it.
    let certificate = Certificate::generate().map_err(StartError::Certificate)?;
    let udp_addrs = sockets.local_addrs().to_vec();
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
    let driver = Driver::new(sockets, certificate, settings.max_message, accepted);
    let offers = Offers {
        identity: Arc::new(identity),
        auth,
        public_ip: settings.public_ip,
        udp_addrs,
        driver: driver.commands.clone(),
    };
    let offers = tokio::spawn(offers.run(offers_rx));
    Ok((Handle { signaling, offers }, driver))
}

/// Controls the signaling server and the offer task; the UDP task stops the driver.
#[derive(Debug)]
pub(crate) struct Handle {
    signaling: signaling::Handle,
    offers: JoinHandle<()>,
}

impl Handle {
    /// The bound TCP signaling addresses (IPv4 first).
    pub(crate) fn signaling_addrs(&self) -> &[SocketAddr] {
        self.signaling.local_addrs()
    }

    /// Stops signaling and every negotiation. A peer request still on its way to the driver is
    /// refused there once the driver stops.
    pub(crate) async fn shutdown(self) {
        self.signaling.shutdown().await;
        self.offers.abort();
    }
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

/// One NetherNet connection, held by its session. Dropping it closes the connection.
#[derive(Debug)]
pub(crate) struct Connection {
    id: u64,
    addr: SocketAddr,
    client_key: Option<PublicKey>,
    inbound: mpsc::Receiver<Inbound>,
    commands: mpsc::Sender<Command>,
    signal: watch::Sender<Flags>,
    ping: watch::Receiver<Option<Duration>>,
    undelivered: watch::Receiver<usize>,
}

impl Connection {
    /// The client address the data channels run on.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The round trip time of the latest ICE consent check on the connection's address pair;
    /// `None` until one was answered.
    pub(crate) fn ping(&self) -> Option<Duration> {
        *self.ping.borrow()
    }

    /// The bytes sent on the connection that are not delivered yet, queued in the peer or
    /// unacknowledged in SCTP, as of the driver's last poll of the peer; the sender is dropped
    /// once the connection is gone.
    pub(crate) fn undelivered(&self) -> watch::Receiver<usize> {
        self.undelivered.clone()
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

/// The offer task: one negotiation task per offer. A negotiation ends once its signaling request
/// is gone, so the signaling server's connection limit bounds how many run at once.
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

    /// Negotiates `offer` until it is answered or its signaling request is gone (the client
    /// left, or the request deadline passed), which also bounds the wait for a JWKS refresh.
    async fn answer(self: Arc<Self>, offer: Offer) {
        let Offer {
            network_id,
            sdp,
            peer,
            local,
            mut reply,
        } = offer;
        let negotiated = tokio::select! {
            // First, so that an offer whose request is already gone is not negotiated at all.
            biased;
            () = reply.closed() => None,
            negotiated = self.negotiate(&sdp, peer.ip(), local.ip()) => Some(negotiated),
        };
        let answer = match negotiated {
            Some(Ok(answer)) => Reply::Answer(answer),
            Some(Err(refusal)) => {
                debug!(network_id, %peer, ?refusal, "nethernet offer refused");
                refusal
            }
            None => {
                trace!(network_id, "signaling connection gone before the answer");
                return;
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
            candidates: offer.candidates,
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
            .assertion(&fingerprint, clock::token_now())
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
            return identity::verify_offer(
                assertion,
                fingerprint,
                Mode::Offline,
                clock::token_now(),
            )
            .map_err(refused);
        };
        let keys = auth.keys();
        match identity::verify_offer(
            assertion,
            fingerprint,
            Mode::Online(&keys),
            clock::token_now(),
        ) {
            Err(identity::Error::Token(login::Error::UnknownKeyId)) => {}
            result => return result.map_err(refused),
        }
        let keys = auth
            .refresh_keys()
            .await
            .ok_or(Reply::Code(CODE_IDENTITY))?;
        identity::verify_offer(
            assertion,
            fingerprint,
            Mode::Online(&keys),
            clock::token_now(),
        )
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

/// The driver's side of an accepted connection.
#[derive(Debug)]
struct Link {
    inbox: Inbox,
    signal: watch::Receiver<Flags>,
    ping: watch::Sender<Option<Duration>>,
    undelivered: watch::Sender<usize>,
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

/// The driver, run by the UDP task: every peer and their indexes.
pub(crate) struct Driver {
    sockets: Arc<Sockets>,
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
    /// Handed to each connection and the offer task.
    commands: mpsc::Sender<Command>,
    pending: mpsc::Receiver<Command>,
    doomed: Vec<u64>,
    /// Set once the driver is stopping: new offers are refused.
    stopping: bool,
}

impl Driver {
    fn new(
        sockets: Arc<Sockets>,
        certificate: Certificate,
        max_message: usize,
        accepted: mpsc::Sender<Connection>,
    ) -> Self {
        let (commands, pending) = mpsc::channel(COMMAND_QUEUE);
        Self {
            sockets,
            certificate,
            max_message,
            peers: BTreeMap::new(),
            by_ufrag: BTreeMap::new(),
            by_addr: BTreeMap::new(),
            pre_login: BTreeMap::new(),
            pre_login_total: 0,
            next_id: 0,
            accepted,
            commands,
            pending,
            doomed: Vec::new(),
            stopping: false,
        }
    }

    /// Waits for a command, then applies it and those queued behind it, at most a batch;
    /// `false` once nothing can queue more. Cancel-safe: it waits only for the first.
    pub(crate) async fn serve(&mut self) -> bool {
        let Some(command) = self.pending.recv().await else {
            return false;
        };
        self.command(command);
        for _ in 1..COMMAND_BATCH {
            let Ok(command) = self.pending.try_recv() else {
                break;
            };
            self.command(command);
        }
        true
    }

    /// The timer handler.
    pub(crate) fn tick(&mut self) {
        self.drain();
        self.update();
    }

    /// Whether no peer is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Applies what sessions queued, at most a full queue. A session queues its last packets
    /// before it sets its close flag, so the timer and stop handlers call this first: a closing
    /// peer is not judged drained before it has them.
    fn drain(&mut self) {
        for _ in 0..COMMAND_QUEUE {
            let Ok(command) = self.pending.try_recv() else {
                break;
            };
            self.command(command);
        }
    }

    /// The stop handler: new offers are refused, peers without a connection go at once, the
    /// others close as a session close does, with the same linger.
    pub(crate) fn stop(&mut self) {
        self.drain();
        self.stopping = true;
        let now = clock::now();
        let until = now.checked_add(CLOSE_LINGER).unwrap_or(now);
        for (&id, entry) in &mut self.peers {
            if entry.link.is_none() {
                self.doomed.push(id);
            } else if entry.closing.is_none() {
                entry.closing = Some(until);
            }
        }
        self.reap();
    }

    /// Handles a datagram from `from`.
    pub(crate) fn receive(&mut self, from: SocketAddr, datagram: &[u8]) {
        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
        let id = match peer::stun_server_ufrag(datagram) {
            Some(ufrag) => self.by_ufrag.get(ufrag).copied(),
            None => self.by_addr.get(&from).copied(),
        };
        let Some(id) = id else {
            trace!(%from, "nethernet datagram for no peer dropped");
            return;
        };
        let Some(entry) = self.peers.get_mut(&id) else {
            return;
        };
        if !admit(&mut self.by_addr, &mut entry.addrs, id, from) {
            trace!(%from, "nethernet datagram from a refused address dropped");
            return;
        }
        let now = clock::now();
        // The peer must be polled before the next datagram reaches it (peer.rs).
        entry.peer.receive(now, from, datagram);
        self.drive(id, now);
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
        if self.stopping || self.pre_login_total >= MAX_PRE_LOGIN || per_ip >= MAX_PRE_LOGIN_PER_IP
        {
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
                    // A full send buffer drops the datagram; SCTP resends what matters.
                    if let Err(error) = self.sockets.send(to, &data) {
                        trace!(%to, %error, "nethernet datagram not sent");
                    }
                }
                peer::Output::Timeout(at) => {
                    entry.timeout = at;
                    let Some(link) = &entry.link else {
                        return;
                    };
                    // A failed count closes the peer; polling again reports it.
                    let Ok(undelivered) = entry.peer.undelivered() else {
                        continue;
                    };
                    report_undelivered(&link.undelivered, undelivered);
                    return;
                }
                peer::Output::Open => {
                    let Some(addr) = entry.peer.remote_addr() else {
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
                    // Data before both channels opened is not game data yet.
                    if !entry
                        .link
                        .as_ref()
                        .is_some_and(|link| link.inbox.deliver(message))
                    {
                        debug!(peer = id, "nethernet session not keeping up; closing");
                        self.doomed.push(id);
                        return;
                    }
                }
                peer::Output::Rtt(rtt) => {
                    if let Some(link) = &entry.link {
                        link.ping.send_replace(Some(rtt));
                    }
                }
                peer::Output::Closed(reason) => {
                    debug!(peer = id, ufrag = %entry.ufrag, %reason, "nethernet peer closed");
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
            if let Some(link) = &mut entry.link
                && let Some(flags) = Flags::changed(&mut link.signal)
            {
                if flags.logged_in
                    && let Some(key) = entry.pre_login.take()
                {
                    release(&mut self.pre_login, &mut self.pre_login_total, key);
                }
                if flags.closed && entry.closing.is_none() {
                    entry.closing = Some(now.checked_add(CLOSE_LINGER).unwrap_or(now));
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
            self.remove(id);
        }
    }

    fn remove(&mut self, id: u64) {
        let Some(mut entry) = self.peers.remove(&id) else {
            return;
        };
        debug!(peer = id, "nethernet peer removed");
        entry.peer.close();
        let now = clock::now();
        // Flush what closing produced (SCTP abort, DTLS close notify) without keeping the peer.
        while let peer::Output::Transmit { to, data } = entry.peer.poll(now) {
            if let Err(error) = self.sockets.send(to, &data) {
                trace!(%to, %error, "nethernet datagram not sent");
            }
        }
        self.by_ufrag.remove(&entry.ufrag);
        forget(&mut self.by_addr, &entry.addrs);
        if let Some(key) = entry.pre_login {
            release(&mut self.pre_login, &mut self.pre_login_total, key);
        }
    }
}

/// Whether a datagram on the shared UDP port is NetherNet's (RFC 7983 demultiplexing): DTLS,
/// first byte 20 to 63, or STUN, first byte 0 or 1 with the magic cookie and a length that
/// matches. A datagram RakNet takes never starts with 20 to 63 or 0; its ping, the one starting
/// with 1, passes as STUN only if the client's timestamp spells the cookie and the length, and a
/// client that crafts that loses only its own ping. A real ping is 33 bytes, a length STUN's
/// 4-byte attribute padding never gives.
pub(crate) fn is_nethernet(datagram: &[u8]) -> bool {
    match datagram {
        [20..=63, ..] => true,
        [0..=1, _, l0, l1, c0, c1, c2, c3, rest @ ..] => {
            const TRANSACTION_ID: usize = 12;
            let length = usize::from(u16::from_be_bytes([*l0, *l1]));
            [*c0, *c1, *c2, *c3] == STUN_MAGIC_COOKIE
                && length % 4 == 0
                && rest.len().checked_sub(TRANSACTION_ID) == Some(length)
        }
        _ => false,
    }
}

/// Records `from` as a source address of peer `id`, whose addresses are `addrs`; `false` drops
/// the datagram. Only a STUN request with the peer's ufrag reaches here from a new address. An
/// address another live peer already has stays that peer's: a client knows its own ufrag and
/// password, so its valid STUN request can carry any spoofed source, and taking the address
/// would cut the other peer off from its own datagrams.
fn admit(
    by_addr: &mut BTreeMap<SocketAddr, u64>,
    addrs: &mut Vec<SocketAddr>,
    id: u64,
    from: SocketAddr,
) -> bool {
    if addrs.contains(&from) {
        return true;
    }
    if addrs.len() >= MAX_PEER_ADDRS || by_addr.contains_key(&from) {
        return false;
    }
    addrs.push(from);
    by_addr.insert(from, id);
    true
}

/// Takes a removed peer's addresses out of the address index; `admit` gave each one to it
/// alone.
fn forget(by_addr: &mut BTreeMap<SocketAddr, u64>, addrs: &[SocketAddr]) {
    for addr in addrs {
        by_addr.remove(addr);
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
    let (inbox, receiver) = Inbox::new(max_message);
    let (signal, watcher) = watch::channel(Flags::default());
    let (ping, pings) = watch::channel(None);
    let (undelivered, undelivered_watcher) = watch::channel(0);
    let connection = Connection {
        id,
        addr,
        client_key: entry.client_key.take(),
        inbound: receiver,
        commands: commands.clone(),
        signal,
        ping: pings,
        undelivered: undelivered_watcher,
    };
    if let Err(error) = accepted.try_send(connection) {
        debug!(%addr, %error, "nethernet connection not accepted");
        return None;
    }
    Some(Link {
        inbox,
        signal: watcher,
        ping,
        undelivered,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::Ipv4Addr;

    use str0m::net::{Protocol, Receive};
    use str0m::{DATAGRAM_MTU_TARGET, Input};
    use tokio::net::UdpSocket;

    use super::*;
    use crate::raknet::tests::run;
    use crate::udp::{self, RECV_BUFFER, UPDATE_INTERVAL};
    use peer::tests::{Client, MAX_MESSAGE, certificates, reliable, unreliable};

    const WAIT: Duration = Duration::from_secs(20);

    /// A driver on `sockets`, and the connections it accepts.
    pub(crate) fn driver(sockets: Arc<Sockets>) -> (Driver, mpsc::Receiver<Connection>) {
        let (accepted, connections) = mpsc::channel(1);
        let driver = Driver::new(sockets, certificates().0, MAX_MESSAGE, accepted);
        (driver, connections)
    }

    /// Sends the client's output until it waits for time or input; `false` once the client
    /// failed (after the peer went).
    async fn flush(client: &mut Client, socket: &UdpSocket) -> bool {
        loop {
            match client.rtc.poll_output() {
                Ok(str0m::Output::Transmit(t)) => {
                    socket.send_to(&t.contents, t.destination).await.unwrap();
                }
                Ok(str0m::Output::Event(event)) => client.event(event),
                Ok(str0m::Output::Timeout(at)) => {
                    client.timeout = at;
                    return true;
                }
                Err(_) => return false,
            }
        }
    }

    /// Hands the client a datagram from `from`; `false` once the client failed.
    fn feed(client: &mut Client, socket: &UdpSocket, from: SocketAddr, datagram: &[u8]) -> bool {
        let to = socket.local_addr().unwrap();
        let receive = Receive::new(Protocol::Udp, from, to, datagram).unwrap();
        client
            .rtc
            .handle_input(Input::Receive(clock::now(), receive))
            .is_ok()
    }

    /// Sends the client's output, then takes one datagram or its timeout; `false` once the
    /// client failed (after the peer went).
    async fn step(client: &mut Client, socket: &UdpSocket) -> bool {
        if !flush(client, socket).await {
            return false;
        }
        let wait = client
            .timeout
            .saturating_duration_since(clock::now())
            .clamp(Duration::from_millis(1), UPDATE_INTERVAL);
        let mut buf = [0; RECV_BUFFER];
        match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, from))) => feed(client, socket, from, &buf[..len]),
            // A timeout, or Windows reporting an earlier datagram's port unreachable.
            Ok(Err(_)) | Err(_) => client
                .rtc
                .handle_input(Input::Timeout(clock::now()))
                .is_ok(),
        }
    }

    /// Hands the client every datagram already waiting on `socket`, without waiting for more.
    async fn take_waiting(client: &mut Client, socket: &UdpSocket) {
        let mut buf = [0; RECV_BUFFER];
        while flush(client, socket).await {
            match socket.try_recv_from(&mut buf) {
                Ok((len, from)) => {
                    if !feed(client, socket, from, &buf[..len]) {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // Windows reporting an earlier datagram's port unreachable.
                Err(_) => {}
            }
        }
    }

    #[test]
    fn shutdown_delivers_the_last_message_of_every_peer() {
        run(shutdown_delivers_the_last_message());
    }

    /// The UDP task on a loopback port with both drivers: its handle, the port, the NetherNet
    /// driver's command queue and the NetherNet connections it accepts.
    fn spawn_both() -> (
        udp::Handle,
        SocketAddr,
        mpsc::Sender<Command>,
        mpsc::Receiver<Connection>,
    ) {
        let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let sockets = Sockets::bind(Some(localhost), None).unwrap();
        let server = sockets.local_addrs()[0];
        let (nethernet, connections) = driver(Arc::clone(&sockets));
        let commands = nethernet.commands.clone();
        let (raknet, _) = crate::raknet::tests::driver(Arc::clone(&sockets));
        let handle = udp::spawn(sockets, raknet, nethernet);
        (handle, server, commands, connections)
    }

    /// A test client on a loopback socket of its own, connected through the driver behind
    /// `commands` at `server`, and the connection the driver accepted for it.
    async fn connect(
        commands: &mpsc::Sender<Command>,
        connections: &mut mpsc::Receiver<Connection>,
        server: SocketAddr,
    ) -> (Client, UdpSocket, Connection) {
        let (_, client_cert) = certificates();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let start = clock::now();
        let fingerprint = *client_cert.fingerprint();
        let (mut client, remote) =
            Client::new(&client_cert, addr, DATAGRAM_MTU_TARGET, fingerprint, start);
        let (reply, answer) = oneshot::channel();
        let open = Command::Open {
            remote,
            local: server,
            client_key: None,
            ip: addr.ip(),
            reply,
        };
        commands.send(open).await.unwrap();
        let local = answer.await.unwrap().unwrap();
        client.answer(&local, addr, server, &[reliable(), unreliable()]);
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            assert!(clock::now() < start + WAIT, "no connection");
            assert!(step(&mut client, &socket).await, "client failed");
        };
        (client, socket, connection)
    }

    async fn shutdown_delivers_the_last_message() {
        let start = clock::now();
        let (udp, server, commands, mut connections) = spawn_both();
        let (mut client, socket, connection) = connect(&commands, &mut connections, server).await;

        // The session's last packets, then the server stops at once.
        let big = vec![7; 1 << 20];
        connection.send(big.clone()).await.unwrap();
        connection.send(b"bye".to_vec()).await.unwrap();
        let task = tokio::spawn(udp.shutdown());
        while client.received.len() < 2 {
            assert!(clock::now() < start + WAIT, "timed out");
            // The stopped driver sends nothing more: what it sent is already on the loopback
            // socket. (A driver that ended because the client acknowledged everything leaves
            // nothing to take: the client had emitted each message by the time it acknowledged
            // it.)
            if task.is_finished() {
                take_waiting(&mut client, &socket).await;
                break;
            }
            if !step(&mut client, &socket).await {
                break;
            }
        }
        let got = client.received.len();
        assert_eq!(got, 2, "the client got {got} of the last 2 messages");
        assert_eq!(client.received[0], big);
        assert_eq!(client.received[1], b"bye");
        while !task.is_finished() {
            assert!(clock::now() < start + WAIT, "the driver did not stop");
            step(&mut client, &socket).await;
        }
        task.await.unwrap();
    }

    /// A negotiation ends once its signaling request is gone, whatever it waits for, so the
    /// negotiations never outnumber the signaling connections.
    #[test]
    fn a_negotiation_ends_when_its_signaling_request_is_gone() {
        run(negotiation_ends_when_its_request_is_gone());
    }

    async fn negotiation_ends_when_its_request_is_gone() {
        let (driver, mut commands) = mpsc::channel(1);
        let server = SocketAddr::from((Ipv4Addr::LOCALHOST, 19133));
        let offers = Arc::new(Offers {
            identity: Arc::new(ServerIdentity::generate().unwrap()),
            auth: None,
            public_ip: None,
            udp_addrs: vec![server],
            driver,
        });
        let (reply, replied) = oneshot::channel();
        let identity = format!("{}\n", sdp::tests::identity_line());
        let offer = Offer {
            network_id: 1,
            // Offline, an offer without an identity goes on to the driver.
            sdp: sdp::tests::OFFER.replace(&identity, ""),
            peer: SocketAddr::from((Ipv4Addr::LOCALHOST, 50_000)),
            local: server,
            reply,
        };
        let task = tokio::spawn(offers.answer(offer));
        // The driver takes the peer request and never answers it.
        let held = tokio::time::timeout(WAIT, commands.recv()).await.unwrap();
        assert!(matches!(held, Some(Command::Open { .. })));
        drop(replied);
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        ended
            .expect("the negotiation outlived its request")
            .unwrap();
        drop(held);
    }

    #[test]
    fn stun_from_another_peers_address_does_not_steal_it() {
        const A: u64 = 1;
        const B: u64 = 2;
        let victim = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 50_000));
        let mut by_addr = BTreeMap::new();
        let mut a_addrs = Vec::new();
        let mut b_addrs = Vec::new();
        assert!(admit(&mut by_addr, &mut a_addrs, A, victim));
        // B's own ufrag in a STUN request with the victim's address as its source.
        assert!(!admit(&mut by_addr, &mut b_addrs, B, victim));
        assert_eq!(by_addr.get(&victim), Some(&A));
        assert!(b_addrs.is_empty());
        forget(&mut by_addr, &b_addrs);
        assert_eq!(by_addr.get(&victim), Some(&A));
        // A's address stays usable by A.
        assert!(admit(&mut by_addr, &mut a_addrs, A, victim));
    }

    /// A NetherNet peer and a RakNet ping reach their own drivers through the one UDP port, so
    /// a server needs only `server-port` open.
    #[test]
    fn nethernet_and_raknet_share_the_udp_port() {
        run(share_the_udp_port());
    }

    async fn share_the_udp_port() {
        let start = clock::now();
        let (udp, server, commands, mut connections) = spawn_both();
        // The channels open only if every STUN, DTLS and SCTP datagram reached the peer.
        let (mut client, socket, connection) = connect(&commands, &mut connections, server).await;
        let pinger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let pong = crate::raknet::tests::ping(&pinger, server).await;
        assert_eq!(pong.first(), Some(&0x1c), "an unconnected pong");

        connection
            .send(b"over the shared port".to_vec())
            .await
            .unwrap();
        while client.received.is_empty() {
            assert!(clock::now() < start + WAIT, "timed out");
            assert!(step(&mut client, &socket).await, "client failed");
        }
        assert_eq!(client.received, [b"over the shared port"]);
        drop(connection);
        udp.shutdown().await;
    }

    /// STUN and DTLS go to the NetherNet driver; everything a RakNet client sends goes to
    /// RakNet, even a ping whose timestamp spells the STUN cookie.
    #[test]
    fn datagrams_go_to_the_transport_their_first_bytes_name() {
        let stun = |length: u16, attributes: usize| {
            let mut datagram = vec![0x00, 0x01];
            datagram.extend_from_slice(&length.to_be_bytes());
            datagram.extend_from_slice(&STUN_MAGIC_COOKIE);
            datagram.extend_from_slice(&[7; 12]);
            datagram.resize(datagram.len() + attributes, 0);
            datagram
        };
        assert!(is_nethernet(&stun(0, 0)));
        assert!(is_nethernet(&stun(8, 8)));
        assert!(!is_nethernet(&stun(8, 4)));
        for first in 20..=63 {
            assert!(is_nethernet(&[first, 0xfe, 0xfd]));
        }

        let ping = crate::raknet::tests::unconnected_ping();
        assert_eq!(ping.len(), 33);
        let mut cookie = ping.clone();
        cookie[4..8].copy_from_slice(&STUN_MAGIC_COOKIE);
        for raknet in [
            ping,
            cookie,
            vec![0x05; 1464],
            vec![0x07; 34],
            vec![0x84, 0, 0, 0],
            vec![0xc0, 0, 1],
            vec![0xa0, 0, 1],
            Vec::new(),
        ] {
            assert!(!is_nethernet(&raknet), "{raknet:02x?}");
        }
    }
}
