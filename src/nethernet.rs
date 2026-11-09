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

pub(crate) use peer::MAX_DATAGRAM_BYTES;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tracing::{debug, trace, warn};

use crate::auth::Auth;
use crate::clock;
use crate::crypto::{self, PublicKey};
use crate::login::{self, Mode};
use crate::raknet::{
    COMMAND_BATCH, COMMAND_QUEUE, Flags, Inbound, Inbox, MAX_PRE_LOGIN, MAX_PRE_LOGIN_PER_IP,
    ip_key, report_undelivered,
};
use crate::tick::queue::LeaveReason;
use crate::udp::Sockets;
use identity::ServerIdentity;
use peer::{Certificate, Peer};
use signaling::{Code, Host, Offer, Reply};

/// How long a closed connection may take to deliver what it has sent, as RakNet allows.
const CLOSE_LINGER: Duration = Duration::from_secs(5);
/// The first byte of a DTLS record on the shared UDP port (RFC 7983 7).
const DTLS_FIRST_BYTE_RANGE: RangeInclusive<u8> = 20..=63;
/// How long the addresses of a successful lookup are kept. Honest clients of one server give the
/// same name, so once it resolved, names whose lookups hang cannot keep it from their answers;
/// the time is short so that a changed DNS record reaches the answers soon.
const HOST_CACHE_LIFETIME: Duration = Duration::from_secs(60);
/// How long an offer waits for the lookup of the DNS name its client gave. A resolver answers
/// in milliseconds, the more so as the client has just resolved the same name; past this the
/// answer goes without the name's addresses, well inside the signaling request deadline.
const HOST_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
/// The identity key file in the data folder.
pub(crate) const IDENTITY_FILE: &str = "identity.key";
/// The lookup requests waiting for the lookup task. Each negotiation sends at most one, and
/// the signaling connection limit bounds the negotiations.
const LOOKUP_QUEUE: usize = 64;
/// The host candidates one answer advertises at most: the signaling address and the addresses
/// of the host the client gave.
const MAX_ANSWER_CANDIDATES: usize = 4;
/// The names whose addresses are kept at most. While it is full a new name's addresses are not
/// kept, so names a client makes up cannot push out a name honest clients give.
const MAX_CACHED_HOSTS: usize = 64;
/// The peers that may linger after their session closed them, at most; a new one removes the
/// oldest. A closing peer keeps up to `MAX_UNSENT_BYTES` unacknowledged and its str0m state for
/// `CLOSE_LINGER`, and the client can withhold the acknowledgements, so the count is bounded as
/// the pre-login peers are: by `MAX_PRE_LOGIN`.
const MAX_CLOSING_PEERS: usize = MAX_PRE_LOGIN;
/// The DNS lookups of client-given names that run at once. A lookup runs on a blocking thread
/// and keeps it until the OS resolver returns, even after its offer is gone, so this bounds the
/// blocking threads names whose lookups hang can hold (`runtime` reserves them). One client IP
/// key runs or waits for one lookup at a time, so holding them all takes this many keys.
pub(crate) const MAX_HOST_LOOKUPS: usize = 2;
/// The addresses taken from one lookup, at most, in the resolver's order.
const MAX_LOOKUP_ADDRS: usize = 16;
/// The source addresses one peer may be reached from. A client knows its own ICE credentials, so
/// its valid STUN request can carry any source address; the bound keeps one peer from filling
/// the address index.
const MAX_PEER_ADDRS: usize = 8;
/// The largest message a peer takes before its player logged in, in bytes: it holds a Login with
/// the largest skin (`MAX_SKIN_BYTES`), so that unfinished messages of the pre-login peers
/// (`MAX_PRE_LOGIN`) hold a bounded share of memory. A peer takes no more than the message limit
/// derived from `max-batch-bytes` either. RakNet derives its pre-login reassembly limit from it
/// too.
pub(crate) const MAX_PRE_LOGIN_MESSAGE_BYTES: usize = 8 << 20;
/// The offers waiting for the offer task; the signaling server answers 503 when it is full.
const OFFER_QUEUE: usize = 64;
/// The first byte of a STUN message on the shared UDP port (RFC 7983 7).
const STUN_FIRST_BYTE_RANGE: RangeInclusive<u8> = 0..=1;
/// The length of a STUN message header in bytes (RFC 5389): the type, the length, the magic
/// cookie and the transaction ID.
const STUN_HEADER_LEN: usize = 20;
/// The STUN magic cookie (RFC 5389), bytes 4 to 7 of every STUN message.
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
/// The tries at a peer whose 24-bit ufrag no other peer uses; a third collision in a row means
/// the table is far beyond any real load.
const UFRAG_TRIES: usize = 3;

/// Addresses and limits for [`start`].
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// TCP signaling addresses (`server-port`).
    pub(crate) signaling_v4: Option<SocketAddrV4>,
    /// The IPv6 signaling address.
    pub(crate) signaling_v6: Option<SocketAddrV6>,
    /// The one host candidate address answered instead of the signaling address the client
    /// reached and the addresses of the host it gave (`public-ip`); it is the same family as a
    /// UDP socket the server has.
    pub(crate) public_ip: Option<IpAddr>,
    /// The largest game message a client may send.
    pub(crate) max_message_bytes: usize,
    /// PEM certificate chain and key for HTTPS signaling.
    pub(crate) tls: Option<(PathBuf, PathBuf)>,
}

