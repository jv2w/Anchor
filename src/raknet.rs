//! Server-side RakNet: the protocol core and its driver.
//!
//! `wire`, `peer` and `listener` are the core, free of sockets and clocks; the driver feeds it
//! the datagrams the shared UDP task (`udp`) hands over, runs its timers there and hands finished
//! connections to the session layer. The 0xfe game message byte is added and removed here.
//! The session link types here serve the NetherNet driver too.

mod listener;
mod peer;
mod wire;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tracing::{debug, trace};

use crate::clock;
use crate::udp::Sockets;

pub(crate) use listener::ip_key;
use listener::{Config, Listener, Sink};
use peer::{Limits, MAX_MTU, MIN_MTU};

/// Messages from all sessions to the driver. Sends wait when it is full, which slows sessions to
/// the pace of the UDP task instead of buffering without bound.
const COMMAND_QUEUE: usize = 4096;
/// Messages received on one connection that its session has not taken yet. Bytes are bounded
/// separately ([`Inbox`]); a full queue disconnects.
const INBOUND_QUEUE: usize = 1024;
/// Commands handled before the UDP task serves its other inputs.
const COMMAND_BATCH: usize = 256;
/// The largest valid datagram: one at the largest MTU.
pub(crate) const MAX_DATAGRAM: usize = MAX_MTU as usize - wire::UDP_HEADER;

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

/// A RakNet result.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

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

/// Limit settings for [`Driver::new`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// Largest MTU to negotiate (`max-mtu-size`, at least 400).
    pub(crate) max_mtu: u16,
    /// Largest game message in bytes, without the 0xfe byte.
    pub(crate) max_message: usize,
    /// `false` (config `raknet = false`): answer pings only.
    pub(crate) accept: bool,
    /// The server GUID in pongs and replies.
    pub(crate) guid: u64,
}

/// What a connection tells its driver through its watch channel (RakNet and NetherNet).
#[derive(Clone, Copy, Debug, Default)]
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

/// A game message for the driver to send.
#[derive(Debug)]
struct Outbound {
    addr: SocketAddr,
    id: u64,
    message: Vec<u8>,
}

/// A received game message and its share of the connection's inbound byte budget.
#[derive(Debug)]
pub(crate) struct Inbound {
    pub(crate) message: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

/// The driver's end of one connection's inbound queue and its byte budget (RakNet and
/// NetherNet).
#[derive(Debug)]
pub(crate) struct Inbox {
    inbound: mpsc::Sender<Inbound>,
    budget: Arc<Semaphore>,
}

impl Inbox {
    /// An inbox whose session may leave two of the largest delivered messages, `max_message`
    /// bytes each, untaken, and the session's end. The budget is cut to what a semaphore counts,
    /// far above what any setting asks.
    pub(crate) fn new(max_message: usize) -> (Self, mpsc::Receiver<Inbound>) {
        let (inbound, receiver) = mpsc::channel(INBOUND_QUEUE);
        let bytes = max_message.saturating_mul(2).min(Semaphore::MAX_PERMITS);
        let budget = Arc::new(Semaphore::new(bytes));
        (Self { inbound, budget }, receiver)
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
}

impl Connection {
    /// The client address.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The round trip time of the latest connected ping the client answered; `None` until one
    /// was (`RakLib`'s ping measure, which PMMP reports).
    pub(crate) fn ping(&self) -> Option<Duration> {
        *self.ping.borrow()
    }

    /// The bytes sent on the connection that the client has not acknowledged yet, as of the
    /// driver's last update; the sender is dropped once the connection is gone.
    pub(crate) fn undelivered(&self) -> watch::Receiver<usize> {
        self.undelivered.clone()
    }

    /// The next game message (after its 0xfe byte), or `None` once the connection is closed.
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

    /// The session logged the player in: the connection stops counting against the pre-login
    /// limits.
    pub(crate) fn logged_in(&self) {
        self.signal.send_modify(|flags| flags.logged_in = true);
    }

    /// Closes the connection after what is queued has been delivered.
    pub(crate) fn close(&self) {
        self.signal.send_modify(|flags| flags.closed = true);
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
}

/// The sockets and session channels: the listener's `Sink`.
#[derive(Debug)]
struct Io {
    sockets: Arc<Sockets>,
    accepted: mpsc::Sender<Connection>,
    outbound: mpsc::Sender<Outbound>,
    links: BTreeMap<u64, Link>,
    /// The largest game message delivered, without the 0xfe byte ([`Inbox::new`]).
    max_message: usize,
}

impl Sink for Io {
    fn transmit(&mut self, to: SocketAddr, datagram: &[u8]) {
        // A full send buffer drops the datagram; the reliability layer resends what matters.
        if let Err(error) = self.sockets.send(to, datagram) {
            trace!(%to, %error, "raknet datagram not sent");
        }
    }

