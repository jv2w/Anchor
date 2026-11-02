//! The server side of RakNet without sockets or clocks: offline messages (ping, open
//! connection), the connection table and its limits, the connected handshake and RakNet's own
//! connected messages. The driver feeds it datagrams and time and it answers through a `Sink`.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, trace};

use super::peer::{Limits, MAX_MTU, MIN_MTU, Message, Peer};
use super::wire::{self, Cursor, FLAG_DATAGRAM, UDP_HEADER_BYTES};
use super::{Error, MAX_PRE_LOGIN, MAX_PRE_LOGIN_PER_IP, Result};
use crate::bucket::{Bucket, Rate};

/// How long a closing connection may take to deliver what it queued (go-raknet `Close`).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// From `OpenConnectionRequest2` to `NewIncomingConnection` at most (go-raknet listener).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Silence after which a connection is dropped, plus two round trips (go-raknet `startTicking`).
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// IPs whose ping rate is tracked. When full, pings from new IPs get no reply.
const MAX_PING_IPS: usize = 4096;
/// The largest pong payload: one datagram at the largest MTU after the pong's fixed fields.
const MAX_PONG_DATA_BYTES: usize = MAX_MTU as usize - UDP_HEADER_BYTES - 35;
/// How long a ping rate entry lasts without pings before it is dropped.
const PING_IDLE: Duration = Duration::from_secs(60);
/// How often a connected ping goes out, keeping both ends' idle timers fed (go-raknet).
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// Pings answered back to back from one IP (`capacity`), and per second once the burst is spent.
const PING_RATE: Rate = Rate {
    capacity: 10,
    per_second: 5,
};
/// How often idle ping rate entries are dropped.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The RakNet server state: every connection and the offline handshake.
#[derive(Debug)]
pub(super) struct Listener {
    config: Config,
    pong: Arc<[u8]>,
    conns: BTreeMap<SocketAddr, Conn>,
    next_id: u64,
    pre_login: usize,
    pre_login_per_ip: BTreeMap<u128, usize>,
    pings: BTreeMap<u128, Bucket>,
    next_sweep: Duration,
    shutting_down: bool,
    buf: Vec<u8>,
    messages: Vec<Message>,
    doomed: Vec<(SocketAddr, Close)>,
}

impl Listener {
    pub(super) fn new(config: Config, pong: Arc<[u8]>) -> Self {
        Self {
            config,
            pong,
            conns: BTreeMap::new(),
            next_id: 0,
            pre_login: 0,
            pre_login_per_ip: BTreeMap::new(),
            pings: BTreeMap::new(),
            next_sweep: Duration::ZERO,
            shutting_down: false,
            buf: Vec::with_capacity(usize::from(MAX_MTU)),
            messages: Vec::new(),
            doomed: Vec::new(),
        }
    }

    /// Whether no connection is left (after `shutdown`, the driver stops then).
    pub(super) fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    pub(super) fn set_pong(&mut self, pong: Arc<[u8]>) {
        self.pong = pong;
    }

