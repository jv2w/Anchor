//! Server-side RakNet: the protocol core and its UDP driver.
//!
//! One tokio task owns both UDP sockets (IPv4 and IPv6 on one port) and every connection, so
//! connection state needs no locks. `wire`, `peer` and `listener` are the core,
//! free of sockets and clocks; this module binds the sockets, runs the task and hands finished
//! connections to the session layer. The 0xfe game message byte is added and removed here.
//! The socket setup and the session link types here serve the NetherNet driver too.

mod listener;
mod peer;
mod wire;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, trace};

use crate::clock;

pub(crate) use listener::ip_key;
use listener::{Config, Listener, Sink};
use peer::{Limits, MAX_MTU, MIN_MTU};

/// Messages from all sessions to the socket task. Sends wait when it is full, which slows
/// sessions to the pace of the socket task instead of buffering without bound.
const COMMAND_QUEUE: usize = 4096;
/// Messages received on one connection that its session has not taken yet. Bytes are bounded
/// separately ([`Inbox`]); a full queue disconnects.
const INBOUND_QUEUE: usize = 1024;
/// Datagrams read from one socket before the task serves its other inputs.
const RECV_BATCH: usize = 256;
/// Commands handled before the task serves its other inputs.
const COMMAND_BATCH: usize = 256;
/// Kernel socket buffers: a burst from go-raknet's dialer, which has no congestion control,
/// is several MiB. Linux caps this at `net.core.rmem_max` without an error.
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;
/// How often timers run: ACKs, resends, pings and timeouts.
const UPDATE_INTERVAL: Duration = Duration::from_millis(10);
/// The receive buffer: larger than any valid datagram so that an oversized one is seen.
const RECV_BUFFER: usize = 1500;
/// The largest valid datagram: one at the largest MTU.
const MAX_DATAGRAM: usize = MAX_MTU as usize - wire::UDP_HEADER;

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

/// Socket and limit settings for `spawn`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// IPv4 address to bind, if any.
    pub(crate) v4: Option<SocketAddrV4>,
    /// IPv6 address to bind (IPv6 only), if any.
    pub(crate) v6: Option<SocketAddrV6>,
    /// Largest MTU to negotiate (`max-mtu-size`, at least 400).
    pub(crate) max_mtu: u16,
    /// Largest game message in bytes, without the 0xfe byte.
    pub(crate) max_message: usize,
    /// `false` (config `raknet = false`): answer pings only.
    pub(crate) accept: bool,
    /// The server GUID in pongs and replies.
    pub(crate) guid: u64,
}

/// Binds the sockets and spawns the socket task on the current tokio runtime. `pong` is the
/// whole pong payload; `accepted` receives connections that finished the handshake (when it is
/// full, the new connection is closed).
pub(crate) fn spawn(
    settings: Settings,
    pong: watch::Receiver<Arc<[u8]>>,
    accepted: mpsc::Sender<Connection>,
) -> io::Result<Handle> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what);
    let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    if !(MIN_MTU..=MAX_MTU).contains(&settings.max_mtu) {
        return Err(invalid("raknet max mtu out of range"));
    }
    let limits = Limits::new(settings.max_message).map_err(io::Error::other)?;
    let inbound_bytes = settings
        .max_message
        .checked_add(1)
        .and_then(|message| message.checked_mul(2))
        .filter(|&bytes| bytes <= Semaphore::MAX_PERMITS)
        .ok_or_else(|| invalid("raknet max message out of range"))?;
    let _guard = runtime.enter();
    let v4 = settings.v4.map(|addr| bind(addr.into())).transpose()?;
    let v6 = settings.v6.map(|addr| bind(addr.into())).transpose()?;
    let local_addrs = v4
        .iter()
        .chain(&v6)
        .map(UdpSocket::local_addr)
        .collect::<io::Result<Vec<_>>>()?;
    if local_addrs.is_empty() {
        return Err(invalid("raknet needs an address"));
    }
    let config = Config {
        guid: settings.guid,
        max_mtu: settings.max_mtu,
        accept: settings.accept,
        limits,
    };
    let listener = Listener::new(config, Arc::clone(&pong.borrow()));
    let (outbound, commands) = mpsc::channel(COMMAND_QUEUE);
    let (stop, stopped) = oneshot::channel();
    let driver = Driver {
        listener,
        io: Io {
            v4,
            v6,
            accepted,
            outbound,
            links: BTreeMap::new(),
            inbound_bytes,
        },
        pong,
        start: clock::now(),
        buf: vec![0; RECV_BUFFER],
        signals: Vec::new(),
    };
    let task = runtime.spawn(driver.run(commands, stopped));
    Ok(Handle {
        local_addrs,
        stop,
        task,
    })
}

