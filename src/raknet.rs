//! Server-side RakNet: the protocol core and its driver.
//!
//! `wire`, `peer` and `listener` are the core, free of sockets and clocks; the driver feeds it
//! the datagrams the shared UDP task (`udp`) hands over, runs its timers there and hands finished
//! connections to the session layer. The 0xfe game message byte is added and removed here.
//! The session link types here serve the NetherNet driver too.

mod listener;
mod peer;
mod wire;

pub(crate) use listener::ip_key;
pub(crate) use peer::MAX_MTU;

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tracing::{debug, trace};

use crate::clock;
use crate::nethernet::MAX_PRE_LOGIN_MESSAGE_BYTES;
use crate::tick::queue::LeaveReason;
use crate::udp::Sockets;
use listener::{Config, Listener, Sink};
use peer::Limits;

/// Commands a driver (RakNet or NetherNet) handles before the UDP task serves its other inputs.
pub(crate) const COMMAND_BATCH: usize = 256;
/// Messages from sessions to a driver. Sends wait when it is full, which slows sessions to the
/// pace of the UDP task instead of buffering without bound.
pub(crate) const COMMAND_QUEUE: usize = 4096;
/// Messages received on one connection that its session has not taken yet. Bytes are bounded
/// separately ([`Inbox`]); a full queue disconnects.
const INBOUND_QUEUE: usize = 1024;
/// The largest valid datagram: one at the largest MTU.
pub(crate) const MAX_DATAGRAM_BYTES: usize = MAX_MTU as usize - wire::UDP_HEADER_BYTES;
/// Connections of one transport (RakNet or NetherNet) from its handshake until the session's
/// login.
pub(crate) const MAX_PRE_LOGIN: usize = 64;
/// Such connections of one transport from one IPv4 address or IPv6 /64.
pub(crate) const MAX_PRE_LOGIN_PER_IP: usize = 4;
/// Bytes sent on one connection (RakNet or NetherNet) that are queued or not delivered yet, at
/// most; more disconnects it.
pub(crate) const MAX_UNSENT_BYTES: usize = 32 * 1024 * 1024;