    /// Handles one UDP datagram from `from`.
    pub(super) fn receive(
        &mut self,
        now: Duration,
        from: SocketAddr,
        data: &[u8],
        sink: &mut impl Sink,
    ) {
        let Some(&flags) = data.first() else {
            return;
        };
        if flags & FLAG_DATAGRAM == 0 {
            if let Err(error) = self.offline(now, from, data, sink) {
                trace!(
                    addr = %from,
                    error = &error as &(dyn std::error::Error + 'static),
                    "raknet offline message dropped"
                );
            }
            return;
        }
        let Some(conn) = self.conns.get_mut(&from) else {
            trace!(addr = %from, "raknet datagram from an unknown address dropped");
            return;
        };
        if let Err(reason) = conn.receive(now, from, data, &mut self.messages, sink) {
            self.remove(from, reason, sink);
        }
    }

    /// Runs timers: ACKs, resends, pings, timeouts, graceful closes and table sweeps.
    pub(super) fn update(&mut self, now: Duration, sink: &mut impl Sink) {
        if now >= self.next_sweep {
            self.pings
                .retain(|_, bucket| now.saturating_sub(bucket.refilled()) < PING_IDLE);
            self.next_sweep = now.saturating_add(SWEEP_INTERVAL);
        }
        let mut doomed = std::mem::take(&mut self.doomed);
        for (&addr, conn) in &mut self.conns {
            if let Err(reason) = conn.update(now, addr, sink) {
                doomed.push((addr, reason));
            }
        }
        for (addr, reason) in doomed.drain(..) {
            self.remove(addr, reason, sink);
        }
        self.doomed = doomed;
    }

    /// Sends a game message (0xfe is prepended) on connection `id` at `addr`.
    pub(super) fn send(
        &mut self,
        now: Duration,
        addr: SocketAddr,
        id: u64,
        message: Vec<u8>,
        sink: &mut impl Sink,
    ) {
        let Some(conn) = self.conns.get_mut(&addr) else {
            return;
        };
        if conn.id != id || conn.state != State::Open {
            return;
        }
        let sent = conn
            .peer
            .send(wire::ID_GAME, message)
            .and_then(|()| conn.peer.flush(now, &mut |data| sink.transmit(addr, data)));
        if let Err(error) = sent {
            self.remove(addr, error.into(), sink);
        }
    }

    /// Marks connection `id` as logged in by the session: it no longer counts against the
    /// pre-login limits.
    pub(super) fn logged_in(&mut self, addr: SocketAddr, id: u64) {
        let Some(conn) = self.conns.get_mut(&addr) else {
            return;
        };
        if conn.id == id && conn.pre_login {
            conn.pre_login = false;
            let ip = conn.ip;
            self.release(ip);
        }
    }

    /// Closes connection `id` after what is queued has been delivered.
    pub(super) fn close(&mut self, now: Duration, addr: SocketAddr, id: u64) {
        if let Some(conn) = self.conns.get_mut(&addr)
            && conn.id == id
            && conn.state == State::Open
        {
            conn.state = State::Closing {
                deadline: now.saturating_add(CLOSE_TIMEOUT),
            };
        }
    }

    /// Stops accepting connections, drops unfinished handshakes and closes the rest gracefully.
    pub(super) fn shutdown(&mut self, now: Duration, sink: &mut impl Sink) {
        self.shutting_down = true;
        let mut doomed = std::mem::take(&mut self.doomed);
        for (&addr, conn) in &mut self.conns {
            match conn.state {
                State::Handshake => doomed.push((addr, Close::Shutdown)),
                State::Open => {
                    conn.state = State::Closing {
                        deadline: now.saturating_add(CLOSE_TIMEOUT),
                    };
                }
                State::Closing { .. } => {}
            }
        }
        for (addr, reason) in doomed.drain(..) {
            self.remove(addr, reason, sink);
        }
        self.doomed = doomed;
    }

    fn remove(&mut self, addr: SocketAddr, reason: Close, sink: &mut impl Sink) {
        let Some(mut conn) = self.conns.remove(&addr) else {
            return;
        };
        if !matches!(reason, Close::Remote) {
            let mut transmit = |data: &[u8]| sink.transmit(addr, data);
            if let Err(error) =
                conn.peer
                    .send_now(wire::ID_DISCONNECT_NOTIFICATION, &[], &mut transmit)
            {
                debug!(
                    %addr,
                    error = &error as &(dyn std::error::Error + 'static),
                    "raknet disconnect notification not sent"
                );
            }
        }
        if conn.pre_login {
            self.release(conn.ip);
        }
        if conn.state != State::Handshake {
            sink.close(conn.id);
        }
        debug!(%addr, id = conn.id, %reason, "raknet connection closed");
    }

    fn release(&mut self, ip: u128) {
        self.pre_login = self.pre_login.saturating_sub(1);
        if let Entry::Occupied(mut entry) = self.pre_login_per_ip.entry(ip) {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.remove();
            }
        }
    }

    fn offline(
        &mut self,
        now: Duration,
        from: SocketAddr,
        data: &[u8],
        sink: &mut impl Sink,
    ) -> Result<()> {
        let mut cur = Cursor::new(data);
        match cur.u8()? {
            wire::ID_UNCONNECTED_PING | wire::ID_UNCONNECTED_PING_OPEN_CONNECTIONS => {
                let time = cur.array()?;
                cur.magic()?;
                cur.u64()?;
                if self.pong.len() > MAX_PONG_DATA_BYTES {
                    return Err(Error::Limit("pong data length"));
                }
                if !self.allow_ping(now, ip_key(from.ip())) {
                    return Err(Error::Limit("ping rate"));
                }
                self.buf.clear();
                wire::unconnected_pong(&mut self.buf, time, self.config.guid, &self.pong)?;
            }
            wire::ID_OPEN_CONNECTION_REQUEST_1 if self.config.accept => {
                cur.magic()?;
                let protocol = cur.u8()?;
                // The request is padded to the MTU it probes (go-raknet `OpenConnectionRequest1`).
                let mtu = data.len().saturating_add(UDP_HEADER_BYTES);
                if mtu < usize::from(MIN_MTU) {
                    return Err(Error::Malformed("mtu"));
                }
                self.buf.clear();
                if protocol == wire::PROTOCOL_VERSION {
                    let mtu = u16::try_from(mtu)
                        .unwrap_or(u16::MAX)
                        .min(self.config.max_mtu);
                    wire::open_connection_reply_1(&mut self.buf, self.config.guid, mtu);
                } else {
                    wire::incompatible_protocol_version(&mut self.buf, self.config.guid);
                }
            }
            wire::ID_OPEN_CONNECTION_REQUEST_2 if self.config.accept => {
                return self.request_2(now, from, cur, sink);
            }
            _ => return Err(Error::Malformed("unexpected offline message")),
        }
        sink.transmit(from, &self.buf);
        Ok(())
    }

    fn request_2(
        &mut self,
        now: Duration,
        from: SocketAddr,
        mut cur: Cursor<'_>,
        sink: &mut impl Sink,
    ) -> Result<()> {
        // No cookie: BDS replies with security off, and the official client's cookie handling
        // cannot be observed. A spoofed source therefore can open a connection state here; the
        // pre-login caps (64 in total, 4 per IP, the 10 s handshake deadline and the 5 s idle
        // timeout) bound what such requests hold.
        cur.magic()?;
        cur.address()?;
        let mtu = cur.u16()?;
        let guid = cur.u64()?;
        // Vanilla clients always send a negative GUID (go-raknet).
        if guid >> 63 == 0 {
            return Err(Error::Malformed("client guid"));
        }
        if mtu < MIN_MTU {
            return Err(Error::Malformed("mtu"));
        }
        let mtu = mtu.min(self.config.max_mtu);
        if let Some(conn) = self.conns.get(&from) {
            // The reply was lost: answer again until the client's first datagram arrives.
            if conn.state == State::Handshake && !conn.peer.received_any() && conn.peer.mtu() == mtu
            {
                self.reply_2(from, mtu, sink);
            }
            return Ok(());
        }
        if self.shutting_down {
            return Err(Error::Closed);
        }
        let ip = ip_key(from.ip());
        let per_ip = self.pre_login_per_ip.get(&ip).copied().unwrap_or(0);
        if self.pre_login >= MAX_PRE_LOGIN || per_ip >= MAX_PRE_LOGIN_PER_IP {
            self.buf.clear();
            wire::no_free_incoming_connections(&mut self.buf, self.config.guid);
            sink.transmit(from, &self.buf);
            debug!(addr = %from, "raknet connection refused: too many before login");
            return Ok(());
        }
        let peer = Peer::new(now, mtu, self.config.limits)?;
        let id = self.next_id;
        self.next_id = id.saturating_add(1);
        self.conns.insert(
            from,
            Conn {
                id,
                ip,
                peer,
                state: State::Handshake,
                pre_login: true,
                created: now,
                next_ping: now.saturating_add(PING_INTERVAL),
            },
        );
        self.pre_login += 1;
        *self.pre_login_per_ip.entry(ip).or_default() += 1;
        self.reply_2(from, mtu, sink);
        Ok(())
    }

    fn reply_2(&mut self, to: SocketAddr, mtu: u16, sink: &mut impl Sink) {
        self.buf.clear();
        wire::open_connection_reply_2(&mut self.buf, self.config.guid, to, mtu);
        sink.transmit(to, &self.buf);
    }

    fn allow_ping(&mut self, now: Duration, ip: u128) -> bool {
        let tracked = self.pings.len();
        match self.pings.entry(ip) {
            Entry::Occupied(mut entry) => entry.get_mut().take(1, PING_RATE, now),
            Entry::Vacant(entry) => {
                if tracked >= MAX_PING_IPS {
                    return false;
                }
                entry
                    .insert(Bucket::full(PING_RATE, now))
                    .take(1, PING_RATE, now)
            }
        }
    }
}

/// Listener settings.
#[derive(Debug, Clone, Copy)]
pub(super) struct Config {
    pub(super) guid: u64,
    pub(super) max_mtu: u16,
    /// Whether to accept connections; `false` answers pings only.
    pub(super) accept: bool,
    pub(super) limits: Limits,
}

/// What the listener does to the outside world.
pub(super) trait Sink {
    /// Sends one UDP datagram. Delivery is best effort.
    fn transmit(&mut self, to: SocketAddr, datagram: &[u8]);
    /// Hands a connection that finished the handshake to the session layer. `false` rejects it.
    fn accept(&mut self, id: u64, addr: SocketAddr) -> bool;
    /// Hands a game message (after 0xfe) to the connection's session. `false` means its
    /// inbound queue is full and the connection closes.
    fn deliver(&mut self, id: u64, message: Vec<u8>) -> bool;
    /// Reports that an accepted connection is gone.
    fn close(&mut self, id: u64);
    /// Reports that an accepted connection's client answered a connected ping after `rtt`.
    fn ping(&mut self, id: u64, rtt: Duration);
    /// Reports an open connection's bytes still queued or unacknowledged, after each update.
    fn undelivered(&mut self, id: u64, bytes: usize);
}

/// One connection of the table: its reliability layer and where it is in its life.
#[derive(Debug)]
struct Conn {
    id: u64,
    ip: u128,
    peer: Peer,
    state: State,
    pre_login: bool,
    created: Duration,
    next_ping: Duration,
}

impl Conn {
    fn receive(
        &mut self,
        now: Duration,
        addr: SocketAddr,
        data: &[u8],
        messages: &mut Vec<Message>,
        sink: &mut impl Sink,
    ) -> Result<(), Close> {
        messages.clear();
        self.peer
            .receive(now, data, messages, &mut |data| sink.transmit(addr, data))?;
        for message in messages.drain(..) {
            self.handle(now, addr, message, sink)?;
        }
        self.peer
            .flush(now, &mut |data| sink.transmit(addr, data))?;
        Ok(())
    }