/// Opens a nonblocking UDP socket on `addr` with large kernel buffers (RakNet and NetherNet).
pub(crate) fn bind(addr: SocketAddr) -> io::Result<UdpSocket> {
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

/// Controls the socket task.
#[derive(Debug)]
pub(crate) struct Handle {
    local_addrs: Vec<SocketAddr>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Handle {
    /// The bound addresses (IPv4 first).
    pub(crate) fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// Stops accepting, closes every connection (each gets up to 5 s to deliver what it
    /// queued, then a disconnect notification) and waits for the task to end. Dropping the
    /// handle does the same without waiting.
    pub(crate) async fn shutdown(self) {
        if self.stop.send(()).is_err() {
            debug!("raknet task already stopped");
        }
        if let Err(error) = self.task.await {
            error!(%error, "raknet task failed");
        }
    }
}

/// What a connection tells its socket task through its watch channel (RakNet and NetherNet).
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

/// A game message for the socket task to send.
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

/// The socket task's end of one connection's inbound queue and its byte budget (RakNet and
/// NetherNet).
#[derive(Debug)]
pub(crate) struct Inbox {
    inbound: mpsc::Sender<Inbound>,
    budget: Arc<Semaphore>,
}

impl Inbox {
    /// An inbox whose session may leave `bytes` received bytes untaken, and the session's end.
    pub(crate) fn new(bytes: usize) -> (Self, mpsc::Receiver<Inbound>) {
        let (inbound, receiver) = mpsc::channel(INBOUND_QUEUE);
        let budget = Arc::new(Semaphore::new(bytes));
        (Self { inbound, budget }, receiver)
    }

    /// Passes a message to the session; `false` when its queue or byte budget is full.
    pub(crate) fn deliver(&self, message: Vec<u8>) -> bool {
        let Ok(len) = u32::try_from(message.len()) else {
            return false;
        };
        let Ok(budget) = Arc::clone(&self.budget).try_acquire_many_owned(len) else {
            return false;
        };
        self.inbound
            .try_send(Inbound {
                message,
                _budget: budget,
            })
            .is_ok()
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

    /// The next game message (after its 0xfe byte), or `None` once the connection is closed.
    pub(crate) async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inbound.recv().await.map(|inbound| inbound.message)
    }

    /// Queues a game message (0xfe is prepended). Waits while the socket task is busy. An
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

/// The socket task's side of an accepted connection.
#[derive(Debug)]
struct Link {
    addr: SocketAddr,
    inbox: Inbox,
    signal: watch::Receiver<Flags>,
    ping: watch::Sender<Option<Duration>>,
}

/// The sockets and session channels: the listener's `Sink`.
#[derive(Debug)]
struct Io {
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    accepted: mpsc::Sender<Connection>,
    outbound: mpsc::Sender<Outbound>,
    links: BTreeMap<u64, Link>,
    /// Received bytes one connection's session may leave untaken: two largest messages.
    inbound_bytes: usize,
}

impl Sink for Io {
    fn transmit(&mut self, to: SocketAddr, datagram: &[u8]) {
        let socket = match to {
            SocketAddr::V4(_) => self.v4.as_ref(),
            SocketAddr::V6(_) => self.v6.as_ref(),
        };
        // A full send buffer drops the datagram; the reliability layer resends what matters.
        if let Some(socket) = socket
            && let Err(error) = socket.try_send_to(datagram, to)
        {
            trace!(%to, %error, "raknet datagram not sent");
        }
    }

    fn accept(&mut self, id: u64, addr: SocketAddr) -> bool {
        let (inbox, receiver) = Inbox::new(self.inbound_bytes);
        let (signal, watcher) = watch::channel(Flags::default());
        let (ping, pings) = watch::channel(None);
        let connection = Connection {
            id,
            addr,
            inbound: receiver,
            outbound: self.outbound.clone(),
            signal,
            ping: pings,
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
}

/// The socket task: the listener, the sockets and the clock.
#[derive(Debug)]
struct Driver {
    listener: Listener,
    io: Io,
    pong: watch::Receiver<Arc<[u8]>>,
    start: Instant,
    buf: Vec<u8>,
    signals: Vec<(SocketAddr, u64, Flags)>,
}

impl Driver {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Outbound>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(UPDATE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stopping = false;
        loop {
            tokio::select! {
                _ = readable(self.io.v4.as_ref()) => self.receive(false),
                _ = readable(self.io.v6.as_ref()) => self.receive(true),
                Some(command) = commands.recv() => {
                    self.send(command);
                    for _ in 1..COMMAND_BATCH {
                        let Ok(command) = commands.try_recv() else { break };
                        self.send(command);
                    }
                }
                _ = interval.tick() => self.tick(&mut commands),
                _ = &mut stopped, if !stopping => {
                    stopping = true;
                    self.stop(&mut commands);
                }
            }
            if stopping && self.listener.is_empty() {
                debug!("raknet task stopped");
                return;
            }
        }
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

    fn receive(&mut self, v6: bool) {
        let now = self.now();
        self.refresh_pong();
        for _ in 0..RECV_BATCH {
            let socket = if v6 { &self.io.v6 } else { &self.io.v4 };
            let Some(socket) = socket else {
                return;
            };
            match socket.try_recv_from(&mut self.buf) {
                Ok((len, from)) => match self.buf.get(..len) {
                    Some(data) if len <= MAX_DATAGRAM => {
                        self.listener.receive(now, from, data, &mut self.io);
                    }
                    _ => trace!(%from, len, "raknet oversized datagram dropped"),
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                // Windows reports ICMP port unreachable as WSAECONNRESET on the next receive
                // (tokio-rs/tokio#2017); that and other per-datagram errors are skipped.
                Err(error) => trace!(%error, "raknet receive error skipped"),
            }
        }
    }

    fn send(&mut self, command: Outbound) {
        let now = self.now();
        self.listener
            .send(now, command.addr, command.id, command.message, &mut self.io);
    }

    /// The timer handler.
    fn tick(&mut self, commands: &mut mpsc::Receiver<Outbound>) {
        self.drain(commands);
        self.update();
    }

    /// The stop handler: stops accepting and closes every connection.
    fn stop(&mut self, commands: &mut mpsc::Receiver<Outbound>) {
        self.drain(commands);
        let now = self.now();
        self.listener.shutdown(now, &mut self.io);
    }

    /// Sends what sessions queued, at most a full queue. A session queues its last packets
    /// before it sets its close flag, and a closing connection takes no more, so the timer and
    /// stop handlers call this before they apply close flags.
    fn drain(&mut self, commands: &mut mpsc::Receiver<Outbound>) {
        for _ in 0..COMMAND_QUEUE {
            let Ok(command) = commands.try_recv() else {
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

/// Waits until `socket` has a datagram; never returns without a socket (RakNet and NetherNet).
pub(crate) async fn readable(socket: Option<&UdpSocket>) -> io::Result<()> {
    match socket {
        Some(socket) => socket.readable().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::listener::tests::{Client, GUID, MAX_MESSAGE};
    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn settings(v4: SocketAddrV4, v6: Option<SocketAddrV6>) -> Settings {
        Settings {
            v4: Some(v4),
            v6,
            max_mtu: MAX_MTU,
            max_message: MAX_MESSAGE,
            accept: true,
            guid: GUID,
        }
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

    /// The pong `server` answers an unconnected ping from `socket` with.
    pub(crate) async fn ping(socket: &UdpSocket, server: SocketAddr) -> Vec<u8> {
        socket.send_to(&Client::ping(1), server).await.unwrap();
        let mut buf = vec![0; RECV_BUFFER];
        let received = tokio::time::timeout(WAIT, socket.recv_from(&mut buf)).await;
        let (len, _) = received.unwrap().unwrap();
        buf.truncate(len);
        buf
    }

    /// Runs a test on its own single-thread runtime.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test drives the socket task on a runtime of its own"
    )]
    pub(crate) fn run<F: Future>(test: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(test)
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
        let probe = spawn(settings(localhost, None), pongs.clone(), accepted.clone()).unwrap();
        let port = probe.local_addrs()[0].port();
        probe.shutdown().await;
        let v4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let v6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0);
        let handle = spawn(settings(v4, Some(v6)), pongs, accepted).unwrap();
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

    /// A driver on a loopback socket that the test runs by calling its handlers, its command
    /// queue and the connections it accepts.
    fn driver() -> (Driver, mpsc::Receiver<Outbound>, mpsc::Receiver<Connection>) {
        let socket = bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let (_, pong) = watch::channel(Arc::<[u8]>::from(&b""[..]));
        let (accepted, connections) = mpsc::channel(1);
        let (outbound, commands) = mpsc::channel(COMMAND_QUEUE);
        let listener = Listener::new(listener::tests::config(), Arc::clone(&pong.borrow()));
        let driver = Driver {
            listener,
            io: Io {
                v4: Some(socket),
                v6: None,
                accepted,
                outbound,
                links: BTreeMap::new(),
                inbound_bytes: 2 * (MAX_MESSAGE + 1),
            },
            pong,
            start: clock::now(),
            buf: vec![0; RECV_BUFFER],
            signals: Vec::new(),
        };
        (driver, commands, connections)
    }

    /// Runs the client's timers, the driver's socket and timer handlers, then the client's
    /// receive, once.
    async fn step(
        driver: &mut Driver,
        commands: &mut mpsc::Receiver<Outbound>,
        remote: &mut Remote,
    ) {
        assert!(remote.now() < WAIT, "timed out");
        let now = remote.now();
        remote.client.update(now);
        remote.flush().await;
        let readable = readable(driver.io.v4.as_ref());
        if tokio::time::timeout(UPDATE_INTERVAL, readable)
            .await
            .is_ok()
        {
            driver.receive(false);
        }
        driver.tick(commands);
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
        let (mut driver, mut commands, mut connections) = driver();
        let server = driver.io.v4.as_ref().unwrap().local_addr().unwrap();
        let mut remote = Remote::new(server).await;
        remote.client.start();
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            step(&mut driver, &mut commands, &mut remote).await;
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
            step(&mut driver, &mut commands, &mut remote).await;
        };
        let mut pong = sent.to_be_bytes().to_vec();
        pong.extend_from_slice(&0_u64.to_be_bytes());
        let now = remote.now();
        remote.client.send(now, wire::ID_CONNECTED_PONG, pong);
        while connection.ping().is_none() {
            step(&mut driver, &mut commands, &mut remote).await;
        }
        assert!(connection.ping().unwrap() < WAIT);
    }

    #[test]
    fn a_message_sent_just_before_close_is_delivered() {
        run(message_sent_just_before_close_is_delivered());
    }

    async fn message_sent_just_before_close_is_delivered() {
        let (mut driver, mut commands, mut connections) = driver();
        let server = driver.io.v4.as_ref().unwrap().local_addr().unwrap();
        let mut remote = Remote::new(server).await;
        remote.client.start();
        let connection = loop {
            if let Ok(connection) = connections.try_recv() {
                break connection;
            }
            step(&mut driver, &mut commands, &mut remote).await;
        };
        // The session's last packets, then its close flag, all before the timer runs.
        for i in 0..300_u32 {
            connection.send(i.to_le_bytes().to_vec()).await.unwrap();
        }
        connection.send(b"bye".to_vec()).await.unwrap();
        connection.close();
        while !remote.client.disconnected() {
            step(&mut driver, &mut commands, &mut remote).await;
        }
        let game = remote.client.game();
        assert_eq!(game.len(), 301);
        assert_eq!(game.last(), Some(&&b"bye"[..]));
    }

    #[test]
    fn spawn_refuses_bad_settings() {
        run(async { refuse_bad_settings() });
        // Outside a runtime: an error, not a panic.
        let (_, pongs) = watch::channel(Arc::<[u8]>::from(&b""[..]));
        let (accepted, _) = mpsc::channel(1);
        let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        assert!(spawn(settings(localhost, None), pongs, accepted).is_err());
    }

    fn refuse_bad_settings() {
        let (_, pongs) = watch::channel(Arc::<[u8]>::from(&b""[..]));
        let (accepted, _) = mpsc::channel(1);
        let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let mut bad = settings(localhost, None);
        bad.max_mtu = MIN_MTU - 1;
        assert!(spawn(bad, pongs.clone(), accepted.clone()).is_err());
        let mut bad = settings(localhost, None);
        bad.max_message = usize::MAX;
        assert!(spawn(bad, pongs.clone(), accepted.clone()).is_err());
        let mut bad = settings(localhost, None);
        bad.v4 = None;
        assert!(spawn(bad, pongs, accepted).is_err());
    }
}