/// A connection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The connection or the driver is closed.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("connection closed"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

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
    /// A question whether a peer from `ip` would be admitted, asked before the offer's costly
    /// verification. The answer is only a hint: `Open` checks the limits again.
    Admit {
        ip: IpAddr,
        reply: oneshot::Sender<bool>,
    },
    /// A peer wanted for a verified offer.
    Open {
        remote: peer::Remote,
        /// The advertised host candidate.
        local: SocketAddr,
        client_key: Option<PublicKey>,
        /// The signaling client's IP, for the pre-login limits.
        ip: IpAddr,
        reply: oneshot::Sender<Result<peer::Local, Refusal>>,
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
    reason: watch::Receiver<LeaveReason>,
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

    /// Why the connection ended, once [`recv`](Self::recv) returned `None`.
    pub(crate) fn leave_reason(&self) -> LeaveReason {
        *self.reason.borrow()
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

    /// Closes the connection after what is queued has been delivered.
    pub(crate) fn close(&self) {
        self.signal.send_modify(|flags| flags.closed = true);
    }

    /// Marks the player as logged in: the connection leaves the pre-login limits.
    pub(crate) fn logged_in(&self) {
        self.signal.send_modify(|flags| flags.logged_in = true);
    }

    /// Takes the next game message, or `None` once the connection is closed.
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
}

/// The offer task: one negotiation task per offer. A negotiation ends once its signaling request
/// is gone, so the signaling server's connection limit bounds how many run at once.
struct Offers {
    identity: Arc<ServerIdentity>,
    auth: Option<Auth>,
    public_ip: Option<IpAddr>,
    udp_addrs: Vec<SocketAddr>,
    driver: mpsc::Sender<Command>,
    /// The lookup task ([`HostLookups`]).
    lookups: mpsc::Sender<HostLookup>,
}

impl Offers {
    /// Spawns a negotiation task for each offer until the signaling server is gone.
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
                        warn!(
                            error = &e as &(dyn std::error::Error + 'static),
                            "nethernet offer task failed"
                        );
                    }
                }
            }
        }
        tasks.shutdown().await;
    }

    /// Negotiates `offer` until it is answered or its signaling request is gone (its deadline
    /// passed), which also bounds the wait for a JWKS refresh.
    async fn answer(self: Arc<Self>, offer: Offer) {
        let Offer {
            network_id,
            sdp,
            peer,
            local,
            host,
            mut reply,
        } = offer;
        let negotiated = tokio::select! {
            // First, so that an offer whose request is already gone is not negotiated at all.
            biased;
            () = reply.closed() => None,
            negotiated = self.negotiate(&sdp, peer.ip(), local.ip(), host) => Some(negotiated),
        };
        let answer = match negotiated {
            Some(Ok(answer)) => Reply::Answer(answer),
            Some(Err(refusal)) => {
                let refused = refusal.reply();
                let error = &refusal as &(dyn std::error::Error + 'static);
                if refused == Reply::Code(Code::Internal) {
                    warn!(network_id, addr = %peer, error, "nethernet offer not answered");
                } else {
                    debug!(network_id, addr = %peer, error, "nethernet offer refused");
                }
                refused
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

    /// Verifies `text` and makes a peer for it; the answer SDP, or why the offer is refused.
    /// `host` is the address the client was given, from its request.
    async fn negotiate(
        &self,
        text: &str,
        client_ip: IpAddr,
        local_ip: IpAddr,
        host: Option<Host>,
    ) -> Result<String, Refusal> {
        let offer = sdp::parse_offer(text).map_err(Refusal::Offer)?;
        if offer.identity.is_none() && self.auth.is_some() {
            return Err(Refusal::NoIdentity);
        }
        // Before the verification, which costs signature checks and may fetch keys.
        self.admit(client_ip).await?;
        let client_key = match &offer.identity {
            Some(assertion) => Some(self.verify(assertion, &offer.fingerprint).await?),
            None => None,
        };
        let advertised = self.advertised(local_ip)?;
        // After the verification, so that only a verified offer makes the server look up a name.
        let candidates = self.candidates(advertised, host, client_ip).await;
        let remote = peer::Remote {
            ice_ufrag: offer.ice_ufrag,
            ice_pwd: offer.ice_pwd,
            fingerprint: offer.fingerprint.bytes,
            candidates: offer.candidates,
            max_message_bytes: offer.max_message_bytes,
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
            .map_err(|_| Refusal::DriverGone)?;
        let local = answer.await.map_err(|_| Refusal::DriverGone)??;
        let fingerprint = sdp::fingerprint_text(&local.fingerprint);
        let assertion = self
            .identity
            .assertion(&fingerprint, clock::token_now())
            .map_err(Refusal::Sign)?;
        let mut session_id = [0; 8];
        crypto::fill_random(&mut session_id).map_err(Refusal::Random)?;
        Ok(sdp::write_answer(&sdp::Answer {
            session_id: u64::from_le_bytes(session_id),
            identity: &assertion,
            addrs: &candidates,
            ice_ufrag: &local.ice_ufrag,
            ice_pwd: &local.ice_pwd,
            fingerprint: &fingerprint,
        }))
    }

    /// Asks the driver whether a peer from `client_ip` fits the pre-login limits.
    async fn admit(&self, client_ip: IpAddr) -> Result<(), Refusal> {
        let (reply, answer) = oneshot::channel();
        self.driver
            .send(Command::Admit {
                ip: client_ip,
                reply,
            })
            .await
            .map_err(|_| Refusal::DriverGone)?;
        if answer.await.map_err(|_| Refusal::DriverGone)? {
            Ok(())
        } else {
            Err(Refusal::Busy)
        }
    }

    /// The first host candidate to advertise, the one str0m holds: `public-ip`, or the address
    /// the client reached the signaling server on, with the UDP port of that address family.
    fn advertised(&self, local_ip: IpAddr) -> Result<SocketAddr, Refusal> {
        let ip = self.public_ip.unwrap_or(local_ip).to_canonical();
        self.udp_addrs
            .iter()
            .find(|a| a.is_ipv4() == ip.is_ipv4())
            .map(|a| SocketAddr::new(ip, a.port()))
            .ok_or(Refusal::NoSocket)
    }

    /// Every host candidate to advertise: `first`, then, without `public-ip`, the addresses of
    /// `host` in its family and with its port, without repeats and at most
    /// [`MAX_ANSWER_CANDIDATES`] in all. Behind a NAT the signaling address is a private one; the
    /// host the client was given is an address it can reach. Only this client's answer carries
    /// them: the server never sends to its own candidates. `client_ip` is the signaling client's,
    /// for the lookup limit.
    async fn candidates(
        &self,
        first: SocketAddr,
        host: Option<Host>,
        client_ip: IpAddr,
    ) -> Vec<SocketAddr> {
        let mut candidates = vec![first];
        let Some(host) = host.filter(|_| self.public_ip.is_none()) else {
            return candidates;
        };
        for ip in self.resolve(host, client_ip).await {
            let addr = SocketAddr::new(ip.to_canonical(), first.port());
            if candidates.len() == MAX_ANSWER_CANDIDATES {
                break;
            }
            if addr.is_ipv4() == first.is_ipv4() && !candidates.contains(&addr) {
                candidates.push(addr);
            }
        }
        candidates
    }

    /// The addresses of `host`: the literal itself, or what the lookup task gives for the name
    /// within [`HOST_LOOKUP_TIMEOUT`]; none when the lookup fails, is refused or takes longer.
    async fn resolve(&self, host: Host, client_ip: IpAddr) -> Vec<IpAddr> {
        let name = match host {
            Host::Ip(ip) => return vec![ip],
            Host::Name(name) => name,
        };
        let (reply, answer) = oneshot::channel();
        let request = HostLookup {
            name: name.clone(),
            ip_key: ip_key(client_ip),
            reply,
        };
        let asked = async {
            self.lookups.send(request).await.ok()?;
            answer.await.ok()
        };
        match tokio::time::timeout(HOST_LOOKUP_TIMEOUT, asked).await {
            Ok(Some(addrs)) => addrs,
            Ok(None) => {
                debug!(host = %name, "nethernet host not looked up");
                Vec::new()
            }
            Err(_) => {
                debug!(host = %name, "nethernet host lookup timed out");
                Vec::new()
            }
        }
    }

    /// Verifies the offer identity: online against the authorization keys through
    /// `Auth::verify_with_refresh`, as the Login check does.
    async fn verify(
        &self,
        assertion: &sdp::Assertion,
        fingerprint: &sdp::Fingerprint,
    ) -> Result<PublicKey, Refusal> {
        let Some(auth) = &self.auth else {
            return identity::verify_offer(
                assertion,
                fingerprint,
                Mode::Offline,
                clock::token_now(),
            )
            .map_err(Refusal::Identity);
        };
        auth.verify_with_refresh(
            |keys| {
                identity::verify_offer(
                    assertion,
                    fingerprint,
                    Mode::Online(keys),
                    clock::token_now(),
                )
            },
            |e| matches!(e, identity::Error::Token(login::Error::UnknownKeyId)),
        )
        .await
        .ok_or(Refusal::Keys)?
        .map_err(Refusal::Identity)
    }
}

/// Why an offer gets no answer SDP. The client sees only [`Refusal::reply`]; the operator reads
/// the cause in the log.
#[derive(Debug)]
enum Refusal {
    /// The offer's SDP breaks a rule.
    Offer(sdp::Error),
    /// An online offer carries no identity.
    NoIdentity,
    /// The offer's identity does not verify.
    Identity(identity::Error),
    /// The authorization keys could not be refreshed for an unknown key ID.
    Keys,
    /// The driver holds too many peers before login, or is stopping.
    Busy,
    /// The server has no UDP socket of the advertised address's family.
    NoSocket,
    /// The driver is gone.
    DriverGone,
    /// The peer could not be made.
    Peer(peer::Error),
    /// Every ICE username fragment drawn was in use.
    Ufrag,
    /// The answer's identity could not be signed.
    Sign(identity::Error),
    /// The answer's session ID could not be drawn.
    Random(crypto::Error),
}

impl Refusal {
    /// What the client gets.
    fn reply(&self) -> Reply {
        match self {
            Self::Offer(_) => Reply::Code(Code::BadOffer),
            Self::NoIdentity | Self::Identity(_) | Self::Keys => Reply::Code(Code::Identity),
            Self::Busy => Reply::Busy,
            Self::NoSocket
            | Self::DriverGone
            | Self::Peer(_)
            | Self::Ufrag
            | Self::Sign(_)
            | Self::Random(_) => Reply::Code(Code::Internal),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offer(_) => f.write_str("the offer breaks a rule"),
            Self::NoIdentity => f.write_str("the offer has no identity"),
            Self::Identity(_) => f.write_str("the offer identity does not verify"),
            Self::Keys => f.write_str("the authorization keys could not be refreshed"),
            Self::Busy => f.write_str("too many peers before login"),
            Self::NoSocket => f.write_str("no udp socket of the advertised address family"),
            Self::DriverGone => f.write_str("the driver is gone"),
            Self::Peer(_) => f.write_str("the peer could not be made"),
            Self::Ufrag => f.write_str("every ice username fragment drawn was in use"),
            Self::Sign(_) => f.write_str("the answer identity could not be signed"),
            Self::Random(_) => f.write_str("the answer session id could not be drawn"),
        }
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Offer(source) => Some(source),
            Self::Identity(source) | Self::Sign(source) => Some(source),
            Self::Peer(source) => Some(source),
            Self::Random(source) => Some(source),
            _ => None,
        }
    }
}

/// A negotiation's request for the addresses of the DNS name its client gave.
#[derive(Debug)]
struct HostLookup {
    name: String,
    /// The signaling client's per-IP key.
    ip_key: u128,
    /// Gets the addresses; dropped unanswered when the lookup is refused.
    reply: AddressReply,
}

/// The lookup task's state: the addresses of recent successful lookups, and the lookups running
/// or waiting, at most one per client IP key and [`MAX_HOST_LOOKUPS`] running at once. Requests
/// for a name already pending share its lookup.
#[derive(Default)]
struct HostLookups {
    /// The addresses of successful lookups by name, with when they expire.
    cache: BTreeMap<String, (Instant, Vec<IpAddr>)>,
    /// The cached names by expiry, the first expiring first.
    expiries: BTreeSet<(Instant, String)>,
    /// The names with a lookup running or waiting: the IP key that asked first, and every reply
    /// waiting for the addresses.
    pending: BTreeMap<String, (u128, Vec<AddressReply>)>,
    /// The IP keys of the pending lookups.
    keys: BTreeSet<u128>,
    /// The pending names whose lookup has not started, oldest first.
    waiting: VecDeque<String>,
    /// The running lookups: the name and what its blocking thread gave.
    running: JoinSet<(String, Resolved)>,
}

/// Where a lookup's addresses go.
type AddressReply = oneshot::Sender<Vec<IpAddr>>;

/// What a lookup's blocking thread gave.
type Resolved = Result<io::Result<Vec<IpAddr>>, JoinError>;

impl HostLookups {
    /// Spawns the lookup task; it ends once every sender is gone, and a lookup still running then
    /// keeps only its blocking thread, until the OS resolver returns.
    fn spawn() -> mpsc::Sender<HostLookup> {
        let (requests, received) = mpsc::channel(LOOKUP_QUEUE);
        tokio::spawn(Self::default().run(received));
        requests
    }

    /// Serves requests and finished lookups until every sender is gone.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a lookup task only awaits its blocking thread and is never aborted"
    )]
    async fn run(mut self, mut requests: mpsc::Receiver<HostLookup>) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    self.request(request, clock::now());
                }
                Some(done) = self.running.join_next(), if !self.running.is_empty() => {
                    let (name, looked_up) = done.expect("a lookup task does not fail");
                    self.finish(name, looked_up, clock::now());
                }
            }
        }
    }

    /// Answers the requests waiting for `name` with what its lookup gave, keeps the addresses
    /// of a success, and starts the next lookups.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a running name stays pending until its lookup finishes"
    )]
    fn finish(
        &mut self,
        name: String,
        looked_up: Result<io::Result<Vec<IpAddr>>, JoinError>,
        now: Instant,
    ) {
        let addrs = match looked_up {
            Ok(Ok(addrs)) => addrs,
            Ok(Err(e)) => {
                debug!(
                    host = %name,
                    error = &e as &(dyn std::error::Error + 'static),
                    "nethernet host lookup failed"
                );
                Vec::new()
            }
            Err(e) => {
                debug!(
                    host = %name,
                    error = &e as &(dyn std::error::Error + 'static),
                    "nethernet host lookup task failed"
                );
                Vec::new()
            }
        };
        let (ip_key, replies) = self
            .pending
            .remove(&name)
            .expect("a running name is pending");
        self.keys.remove(&ip_key);
        for reply in replies {
            if reply.send(addrs.clone()).is_err() {
                trace!(host = %name, "nethernet host lookup withdrawn");
            }
        }
        if !addrs.is_empty() {
            self.keep(name, addrs, now);
        }
        self.start();
    }

    /// Keeps the addresses of `name` for [`HOST_CACHE_LIFETIME`], unless the cache is full.
    fn keep(&mut self, name: String, addrs: Vec<IpAddr>, now: Instant) {
        self.expire(now);
        if self.cache.len() >= MAX_CACHED_HOSTS {
            return;
        }
        let expires = now.checked_add(HOST_CACHE_LIFETIME).unwrap_or(now);
        self.expiries.insert((expires, name.clone()));
        self.cache.insert(name, (expires, addrs));
    }

    /// Answers `request` from the cache, adds it to the pending lookup of its name, or queues a
    /// lookup when its IP key has none pending; otherwise drops its reply.
    fn request(&mut self, request: HostLookup, now: Instant) {
        self.expire(now);
        let HostLookup {
            name,
            ip_key,
            reply,
        } = request;
        if let Some((_, addrs)) = self.cache.get(&name) {
            if reply.send(addrs.clone()).is_err() {
                trace!(host = %name, "nethernet host lookup withdrawn");
            }
            return;
        }
        if let Some((_, replies)) = self.pending.get_mut(&name) {
            replies.push(reply);
            return;
        }
        if !self.keys.insert(ip_key) {
            debug!(host = %name, "nethernet host lookup refused: one is pending for the client");
            return;
        }
        self.pending.insert(name.clone(), (ip_key, vec![reply]));
        self.waiting.push_back(name);
        self.start();
    }

    /// Removes the cached names whose time has come.
    fn expire(&mut self, now: Instant) {
        while self
            .expiries
            .first()
            .is_some_and(|(expires, _)| *expires <= now)
            && let Some((_, name)) = self.expiries.pop_first()
        {
            self.cache.remove(&name);
        }
    }

    /// Starts waiting lookups while fewer than [`MAX_HOST_LOOKUPS`] run. A lookup nobody waits
    /// for any more is dropped instead.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a waiting name stays pending until its lookup finishes"
    )]
    fn start(&mut self) {
        while self.running.len() < MAX_HOST_LOOKUPS
            && let Some(name) = self.waiting.pop_front()
        {
            let (ip_key, replies) = self
                .pending
                .get_mut(&name)
                .expect("a waiting name is pending");
            replies.retain(|reply| !reply.is_closed());
            if replies.is_empty() {
                let ip_key = *ip_key;
                self.pending.remove(&name);
                self.keys.remove(&ip_key);
                continue;
            }
            let looked_up = name.clone();
            self.running.spawn(async move {
                let addrs = tokio::task::spawn_blocking(move || lookup(&looked_up)).await;
                (name, addrs)
            });
        }
    }
}