    fn handle(
        &mut self,
        now: Duration,
        addr: SocketAddr,
        message: Message,
        sink: &mut impl Sink,
    ) -> Result<(), Close> {
        match message.id {
            wire::ID_GAME => match self.state {
                State::Open => {
                    if !sink.deliver(self.id, message.body) {
                        return Err(Error::Limit("inbound queue").into());
                    }
                }
                State::Closing { .. } => {}
                State::Handshake => {
                    return Err(Error::Malformed("game data before the handshake").into());
                }
            },
            wire::ID_CONNECTED_PING => {
                let ping_time = Cursor::new(&message.body).u64()?;
                let mut reply = Vec::with_capacity(16);
                reply.extend_from_slice(&ping_time.to_be_bytes());
                reply.extend_from_slice(&millis(now).to_be_bytes());
                self.peer
                    .send_now(wire::ID_CONNECTED_PONG, &reply, &mut |data| {
                        sink.transmit(addr, data);
                    })?;
            }
            wire::ID_CONNECTED_PONG => {
                let mut cur = Cursor::new(&message.body);
                let sent = cur.u64()?;
                cur.u64()?;
                // The pong echoes the time of the ping it answers (RakLib's ping measure); one
                // from the future is the client's own invention and measures nothing.
                if self.state == State::Open
                    && let Some(rtt) = millis(now).checked_sub(sent)
                {
                    sink.ping(self.id, Duration::from_millis(rtt));
                }
            }
            wire::ID_DETECT_LOST_CONNECTIONS => self.ping(now, addr, sink)?,
            wire::ID_CONNECTION_REQUEST if self.state == State::Handshake => {
                let mut cur = Cursor::new(&message.body);
                cur.u64()?;
                let time = cur.u64()?;
                cur.u8()?;
                let accepted = wire::connection_request_accepted(addr, time, millis(now));
                self.peer
                    .send(wire::ID_CONNECTION_REQUEST_ACCEPTED, accepted)?;
            }
            wire::ID_NEW_INCOMING_CONNECTION if self.state == State::Handshake => {
                if !sink.accept(self.id, addr) {
                    return Err(Close::Rejected);
                }
                self.state = State::Open;
                debug!(%addr, id = self.id, mtu = self.peer.mtu(), "raknet connection opened");
            }
            wire::ID_DISCONNECT_NOTIFICATION => return Err(Close::Remote),
            _ => return Err(Error::Malformed("unexpected connected message").into()),
        }
        Ok(())
    }

    fn update(
        &mut self,
        now: Duration,
        addr: SocketAddr,
        sink: &mut impl Sink,
    ) -> Result<(), Close> {
        match self.state {
            State::Handshake if now.saturating_sub(self.created) >= HANDSHAKE_TIMEOUT => {
                return Err(Close::HandshakeTimeout);
            }
            State::Closing { deadline } if self.peer.is_settled() || now >= deadline => {
                return Err(Close::Done);
            }
            _ => {}
        }
        let idle = IDLE_TIMEOUT.saturating_add(self.peer.rtt().saturating_mul(2));
        if now.saturating_sub(self.peer.last_received()) > idle {
            return Err(Close::Timeout);
        }
        if now >= self.next_ping {
            self.ping(now, addr, sink)?;
        }
        self.peer.update(now);
        let mut transmit = |data: &[u8]| sink.transmit(addr, data);
        self.peer.flush_acks(&mut transmit);
        self.peer.flush(now, &mut transmit)?;
        if self.state == State::Open {
            sink.undelivered(self.id, self.peer.unsent());
        }
        Ok(())
    }

    fn ping(&mut self, now: Duration, addr: SocketAddr, sink: &mut impl Sink) -> Result<()> {
        self.next_ping = now.saturating_add(PING_INTERVAL);
        let time = millis(now).to_be_bytes();
        self.peer
            .send_now(wire::ID_CONNECTED_PING, &time, &mut |data| {
                sink.transmit(addr, data);
            })
    }
}

/// Where a connection is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before `NewIncomingConnection`.
    Handshake,
    Open,
    /// Delivering what is queued, then a disconnect notification.
    Closing {
        deadline: Duration,
    },
}

/// Why a connection ended.
#[derive(Debug, Clone, Copy)]
enum Close {
    /// The peer sent a disconnect notification.
    Remote,
    Timeout,
    HandshakeTimeout,
    /// A graceful close finished.
    Done,
    /// The session layer did not take the new connection: its accept queue was full or gone.
    Rejected,
    Shutdown,
    Failed(Error),
}