    fn accept(&mut self, id: u64, addr: SocketAddr) -> bool {
        let (inbox, receiver) = Inbox::new(self.max_message);
        let (signal, watcher) = watch::channel(Flags::default());
        let (ping, pings) = watch::channel(None);
        let (undelivered, undelivered_watcher) = watch::channel(0);
        let connection = Connection {
            id,
            addr,
            inbound: receiver,
            outbound: self.outbound.clone(),
            signal,
            ping: pings,
            undelivered: undelivered_watcher,
        };
        if let Err(error) = self.accepted.try_send(connection) {
            debug!(%addr, %error, "raknet connection not accepted");
            return false;
        }
        let link = Link {
            addr,
            inbox,
            signal: watcher,
            ping,
            undelivered,
        };
        self.links.insert(id, link);
        true
    }

    fn deliver(&mut self, id: u64, message: Vec<u8>) -> bool {
        self.links
            .get(&id)
            .is_some_and(|link| link.inbox.deliver(message))
    }

    fn close(&mut self, id: u64) {
        self.links.remove(&id);
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
    pub(crate) fn new(
        settings: Settings,
        pong: watch::Receiver<Arc<[u8]>>,
        accepted: mpsc::Sender<Connection>,
        sockets: Arc<Sockets>,
    ) -> io::Result<Self> {
        if !(MIN_MTU..=MAX_MTU).contains(&settings.max_mtu) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raknet max mtu out of range",
            ));
        }
        let limits = Limits::new(settings.max_message).map_err(io::Error::other)?;
        let config = Config {
            guid: settings.guid,
            max_mtu: settings.max_mtu,
            accept: settings.accept,
            limits,
        };
        let listener = Listener::new(config, Arc::clone(&pong.borrow()));
        let (outbound, commands) = mpsc::channel(COMMAND_QUEUE);
        Ok(Self {
            listener,
            io: Io {
                sockets,
                accepted,
                outbound,
                links: BTreeMap::new(),
                max_message: settings.max_message,
            },
            commands,
            pong,
            start: clock::now(),
            signals: Vec::new(),
        })
    }