/// The driver's side of an accepted connection.
#[derive(Debug)]
struct Link {
    inbox: Inbox,
    signal: watch::Receiver<Flags>,
    ping: watch::Sender<Option<Duration>>,
    undelivered: watch::Sender<usize>,
    /// Set when the peer reports why it closed; a close the driver causes keeps the initial
    /// [`LeaveReason::ConnectionClosed`].
    reason: watch::Sender<LeaveReason>,
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
    max_message_bytes: usize,
    peers: BTreeMap<u64, Entry>,
    by_ufrag: BTreeMap<String, u64>,
    by_addr: BTreeMap<SocketAddr, u64>,
    /// Peers before login per IP key.
    pre_login: BTreeMap<u128, usize>,
    pre_login_total: usize,
    /// The peers the session closed, as (end of linger, id): the first is the oldest.
    closing_peers: BTreeSet<(Instant, u64)>,
    next_id: u64,
    accepted: mpsc::Sender<Connection>,
    /// Handed to each connection and the offer task.
    commands: mpsc::Sender<Command>,
    pending: mpsc::Receiver<Command>,
    doomed: Vec<u64>,
    /// The peers that need a poll in the timer pass.
    due: Vec<u64>,
    /// Set once the driver is stopping: new offers are refused.
    stopping: bool,
}