/// A RakNet failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The connection or the socket task is closed.
    Closed,
    /// A peer sent bytes that are not valid RakNet.
    Malformed(&'static str),
    /// A peer or a setting went past a limit.
    Limit(&'static str),
    /// RakNet's own bookkeeping is inconsistent; the connection closes.
    Invariant(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("connection closed"),
            Self::Malformed(what) => write!(f, "malformed raknet packet: {what}"),
            Self::Limit(what) => write!(f, "raknet limit exceeded: {what}"),
            Self::Invariant(what) => write!(f, "raknet invariant broken: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// The driver, run by the UDP task: the listener, the session channels and the clock.
#[derive(Debug)]
pub(crate) struct Driver {
    listener: Listener,
    io: Io,
    commands: mpsc::Receiver<Outbound>,
    pong: watch::Receiver<Arc<[u8]>>,
    start: Instant,
    signals: Vec<(SocketAddr, u64, Flags)>,
}

impl Driver {
    /// A driver that sends on `sockets`. `pong` is the whole pong payload; `accepted` receives
    /// connections that finished the handshake (when it is full, the new connection is closed).
    /// It fails when `settings.max_message_bytes` is a limit the reliability layer cannot count
    /// fragments for.
    pub(crate) fn new(
        settings: Settings,
        pong: watch::Receiver<Arc<[u8]>>,
        accepted: mpsc::Sender<Connection>,
        sockets: Arc<Sockets>,
    ) -> Result<Self> {
        let config = Config {
            guid: settings.guid,
            max_mtu: settings.max_mtu,
            accept: settings.accept,
            limits: Limits::new(settings.max_message_bytes)?,
        };
        let listener = Listener::new(config, Arc::clone(&pong.borrow()), settings.cookie_key);
        let (outbound, commands) = mpsc::channel(COMMAND_QUEUE);
        Ok(Self {
            listener,
            io: Io {
                sockets,
                accepted,
                outbound,
                links: BTreeMap::new(),
                max_message_bytes: settings.max_message_bytes,
            },
            commands,
            pong,
            start: clock::now(),
            signals: Vec::new(),
        })
    }

    /// Whether no connection is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.listener.is_empty()
    }

    /// Handles a datagram from `from`.
    pub(crate) fn receive(&mut self, from: SocketAddr, datagram: &[u8]) {
        if datagram.len() > MAX_DATAGRAM_BYTES {
            trace!(addr = %from, len = datagram.len(), "raknet oversized datagram dropped");
            return;
        }
        let now = self.now();
        self.refresh_pong();
        self.listener.receive(now, from, datagram, &mut self.io);
    }

    /// Waits for a session's message, then sends it and those queued behind it, at most a
    /// batch; `false` once no session can queue more. Cancel-safe: it waits only for the first.
    pub(crate) async fn serve(&mut self) -> bool {
        let Some(command) = self.commands.recv().await else {
            return false;
        };
        self.send(command);
        for _ in 1..COMMAND_BATCH {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.send(command);
        }
        true
    }

    /// Handles the stop: stops accepting and closes every connection.
    pub(crate) fn stop(&mut self) {
        self.drain();
        let now = self.now();
        self.listener.shutdown(now, &mut self.io);
    }

    /// Handles the timer: reads the connections' flags, sends what sessions queued, then applies
    /// the flags and runs the listener's timers. The flags are read first because a session
    /// queues its last messages before it sets a flag: whatever was queued before a flag read
    /// here is in the queue when it is drained, so a close never overtakes a message queued
    /// before it.
    pub(crate) fn tick(&mut self) {
        self.collect_signals();
        self.drain();
        self.update();
    }

    fn now(&self) -> Duration {
        clock::now().saturating_duration_since(self.start)
    }

    /// Reads the flags that changed since the last read.
    fn collect_signals(&mut self) {
        for (&id, link) in &mut self.io.links {
            if let Some(flags) = Flags::changed(&mut link.signal) {
                self.signals.push((link.addr, id, flags));
            }
        }
    }

    /// Sends what sessions queued, at most a full queue.
    fn drain(&mut self) {
        for _ in 0..COMMAND_QUEUE {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.send(command);
        }
    }

    /// Applies the flags read by [`collect_signals`](Self::collect_signals) and runs the
    /// listener's timers.
    fn update(&mut self) {
        let now = self.now();
        self.refresh_pong();
        for (addr, id, flags) in self.signals.drain(..) {
            if flags.logged_in {
                if let Some(link) = self.io.links.get_mut(&id) {
                    link.inbox.raise_to_login_budget();
                }
                self.listener.logged_in(addr, id);
            }
            if flags.closed {
                self.listener.close(now, addr, id, &mut self.io);
            }
        }
        self.listener.update(now, &mut self.io);
    }

    fn send(&mut self, command: Outbound) {
        let now = self.now();
        self.listener
            .send(now, command.addr, command.id, command.message, &mut self.io);
    }

    fn refresh_pong(&mut self) {
        if self.pong.has_changed().unwrap_or(false) {
            let pong = Arc::clone(&self.pong.borrow_and_update());
            self.listener.set_pong(pong);
        }
    }
}

/// The settings of [`Driver::new`]: limits, whether to accept connections, the server GUID and
/// the cookie key.
#[derive(Debug)]
pub(crate) struct Settings {
    /// Largest MTU to negotiate (`max-mtu-size`, at least 400).
    pub(crate) max_mtu: u16,
    /// Largest game message in bytes, without the 0xfe byte.
    pub(crate) max_message_bytes: usize,
    /// Whether to accept connections; `false` (config `raknet = false`) answers pings only.
    pub(crate) accept: bool,
    /// The server GUID in pongs and replies.
    pub(crate) guid: u64,
    /// The key of the connection cookies; `None` (config `raknet-cookies = false`) asks for none.
    pub(crate) cookie_key: Option<crate::crypto::CookieKey>,
}

/// The sockets and session channels: the listener's `Sink`.
#[derive(Debug)]
struct Io {
    sockets: Arc<Sockets>,
    accepted: mpsc::Sender<Connection>,
    outbound: mpsc::Sender<Outbound>,
    links: BTreeMap<u64, Link>,
    /// The largest game message delivered, without the 0xfe byte ([`Inbox::new`]).
    max_message_bytes: usize,
}

impl Sink for Io {
    fn transmit(&mut self, to: SocketAddr, datagram: &[u8]) {
        // A full send buffer drops the datagram; the reliability layer resends what matters.
        if let Err(error) = self.sockets.send(to, datagram) {
            trace!(
                addr = %to,
                error = &error as &(dyn std::error::Error + 'static),
                "raknet datagram not sent"
            );
        }
    }

    fn accept(&mut self, id: u64, addr: SocketAddr) -> bool {
        let (inbox, receiver) = Inbox::new(self.max_message_bytes);
        let (signal, watcher) = watch::channel(Flags::default());
        let (ping, pings) = watch::channel(None);
        let (undelivered, undelivered_watcher) = watch::channel(0);
        let (reason, reason_watcher) = watch::channel(LeaveReason::ConnectionClosed);
        let connection = Connection {
            id,
            addr,
            inbound: receiver,
            outbound: self.outbound.clone(),
            signal,
            ping: pings,
            undelivered: undelivered_watcher,
            reason: reason_watcher,
        };
        if let Err(error) = self.accepted.try_send(connection) {
            debug!(
                %addr,
                error = &error as &(dyn std::error::Error + 'static),
                "raknet connection not accepted"
            );
            return false;
        }
        let link = Link {
            addr,
            inbox,
            signal: watcher,
            ping,
            undelivered,
            reason,
        };
        self.links.insert(id, link);
        true
    }

    fn deliver(&mut self, id: u64, message: Vec<u8>) -> bool {
        self.links
            .get(&id)
            .is_some_and(|link| link.inbox.deliver(message))
    }

    fn close(&mut self, id: u64, reason: LeaveReason) {
        if let Some(link) = self.links.remove(&id) {
            // Set before the link goes: dropping its inbox is what ends the session's receive.
            link.reason.send_replace(reason);
        }
    }

    fn ping(&mut self, id: u64, rtt: Duration) {
        if let Some(link) = self.links.get(&id) {
            link.ping.send_replace(Some(rtt));
        }
    }

    fn undelivered(&mut self, id: u64, bytes: usize) {
        if let Some(link) = self.links.get(&id) {
            report_undelivered(&link.undelivered, bytes);
        }
    }
}

/// The driver's side of an accepted connection.
#[derive(Debug)]
struct Link {
    addr: SocketAddr,
    inbox: Inbox,
    signal: watch::Receiver<Flags>,
    ping: watch::Sender<Option<Duration>>,
    undelivered: watch::Sender<usize>,
    reason: watch::Sender<LeaveReason>,
}

/// One RakNet connection, held by its session. Dropping it closes the connection gracefully.
#[derive(Debug)]
pub(crate) struct Connection {
    id: u64,
    addr: SocketAddr,
    inbound: mpsc::Receiver<Inbound>,
    outbound: mpsc::Sender<Outbound>,
    signal: watch::Sender<Flags>,
    ping: watch::Receiver<Option<Duration>>,
    undelivered: watch::Receiver<usize>,
    reason: watch::Receiver<LeaveReason>,
}

impl Connection {
    /// The client address.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Why the connection ended, once [`recv`](Self::recv) returned `None`.
    pub(crate) fn leave_reason(&self) -> LeaveReason {
        *self.reason.borrow()
    }

    /// The round trip time of the latest connected ping the client answered; `None` until one
    /// was (RakNet's ping measure).
    pub(crate) fn ping(&self) -> Option<Duration> {
        *self.ping.borrow()
    }

    /// The bytes sent on the connection that the client has not acknowledged yet, as of the
    /// driver's last update; the sender is dropped once the connection is gone.
    pub(crate) fn undelivered(&self) -> watch::Receiver<usize> {
        self.undelivered.clone()
    }

    /// Closes the connection after what is queued has been delivered.
    pub(crate) fn close(&self) {
        self.signal.send_modify(|flags| flags.closed = true);
    }

    /// Marks the player as logged in by the session: the connection stops counting against the
    /// pre-login limits.
    pub(crate) fn logged_in(&self) {
        self.signal.send_modify(|flags| flags.logged_in = true);
    }

    /// Receives the next game message (after its 0xfe byte), or `None` once the connection is
    /// closed.
    pub(crate) async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inbound.recv().await.map(|inbound| inbound.message)
    }

    /// Queues a game message (0xfe is prepended). Waits while the driver is busy. An
    /// error means the connection is closed.
    pub(crate) async fn send(&self, message: Vec<u8>) -> Result<()> {
        if self.inbound.is_closed() {
            return Err(Error::Closed);
        }
        let outbound = Outbound {
            addr: self.addr,
            id: self.id,
            message,
        };
        self.outbound
            .send(outbound)
            .await
            .map_err(|_| Error::Closed)
    }
}

/// A game message for the driver to send.
#[derive(Debug)]
struct Outbound {
    addr: SocketAddr,
    id: u64,
    message: Vec<u8>,
}

/// The driver's end of one connection's inbound queue and its byte budget (RakNet and
/// NetherNet).
#[derive(Debug)]
pub(crate) struct Inbox {
    inbound: mpsc::Sender<Inbound>,
    budget: Arc<Semaphore>,
    /// The bytes [`raise_to_login_budget`](Self::raise_to_login_budget) adds; 0 once added.
    login_extra_bytes: usize,
}

impl Inbox {
    /// An inbox whose session may leave two of the largest delivered messages, `max_message_bytes`
    /// bytes each, untaken, and the session's end. Until the login the messages are at most
    /// [`MAX_PRE_LOGIN_MESSAGE_BYTES`] (the largest valid Login, as the reassembly limit before
    /// the login), so the budget starts at two of those. The budget is cut to what a semaphore
    /// counts, far above what any setting asks.
    pub(crate) fn new(max_message_bytes: usize) -> (Self, mpsc::Receiver<Inbound>) {
        let (inbound, receiver) = mpsc::channel(INBOUND_QUEUE);
        let budget_of =
            |message_bytes: usize| message_bytes.saturating_mul(2).min(Semaphore::MAX_PERMITS);
        let full = budget_of(max_message_bytes);
        let pre_login = budget_of(max_message_bytes.min(MAX_PRE_LOGIN_MESSAGE_BYTES));
        let inbox = Self {
            inbound,
            budget: Arc::new(Semaphore::new(pre_login)),
            login_extra_bytes: full.saturating_sub(pre_login),
        };
        (inbox, receiver)
    }

    /// Raises the byte budget from the pre-login size to the full one, once the session logged
    /// in.
    pub(crate) fn raise_to_login_budget(&mut self) {
        self.budget
            .add_permits(std::mem::take(&mut self.login_extra_bytes));
    }

    /// Passes a message to the session; `false` when its queue or byte budget is full. A message
    /// for a session that has ended is dropped: closing now would drop the packets the session
    /// queued last (a kick's Disconnect), so its close flag closes the connection instead.
    pub(crate) fn deliver(&self, message: Vec<u8>) -> bool {
        let Ok(len) = u32::try_from(message.len()) else {
            return false;
        };
        let Ok(budget) = Arc::clone(&self.budget).try_acquire_many_owned(len) else {
            return false;
        };
        let inbound = Inbound {
            message,
            _budget: budget,
        };
        match self.inbound.try_send(inbound) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => true,
            Err(mpsc::error::TrySendError::Full(_)) => false,
        }
    }
}

/// A received game message and its share of the connection's inbound byte budget.
#[derive(Debug)]
pub(crate) struct Inbound {
    pub(crate) message: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

/// What a connection tells its driver through its watch channel (RakNet and NetherNet).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Flags {
    pub(crate) logged_in: bool,
    pub(crate) closed: bool,
}

impl Flags {
    /// The flags in `signal` if they changed since the last call; a dropped connection reads as
    /// closed.
    pub(crate) fn changed(signal: &mut watch::Receiver<Self>) -> Option<Self> {
        match signal.has_changed() {
            Ok(false) => None,
            Ok(true) => Some(*signal.borrow_and_update()),
            Err(_) => Some(Self {
                logged_in: false,
                closed: true,
            }),
        }
    }
}

/// Sets a connection's count of bytes not yet delivered, waking its session only when the count
/// changed (RakNet and NetherNet).
pub(crate) fn report_undelivered(undelivered: &watch::Sender<usize>, bytes: usize) {
    undelivered.send_if_modified(|count| std::mem::replace(count, bytes) != bytes);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    use tokio::net::UdpSocket;

    use super::listener::tests::{Client, GUID, MAX_MESSAGE};
    use crate::udp::{self, RECV_BUFFER_BYTES, UPDATE_INTERVAL};

    const WAIT: Duration = Duration::from_secs(10);

    fn settings() -> Settings {
        Settings {
            max_mtu: MAX_MTU,
            max_message_bytes: MAX_MESSAGE,
            accept: true,
            guid: GUID,
            cookie_key: Some(crate::crypto::CookieKey::generate().unwrap()),
        }
    }

    /// A driver on `sockets` with an empty pong, and the connections it accepts.
    pub(crate) fn driver(sockets: Arc<Sockets>) -> (Driver, mpsc::Receiver<Connection>) {
        let (_, pong) = watch::channel(Arc::<[u8]>::from(&b""[..]));
        let (accepted, connections) = mpsc::channel(1);
        let driver = Driver::new(settings(), pong, accepted, sockets).unwrap();
        (driver, connections)
    }

    /// The test client on a real UDP socket.
    struct Remote {
        socket: UdpSocket,
        server: SocketAddr,
        client: Client,
        start: Instant,
        updated: Duration,
    }

    impl Remote {
        async fn new(server: SocketAddr) -> Self {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let addr = socket.local_addr().unwrap();
            Self {
                socket,
                server,
                client: Client::new(addr, MAX_MTU),
                start: clock::now(),
                updated: Duration::ZERO,
            }
        }

        fn now(&self) -> Duration {
            clock::now().saturating_duration_since(self.start)
        }

        async fn flush(&mut self) {
            for data in std::mem::take(&mut self.client.outbox) {
                self.socket.send_to(&data, self.server).await.unwrap();
            }
        }

        /// Exchanges datagrams until `done` holds.
        async fn until(&mut self, mut done: impl FnMut(&Client) -> bool) {
            let mut buf = vec![0; RECV_BUFFER_BYTES];
            while !done(&self.client) {
                assert!(self.now() < WAIT, "timed out");
                if self.now() >= self.updated + UPDATE_INTERVAL {
                    self.updated = self.now();
                    self.client.update(self.updated);
                }
                self.flush().await;
                let received =
                    tokio::time::timeout(UPDATE_INTERVAL, self.socket.recv_from(&mut buf)).await;
                if let Ok(received) = received {
                    let (len, _) = received.unwrap();
                    let now = self.now();
                    self.client.handle(now, &buf[..len]);
                }
            }
            self.flush().await;
        }
    }

    /// An unconnected ping as a RakNet client sends it.
    pub(crate) fn unconnected_ping() -> Vec<u8> {
        Client::ping(1)
    }

    /// The pong `server` answers an unconnected ping from `socket` with.
    pub(crate) async fn ping(socket: &UdpSocket, server: SocketAddr) -> Vec<u8> {
        socket.send_to(&unconnected_ping(), server).await.unwrap();
        let mut buf = vec![0; RECV_BUFFER_BYTES];
        let received = tokio::time::timeout(WAIT, socket.recv_from(&mut buf)).await;
        let (len, _) = received.unwrap().unwrap();
        buf.truncate(len);
        buf
    }

    /// Runs a test on its own single-thread runtime.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test drives the UDP task on a runtime of its own"
    )]
    pub(crate) fn run<F: Future>(test: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(test)
    }

    /// Like [`run`], with time paused: it jumps ahead only while every task waits and no blocking
    /// work is in flight, so a slow machine cannot run a timeout out.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test drives tasks on a runtime of its own"
    )]
    pub(crate) fn run_paused<F: Future>(test: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(test)
    }

    /// A connection without a socket task, for a session test that hands the session its
    /// messages itself: it receives nothing and its sends fail.
    pub(crate) fn detached_connection() -> Connection {
        let (_, inbound) = mpsc::channel(1);
        let (outbound, _) = mpsc::channel(1);
        let (signal, _) = watch::channel(Flags::default());
        let (_, ping) = watch::channel(None);
        let (_, undelivered) = watch::channel(0);
        let (_, reason) = watch::channel(LeaveReason::ConnectionClosed);
        Connection {
            id: 0,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            inbound,
            outbound,
            signal,
            ping,
            undelivered,
            reason,
        }
    }

    /// The socket task's ends of a [`linked_connection`].
    pub(crate) struct Far {
        _inbound: mpsc::Sender<Inbound>,
        sent: mpsc::Receiver<Outbound>,
        /// What the connection reports as not yet delivered.
        pub(crate) undelivered: watch::Sender<usize>,
    }

    impl Far {
        /// The next message the session sent; `None` once the connection is dropped.
        pub(crate) async fn sent(&mut self) -> Option<Vec<u8>> {
            self.sent.recv().await.map(|outbound| outbound.message)
        }
    }

    /// A connection without a socket task, for a session test that plays the socket task
    /// itself: it receives nothing, keeps what its session sends and reports what the test sets
    /// as undelivered.
    pub(crate) fn linked_connection() -> (Connection, Far) {
        let (inbound_sender, inbound) = mpsc::channel(1);
        let (outbound, sent) = mpsc::channel(COMMAND_QUEUE);
        let (signal, _) = watch::channel(Flags::default());
        let (_, ping) = watch::channel(None);
        let (undelivered, undelivered_watcher) = watch::channel(0);
        let (_, reason) = watch::channel(LeaveReason::ConnectionClosed);
        let connection = Connection {
            id: 0,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            inbound,
            outbound,
            signal,
            ping,
            undelivered: undelivered_watcher,
            reason,
        };
        let far = Far {
            _inbound: inbound_sender,
            sent,
            undelivered,
        };
        (connection, far)
    }

    #[test]
    fn sockets_answer_pings_and_carry_connections() {
        run(async {
            let (pong, pongs) = watch::channel(Arc::<[u8]>::from(&b"first"[..]));
            let (accepted, mut connections) = mpsc::channel(1);
            // Find a free port, then bind it for IPv4 and IPv6 at once.
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let port = udp::Sockets::bind(Some(localhost), None)
                .unwrap()
                .local_addrs()[0]
                .port();
            let v4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            let v6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0);
            let sockets = udp::Sockets::bind(Some(v4), Some(v6)).unwrap();
            let raknet = Driver::new(settings(), pongs, accepted, Arc::clone(&sockets)).unwrap();
            let (nethernet, _) = crate::nethernet::tests::driver(Arc::clone(&sockets));
            let handle = udp::spawn(sockets, raknet, nethernet);
            assert_eq!(handle.local_addrs(), [v4.into(), v6.into()]);

            let socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
            assert!(ping(&socket, v6.into()).await.ends_with(b"first"));
            pong.send(Arc::from(&b"second"[..])).unwrap();
            assert!(ping(&socket, v6.into()).await.ends_with(b"second"));

            let mut remote = Remote::new(v4.into()).await;
            remote.client.start();
            remote
                .until(|client| {
                    client
                        .messages
                        .iter()
                        .any(|message| message.id == wire::ID_CONNECTION_REQUEST_ACCEPTED)
                })
                .await;
            let connection = tokio::time::timeout(WAIT, connections.recv()).await;
            let mut connection = connection.unwrap().unwrap();
            assert_eq!(connection.addr(), remote.client.addr);

            let now = remote.now();
            remote.client.send(now, wire::ID_GAME, b"hello".to_vec());
            remote.flush().await;
            let received = tokio::time::timeout(WAIT, connection.recv()).await;
            assert_eq!(received.unwrap(), Some(b"hello".to_vec()));
            connection.send(b"world".to_vec()).await.unwrap();
            remote.until(|client| client.game() == [b"world"]).await;

            connection.logged_in();
            connection.close();
            remote.until(Client::disconnected).await;
            let received = tokio::time::timeout(WAIT, connection.recv()).await;
            assert_eq!(received.unwrap(), None);
            assert_eq!(connection.send(b"late".to_vec()).await, Err(Error::Closed));
            handle.shutdown().await;
        });
    }

    /// EAFNOSUPPORT on Linux, WSAEAFNOSUPPORT on Windows; std gives it no `ErrorKind`.
    const NO_ADDRESS_FAMILY: i32 = if cfg!(windows) { 10_047 } else { 97 };

    /// Whether the host has the IPv6 loopback `::1`: a kernel without IPv6 refuses the address
    /// family, one with IPv6 off on loopback refuses the address, and any other error fails the
    /// test. Runs on a tokio runtime.
    pub(crate) fn has_ipv6_loopback() -> bool {
        match udp::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))) {
            Ok(_) => true,
            Err(udp::Error::Bind { source, .. })
                if source.kind() == io::ErrorKind::AddrNotAvailable
                    || source.raw_os_error() == Some(NO_ADDRESS_FAMILY) =>
            {
                false
            }
            Err(error) => panic!("ipv6 loopback probe failed: {error:?}"),
        }
    }

    /// The addresses a test of IPv4 and IPv6 on one port binds: the defaults `0.0.0.0` and `::`,
    /// which clash on Linux (`bindv6only = 0`) unless the IPv6 socket is IPv6-only. Windows makes
    /// IPv6 sockets IPv6-only by default and its firewall asks about every new program that binds
    /// a wildcard address, so loopback stands in there.
    pub(crate) fn dual_stack_ips() -> (Ipv4Addr, Ipv6Addr) {
        if cfg!(windows) {
            (Ipv4Addr::LOCALHOST, Ipv6Addr::LOCALHOST)
        } else {
            (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED)
        }
    }

    /// A driver on a loopback socket that the test runs by calling its handlers, and the
    /// connections it accepts.
    fn loopback_driver() -> (Driver, mpsc::Receiver<Connection>) {
        let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        driver(udp::Sockets::bind(Some(localhost), None).unwrap())
    }

    /// The driver's address.
    fn server(driver: &Driver) -> SocketAddr {
        driver.io.sockets.local_addrs()[0]
    }

    /// Hands the driver every datagram already waiting on its socket.
    fn receive_waiting(driver: &mut Driver) {
        let sockets = Arc::clone(&driver.io.sockets);
        let mut buf = [0; RECV_BUFFER_BYTES];
        while let Ok((len, from)) = sockets.v4().try_recv_from(&mut buf) {
            driver.receive(from, &buf[..len]);
        }
    }

    /// Runs the client's timers, the driver's socket and timer handlers, then the client's
    /// receive, once.
    async fn step(driver: &mut Driver, remote: &mut Remote) {
        assert!(remote.now() < WAIT, "timed out");
        let now = remote.now();
        remote.client.update(now);
        remote.flush().await;
        let sockets = Arc::clone(&driver.io.sockets);
        if tokio::time::timeout(UPDATE_INTERVAL, sockets.v4().readable())
            .await
            .is_ok()
        {
            receive_waiting(driver);
        }
        driver.tick();
        let mut buf = [0; RECV_BUFFER_BYTES];
        while let Ok((len, _)) = remote.socket.try_recv_from(&mut buf) {
            let now = remote.now();
            remote.client.handle(now, &buf[..len]);
        }
    }

    /// Connects a test client to the RakNet server at `server` over loopback and keeps the link
    /// running until `done` holds or the server disconnects it; whether it did.
    pub(crate) async fn disconnected_before(
        server: SocketAddr,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let mut remote = Remote::new(server).await;
        remote.client.start();
        let mut buf = vec![0; RECV_BUFFER_BYTES];
        while !remote.client.disconnected() && !done() {
            if remote.now() >= remote.updated + UPDATE_INTERVAL {
                remote.updated = remote.now();
                remote.client.update(remote.updated);
            }
            remote.flush().await;
            let received =
                tokio::time::timeout(UPDATE_INTERVAL, remote.socket.recv_from(&mut buf)).await;
            if let Ok(received) = received {
                let (len, _) = received.unwrap();
                let now = remote.now();
                remote.client.handle(now, &buf[..len]);
            }
        }
        remote.client.disconnected()
    }

    /// Steps `driver` and `remote` until the driver accepts the connection of `remote`.
    async fn accepted(
        driver: &mut Driver,
        remote: &mut Remote,
        connections: &mut mpsc::Receiver<Connection>,
    ) -> Connection {
        loop {
            if let Ok(connection) = connections.try_recv() {
                return connection;
            }
            step(driver, remote).await;
        }
    }

    /// The server's connected ping comes back as a pong: the connection's ping is the time
    /// between (RakNet's ping measure).
    #[test]
    fn a_connected_pong_sets_the_connections_ping() {
        run(async {
            let (mut driver, mut connections) = loopback_driver();
            let mut remote = Remote::new(server(&driver)).await;
            remote.client.start();
            let connection = accepted(&mut driver, &mut remote, &mut connections).await;
            assert_eq!(connection.ping(), None);
            let sent = loop {
                let ping = remote
                    .client
                    .messages
                    .iter()
                    .find(|m| m.id == wire::ID_CONNECTED_PING);
                if let Some(ping) = ping {
                    break u64::from_be_bytes(ping.body[..8].try_into().unwrap());
                }
                step(&mut driver, &mut remote).await;
            };
            let mut pong = sent.to_be_bytes().to_vec();
            pong.extend_from_slice(&0_u64.to_be_bytes());
            let now = remote.now();
            remote.client.send(now, wire::ID_CONNECTED_PONG, pong);
            while connection.ping().is_none() {
                step(&mut driver, &mut remote).await;
            }
            assert!(connection.ping().unwrap() < WAIT);
        });
    }

    #[test]
    fn a_message_sent_just_before_close_is_delivered() {
        run(async {
            let (mut driver, mut connections) = loopback_driver();
            let mut remote = Remote::new(server(&driver)).await;
            remote.client.start();
            let connection = accepted(&mut driver, &mut remote, &mut connections).await;
            // The session's last packets, then its close flag, all before the timer runs.
            for i in 0..300_u32 {
                connection.send(i.to_le_bytes().to_vec()).await.unwrap();
            }
            connection.send(b"bye".to_vec()).await.unwrap();
            connection.close();
            while !remote.client.disconnected() {
                step(&mut driver, &mut remote).await;
            }
            let game = remote.client.game();
            assert_eq!(game.len(), 301);
            assert_eq!(game.last(), Some(&&b"bye"[..]));
        });
    }

    /// A session that queues its last message and closes after the driver drained its queue is
    /// not cut short: the timer handler reads the close flags before it drains, so it applies
    /// only a flag whose messages it has already sent.
    #[test]
    fn a_message_queued_after_the_drain_is_sent_before_the_close() {
        run(async {
            let (mut driver, mut connections) = loopback_driver();
            let mut remote = Remote::new(server(&driver)).await;
            remote.client.start();
            let connection = accepted(&mut driver, &mut remote, &mut connections).await;
            // The timer handler, with the session running between its steps.
            driver.collect_signals();
            driver.drain();
            connection.send(b"bye".to_vec()).await.unwrap();
            connection.close();
            driver.update();
            while !remote.client.disconnected() {
                step(&mut driver, &mut remote).await;
            }
            assert_eq!(remote.client.game(), [b"bye"]);
        });
    }

    #[test]
    fn a_message_from_the_client_after_the_session_ended_keeps_the_close_graceful() {
        run(async {
            let (mut driver, mut connections) = loopback_driver();
            let mut remote = Remote::new(server(&driver)).await;
            remote.client.start();
            let connection = accepted(&mut driver, &mut remote, &mut connections).await;
            // A kicked session: its Disconnect, its close flag and its end, then the client's
            // next input reaches the driver before the queued Disconnect does.
            connection.send(b"bye".to_vec()).await.unwrap();
            connection.close();
            drop(connection);
            let now = remote.now();
            remote.client.send(now, wire::ID_GAME, b"input".to_vec());
            remote.flush().await;
            let sockets = Arc::clone(&driver.io.sockets);
            tokio::time::timeout(WAIT, sockets.v4().readable())
                .await
                .unwrap()
                .unwrap();
            receive_waiting(&mut driver);
            while !remote.client.disconnected() {
                step(&mut driver, &mut remote).await;
            }
            assert_eq!(remote.client.game(), [b"bye"]);
        });
    }

    #[test]
    fn a_disconnect_notification_ends_the_connection_as_left() {
        run(async {
            let (mut driver, mut connections) = loopback_driver();
            let mut remote = Remote::new(server(&driver)).await;
            remote.client.start();
            let mut connection = accepted(&mut driver, &mut remote, &mut connections).await;
            let now = remote.now();
            remote
                .client
                .send(now, wire::ID_DISCONNECT_NOTIFICATION, Vec::new());
            remote.flush().await;
            while !connection.inbound.is_closed() {
                step(&mut driver, &mut remote).await;
            }
            assert_eq!(connection.recv().await, None);
            assert_eq!(connection.leave_reason(), LeaveReason::Left);
        });
    }

    #[test]
    fn an_inbox_holds_two_of_the_largest_messages() {
        let (inbox, mut receiver) = Inbox::new(10);
        assert!(inbox.deliver(vec![1; 10]));
        assert!(inbox.deliver(vec![2; 10]));
        assert!(!inbox.deliver(vec![3]));
        assert_eq!(receiver.try_recv().unwrap().message, [1; 10]);
        assert!(inbox.deliver(vec![4; 10]));
        drop(receiver);
        assert!(inbox.deliver(vec![5]));
    }

    /// Before the login an inbox holds two pre-login messages; the login raises it to the full
    /// budget.
    #[test]
    fn an_inbox_before_login_holds_two_pre_login_messages() {
        let pre_login = MAX_PRE_LOGIN_MESSAGE_BYTES;
        let (mut inbox, _receiver) = Inbox::new(4 * pre_login);
        assert!(inbox.deliver(vec![1; pre_login]));
        assert!(inbox.deliver(vec![2; pre_login]));
        assert!(!inbox.deliver(vec![3]));
        inbox.raise_to_login_budget();
        assert!(inbox.deliver(vec![4; 6 * pre_login]));
        assert!(!inbox.deliver(vec![5]));
        inbox.raise_to_login_budget();
        assert!(!inbox.deliver(vec![6]));
    }

    #[test]
    fn a_driver_refuses_a_message_limit_it_cannot_count_fragments_for() {
        run(async {
            let (_, pongs) = watch::channel(Arc::<[u8]>::from(&b""[..]));
            let (accepted, _) = mpsc::channel(1);
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = udp::Sockets::bind(Some(localhost), None).unwrap();
            let bad = Settings {
                max_message_bytes: usize::MAX,
                ..settings()
            };
            let result = Driver::new(bad, pongs, accepted, sockets);
            assert_eq!(result.err(), Some(Error::Limit("max message size")));
        });
    }
}