impl fmt::Display for Close {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote => f.write_str("peer disconnected"),
            Self::Timeout => f.write_str("timed out"),
            Self::HandshakeTimeout => f.write_str("handshake timed out"),
            Self::Done => f.write_str("closed"),
            Self::Rejected => f.write_str("not taken by the session layer"),
            Self::Shutdown => f.write_str("server shutting down"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl From<Error> for Close {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// The key for per-IP limits: an IPv4 address or an IPv6 /64, the block one host usually has.
pub(crate) fn ip_key(ip: IpAddr) -> u128 {
    match ip.to_canonical() {
        IpAddr::V4(ip) => u128::from(ip.to_ipv6_mapped()),
        IpAddr::V6(ip) => u128::from(ip) & !u128::from(u64::MAX),
    }
}

/// The milliseconds in `now`, saturating at the largest `u64`.
fn millis(now: Duration) -> u64 {
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::protocol::{Rng, fuzz};
    use crate::raknet::peer::tests::{datagram, header};
    use crate::raknet::wire::Reliability;

    pub(in crate::raknet) const GUID: u64 = 0x0123_4567_89ab_cdef;
    /// The message limit of the tests: about 2.4 MiB, thousands of fragments at the smallest MTU.
    pub(in crate::raknet) const MAX_MESSAGE: usize = 2_446_727;
    const PONG: &[u8] = b"MCPE;Anchor;1;1.26.60;0;20;1;Anchor;Survival;1;19132;19133;";
    const T: Duration = Duration::from_secs(100);

    pub(in crate::raknet) fn config() -> Config {
        Config {
            guid: GUID,
            max_mtu: MAX_MTU,
            accept: true,
            limits: Limits::new(MAX_MESSAGE).unwrap(),
        }
    }

    fn listener() -> Listener {
        Listener::new(config(), Arc::from(PONG))
    }

    /// Records what the listener does.
    #[derive(Debug)]
    struct Recorder {
        sent: Vec<(SocketAddr, Vec<u8>)>,
        accepted: Vec<u64>,
        delivered: Vec<(u64, Vec<u8>)>,
        closed: Vec<u64>,
        pings: Vec<(u64, Duration)>,
        undelivered: Vec<(u64, usize)>,
        accept: bool,
        deliver: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                sent: Vec::new(),
                accepted: Vec::new(),
                delivered: Vec::new(),
                closed: Vec::new(),
                pings: Vec::new(),
                undelivered: Vec::new(),
                accept: true,
                deliver: true,
            }
        }

        /// Takes the datagrams sent to `to`.
        fn take(&mut self, to: SocketAddr) -> Vec<Vec<u8>> {
            let (mine, rest) = std::mem::take(&mut self.sent)
                .into_iter()
                .partition(|(addr, _)| *addr == to);
            self.sent = rest;
            mine.into_iter().map(|(_, data)| data).collect()
        }
    }

    impl Sink for Recorder {
        fn transmit(&mut self, to: SocketAddr, datagram: &[u8]) {
            assert!(datagram.len() <= usize::from(MAX_MTU) - UDP_HEADER_BYTES);
            self.sent.push((to, datagram.to_vec()));
        }

        fn accept(&mut self, id: u64, _: SocketAddr) -> bool {
            if self.accept {
                self.accepted.push(id);
            }
            self.accept
        }

        fn deliver(&mut self, id: u64, message: Vec<u8>) -> bool {
            if self.deliver {
                self.delivered.push((id, message));
            }
            self.deliver
        }

        fn close(&mut self, id: u64) {
            self.closed.push(id);
        }

        fn ping(&mut self, id: u64, rtt: Duration) {
            self.pings.push((id, rtt));
        }

        fn undelivered(&mut self, id: u64, bytes: usize) {
            self.undelivered.push((id, bytes));
        }
    }

    /// A RakNet client for tests, on the same reliability core as the server.
    #[derive(Debug)]
    pub(in crate::raknet) struct Client {
        pub(in crate::raknet) addr: SocketAddr,
        pub(in crate::raknet) mtu: u16,
        pub(in crate::raknet) peer: Option<Peer>,
        /// Datagrams to send to the server.
        pub(in crate::raknet) outbox: Vec<Vec<u8>>,
        /// Offline replies from the server.
        pub(in crate::raknet) offline: Vec<Vec<u8>>,
        /// Connected messages from the server, in order.
        pub(in crate::raknet) messages: Vec<Message>,
    }

    impl Client {
        pub(in crate::raknet) fn new(addr: SocketAddr, mtu: u16) -> Self {
            Self {
                addr,
                mtu,
                peer: None,
                outbox: Vec::new(),
                offline: Vec::new(),
                messages: Vec::new(),
            }
        }

        pub(in crate::raknet) fn ping(time: u64) -> Vec<u8> {
            let mut data = vec![wire::ID_UNCONNECTED_PING];
            data.extend_from_slice(&time.to_be_bytes());
            data.extend_from_slice(&wire::MAGIC);
            data.extend_from_slice(&7_u64.to_be_bytes());
            data
        }

        /// `OpenConnectionRequest1` padded to `mtu` (go-raknet `MarshalBinary`).
        pub(in crate::raknet) fn request_1(mtu: u16, protocol: u8) -> Vec<u8> {
            let mut data = vec![wire::ID_OPEN_CONNECTION_REQUEST_1];
            data.extend_from_slice(&wire::MAGIC);
            data.push(protocol);
            data.resize(usize::from(mtu) - UDP_HEADER_BYTES, 0);
            data
        }

        /// `OpenConnectionRequest2` without a cookie (go-raknet when the server has no security).
        pub(in crate::raknet) fn request_2(mtu: u16, guid: u64) -> Vec<u8> {
            let mut data = vec![wire::ID_OPEN_CONNECTION_REQUEST_2];
            data.extend_from_slice(&wire::MAGIC);
            wire::put_address(&mut data, SocketAddr::from(([127, 0, 0, 1], 19_132)));
            data.extend_from_slice(&mtu.to_be_bytes());
            data.extend_from_slice(&guid.to_be_bytes());
            data
        }

        pub(in crate::raknet) fn start(&mut self) {
            self.outbox
                .push(Self::request_1(self.mtu, wire::PROTOCOL_VERSION));
        }

        /// Handles one datagram from the server.
        pub(in crate::raknet) fn handle(&mut self, now: Duration, data: &[u8]) {
            let Self {
                peer,
                outbox,
                messages,
                ..
            } = self;
            match data[0] {
                wire::ID_OPEN_CONNECTION_REPLY_1 => {
                    let mtu = u16::from_be_bytes(data[26..28].try_into().unwrap());
                    outbox.push(Self::request_2(mtu, 1 << 63 | 5));
                }
                wire::ID_OPEN_CONNECTION_REPLY_2 if peer.is_none() => {
                    let at = data.len() - 3;
                    let mtu = u16::from_be_bytes(data[at..at + 2].try_into().unwrap());
                    let mut new = Peer::new(now, mtu, config().limits).unwrap();
                    let mut request = 9_u64.to_be_bytes().to_vec();
                    request.extend_from_slice(&1234_u64.to_be_bytes());
                    request.push(0);
                    new.send(wire::ID_CONNECTION_REQUEST, request).unwrap();
                    new.flush(now, &mut |d| outbox.push(d.to_vec())).unwrap();
                    *peer = Some(new);
                }
                flags if flags & FLAG_DATAGRAM != 0 => {
                    let Some(peer) = peer else { return };
                    let mut received = Vec::new();
                    peer.receive(now, data, &mut received, &mut |d| outbox.push(d.to_vec()))
                        .unwrap();
                    for message in received {
                        if message.id == wire::ID_CONNECTION_REQUEST_ACCEPTED {
                            let mut body = Vec::new();
                            wire::put_address(&mut body, SocketAddr::from(([127, 0, 0, 1], 1)));
                            body.extend_from_slice(&[0; 16]);
                            peer.send(wire::ID_NEW_INCOMING_CONNECTION, body).unwrap();
                        }
                        messages.push(message);
                    }
                    peer.flush(now, &mut |d| outbox.push(d.to_vec())).unwrap();
                }
                _ => self.offline.push(data.to_vec()),
            }
        }

        pub(in crate::raknet) fn send(&mut self, now: Duration, id: u8, body: Vec<u8>) {
            let Self { peer, outbox, .. } = self;
            let peer = peer.as_mut().unwrap();
            peer.send(id, body).unwrap();
            peer.flush(now, &mut |d| outbox.push(d.to_vec())).unwrap();
        }

        pub(in crate::raknet) fn update(&mut self, now: Duration) {
            let Self { peer, outbox, .. } = self;
            if let Some(peer) = peer {
                peer.update(now);
                peer.flush_acks(&mut |d| outbox.push(d.to_vec()));
                peer.flush(now, &mut |d| outbox.push(d.to_vec())).unwrap();
            }
        }

        pub(in crate::raknet) fn game(&self) -> Vec<&[u8]> {
            self.messages
                .iter()
                .filter(|message| message.id == wire::ID_GAME)
                .map(|message| message.body.as_slice())
                .collect()
        }

        pub(in crate::raknet) fn disconnected(&self) -> bool {
            self.messages
                .iter()
                .any(|message| message.id == wire::ID_DISCONNECT_NOTIFICATION)
        }
    }

    /// Passes datagrams both ways over a perfect link until both sides are quiet.
    fn pump(listener: &mut Listener, sink: &mut Recorder, client: &mut Client, now: Duration) {
        for _ in 0..64 {
            let outgoing = std::mem::take(&mut client.outbox);
            let incoming = sink.take(client.addr);
            if outgoing.is_empty() && incoming.is_empty() {
                return;
            }
            for data in outgoing {
                listener.receive(now, client.addr, &data, sink);
            }
            for data in incoming {
                client.handle(now, &data);
            }
        }
        panic!("the link never went quiet");
    }

    /// Connects `client` and returns its connection ID.
    fn connect(
        listener: &mut Listener,
        sink: &mut Recorder,
        client: &mut Client,
        now: Duration,
    ) -> u64 {
        client.start();
        pump(listener, sink, client, now);
        assert_eq!(
            listener.conns.get(&client.addr).map(|c| c.state),
            Some(State::Open)
        );
        *sink.accepted.last().unwrap()
    }

    fn offline_reply(listener: &mut Listener, from: SocketAddr, data: &[u8]) -> Vec<Vec<u8>> {
        let mut sink = Recorder::new();
        listener.receive(T, from, data, &mut sink);
        sink.take(from)
    }

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], port))
    }

    #[test]
    fn ping_gets_the_pong() {
        let mut listener = listener();
        let replies = offline_reply(&mut listener, addr(1, 1), &Client::ping(42));
        let mut pong = vec![wire::ID_UNCONNECTED_PONG];
        pong.extend_from_slice(&42_u64.to_be_bytes());
        pong.extend_from_slice(&GUID.to_be_bytes());
        pong.extend_from_slice(&wire::MAGIC);
        pong.extend_from_slice(&u16::try_from(PONG.len()).unwrap().to_be_bytes());
        pong.extend_from_slice(PONG);
        assert_eq!(replies, [pong]);
        // A new payload is used from the next ping on; one too large for a datagram is not sent.
        listener.set_pong(Arc::from(&b"new"[..]));
        let replies = offline_reply(&mut listener, addr(1, 1), &Client::ping(1));
        assert!(replies[0].ends_with(b"\x00\x03new"));
        listener.set_pong(Arc::from(vec![b'x'; MAX_PONG_DATA_BYTES + 1]));
        assert!(offline_reply(&mut listener, addr(1, 1), &Client::ping(1)).is_empty());
        // A wrong magic gets nothing.
        let mut bad = Client::ping(1);
        bad[9] ^= 1;
        assert!(offline_reply(&mut listener, addr(1, 2), &bad).is_empty());
    }

    #[test]
    fn pings_past_the_rate_limit_get_no_reply() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        for _ in 0..11 {
            listener.receive(T, from, &Client::ping(0), &mut sink);
        }
        assert_eq!(sink.take(from).len(), PING_RATE.capacity);
        // Another port of the same IP shares the bucket.
        listener.receive(T, addr(1, 2), &Client::ping(0), &mut sink);
        assert!(sink.take(addr(1, 2)).is_empty());
        // Five per second refill: one every 200 ms.
        let later = T + Duration::from_millis(199);
        listener.receive(later, from, &Client::ping(0), &mut sink);
        assert!(sink.take(from).is_empty());
        let later = T + Duration::from_millis(400);
        listener.receive(later, from, &Client::ping(0), &mut sink);
        assert_eq!(sink.take(from).len(), 1);
    }

    /// Pings closer together than a millisecond still refill at the rate: each one keeps the
    /// fraction of a ping the time since the last refill paid for.
    #[test]
    fn pings_just_under_a_millisecond_apart_refill_at_the_rate() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        for _ in 0..PING_RATE.capacity {
            listener.receive(T, from, &Client::ping(0), &mut sink);
        }
        sink.take(from);
        // Over 999 ms, five per second earn four pings: at 200, 400, 600 and 800 ms.
        for n in 1..=1000 {
            let now = T + Duration::from_micros(999 * n);
            listener.receive(now, from, &Client::ping(0), &mut sink);
        }
        assert_eq!(sink.take(from).len(), 4);
    }

    #[test]
    fn full_ping_table_ignores_new_ips() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        for index in 0..u32::try_from(MAX_PING_IPS).unwrap() {
            let from = SocketAddr::from((index.to_be_bytes(), 1));
            listener.receive(T, from, &Client::ping(0), &mut sink);
        }
        assert_eq!(sink.sent.len(), MAX_PING_IPS);
        let new = addr(200, 1);
        listener.receive(T, new, &Client::ping(0), &mut sink);
        assert!(sink.take(new).is_empty());
        // Entries idle for a minute go away.
        let later = T + PING_IDLE;
        listener.update(later, &mut sink);
        assert!(listener.pings.is_empty());
        listener.receive(later, new, &Client::ping(0), &mut sink);
        assert_eq!(sink.take(new).len(), 1);
    }

    #[test]
    fn ipv6_clients_share_limits_per_64_block() {
        let a = SocketAddr::from(([0x2001, 0xdb8, 0, 1, 0, 0, 0, 1], 1));
        let b = SocketAddr::from(([0x2001, 0xdb8, 0, 1, 9, 9, 9, 9], 1));
        assert_eq!(ip_key(a.ip()), ip_key(b.ip()));
        let c = SocketAddr::from(([0x2001, 0xdb8, 0, 2, 0, 0, 0, 1], 1));
        assert_ne!(ip_key(a.ip()), ip_key(c.ip()));
        let mapped = SocketAddr::from(([0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001], 1));
        assert_eq!(ip_key(mapped.ip()), ip_key(addr(1, 1).ip()));
    }

    #[test]
    fn open_connection_request_1_negotiates_the_mtu() {
        let mut listener = Listener::new(
            Config {
                max_mtu: 1200,
                ..config()
            },
            Arc::from(PONG),
        );
        for (request, reply) in [(1492, 1200), (1200, 1200), (576, 576), (MIN_MTU, MIN_MTU)] {
            let replies = offline_reply(
                &mut listener,
                addr(1, 1),
                &Client::request_1(request, wire::PROTOCOL_VERSION),
            );
            let data = &replies[0];
            assert_eq!(data.len(), 28);
            assert_eq!(data[0], wire::ID_OPEN_CONNECTION_REPLY_1);
            assert_eq!(data[1..17], wire::MAGIC);
            assert_eq!(data[17..25], GUID.to_be_bytes());
            assert_eq!(data[25], 0, "security off, as BDS sends");
            assert_eq!(data[26..28], u16::to_be_bytes(reply));
        }
        let small = Client::request_1(MIN_MTU - 1, wire::PROTOCOL_VERSION);
        assert!(offline_reply(&mut listener, addr(1, 1), &small).is_empty());
    }

    #[test]
    fn protocol_mismatch_gets_incompatible_protocol_version() {
        let mut listener = listener();
        let replies = offline_reply(&mut listener, addr(1, 1), &Client::request_1(576, 10));
        let mut expected = vec![wire::ID_INCOMPATIBLE_PROTOCOL_VERSION, 11];
        expected.extend_from_slice(&wire::MAGIC);
        expected.extend_from_slice(&GUID.to_be_bytes());
        assert_eq!(replies, [expected]);
    }

    #[test]
    fn open_connection_request_2_needs_a_negative_guid_and_a_valid_mtu() {
        let mut listener = listener();
        let from = addr(1, 1);
        let guid = 1 << 63;
        for bad in [
            Client::request_2(1492, 5),
            Client::request_2(MIN_MTU - 1, guid),
            Client::request_2(1492, guid)[..20].to_vec(),
        ] {
            assert!(offline_reply(&mut listener, from, &bad).is_empty());
        }
        assert!(listener.conns.is_empty());
        let replies = offline_reply(&mut listener, from, &Client::request_2(1300, guid));
        assert_eq!(replies.len(), 1);
        let reply = &replies[0];
        assert_eq!(reply[0], wire::ID_OPEN_CONNECTION_REPLY_2);
        assert_eq!(reply[17..25], GUID.to_be_bytes());
        assert_eq!(reply[25..32], [4, !10, !0, !0, !1, 0, 1]);
        assert_eq!(reply[32..34], 1300_u16.to_be_bytes());
        assert_eq!(reply.len(), 35);
        // A larger request is cut to `max-mtu-size`.
        let replies = offline_reply(&mut listener, addr(1, 2), &Client::request_2(1500, guid));
        assert_eq!(replies[0][32..34], MAX_MTU.to_be_bytes());
    }

    #[test]
    fn lost_reply_2_is_sent_again_until_the_first_datagram() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        let request = Client::request_2(1492, 1 << 63);
        listener.receive(T, from, &request, &mut sink);
        listener.receive(T, from, &request, &mut sink);
        assert_eq!(sink.take(from).len(), 2);
        assert_eq!(listener.conns.len(), 1);
        assert_eq!(listener.pre_login, 1);
    }

    #[test]
    fn connection_request_is_accepted_and_new_incoming_connection_opens() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        assert_eq!(sink.accepted, [id]);
        let accepted = &client.messages[0];
        assert_eq!(accepted.id, wire::ID_CONNECTION_REQUEST_ACCEPTED);
        let body = &accepted.body;
        assert_eq!(body[..7], [4, !10, !0, !0, !1, 0, 1]);
        assert_eq!(body[7..9], [0, 0]);
        for address in body[9..9 + 140].chunks(7) {
            assert_eq!(address, [4, 0xff, 0xff, 0xff, 0xff, 0, 0]);
        }
        assert_eq!(body[149..157], 1234_u64.to_be_bytes());
        assert_eq!(body[157..165], millis(T).to_be_bytes());
        assert_eq!(body.len(), 165);
    }

    #[test]
    fn ipv6_addresses_are_written_as_go_raknet_does() {
        let mut buf = Vec::new();
        let ip = [0x2001, 0xdb8, 0, 0, 0, 0, 0, 1];
        wire::put_address(&mut buf, SocketAddr::from((ip, 19_132)));
        let mut expected = vec![6, 23, 0];
        expected.extend_from_slice(&19_132_u16.to_be_bytes());
        expected.extend_from_slice(&[0; 4]);
        expected.extend_from_slice(&std::net::Ipv6Addr::from(ip).octets());
        expected.extend_from_slice(&[0; 4]);
        assert_eq!(buf, expected);
    }

    #[test]
    fn pre_login_limits_reply_no_free_incoming_connections() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut ids = Vec::new();
        for port in 0..4 {
            let mut client = Client::new(addr(1, port), 1492);
            ids.push(connect(&mut listener, &mut sink, &mut client, T));
        }
        let mut fifth = Client::new(addr(1, 4), 1492);
        fifth.start();
        pump(&mut listener, &mut sink, &mut fifth, T);
        let mut refused = vec![wire::ID_NO_FREE_INCOMING_CONNECTIONS];
        refused.extend_from_slice(&wire::MAGIC);
        refused.extend_from_slice(&GUID.to_be_bytes());
        assert_eq!(fifth.offline, [refused.clone()]);
        // A login frees a slot of its IP.
        listener.logged_in(addr(1, 0), ids[0]);
        let mut again = Client::new(addr(1, 4), 1492);
        connect(&mut listener, &mut sink, &mut again, T);
        // 64 in total across IPs.
        for last in 2..17 {
            for port in 0..4 {
                let mut client = Client::new(addr(last, port), 1492);
                connect(&mut listener, &mut sink, &mut client, T);
            }
        }
        assert_eq!(listener.pre_login, 64);
        let mut over = Client::new(addr(99, 1), 1492);
        over.start();
        pump(&mut listener, &mut sink, &mut over, T);
        assert_eq!(over.offline, [refused]);
        // Removal frees slots too.
        listener.close(T, addr(2, 0), sink.accepted[5]);
        listener.update(T, &mut sink);
        assert_eq!(listener.pre_login, 64);
        listener.update(T + CLOSE_TIMEOUT, &mut sink);
        assert_eq!(listener.pre_login, 63);
        assert_eq!(
            listener.pre_login_per_ip.get(&ip_key(addr(2, 0).ip())),
            Some(&3)
        );
    }

    #[test]
    fn game_messages_flow_both_ways() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 576);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        client.send(T, wire::ID_GAME, b"hello".to_vec());
        let big = vec![3; 100_000];
        client.send(T, wire::ID_GAME, big.clone());
        listener.send(T, client.addr, id, big.clone(), &mut sink);
        listener.send(T, client.addr, id + 1, b"stale".to_vec(), &mut sink);
        pump(&mut listener, &mut sink, &mut client, T);
        for step in 1..20 {
            let now = T + Duration::from_millis(step * 10);
            listener.update(now, &mut sink);
            client.update(now);
            pump(&mut listener, &mut sink, &mut client, now);
        }
        assert_eq!(sink.delivered, [(id, b"hello".to_vec()), (id, big.clone())]);
        assert_eq!(client.game(), [big.as_slice()]);
    }

    /// A connection's session learns what it has yet to deliver: a message counts from the
    /// update after it was sent until the client acknowledged it.
    #[test]
    fn updates_report_the_bytes_not_yet_acknowledged() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 576);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        let big = vec![3; 100_000];
        listener.send(T, client.addr, id, big.clone(), &mut sink);
        listener.update(T, &mut sink);
        assert!(
            matches!(sink.undelivered.last(), Some(&(i, bytes)) if i == id && bytes > big.len())
        );
        for step in 1..20 {
            let now = T + Duration::from_millis(step * 10);
            listener.update(now, &mut sink);
            client.update(now);
            pump(&mut listener, &mut sink, &mut client, now);
        }
        assert_eq!(client.game(), [big.as_slice()]);
        assert_eq!(sink.undelivered.last(), Some(&(id, 0)));
    }

    #[test]
    fn accepted_queue_full_closes_the_connection() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        sink.accept = false;
        let mut client = Client::new(addr(1, 1), 1492);
        client.start();
        pump(&mut listener, &mut sink, &mut client, T);
        assert!(listener.conns.is_empty());
        assert!(client.disconnected());
        assert!(sink.closed.is_empty());
        assert_eq!(listener.pre_login, 0);
    }

    #[test]
    fn inbound_queue_full_disconnects() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        sink.deliver = false;
        client.send(T, wire::ID_GAME, b"x".to_vec());
        pump(&mut listener, &mut sink, &mut client, T);
        assert!(listener.conns.is_empty());
        assert_eq!(sink.closed, [id]);
        assert!(client.disconnected());
    }

    #[test]
    fn unexpected_messages_disconnect() {
        for (id, body) in [
            (0x42, Vec::new()),
            (wire::ID_CONNECTION_REQUEST_ACCEPTED, Vec::new()),
            (wire::ID_NEW_INCOMING_CONNECTION, Vec::new()),
            (wire::ID_CONNECTED_PING, vec![1]),
        ] {
            let mut listener = listener();
            let mut sink = Recorder::new();
            let mut client = Client::new(addr(1, 1), 1492);
            connect(&mut listener, &mut sink, &mut client, T);
            client.send(T, id, body);
            pump(&mut listener, &mut sink, &mut client, T);
            assert!(listener.conns.is_empty(), "message {id:#x}");
        }
    }

    #[test]
    fn game_data_before_the_handshake_disconnects() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        listener.receive(T, from, &Client::request_2(1492, 1 << 63), &mut sink);
        let frame = header(Reliability::Unreliable);
        let data = datagram(0, &[(frame, b"\xfegame")]);
        listener.receive(T, from, &data, &mut sink);
        assert!(listener.conns.is_empty());
        assert!(sink.delivered.is_empty());
    }

    #[test]
    fn connected_ping_gets_a_pong() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        connect(&mut listener, &mut sink, &mut client, T);
        client.send(T, wire::ID_CONNECTED_PING, 77_u64.to_be_bytes().to_vec());
        pump(&mut listener, &mut sink, &mut client, T);
        let pong = client.messages.last().unwrap();
        assert_eq!(pong.id, wire::ID_CONNECTED_PONG);
        assert_eq!(pong.body[..8], 77_u64.to_be_bytes());
        assert_eq!(pong.body[8..], millis(T).to_be_bytes());
    }

    /// The server's connected ping and its pong to the client's ping go out at once, even while
    /// reliable data waits for the congestion window: neither is dropped, and neither waits
    /// behind the data, whose wait would count in the round trip time.
    #[test]
    fn connected_pings_and_pongs_pass_a_full_window() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        // The client acknowledges nothing here, so the window stays full.
        listener.send(T, client.addr, id, vec![9; 200_000], &mut sink);
        client.send(T, wire::ID_CONNECTED_PING, 77_u64.to_be_bytes().to_vec());
        for data in std::mem::take(&mut client.outbox) {
            listener.receive(T, client.addr, &data, &mut sink);
        }
        let later = T + PING_INTERVAL;
        listener.update(later, &mut sink);
        for data in sink.take(client.addr) {
            client.handle(later, &data);
        }
        let ids: Vec<u8> = client.messages.iter().map(|message| message.id).collect();
        assert!(ids.contains(&wire::ID_CONNECTED_PONG));
        assert!(ids.contains(&wire::ID_CONNECTED_PING));
    }

    /// A connected pong echoes the time of the server's ping: the difference is the ping the
    /// session reports (`RakLib` `Session::handlePong`, PMMP `NetworkSession::getPing`).
    #[test]
    fn a_connected_pong_measures_the_ping() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        let pong = |time: u64| [time.to_be_bytes(), 0_u64.to_be_bytes()].concat();
        let later = T + Duration::from_millis(40);
        client.send(later, wire::ID_CONNECTED_PONG, pong(millis(T)));
        pump(&mut listener, &mut sink, &mut client, later);
        assert_eq!(sink.pings, [(id, Duration::from_millis(40))]);
        // A pong from the future measures nothing.
        client.send(later, wire::ID_CONNECTED_PONG, pong(millis(later) + 1));
        pump(&mut listener, &mut sink, &mut client, later);
        assert_eq!(sink.pings.len(), 1);
    }

    #[test]
    fn handshake_and_idle_timeouts_close_connections() {
        // No datagram after OpenConnectionReply2: the idle timeout.
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        listener.receive(T, from, &Client::request_2(1492, 1 << 63), &mut sink);
        listener.update(T + IDLE_TIMEOUT, &mut sink);
        assert_eq!(listener.conns.len(), 1);
        listener.update(T + IDLE_TIMEOUT + Duration::from_millis(1), &mut sink);
        assert!(listener.conns.is_empty());
        assert_eq!(listener.pre_login, 0);
        // Activity without NewIncomingConnection: the handshake timeout.
        let mut listener = self::listener();
        let mut sink = Recorder::new();
        listener.receive(T, from, &Client::request_2(1492, 1 << 63), &mut sink);
        let frame = header(Reliability::Unreliable);
        for second in 0..10 {
            let now = T + Duration::from_secs(second);
            let seq = u32::try_from(second).unwrap();
            let ping = datagram(seq, &[(frame, b"\x00\x00\x00\x00\x00\x00\x00\x00\x00")]);
            listener.receive(now, from, &ping, &mut sink);
            listener.update(now, &mut sink);
            assert_eq!(listener.conns.len(), 1);
        }
        listener.update(T + HANDSHAKE_TIMEOUT, &mut sink);
        assert!(listener.conns.is_empty());
        // An open connection that goes silent.
        let mut client = Client::new(addr(1, 2), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        listener.update(T + Duration::from_secs(6), &mut sink);
        assert!(listener.conns.is_empty());
        assert_eq!(sink.closed, [id]);
    }

    #[test]
    fn disconnect_notification_closes_without_reply() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        client.send(T, wire::ID_DISCONNECT_NOTIFICATION, Vec::new());
        let outgoing = std::mem::take(&mut client.outbox);
        for data in outgoing {
            listener.receive(T, client.addr, &data, &mut sink);
        }
        assert!(listener.conns.is_empty());
        assert_eq!(sink.closed, [id]);
        assert!(sink.take(client.addr).is_empty());
    }

    #[test]
    fn graceful_close_delivers_queued_messages_first() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 1492);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        let big = vec![9; 200_000];
        listener.send(T, client.addr, id, big.clone(), &mut sink);
        listener.close(T, client.addr, id);
        // Sends after close are dropped.
        listener.send(T, client.addr, id, b"late".to_vec(), &mut sink);
        let mut now = T;
        while !listener.conns.is_empty() {
            now += Duration::from_millis(10);
            listener.update(now, &mut sink);
            client.update(now);
            pump(&mut listener, &mut sink, &mut client, now);
            assert!(now < T + CLOSE_TIMEOUT);
        }
        assert_eq!(client.game(), [big.as_slice()]);
        assert!(client.disconnected());
        assert_eq!(sink.closed, [id]);
    }

    #[test]
    fn shutdown_closes_every_connection_and_refuses_new_ones() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut open = Client::new(addr(1, 1), 1492);
        connect(&mut listener, &mut sink, &mut open, T);
        let half = addr(1, 2);
        listener.receive(T, half, &Client::request_2(1492, 1 << 63), &mut sink);
        listener.shutdown(T, &mut sink);
        assert_eq!(listener.conns.len(), 1);
        let late = addr(1, 3);
        listener.receive(T, late, &Client::request_2(1492, 1 << 63), &mut sink);
        assert_eq!(listener.conns.len(), 1);
        let mut now = T;
        while !listener.is_empty() {
            now += Duration::from_millis(10);
            listener.update(now, &mut sink);
            open.update(now);
            pump(&mut listener, &mut sink, &mut open, now);
            assert!(now < T + CLOSE_TIMEOUT);
        }
        assert!(open.disconnected());
        // Pings are still answered.
        assert_eq!(
            offline_reply(&mut listener, addr(1, 4), &Client::ping(0)).len(),
            1
        );
    }

    #[test]
    fn accept_off_answers_pings_only() {
        let mut listener = Listener::new(
            Config {
                accept: false,
                ..config()
            },
            Arc::from(PONG),
        );
        let from = addr(1, 1);
        assert_eq!(
            offline_reply(&mut listener, from, &Client::ping(0)).len(),
            1
        );
        let request = Client::request_1(1492, wire::PROTOCOL_VERSION);
        assert!(offline_reply(&mut listener, from, &request).is_empty());
        let request = Client::request_2(1492, 1 << 63);
        assert!(offline_reply(&mut listener, from, &request).is_empty());
        assert!(listener.conns.is_empty());
    }

    /// A link with loss, duplication and reordering, driven by a fixed-seed generator.
    struct Link {
        rng: Rng,
        /// (arrival time, to the server, datagram)
        flying: Vec<(Duration, bool, Vec<u8>)>,
    }

    impl Link {
        fn carry(&mut self, now: Duration, to_server: bool, data: &[u8]) {
            // 10 % lost, 5 % duplicated, 0 to 40 ms of delay (which reorders).
            if self.rng.below(100) < 10 {
                return;
            }
            let copies = if self.rng.below(100) < 5 { 2 } else { 1 };
            for _ in 0..copies {
                let delay = Duration::from_millis(u64::try_from(self.rng.below(41)).unwrap());
                self.flying.push((now + delay, to_server, data.to_vec()));
            }
        }

        fn arrived(&mut self, now: Duration) -> Vec<(bool, Vec<u8>)> {
            let (due, rest) = std::mem::take(&mut self.flying)
                .into_iter()
                .partition(|(at, _, _)| *at <= now);
            self.flying = rest;
            let mut due: Vec<_> = due;
            due.sort_by_key(|(at, _, _)| *at);
            due.into_iter()
                .map(|(_, to_server, data)| (to_server, data))
                .collect()
        }
    }

    #[test]
    fn messages_arrive_in_order_over_a_lossy_link() {
        let mut rng = Rng(0x5eed);
        let mut messages = |count: usize| -> Vec<Vec<u8>> {
            let mut list: Vec<Vec<u8>> = (0..count)
                .map(|index| {
                    let len = 1 + rng.below(3000);
                    let byte = u8::try_from(index % 251).unwrap();
                    vec![byte; len]
                })
                .collect();
            // Largest messages, thousands of fragments each at MTU 576.
            list.insert(10, vec![0xaa; MAX_MESSAGE]);
            list.insert(60, vec![0xbb; MAX_MESSAGE - 1000]);
            list
        };
        let to_server = messages(150);
        let to_client = messages(150);

        let mut listener = listener();
        let mut sink = Recorder::new();
        let mut client = Client::new(addr(1, 1), 576);
        let id = connect(&mut listener, &mut sink, &mut client, T);
        sink.delivered.clear();
        let mut link = Link {
            rng: Rng(0x11e7),
            flying: Vec::new(),
        };

        let mut now = T;
        let mut next = 0;
        let deadline = T + Duration::from_secs(600);
        while sink.delivered.len() < to_server.len() || client.game().len() < to_client.len() {
            now += Duration::from_millis(1);
            assert!(
                now < deadline,
                "stalled at {} and {}",
                sink.delivered.len(),
                client.game().len()
            );
            // Five new messages each way every 10 ms.
            if now.as_millis().is_multiple_of(10) {
                for message in to_server.iter().skip(next).take(5) {
                    client.send(now, wire::ID_GAME, message.clone());
                }
                for message in to_client.iter().skip(next).take(5) {
                    listener.send(now, client.addr, id, message.clone(), &mut sink);
                }
                next += 5;
                listener.update(now, &mut sink);
                client.update(now);
            }
            for data in sink.take(client.addr) {
                link.carry(now, false, &data);
            }
            for data in std::mem::take(&mut client.outbox) {
                link.carry(now, true, &data);
            }
            for (to_server, data) in link.arrived(now) {
                if to_server {
                    listener.receive(now, client.addr, &data, &mut sink);
                } else {
                    client.handle(now, &data);
                }
            }
            assert_eq!(listener.conns.len(), 1, "connection dropped");
        }
        assert!(
            sink.delivered
                .iter()
                .map(|(_, message)| message)
                .eq(to_server.iter())
        );
        assert!(
            client
                .game()
                .into_iter()
                .eq(to_client.iter().map(Vec::as_slice))
        );
        assert!(sink.delivered.iter().all(|(from, _)| *from == id));
    }

    #[test]
    fn mutated_datagrams_never_panic() {
        let mut listener = listener();
        let mut sink = Recorder::new();
        let from = addr(1, 1);
        // Offline messages from many addresses.
        let offline = [
            Client::ping(5),
            Client::request_1(MIN_MTU, wire::PROTOCOL_VERSION),
            Client::request_2(1492, 1 << 63),
            vec![wire::ID_OPEN_CONNECTION_REQUEST_2],
        ];
        let mut port = 0_u16;
        for seed in &offline {
            fuzz(seed, 1250, |data| {
                port = port.wrapping_add(1);
                listener.receive(T, addr(2, port), data, &mut sink);
                listener.receive(T, from, data, &mut sink);
                Ok(())
            });
        }
        listener.update(T + Duration::from_secs(20), &mut sink);
        assert!(listener.conns.is_empty());

        // Connected datagrams: captured from a client, then mutated against a live connection.
        sink.sent.clear();
        let mut client = Client::new(from, 576);
        connect(&mut listener, &mut sink, &mut client, T);
        client.send(T, wire::ID_GAME, vec![1; 2000]);
        client.send(T, wire::ID_CONNECTED_PING, vec![0; 8]);
        let mut seeds = std::mem::take(&mut client.outbox);
        let peer = client.peer.as_mut().unwrap();
        let mut acks = Vec::new();
        peer.flush_acks(&mut |d| acks.push(d.to_vec()));
        seeds.extend(acks);
        let mut nak = vec![FLAG_DATAGRAM | wire::FLAG_NAK, 0, 1, 0];
        wire::put_u24(&mut nak, 0);
        wire::put_u24(&mut nak, 3);
        seeds.push(nak);
        let seeds: Vec<_> = seeds.into_iter().take(5).collect();
        let cases = 5000 / seeds.len();
        for seed in &seeds {
            fuzz(seed, cases, |data| {
                if !listener.conns.contains_key(&from) {
                    let mut client = Client::new(from, 576);
                    connect(&mut listener, &mut sink, &mut client, T);
                }
                listener.receive(T, from, data, &mut sink);
                listener.update(T, &mut sink);
                sink.sent.clear();
                Ok(())
            });
        }
    }
}