impl Driver {
    /// Whether no peer is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Handles a datagram from `from`.
    pub(crate) fn receive(&mut self, from: SocketAddr, datagram: &[u8]) {
        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
        let id = match peer::stun_server_ufrag(datagram) {
            Some(ufrag) => self.by_ufrag.get(ufrag).copied(),
            None => self.by_addr.get(&from).copied(),
        };
        let Some(id) = id else {
            trace!(addr = %from, "nethernet datagram for no peer dropped");
            return;
        };
        let Some(entry) = self.peers.get_mut(&id) else {
            return;
        };
        if !admit(&mut self.by_addr, &mut entry.addrs, id, from) {
            trace!(addr = %from, "nethernet datagram from a refused address dropped");
            return;
        }
        let now = clock::now();
        // The peer must be polled before the next datagram reaches it (peer.rs).
        entry.peer.receive(now, from, datagram);
        self.drive(id, now);
        self.reap();
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

    /// Stops the driver: new offers are refused, peers without a connection go at once, the
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
                entry.peer.stop_reading();
            }
        }
        self.reap();
    }

    /// Runs the timer's work.
    pub(crate) fn tick(&mut self) {
        self.update();
    }

    fn new(
        sockets: Arc<Sockets>,
        certificate: Certificate,
        max_message_bytes: usize,
        accepted: mpsc::Sender<Connection>,
    ) -> Self {
        let (commands, pending) = mpsc::channel(COMMAND_QUEUE);
        Self {
            sockets,
            certificate,
            max_message_bytes,
            peers: BTreeMap::new(),
            by_ufrag: BTreeMap::new(),
            by_addr: BTreeMap::new(),
            pre_login: BTreeMap::new(),
            pre_login_total: 0,
            closing_peers: BTreeSet::new(),
            next_id: 0,
            accepted,
            commands,
            pending,
            doomed: Vec::new(),
            due: Vec::new(),
            stopping: false,
        }
    }

    /// Whether a peer from `ip` fits the pre-login limits and the driver is not stopping.
    fn has_room(&self, ip: IpAddr) -> bool {
        let per_ip = self.pre_login.get(&ip_key(ip)).copied().unwrap_or(0);
        !self.stopping && self.pre_login_total < MAX_PRE_LOGIN && per_ip < MAX_PRE_LOGIN_PER_IP
    }

    /// Runs the timer pass: reads the connections' flags, applies the commands queued before
    /// them, then removes the closing peers that delivered everything and drives those whose
    /// time came.
    fn update(&mut self) {
        let now = clock::now();
        self.read_flags(now);
        self.drain();
        for (&id, entry) in &mut self.peers {
            // A closing peer stays until what was sent is acknowledged, or the linger ends.
            if let Some(until) = entry.closing
                && (now >= until || entry.peer.is_drained())
            {
                self.doomed.push(id);
            } else if now >= entry.timeout {
                self.due.push(id);
            }
        }
        while let Some(id) = self.due.pop() {
            self.drive(id, now);
        }
        self.reap();
    }

    /// Applies what sessions queued, at most a full queue. A session queues its last packets
    /// before it sets its close flag, so the timer and stop passes call this after they read the
    /// flags and before they judge a closing peer drained: it must have those packets by then.
    fn drain(&mut self) {
        for _ in 0..COMMAND_QUEUE {
            let Ok(command) = self.pending.try_recv() else {
                break;
            };
            self.command(command);
        }
    }

    /// Applies one command.
    fn command(&mut self, command: Command) {
        let now = clock::now();
        match command {
            Command::Admit { ip, reply } => {
                // An offer task that is gone no longer needs the answer.
                if reply.send(self.has_room(ip)).is_err() {
                    trace!("nethernet admission question withdrawn");
                }
            }
            Command::Open {
                remote,
                local,
                client_key,
                ip,
                reply,
            } => {
                let result = self.open(&remote, local, client_key, ip, now);
                if let Err(unsent) = reply.send(result) {
                    // The offer task is gone: nobody answers the client, so the peer would
                    // only hold its pre-login place until its deadline.
                    trace!("nethernet offer withdrawn");
                    if let Ok(made) = unsent
                        && let Some(&id) = self.by_ufrag.get(&made.ice_ufrag)
                    {
                        self.doomed.push(id);
                        self.reap();
                    }
                }
            }
            Command::Send { id, message } => {
                let Some(entry) = self.peers.get_mut(&id) else {
                    return;
                };
                if let Err(e) = entry.peer.send(message) {
                    debug!(
                        connection = id,
                        error = &e as &(dyn std::error::Error + 'static),
                        "nethernet send failed"
                    );
                    self.doomed.push(id);
                }
                self.drive(id, now);
                self.reap();
            }
        }
    }

    /// Makes the peer for an offer, or says why it cannot.
    fn open(
        &mut self,
        remote: &peer::Remote,
        local: SocketAddr,
        client_key: Option<PublicKey>,
        ip: IpAddr,
        now: Instant,
    ) -> Result<peer::Local, Refusal> {
        if !self.has_room(ip) {
            return Err(Refusal::Busy);
        }
        let key = ip_key(ip);
        let mut made = None;
        for _ in 0..UFRAG_TRIES {
            let peer = Peer::new(
                remote.clone(),
                local,
                &self.certificate,
                self.max_message_bytes.min(MAX_PRE_LOGIN_MESSAGE_BYTES),
                now,
            )
            .map_err(Refusal::Peer)?;
            if !self.by_ufrag.contains_key(&peer.local().ice_ufrag) {
                made = Some(peer);
                break;
            }
        }
        let peer = made.ok_or(Refusal::Ufrag)?;
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
                peer::Output::Transmit { to, data } => transmit(&self.sockets, to, &data),
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
                        self.max_message_bytes,
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
                        debug!(connection = id, "nethernet session not keeping up; closing");
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
                    debug!(
                        connection = id,
                        ufrag = %entry.ufrag,
                        error = &reason as &(dyn std::error::Error + 'static),
                        "nethernet peer closed"
                    );
                    if let Some(link) = &entry.link {
                        link.reason.send_replace(reason.leave_reason());
                    }
                    self.doomed.push(id);
                    return;
                }
            }
        }
    }

    /// Reads the connections' flags. A peer the session closed stops reading and joins the
    /// closing peers; when more than `MAX_CLOSING_PEERS` are left, the oldest go at once.
    fn read_flags(&mut self, now: Instant) {
        for (&id, entry) in &mut self.peers {
            if let Some(link) = &mut entry.link
                && let Some(flags) = Flags::changed(&mut link.signal)
            {
                if flags.logged_in
                    && let Some(key) = entry.pre_login.take()
                {
                    release(&mut self.pre_login, &mut self.pre_login_total, key);
                    entry.peer.set_max_message_bytes(self.max_message_bytes);
                    link.inbox.raise_to_login_budget();
                }
                if flags.closed && entry.closing.is_none() {
                    let until = now.checked_add(CLOSE_LINGER).unwrap_or(now);
                    entry.closing = Some(until);
                    entry.peer.stop_reading();
                    self.closing_peers.insert((until, id));
                }
            }
        }
        while self.closing_peers.len() > MAX_CLOSING_PEERS
            && let Some((_, id)) = self.closing_peers.pop_first()
        {
            self.doomed.push(id);
        }
        self.reap();
    }

    /// Removes the peers marked for closing.
    fn reap(&mut self) {
        while let Some(id) = self.doomed.pop() {
            self.remove(id);
        }
    }

    /// Removes a peer and its index entries.
    fn remove(&mut self, id: u64) {
        let Some(mut entry) = self.peers.remove(&id) else {
            return;
        };
        debug!(connection = id, "nethernet peer removed");
        if let Some(until) = entry.closing {
            self.closing_peers.remove(&(until, id));
        }
        entry.peer.close();
        let now = clock::now();
        // Flush what closing produced (SCTP abort, DTLS close notify) without keeping the peer.
        while let peer::Output::Transmit { to, data } = entry.peer.poll(now) {
            transmit(&self.sockets, to, &data);
        }
        self.by_ufrag.remove(&entry.ufrag);
        forget(&mut self.by_addr, &entry.addrs);
        if let Some(key) = entry.pre_login {
            release(&mut self.pre_login, &mut self.pre_login_total, key);
        }
    }
}

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
) -> crate::Result<(Handle, Driver)> {
    let path = data_dir.join(IDENTITY_FILE);
    let identity = tokio::task::spawn_blocking(move || ServerIdentity::load_or_create(&path))
        .await
        .map_err(|e| crate::Error::Start(io::Error::other(e)))?
        .map_err(|e| crate::Error::IdentityKey(Box::new(e)))?;
    let tls = match settings.tls {
        Some((cert, key)) => Some(
            tokio::task::spawn_blocking(move || signaling::load_tls(&cert, &key))
                .await
                .map_err(|e| crate::Error::Start(io::Error::other(e)))?
                .map_err(|e| crate::Error::TlsCertificate(Box::new(e)))?,
        ),
        None => None,
    };
    // One certificate for every peer, made now so that no join pays for it.
    let certificate =
        Certificate::generate().map_err(|e| crate::Error::Start(io::Error::other(e)))?;
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
    .map_err(|e| match e {
        signaling::Error::Bind { addr, source } => crate::Error::Network {
            socket: signaling_socket(addr),
            source,
        },
        e => crate::Error::Start(io::Error::other(e)),
    })?;
    let driver = Driver::new(sockets, certificate, settings.max_message_bytes, accepted);
    let offers = Offers {
        identity: Arc::new(identity),
        auth,
        public_ip: settings.public_ip,
        udp_addrs,
        driver: driver.commands.clone(),
        lookups: HostLookups::spawn(),
    };
    let offers = tokio::spawn(offers.run(offers_rx));
    Ok((Handle { signaling, offers }, driver))
}