    /// Handles a datagram from `from`.
    pub(crate) fn receive(&mut self, from: SocketAddr, datagram: &[u8]) {
        if datagram.len() > MAX_DATAGRAM {
            trace!(%from, len = datagram.len(), "raknet oversized datagram dropped");
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

    /// The timer handler.
    pub(crate) fn tick(&mut self) {
        self.drain();
        self.update();
    }

    /// The stop handler: stops accepting and closes every connection.
    pub(crate) fn stop(&mut self) {
        self.drain();
        let now = self.now();
        self.listener.shutdown(now, &mut self.io);
    }

    /// Whether no connection is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.listener.is_empty()
    }

    fn now(&self) -> Duration {
        clock::now().saturating_duration_since(self.start)
    }

    fn refresh_pong(&mut self) {
        if self.pong.has_changed().unwrap_or(false) {
            let pong = Arc::clone(&self.pong.borrow_and_update());
            self.listener.set_pong(pong);
        }
    }

    fn send(&mut self, command: Outbound) {
        let now = self.now();
        self.listener
            .send(now, command.addr, command.id, command.message, &mut self.io);
    }

    /// Sends what sessions queued, at most a full queue. A session queues its last packets
    /// before it sets its close flag, and a closing connection takes no more, so the timer and
    /// stop handlers call this before they apply close flags.
    fn drain(&mut self) {
        for _ in 0..COMMAND_QUEUE {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.send(command);
        }
    }

    fn update(&mut self) {
        let now = self.now();
        self.refresh_pong();
        for (&id, link) in &mut self.io.links {
            if let Some(flags) = Flags::changed(&mut link.signal) {
                self.signals.push((link.addr, id, flags));
            }
        }
        for (addr, id, flags) in self.signals.drain(..) {
            if flags.logged_in {
                self.listener.logged_in(addr, id);
            }
            if flags.closed {
                self.listener.close(now, addr, id);
            }
        }
        self.listener.update(now, &mut self.io);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    use tokio::net::UdpSocket;

    use super::listener::tests::{Client, GUID, MAX_MESSAGE};
    use super::*;
    use crate::udp::{self, RECV_BUFFER, UPDATE_INTERVAL};

    const WAIT: Duration = Duration::from_secs(10);

    fn settings() -> Settings {
        Settings {
            max_mtu: MAX_MTU,
            max_message: MAX_MESSAGE,
            accept: true,
            guid: GUID,
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
            let mut buf = vec![0; RECV_BUFFER];
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
        let mut buf = vec![0; RECV_BUFFER];
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

    /// A connection without a socket task, for a session test that hands the session its
    /// messages itself: it receives nothing and its sends fail.
    pub(crate) fn detached_connection() -> Connection {
        let (_, inbound) = mpsc::channel(1);
        let (outbound, _) = mpsc::channel(1);
        let (signal, _) = watch::channel(Flags::default());
        let (_, ping) = watch::channel(None);
        let (_, undelivered) = watch::channel(0);
        Connection {
            id: 0,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            inbound,
            outbound,
            signal,
            ping,
            undelivered,
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
        let connection = Connection {
            id: 0,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            inbound,
            outbound,
            signal,
            ping,
            undelivered: undelivered_watcher,
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
        run(answer_pings_and_carry_connections());
    }

    async fn answer_pings_and_carry_connections() {
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
    }

    /// Whether the host has the IPv6 loopback `::1`: a kernel without IPv6 refuses the address
    /// family, one with IPv6 off on loopback refuses the address, and any other error fails the
    /// test. Runs on a tokio runtime.
    pub(crate) fn has_ipv6_loopback() -> bool {
        /// EAFNOSUPPORT on Linux, WSAEAFNOSUPPORT on Windows; std gives it no `ErrorKind`.
        const NO_ADDRESS_FAMILY: i32 = if cfg!(windows) { 10047 } else { 97 };
        match udp::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))) {
            Ok(_) => true,
            Err(e)
                if e.kind() == io::ErrorKind::AddrNotAvailable
                    || e.raw_os_error() == Some(NO_ADDRESS_FAMILY) =>
            {
                false
            }
            Err(e) => panic!("ipv6 loopback probe failed: {e}"),
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
        let mut buf = [0; RECV_BUFFER];
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
        let mut buf = [0; RECV_BUFFER];
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
        let mut buf = vec![0; RECV_BUFFER];
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

    /// The server's connected ping comes back as a pong: the connection's ping is the time
    /// between (`RakLib`'s ping measure, PMMP `NetworkSession::getPing`).
    #[test]
    fn a_connected_pong_sets_the_connections_ping() {
        run(connected_pong_sets_the_connections_ping());
    }

    async fn connected_pong_sets_the_connections_ping() {
        let (mut driver, mut connections) = loopback_driver();
        let mut remote = Remote::new(server(&driver)).await;
        remote.client.start();
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            step(&mut driver, &mut remote).await;
        };
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
    }

    #[test]
    fn a_message_sent_just_before_close_is_delivered() {
        run(message_sent_just_before_close_is_delivered());
    }

    async fn message_sent_just_before_close_is_delivered() {
        let (mut driver, mut connections) = loopback_driver();
        let mut remote = Remote::new(server(&driver)).await;
        remote.client.start();
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            step(&mut driver, &mut remote).await;
        };
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
    }

    #[test]
    fn a_message_from_the_client_after_the_session_ended_keeps_the_close_graceful() {
        run(message_after_session_ended_keeps_close_graceful());
    }

    async fn message_after_session_ended_keeps_close_graceful() {
        let (mut driver, mut connections) = loopback_driver();
        let mut remote = Remote::new(server(&driver)).await;
        remote.client.start();
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            step(&mut driver, &mut remote).await;
        };
        // A kicked session: its Disconnect, its close flag and its end, then the client's next
        // input reaches the driver before the queued Disconnect does.
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

    #[test]
    fn a_driver_refuses_bad_settings() {
        run(async {
            let (_, pongs) = watch::channel(Arc::<[u8]>::from(&b""[..]));
            let (accepted, _) = mpsc::channel(1);
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let sockets = udp::Sockets::bind(Some(localhost), None).unwrap();
            let mut bad = settings();
            bad.max_mtu = MIN_MTU - 1;
            let driver = Driver::new(bad, pongs.clone(), accepted.clone(), Arc::clone(&sockets));
            assert!(driver.is_err());
            let mut bad = settings();
            bad.max_message = usize::MAX;
            assert!(Driver::new(bad, pongs, accepted, sockets).is_err());
        });
    }
}