/// The signaling socket on `addr` as an operator reads it: protocol, address, the settings that
/// decide it and its use.
fn signaling_socket(addr: SocketAddr) -> String {
    let settings = if addr.is_ipv6() {
        "server-port, enable-ipv6"
    } else {
        "server-port"
    };
    format!("TCP {addr} ({settings}, NetherNet signaling)")
}

/// The first [`MAX_LOOKUP_ADDRS`] IP addresses the OS resolver gives for `name`. Blocking: call
/// it on the blocking pool.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the lookup task runs it on the blocking pool"
)]
fn lookup(name: &str) -> io::Result<Vec<IpAddr>> {
    Ok((name, 0)
        .to_socket_addrs()?
        .map(|addr| addr.ip())
        .take(MAX_LOOKUP_ADDRS)
        .collect())
}

/// Whether a datagram on the shared UDP port is NetherNet's (RFC 7983 demultiplexing): DTLS,
/// first byte 20 to 63, or STUN, first byte 0 or 1 with the magic cookie and a length that
/// matches. A datagram RakNet takes never starts with 20 to 63 or 0; its ping, the one starting
/// with 1, passes as STUN only if the client's timestamp spells the cookie and the length, and a
/// client that crafts that loses only its own ping. A real ping is 33 bytes, a length STUN's
/// 4-byte attribute padding never gives.
pub(crate) fn is_nethernet(datagram: &[u8]) -> bool {
    match datagram {
        [first, ..] if DTLS_FIRST_BYTE_RANGE.contains(first) => true,
        [first, _, l0, l1, c0, c1, c2, c3, ..] if STUN_FIRST_BYTE_RANGE.contains(first) => {
            let length = usize::from(u16::from_be_bytes([*l0, *l1]));
            [*c0, *c1, *c2, *c3] == STUN_MAGIC_COOKIE
                && length % 4 == 0
                && datagram.len().checked_sub(STUN_HEADER_LEN) == Some(length)
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

/// Sends a peer's datagram. A full send buffer drops it; SCTP resends what matters.
fn transmit(sockets: &Sockets, to: SocketAddr, data: &[u8]) {
    if let Err(error) = sockets.send(to, data) {
        trace!(
            addr = %to,
            error = &error as &(dyn std::error::Error + 'static),
            "nethernet datagram not sent"
        );
    }
}

/// Hands a newly open peer to the session layer; `None` when the accept queue is full.
fn accept(
    accepted: &mpsc::Sender<Connection>,
    commands: &mpsc::Sender<Command>,
    id: u64,
    addr: SocketAddr,
    entry: &mut Entry,
    max_message_bytes: usize,
) -> Option<Link> {
    let (inbox, receiver) = Inbox::new(max_message_bytes);
    let (signal, watcher) = watch::channel(Flags::default());
    let (ping, pings) = watch::channel(None);
    let (undelivered, undelivered_watcher) = watch::channel(0);
    let (reason, reason_watcher) = watch::channel(LeaveReason::ConnectionClosed);
    let connection = Connection {
        id,
        addr,
        client_key: entry.client_key.take(),
        inbound: receiver,
        commands: commands.clone(),
        signal,
        ping: pings,
        undelivered: undelivered_watcher,
        reason: reason_watcher,
    };
    if let Err(error) = accepted.try_send(connection) {
        debug!(
            %addr,
            error = &error as &(dyn std::error::Error + 'static),
            "nethernet connection not accepted"
        );
        return None;
    }
    Some(Link {
        inbox,
        signal: watcher,
        ping,
        undelivered,
        reason,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::net::{Ipv4Addr, Ipv6Addr};

    use str0m::net::{Protocol, Receive};
    use str0m::{DATAGRAM_MTU_TARGET, Input};
    use tokio::net::{TcpListener, UdpSocket};

    use crate::raknet::tests::run;
    use crate::save::tests::TempDir;
    use crate::udp::{self, RECV_BUFFER_BYTES, UPDATE_INTERVAL};
    use peer::tests::{Client, MAX_MESSAGE_BYTES, certificates, reliable, unreliable};

    /// The id of the first test peer in the address index test.
    const FIRST_PEER: u64 = 1;
    /// The id of the second test peer in the address index test.
    const SECOND_PEER: u64 = 2;
    /// The longest a test waits for the network.
    const WAIT: Duration = Duration::from_secs(20);

    /// A driver on `sockets`, and the connections it accepts.
    pub(crate) fn driver(sockets: Arc<Sockets>) -> (Driver, mpsc::Receiver<Connection>) {
        let (accepted, connections) = mpsc::channel(1);
        let driver = Driver::new(sockets, certificates().0, MAX_MESSAGE_BYTES, accepted);
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
        let mut buf = [0; RECV_BUFFER_BYTES];
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
        let mut buf = [0; RECV_BUFFER_BYTES];
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
        let handle = udp::spawn(sockets, raknet, Some(nethernet));
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

    #[test]
    fn shutdown_delivers_the_last_messages_of_a_peer() {
        run(shutdown_delivers_the_last_message());
    }

    /// A message a session queued before it set its close flag is applied before the driver
    /// judges the closing peer drained: the timer pass reads the flag first and drains the queue
    /// after it.
    #[test]
    fn the_messages_queued_before_a_close_flag_are_applied_before_the_peer_is_judged() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (mut driver, mut connections) = driver(sockets);
            let (_, client_cert) = certificates();
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 50_000));
            let fingerprint = *client_cert.fingerprint();
            let (_client, remote) = Client::new(
                &client_cert,
                addr,
                DATAGRAM_MTU_TARGET,
                fingerprint,
                clock::now(),
            );
            let (reply, answer) = oneshot::channel();
            driver.command(Command::Open {
                remote,
                local: addr,
                client_key: None,
                ip: addr.ip(),
                reply,
            });
            answer.await.unwrap().unwrap();
            // The driver hands the session its connection when both channels are open; here
            // the connection is made by hand.
            let (&id, entry) = driver.peers.iter_mut().next().unwrap();
            entry.link = accept(
                &driver.accepted,
                &driver.commands,
                id,
                addr,
                entry,
                MAX_MESSAGE_BYTES,
            );
            let connection = connections.try_recv().unwrap();

            connection.send(b"bye".to_vec()).await.unwrap();
            connection.close();
            driver.update();

            assert!(
                driver.pending.try_recv().is_err(),
                "the message was not applied"
            );
        });
    }

    /// An offer whose answer nobody waits for leaves no peer holding a pre-login place.
    #[test]
    fn an_offer_withdrawn_before_its_answer_leaves_no_peer() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (mut driver, _connections) = driver(sockets);
            let (_, client_cert) = certificates();
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 50_000));
            let fingerprint = *client_cert.fingerprint();
            let (_client, remote) = Client::new(
                &client_cert,
                addr,
                DATAGRAM_MTU_TARGET,
                fingerprint,
                clock::now(),
            );
            let (reply, answer) = oneshot::channel();
            drop(answer);
            driver.command(Command::Open {
                remote,
                local: addr,
                client_key: None,
                ip: addr.ip(),
                reply,
            });
            assert!(driver.peers.is_empty());
            assert_eq!(driver.pre_login_total, 0);
            assert!(driver.by_ufrag.is_empty());
        });
    }

    /// Pre-login peers are capped per IP and in total, and removing them gives every place and
    /// index entry back.
    #[test]
    fn pre_login_peers_are_capped_and_give_their_places_back() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (mut driver, _connections) = driver(sockets);
            let (_, client_cert) = certificates();
            let fingerprint = *client_cert.fingerprint();
            let open = |driver: &mut Driver, last: u8| {
                let addr = SocketAddr::from((Ipv4Addr::new(10, 0, 0, last), 50_000));
                let (_client, remote) = Client::new(
                    &client_cert,
                    addr,
                    DATAGRAM_MTU_TARGET,
                    fingerprint,
                    clock::now(),
                );
                let (reply, mut answer) = oneshot::channel();
                driver.command(Command::Open {
                    remote,
                    local: addr,
                    client_key: None,
                    ip: addr.ip(),
                    reply,
                });
                answer.try_recv().unwrap()
            };
            for last in 0..u8::try_from(MAX_PRE_LOGIN / MAX_PRE_LOGIN_PER_IP).unwrap() {
                for _ in 0..MAX_PRE_LOGIN_PER_IP {
                    assert!(open(&mut driver, last).is_ok());
                }
            }
            assert!(matches!(open(&mut driver, 0), Err(Refusal::Busy)));
            assert!(matches!(open(&mut driver, 200), Err(Refusal::Busy)));
            assert_eq!(driver.peers.len(), MAX_PRE_LOGIN);
            assert_eq!(driver.by_ufrag.len(), MAX_PRE_LOGIN);
            assert_eq!(driver.pre_login_total, MAX_PRE_LOGIN);

            driver.doomed.extend(driver.peers.keys().copied());
            driver.reap();
            assert!(driver.peers.is_empty());
            assert!(driver.by_ufrag.is_empty());
            assert!(driver.pre_login.is_empty());
            assert_eq!(driver.pre_login_total, 0);
        });
    }

    /// Closing peers are capped, the oldest go first, and once they delivered everything every
    /// table is empty again.
    #[test]
    fn closing_peers_are_capped_and_give_their_places_back() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (mut driver, mut connections) = driver(sockets);
            let (_, client_cert) = certificates();
            let fingerprint = *client_cert.fingerprint();
            let total = MAX_CLOSING_PEERS + 6;
            let mut open = Vec::new();
            for index in 0..total {
                let last = u8::try_from(index).unwrap();
                let addr = SocketAddr::from((Ipv4Addr::new(10, 0, 0, last), 50_000));
                let (_client, remote) = Client::new(
                    &client_cert,
                    addr,
                    DATAGRAM_MTU_TARGET,
                    fingerprint,
                    clock::now(),
                );
                let (reply, mut answer) = oneshot::channel();
                driver.command(Command::Open {
                    remote,
                    local: addr,
                    client_key: None,
                    ip: addr.ip(),
                    reply,
                });
                answer.try_recv().unwrap().unwrap();
                let id = driver.next_id - 1;
                let entry = driver.peers.get_mut(&id).unwrap();
                entry.link = accept(
                    &driver.accepted,
                    &driver.commands,
                    id,
                    addr,
                    entry,
                    MAX_MESSAGE_BYTES,
                );
                let connection = connections.try_recv().unwrap();
                connection.logged_in();
                driver.read_flags(clock::now());
                open.push(connection);
            }
            assert_eq!(driver.peers.len(), total);
            for connection in &open {
                connection.close();
            }
            driver.read_flags(clock::now());
            assert_eq!(driver.peers.len(), MAX_CLOSING_PEERS);
            assert_eq!(driver.closing_peers.len(), MAX_CLOSING_PEERS);
            // The oldest closed first: the survivors are the last ones opened.
            assert!(!driver.peers.contains_key(&0));
            assert!(
                driver
                    .peers
                    .contains_key(&u64::try_from(total - 1).unwrap())
            );

            driver.update();
            assert!(driver.peers.is_empty());
            assert!(driver.closing_peers.is_empty());
            assert!(driver.by_ufrag.is_empty());
            assert!(driver.by_addr.is_empty());
            assert_eq!(driver.pre_login_total, 0);
        });
    }

    /// One peer holds at most `MAX_PEER_ADDRS` source addresses, however many a client claims,
    /// and removing it gives them all back.
    #[test]
    fn a_peer_holds_at_most_a_few_source_addresses_and_gives_them_back() {
        let mut by_addr = BTreeMap::new();
        let mut addrs = Vec::new();
        for port in 0..1000 {
            let from = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 50_000 + port));
            admit(&mut by_addr, &mut addrs, FIRST_PEER, from);
        }
        assert_eq!(addrs.len(), MAX_PEER_ADDRS);
        assert_eq!(by_addr.len(), MAX_PEER_ADDRS);
        forget(&mut by_addr, &addrs);
        assert!(by_addr.is_empty());
    }

    /// An offer offline, without an identity, as a client behind a NAT sends it.
    fn offer_without_identity() -> String {
        let identity = format!("{}\n", sdp::tests::identity_line());
        sdp::tests::OFFER.replace(&identity, "")
    }

    async fn negotiation_ends_when_its_request_is_gone() {
        let (driver, mut commands) = mpsc::channel(1);
        let server = SocketAddr::from((Ipv4Addr::LOCALHOST, 19_133));
        let offers = Arc::new(Offers {
            identity: Arc::new(ServerIdentity::generate().unwrap()),
            auth: None,
            public_ip: None,
            udp_addrs: vec![server],
            driver,
            lookups: HostLookups::spawn(),
        });
        let (reply, replied) = oneshot::channel();
        let offer = Offer {
            network_id: 1,
            // Offline, an offer without an identity goes on to the driver.
            sdp: offer_without_identity(),
            peer: SocketAddr::from((Ipv4Addr::LOCALHOST, 50_000)),
            local: server,
            host: None,
            reply,
        };
        let task = tokio::spawn(offers.answer(offer));
        let admit = tokio::time::timeout(WAIT, commands.recv()).await.unwrap();
        let Some(Command::Admit { reply, .. }) = admit else {
            panic!("the offer did not ask for admission");
        };
        reply.send(true).unwrap();
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

    /// A negotiation ends once its signaling request is gone, whatever it waits for, so the
    /// negotiations never outnumber the signaling connections.
    #[test]
    fn a_negotiation_ends_when_its_signaling_request_is_gone() {
        run(negotiation_ends_when_its_request_is_gone());
    }

    /// An offer is refused with a reason the log shows, and the client gets the code of that
    /// reason.
    #[test]
    fn an_offer_is_refused_with_its_reason() {
        run(async {
            let (driver, mut commands) = mpsc::channel(1);
            let admitting = tokio::spawn(async move {
                while let Some(Command::Admit { reply, .. }) = commands.recv().await {
                    reply.send(true).unwrap();
                }
            });
            let offers = Offers {
                identity: Arc::new(ServerIdentity::generate().unwrap()),
                auth: None,
                public_ip: None,
                udp_addrs: Vec::new(),
                driver,
                lookups: HostLookups::spawn(),
            };
            let client = IpAddr::V4(Ipv4Addr::LOCALHOST);

            let refusal = offers
                .negotiate("v=1\r\n", client, client, None)
                .await
                .unwrap_err();
            assert!(matches!(refusal, Refusal::Offer(sdp::Error::Malformed(_))));
            assert_eq!(refusal.reply(), Reply::Code(Code::BadOffer));

            // The identity signs the fingerprint as written, so another digest fails it.
            let offer = sdp::tests::OFFER.replace("8a:74:fb", "8a:74:fc");
            let refusal = offers
                .negotiate(&offer, client, client, None)
                .await
                .unwrap_err();
            assert!(matches!(refusal, Refusal::Identity(_)), "{refusal:?}");
            assert_eq!(refusal.reply(), Reply::Code(Code::Identity));

            // Offline, an offer without an identity passes, and the server has no UDP socket
            // to advertise.
            let offer = offer_without_identity();
            let refusal = offers
                .negotiate(&offer, client, client, None)
                .await
                .unwrap_err();
            assert!(matches!(refusal, Refusal::NoSocket), "{refusal:?}");
            assert_eq!(refusal.reply(), Reply::Code(Code::Internal));
            admitting.abort();
        });
    }

    /// An offer from an IP whose pre-login places are full is refused before its identity is
    /// verified: an offer with a bad identity gets the busy refusal, not the identity one.
    #[test]
    fn an_offer_over_the_pre_login_limits_is_refused_before_it_is_verified() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (mut driver, _connections) = driver(sockets);
            let (_, client_cert) = certificates();
            let fingerprint = *client_cert.fingerprint();
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 50_000));
            for _ in 0..MAX_PRE_LOGIN_PER_IP {
                let (_client, remote) = Client::new(
                    &client_cert,
                    addr,
                    DATAGRAM_MTU_TARGET,
                    fingerprint,
                    clock::now(),
                );
                let (reply, mut answer) = oneshot::channel();
                driver.command(Command::Open {
                    remote,
                    local: addr,
                    client_key: None,
                    ip: addr.ip(),
                    reply,
                });
                answer.try_recv().unwrap().unwrap();
            }
            let offers = Offers {
                identity: Arc::new(ServerIdentity::generate().unwrap()),
                auth: None,
                public_ip: None,
                udp_addrs: vec![addr],
                driver: driver.commands.clone(),
                lookups: HostLookups::spawn(),
            };
            let serving = tokio::spawn(async move { while driver.serve().await {} });

            // The identity signs the fingerprint as written, so another digest fails it.
            let offer = sdp::tests::OFFER.replace("8a:74:fb", "8a:74:fc");
            let refusal = offers
                .negotiate(&offer, addr.ip(), addr.ip(), None)
                .await
                .unwrap_err();
            assert!(matches!(refusal, Refusal::Busy), "{refusal:?}");
            assert_eq!(refusal.reply(), Reply::Busy);
            serving.abort();
        });
    }

    /// Offers answered by a serving driver on a loopback UDP port, with `public_ip`; the port.
    fn serving_offers(public_ip: Option<IpAddr>) -> (Offers, u16, JoinHandle<()>) {
        let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let sockets = Sockets::bind(Some(localhost), None).unwrap();
        let port = sockets.local_addrs()[0].port();
        let (mut driver, _connections) = driver(sockets);
        let offers = Offers {
            identity: Arc::new(ServerIdentity::generate().unwrap()),
            auth: None,
            public_ip,
            udp_addrs: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))],
            driver: driver.commands.clone(),
            lookups: HostLookups::spawn(),
        };
        let serving = tokio::spawn(async move { while driver.serve().await {} });
        (offers, port, serving)
    }

    /// The `a=candidate` addresses of an answer.
    fn answer_candidates(answer: &str) -> Vec<String> {
        answer
            .lines()
            .filter_map(|line| line.strip_prefix("a=candidate:"))
            .map(|candidate| {
                let fields: Vec<&str> = candidate.split(' ').collect();
                format!("{}:{}", fields[4], fields[5])
            })
            .collect()
    }

    /// Behind a NAT the address a client reached the signaling server on is private, so the
    /// answer also advertises the host the client was given, at the UDP port: a client outside
    /// can reach that one.
    #[test]
    fn an_answer_advertises_the_host_the_client_was_given() {
        run(async {
            let (offers, port, serving) = serving_offers(None);
            let reached = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 160));
            let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
            let given = Host::Ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
            let answer = offers
                .negotiate(&offer_without_identity(), client, reached, Some(given))
                .await
                .unwrap();
            assert_eq!(
                answer_candidates(&answer),
                [
                    format!("192.168.0.160:{port}"),
                    format!("203.0.113.7:{port}")
                ]
            );
            assert!(answer.contains("\r\nc=IN IP4 192.168.0.160\r\n"));
            serving.abort();
        });
    }

    /// The host's addresses join the first candidate only when they are new, of its family and
    /// within the candidate limit; a DNS name is looked up; `public-ip` stays the only one.
    #[test]
    fn host_addresses_join_only_new_candidates_of_one_family() {
        run(async {
            let (offers, port, serving) = serving_offers(None);
            let first = SocketAddr::from((Ipv4Addr::new(192, 168, 0, 160), port));
            let at = |ip: IpAddr| SocketAddr::new(ip, port);
            let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
            let same = Host::Ip(first.ip());
            assert_eq!(offers.candidates(first, Some(same), client).await, [first]);
            let v6 = Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
            assert_eq!(offers.candidates(first, Some(v6), client).await, [first]);
            assert_eq!(offers.candidates(first, None, client).await, [first]);
            let name = Host::Name("localhost".to_owned());
            let loopback = at(IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert_eq!(
                offers.candidates(first, Some(name), client).await,
                [first, loopback]
            );
            let mapped = Host::Ip(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()));
            assert_eq!(
                offers.candidates(first, Some(mapped), client).await,
                [first, loopback]
            );
            serving.abort();

            let public = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
            let (offers, port, serving) = serving_offers(Some(public));
            let first = offers.advertised(first.ip()).unwrap();
            assert_eq!(first, SocketAddr::new(public, port));
            let given = Host::Ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
            assert_eq!(offers.candidates(first, Some(given), client).await, [first]);
            serving.abort();
        });
    }

    /// A request for the addresses of `name` from the client IP key `ip_key`, and where its
    /// answer arrives.
    fn host_lookup(name: &str, ip_key: u128) -> (HostLookup, oneshot::Receiver<Vec<IpAddr>>) {
        let (reply, answer) = oneshot::channel();
        let request = HostLookup {
            name: name.to_owned(),
            ip_key,
            reply,
        };
        (request, answer)
    }

    /// One client IP key runs or waits for one lookup at a time, so names whose lookups hang
    /// cannot take every lookup from one address; another client asking for the same name
    /// shares the lookup, and a finished lookup frees the key.
    #[test]
    fn a_client_ip_has_one_host_lookup_pending_at_a_time() {
        run(async {
            let mut lookups = HostLookups::default();
            let now = clock::now();
            let (first, mut first_answer) = host_lookup("localhost", 1);
            lookups.request(first, now);
            let (second, mut second_answer) = host_lookup("other.example", 1);
            lookups.request(second, now);
            assert!(matches!(
                second_answer.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ));
            let (shared, mut shared_answer) = host_lookup("localhost", 2);
            lookups.request(shared, now);
            assert_eq!(lookups.running.len(), 1);
            assert_eq!(lookups.keys, BTreeSet::from([1]));

            let (name, looked_up) = lookups.running.join_next().await.unwrap().unwrap();
            lookups.finish(name, looked_up, now);
            let addrs = first_answer.try_recv().unwrap();
            assert!(
                addrs.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
                "{addrs:?}"
            );
            assert_eq!(shared_answer.try_recv().unwrap(), addrs);
            assert!(lookups.pending.is_empty());
            assert!(lookups.keys.is_empty());
            assert!(lookups.waiting.is_empty());
        });
    }

    /// A name that resolved is answered from the cache without a lookup, from any client,
    /// until its time is over; then it is looked up again.
    #[test]
    fn a_resolved_host_is_answered_from_the_cache_until_it_expires() {
        run(async {
            let mut lookups = HostLookups::default();
            let now = clock::now();
            let (first, _first_answer) = host_lookup("localhost", 1);
            lookups.request(first, now);
            let (name, looked_up) = lookups.running.join_next().await.unwrap().unwrap();
            lookups.finish(name, looked_up, now);

            let (cached, mut cached_answer) = host_lookup("localhost", 2);
            lookups.request(cached, now);
            assert!(lookups.running.is_empty());
            let addrs = cached_answer.try_recv().unwrap();
            assert!(
                addrs.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
                "{addrs:?}"
            );

            let later = now + HOST_CACHE_LIFETIME;
            let (expired, _expired_answer) = host_lookup("localhost", 2);
            lookups.request(expired, later);
            assert_eq!(lookups.running.len(), 1);
            assert!(lookups.cache.is_empty());
            assert!(lookups.expiries.is_empty());
        });
    }

    /// While the cache is full, a new name's addresses are not kept, so names a client makes up
    /// cannot push out the names already kept.
    #[test]
    fn a_full_host_cache_keeps_its_names() {
        let mut lookups = HostLookups::default();
        let now = clock::now();
        let addr = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
        for n in 0..MAX_CACHED_HOSTS {
            lookups.keep(format!("host{n}.example"), addr.clone(), now);
        }
        lookups.keep("new.example".to_owned(), addr, now);
        assert_eq!(lookups.cache.len(), MAX_CACHED_HOSTS);
        assert!(lookups.cache.contains_key("host0.example"));
        assert!(!lookups.cache.contains_key("new.example"));
    }

    /// A signaling port that is taken stops the start with the socket named.
    #[test]
    fn a_taken_signaling_port_is_reported_with_its_socket() {
        run(async {
            let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = taken.local_addr().unwrap().port();
            let dir = TempDir::new("nethernet-start");
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = Sockets::bind(Some(localhost), None).unwrap();
            let (accepted, _connections) = mpsc::channel(1);
            let (_status, statuses) = watch::channel(Arc::<[u8]>::from(&b"{}"[..]));
            let settings = Settings {
                signaling_v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
                signaling_v6: None,
                public_ip: None,
                max_message_bytes: MAX_MESSAGE_BYTES,
                tls: None,
            };
            let started = start(settings, &dir.0, statuses, None, accepted, sockets).await;
            let Err(crate::Error::Network { socket, .. }) = started else {
                panic!("the taken port was bound twice");
            };
            assert_eq!(
                socket,
                format!("TCP 127.0.0.1:{port} (server-port, NetherNet signaling)")
            );
        });
    }

    #[test]
    fn stun_from_another_peers_address_does_not_steal_it() {
        let victim = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 50_000));
        let mut by_addr = BTreeMap::new();
        let mut first_addrs = Vec::new();
        let mut second_addrs = Vec::new();
        assert!(admit(&mut by_addr, &mut first_addrs, FIRST_PEER, victim));
        // The second peer's own ufrag in a STUN request with the victim's address as its source.
        assert!(!admit(&mut by_addr, &mut second_addrs, SECOND_PEER, victim));
        assert_eq!(by_addr.get(&victim), Some(&FIRST_PEER));
        assert!(second_addrs.is_empty());
        forget(&mut by_addr, &second_addrs);
        assert_eq!(by_addr.get(&victim), Some(&FIRST_PEER));
        // The first peer's address stays usable by it.
        assert!(admit(&mut by_addr, &mut first_addrs, FIRST_PEER, victim));
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

    /// A NetherNet peer and a RakNet ping reach their own drivers through the one UDP port, so
    /// a server needs only `server-port` open.
    #[test]
    fn nethernet_and_raknet_share_the_udp_port() {
        run(share_the_udp_port());
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
        for first in DTLS_FIRST_BYTE_RANGE {
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
