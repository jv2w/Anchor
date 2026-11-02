//! Connection sessions: batches, encryption, the login sequence and the tick queues.
//!
//! [`start`] opens both transports (one UDP port for both, and NetherNet's TCP signaling on the
//! same port number) and spawns the accept task. The
//! accept task numbers new connections of both in arrival order (the one place that allocates
//! [`SessionId`]s, so no shared counter is needed), spawns a session task for each and keeps the
//! server-list status current (the RakNet pong and the NetherNet JSON).
//! A session task owns one connection: it decodes batches, runs the login sequence (with the
//! resource pack download of [`packs`]) and then relays packets between the connection and the
//! tick.
//!
//! The accept task also does what the tick asks of the networking ([`Control`]: the server
//! name, blocked addresses) and reports the connection counts to the tick.

mod connection;
mod packs;
mod transport;

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::{join, pin, select};
use tracing::{debug, error, info, warn};

use crate::auth::Auth;
use crate::clock;
use crate::config::{Config, GameMode};
use crate::crypto::{self, ServerKey};
use crate::error::{Error, Result};
use crate::nethernet;
use crate::pack::Offer;
use crate::protocol::batch;
use crate::protocol::packet::{GAME_VERSION, PROTOCOL_VERSION};
use crate::raknet;
use crate::tick::queue::{Control, InputKind, InputSender, Measure, SessionId, SessionInputs};
use crate::udp;

use transport::Transport;

/// New connections waiting for the accept task, per transport; when full, the transport closes
/// the newest.
const ACCEPT_QUEUE: usize = 64;
/// Session events waiting for the accept task. A session sends at most two (logged in, ended)
/// and only after taking a permit with `send().await`, so a full queue only delays it.
const EVENT_QUEUE: usize = 256;
/// Addresses blocked at once; a block past it is refused with a warning.
const MAX_BLOCKED: usize = 4096;
/// How often the accept task reports changed connection counts to the tick and forgets ended
/// blocks. Each report is a logged input, so the counts the tick reads are at most this old.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// The player lists the tick sends a player before its spawn status: one with the start game and
/// one in the spawn window.
const SPAWN_PLAYER_LISTS: usize = 2;
/// How long [`Network::stop`] waits for sessions to send what they still hold.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Running networking: the transports, the accept task and every session.
#[derive(Debug)]
pub(crate) struct Network {
    udp: udp::Handle,
    nethernet: nethernet::Handle,
    stop: oneshot::Sender<()>,
    accept: JoinHandle<()>,
}

impl Network {
    /// Stops accepting, lets sessions send what they still hold (at most [`STOP_GRACE`]) and
    /// closes every connection. Call it after the tick has stopped: joined sessions end when the
    /// tick drops their outbound queues.
    pub(crate) async fn stop(self) {
        if self.stop.send(()).is_err() {
            debug!("accept task already ended");
        }
        if let Err(e) = self.accept.await {
            warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "accept task failed"
            );
        }
        // Signaling stops while the UDP task lets both transports' connections deliver their
        // last messages.
        join!(self.udp.shutdown(), self.nethernet.shutdown());
    }
}

/// What every session shares, read-only.
#[derive(Debug)]
struct Shared {
    batch: batch::Settings,
    auth: Option<Auth>,
    server_key: ServerKey,
    /// The resource packs offered to every player.
    packs: Offer,
    inputs: InputSender,
    events: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
    /// What a session that is still in the login sequence when the server stops sees.
    shutdown_message: String,
    /// Whether RakNet logins are encrypted (`enable-encryption`).
    encryption: bool,
    /// One permit per session that has logged in but not joined: `max-players` at start.
    /// Logged-in sessions have left the pre-login limit, and the tick counts players only
    /// at the join.
    waiting: Arc<Semaphore>,
    /// The most a session's output before its spawn status moves its spawn deadline, so the
    /// deadline ends even when no spawn status comes ([`max_send_extension`]).
    max_send_extension: Duration,
}

/// A session's report to the accept task.
#[derive(Debug, Clone, Copy)]
enum Event {
    /// The session passed login verification.
    LoggedIn,
    /// A session that had sent [`Event::LoggedIn`] ended.
    Ended,
}

/// The accept task: numbers connections, spawns sessions, reaps them, keeps the status current
/// and does the tick's controls.
struct Accept {
    shared: Arc<Shared>,
    controls: mpsc::Receiver<Control>,
    blocks: Blocks,
    /// Where the connection counts go to the tick.
    network: SessionInputs,
    /// The counts last reported: all connections, logged in.
    reported: (u32, u32),
    raknet: mpsc::Receiver<raknet::Connection>,
    nethernet: mpsc::Receiver<nethernet::Connection>,
    events: mpsc::Receiver<Event>,
    sessions: JoinSet<()>,
    next_session: u64,
    logged_in: usize,
    status: Status,
    pong: watch::Sender<Arc<[u8]>>,
    json: watch::Sender<Arc<[u8]>>,
    shutdown: watch::Sender<bool>,
}

impl Accept {
    async fn run(mut self, mut stop: oneshot::Receiver<()>) {
        // `false` once a transport ends, so a closed queue is not polled again in a loop.
        let (mut raknet, mut nethernet) = (true, true);
        // `false` once the tick has stopped and dropped its end.
        let mut controls = true;
        let mut report = tokio::time::interval(REPORT_INTERVAL);
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            select! {
                _ = &mut stop => break,
                control = self.controls.recv(), if controls => match control {
                    Some(control) => self.on_control(control),
                    None => controls = false,
                },
                _ = report.tick() => self.report(),
                conn = self.raknet.recv(), if raknet => {
                    if let Some(conn) = conn {
                        self.spawn(Transport::Raknet(conn));
                    } else {
                        error!("the raknet transport stopped; it accepts no new connections");
                        raknet = false;
                    }
                }
                conn = self.nethernet.recv(), if nethernet => {
                    if let Some(conn) = conn {
                        self.spawn(Transport::Nethernet(conn));
                    } else {
                        error!("the nethernet transport stopped; it accepts no new connections");
                        nethernet = false;
                    }
                }
                // `Shared` keeps a sender, so this queue never closes while the task runs.
                Some(event) = self.events.recv() => self.on_event(event),
                Some(ended) = self.sessions.join_next(), if !self.sessions.is_empty() => {
                    if let Err(e) = ended {
                        // A panic in one session ends only that connection.
                        warn!(error = &e as &(dyn std::error::Error + 'static), "session task failed");
                    }
                }
            }
        }
        self.drain().await;
    }

    fn spawn(&mut self, conn: Transport) {
        let ip = conn.addr().ip();
        if self.blocks.is_blocked(ip, clock::now()) {
            debug!(addr = %conn.addr(), "connection from a blocked address refused");
            conn.close();
            return;
        }
        let id = SessionId::new(self.next_session);
        // A u64 counter does not wrap in a process lifetime; saturating keeps it panic-free.
        self.next_session = self.next_session.saturating_add(1);
        debug!(session = id.get(), addr = %conn.addr(), "connection accepted");
        self.sessions
            .spawn(connection::run(id, conn, Arc::clone(&self.shared)));
    }

    /// Does what the tick asked: a new server name in the pong and the join status, or a block
    /// or its end.
    fn on_control(&mut self, control: Control) {
        match control {
            Control::Motd(motd) => {
                self.status.motd = motd;
                self.pong.send_replace(self.status.pong(self.logged_in));
                self.json.send_replace(self.status.json(self.logged_in));
            }
            Control::Block { ip, duration } => {
                if !self.blocks.block(ip, duration, clock::now()) {
                    warn!(%ip, max = MAX_BLOCKED, "too many blocked addresses; block refused");
                }
            }
            Control::Unblock(ip) => self.blocks.unblock(ip),
        }
    }

    /// Forgets ended blocks and reports the connection counts to the tick if they changed. A
    /// full share of the tick queue skips the report; the next one sends the counts then.
    fn report(&mut self) {
        self.blocks.forget_ended(clock::now());
        let all = u32::try_from(self.sessions.len()).unwrap_or(u32::MAX);
        let logged_in = u32::try_from(self.logged_in).unwrap_or(u32::MAX);
        if self.reported == (all, logged_in) {
            return;
        }
        let measure = Measure::Connections { all, logged_in };
        match self.network.try_send(InputKind::Measure(measure)) {
            Ok(()) => self.reported = (all, logged_in),
            Err(e) => debug!(
                error = &e as &(dyn std::error::Error + 'static),
                "connection counts not reported"
            ),
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::LoggedIn => self.logged_in = self.logged_in.saturating_add(1),
            Event::Ended => self.logged_in = self.logged_in.saturating_sub(1),
        }
        self.pong.send_replace(self.status.pong(self.logged_in));
        self.json.send_replace(self.status.json(self.logged_in));
    }

    /// Tells sessions to end and waits for them, at most [`STOP_GRACE`]; the rest are aborted.
    async fn drain(&mut self) {
        self.shutdown.send_replace(true);
        self.raknet.close();
        self.nethernet.close();
        let grace = tokio::time::sleep(STOP_GRACE);
        pin!(grace);
        loop {
            select! {
                () = &mut grace => {
                    warn!(sessions = self.sessions.len(), "sessions did not end in time");
                    self.sessions.shutdown().await;
                    return;
                }
                // Sessions may still report while ending; keep their queue moving.
                Some(_) = self.events.recv() => {}
                ended = self.sessions.join_next() => match ended {
                    Some(Ok(())) => {}
                    Some(Err(e)) => {
                        warn!(
                            error = &e as &(dyn std::error::Error + 'static),
                            "session task failed"
                        );
                    }
                    None => return,
                },
            }
        }
    }
}

/// Addresses whose new connections are refused, by canonical IP
/// (an IPv4-mapped IPv6 address is its IPv4 address), with when each block ends; `None` lasts
/// until it is lifted. At most [`MAX_BLOCKED`].
#[derive(Debug, Default)]
struct Blocks(BTreeMap<IpAddr, Option<Instant>>);

impl Blocks {
    /// Whether connections from `ip` are refused at `now`.
    fn is_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        self.0
            .get(&ip.to_canonical())
            .is_some_and(|until| until.is_none_or(|until| now < until))
    }

    /// Blocks `ip` from `now` for `duration` (endless if its end is past what [`Instant`]
    /// holds), replacing an earlier block of it. False, blocking nothing, when [`MAX_BLOCKED`]
    /// other addresses stay blocked after the ended blocks are forgotten.
    fn block(&mut self, ip: IpAddr, duration: Duration, now: Instant) -> bool {
        let ip = ip.to_canonical();
        if !self.0.contains_key(&ip) && self.0.len() >= MAX_BLOCKED {
            self.forget_ended(now);
            if self.0.len() >= MAX_BLOCKED {
                return false;
            }
        }
        self.0.insert(ip, now.checked_add(duration));
        true
    }

    fn unblock(&mut self, ip: IpAddr) {
        self.0.remove(&ip.to_canonical());
    }

    fn forget_ended(&mut self, now: Instant) {
        self.0
            .retain(|_, until| until.is_none_or(|until| now < until));
    }
}

/// The server-list status. The RakNet pong uses the field order the official server 1.26.52 answers with (P6):
/// `MCPE;motd;protocol;version;players;max;guid;level;mode;1;port v4;port v6;0;1;1;`. It sends
/// `Survival;1`; what it sends after other modes is unverified, so the `1` is kept for all. The
/// NetherNet JSON uses the key order of the official server's `GET /v1/join`.
#[derive(Debug)]
struct Status {
    motd: String,
    max_players: u32,
    guid: u64,
    level_name: String,
    mode: GameMode,
    port: u16,
}

impl Status {
    fn new(config: &Config, guid: u64) -> Self {
        let general = &config.general;
        Self {
            motd: general.motd.clone(),
            max_players: general.max_players,
            guid,
            level_name: general.level_name.clone(),
            mode: general.gamemode,
            port: general.server_port,
        }
    }

    /// The pong for `players` online. Config validation keeps `;` out of motd and level-name,
    /// so the fields stay apart. The IPv6 socket shares the IPv4 port.
    fn pong(&self, players: usize) -> Arc<[u8]> {
        let Self {
            motd,
            max_players,
            guid,
            level_name,
            mode,
            port,
        } = self;
        let mode = match mode {
            GameMode::Survival => "Survival",
            GameMode::Creative => "Creative",
            GameMode::Adventure => "Adventure",
            GameMode::Spectator => "Spectator",
        };
        let text = format!(
            "MCPE;{motd};{PROTOCOL_VERSION};{GAME_VERSION};{players};{max_players};{guid};\
             {level_name};{mode};1;{port};{port};0;1;1;"
        );
        Arc::from(text.into_bytes())
    }

    /// The `GET /v1/join` body for `players` online. `gameType` is the Bedrock game type number
    /// (spectator is 6; the Mojang guide lists only 0 to 2).
    fn json(&self, players: usize) -> Arc<[u8]> {
        let join = Join {
            name: &self.motd,
            protocol: PROTOCOL_VERSION,
            version: GAME_VERSION,
            level: &self.level_name,
            players,
            max_players: self.max_players,
            game_type: i32::from(self.mode.game_type()),
        };
        #[expect(
            clippy::expect_used,
            reason = "invariant: a struct of strings and integers serializes into memory"
        )]
        let json = serde_json::to_vec(&join).expect("the join status serializes");
        Arc::from(json)
    }
}

/// The body of `GET /v1/join`, keys in the official server's order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Join<'a> {
    name: &'a str,
    protocol: i32,
    version: &'a str,
    level: &'a str,
    players: usize,
    max_players: u32,
    game_type: i32,
}

/// Starts networking on the current runtime; sessions queue their inputs on `inputs`.
///
/// With `xbox-auth = true` the authorization keys load first, and a server without keys does not
/// start. NetherNet loads its identity key from `data_dir`. The accept task takes the
/// tick's `controls` and reports the connection counts on `inputs`. Players are offered `packs`.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one function opens every part of networking in its order"
)]
pub(crate) async fn start(
    config: Arc<Config>,
    packs: Offer,
    data_dir: &Path,
    inputs: InputSender,
    controls: mpsc::Receiver<Control>,
) -> Result<Network> {
    let batch = config.batch_settings()?;
    if !packs.packs().is_empty() {
        info!(count = packs.packs().len(), "resource packs loaded");
    }
    let auth = if config.general.xbox_auth {
        let auth = Auth::start(data_dir, clock::token_now)
            .await
            .map_err(|e| Error::Auth(Box::new(e)))?;
        Some(auth)
    } else {
        None
    };
    let server_key = ServerKey::generate().map_err(|e| Error::Start(std::io::Error::other(e)))?;
    let mut guid = [0; 8];
    crypto::fill_random(&mut guid).map_err(|e| Error::Start(std::io::Error::other(e)))?;
    let guid = u64::from_le_bytes(guid);

    let general = &config.general;
    let v4 = Some(general.server_ip);
    let v6 = general.enable_ipv6.then_some(general.server_ipv6);
    let settings = raknet::Settings {
        max_mtu: config.developer.max_mtu_size,
        // An encrypted RakNet message adds the checksum to the batch.
        max_message_bytes: max_message_bytes(&batch)
            .saturating_add(crypto::encryption::CHECKSUM_BYTES),
        accept: config.developer.raknet,
        guid,
    };
    let status = Status::new(&config, guid);
    let (pong_tx, pong_rx) = watch::channel(status.pong(0));
    let (json_tx, json_rx) = watch::channel(status.json(0));

    let v4 = v4.map(|ip| SocketAddrV4::new(ip, general.server_port));
    let v6 = v6.map(|ip| SocketAddrV6::new(ip, general.server_port, 0, 0));
    let sockets = udp::Sockets::bind(v4, v6)?;
    // The UDP task starts last: a pong then means that every listener is ready.
    let nethernet_settings = nethernet::Settings {
        signaling_v4: v4,
        signaling_v6: v6,
        public_ip: general.public_ip,
        max_message_bytes: max_message_bytes(&batch),
        tls: config
            .developer
            .nethernet_tls_cert
            .clone()
            .zip(config.developer.nethernet_tls_key.clone()),
    };
    let (nethernet_tx, nethernet_rx) = mpsc::channel(ACCEPT_QUEUE);
    let (nethernet, nethernet_driver) = nethernet::start(
        nethernet_settings,
        data_dir,
        json_rx,
        auth.clone(),
        nethernet_tx,
        Arc::clone(&sockets),
    )
    .await?;
    for addr in nethernet.signaling_addrs() {
        info!(%addr, "listening for NetherNet signaling on TCP");
    }
    let (raknet_tx, raknet_rx) = mpsc::channel(ACCEPT_QUEUE);
    let raknet = match raknet::Driver::new(settings, pong_rx, raknet_tx, Arc::clone(&sockets)) {
        Ok(raknet) => raknet,
        Err(e) => {
            nethernet.shutdown().await;
            return Err(Error::Start(std::io::Error::other(e)));
        }
    };
    let udp = udp::spawn(sockets, raknet, nethernet_driver);
    let udp_use = if config.developer.raknet {
        "listening for RakNet, NetherNet game data and server list pings on UDP"
    } else {
        "listening for NetherNet game data and server list pings on UDP (raknet = false)"
    };
    for addr in udp.local_addrs() {
        info!(%addr, "{udp_use}");
    }

    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    #[expect(
        clippy::expect_used,
        reason = "invariant: the configuration keeps `max-players` within 1 to 10,000"
    )]
    let waiting = usize::try_from(config.general.max_players).expect("max-players fits a usize");
    let shared = Arc::new(Shared {
        batch,
        auth,
        server_key,
        packs,
        inputs,
        events: events_tx,
        shutdown: shutdown_rx,
        shutdown_message: config.general.shutdown_message.clone(),
        encryption: config.developer.enable_encryption,
        waiting: Arc::new(Semaphore::new(waiting)),
        max_send_extension: max_send_extension(config.general.max_players),
    });
    let (stop_tx, stop_rx) = oneshot::channel();
    let network = shared.inputs.session(SessionId::NETWORK);
    let accept = Accept {
        shared,
        controls,
        blocks: Blocks::default(),
        network,
        reported: (0, 0),
        raknet: raknet_rx,
        nethernet: nethernet_rx,
        events: events_rx,
        sessions: JoinSet::new(),
        next_session: 0,
        logged_in: 0,
        status,
        pong: pong_tx,
        json: json_tx,
        shutdown: shutdown_tx,
    };
    let accept = tokio::spawn(accept.run(stop_rx));
    Ok(Network {
        udp,
        nethernet,
        stop: stop_tx,
        accept,
    })
}

/// The largest game message a client may send, without transport framing: the
/// reassembly bound (the worst Snappy expansion of the largest batch and its header byte).
fn max_message_bytes(batch: &batch::Settings) -> usize {
    let max = batch.max_batch_bytes();
    max.saturating_add(max / 6).saturating_add(42)
}

/// The most a session's output before its spawn status moves its spawn deadline: the
/// time the player lists sent before it take at the download floor rate
/// ([`packs::send_allowance`]) when `max_players` players (`max-players` at start) have skins of
/// the largest size. The deadline's own 60 seconds are left for the rest of that output.
fn max_send_extension(max_players: u32) -> Duration {
    let players = usize::try_from(max_players).unwrap_or(usize::MAX);
    let bytes = players
        .saturating_mul(SPAWN_PLAYER_LISTS)
        .saturating_mul(crate::tick::MAX_LIST_ENTRY_BYTES);
    packs::send_allowance(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_join_status_has_the_official_keys_in_order() {
        let mut config = Config::default();
        "Say \"hi\"".clone_into(&mut config.general.motd);
        let status = Status::new(&config, 7);
        let json = status.json(3);
        let expected = format!(
            "{{\"name\":\"Say \\\"hi\\\"\",\"protocol\":{PROTOCOL_VERSION},\"version\":\"{GAME_VERSION}\",\
             \"level\":\"world\",\"players\":3,\"maxPlayers\":20,\"gameType\":0}}"
        );
        assert_eq!(std::str::from_utf8(&json).unwrap(), expected);
    }

    // api: Network::blockAddress, Network::unblockAddress
    /// A block ends after its duration or when lifted, an endless one stays, an IPv4-mapped
    /// address is its IPv4 address, a new block replaces the old end, and past the bound only
    /// ended blocks make room.
    #[test]
    fn blocks_end_after_their_duration_and_stay_bounded() {
        let now = clock::now();
        let later = |secs| now + Duration::from_secs(secs);
        let ip = IpAddr::from([192, 0, 2, 7]);
        let mut blocks = Blocks::default();
        assert!(!blocks.is_blocked(ip, now));
        assert!(blocks.block(ip, Duration::from_secs(300), now));
        assert!(blocks.is_blocked(ip, later(299)));
        assert!(blocks.is_blocked("::ffff:192.0.2.7".parse().unwrap(), now));
        assert!(!blocks.is_blocked(ip, later(300)));
        // A new block replaces the end; an endless one outlives the clock.
        assert!(blocks.block(ip, Duration::from_secs(10), now));
        assert!(!blocks.is_blocked(ip, later(10)));
        assert!(blocks.block(ip, Duration::MAX, now));
        assert!(blocks.is_blocked(ip, later(1_000_000_000)));
        blocks.unblock("::ffff:192.0.2.7".parse().unwrap());
        assert!(!blocks.is_blocked(ip, now));
        // The bound: one more address is refused until a block has ended.
        for n in 0..MAX_BLOCKED {
            let ip = IpAddr::from(u32::try_from(n).unwrap().to_be_bytes());
            assert!(blocks.block(ip, Duration::from_secs(1), now));
        }
        assert!(!blocks.block(ip, Duration::from_secs(1), now));
        assert!(!blocks.is_blocked(ip, now));
        assert!(blocks.block(ip, Duration::from_secs(1), later(1)));
        assert_eq!(blocks.0.len(), 1, "the ended blocks were forgotten");
    }

    /// Waits for a pong of the server at `raknet` that contains `name`, at most 10 seconds.
    async fn shows(socket: &tokio::net::UdpSocket, raknet: std::net::SocketAddr, name: &str) {
        let start = clock::now();
        loop {
            let pong = crate::raknet::tests::ping(socket, raknet).await;
            if pong.windows(name.len()).any(|w| w == name.as_bytes()) {
                return;
            }
            assert!(
                clock::now() - start < Duration::from_secs(10),
                "the pong never showed {name:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Fetches the `GET /v1/join` body of the signaling server at `addr`. The limit only catches
    /// a server that never ends its answer.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test client reads the whole reply of the server under test"
    )]
    async fn join_status(addr: std::net::SocketAddr) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(b"GET /v1/join HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), tcp.read_to_end(&mut response))
            .await
            .expect("the signaling server ends its answer")
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        response.split("\r\n\r\n").nth(1).unwrap().to_owned()
    }

    // api: Network::setName, Network::blockAddress, Network::unblockAddress
    // api: Network::getConnectionCount, Network::getValidConnectionCount
    /// The tick's controls reach the networking: a new name shows in the RakNet pong and in
    /// `GET /v1/join`, a blocked address is refused at connect until it is unblocked, and the
    /// connection counts reach the tick as logged inputs.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test keeps a data folder"
    )]
    fn controls_rename_block_and_unblock_and_the_counts_reach_the_tick() {
        use std::net::{Ipv4Addr, SocketAddr};

        use crate::tick::queue::{control_queue, input_queue};

        let dir =
            std::env::temp_dir().join(format!("anchor-session-controls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Port 0: the system picks the ports at bind time, so no other process can take them
        // first; the test reads the bound addresses back.
        let mut config = Config::default();
        config.general.xbox_auth = false;
        config.general.enable_ipv6 = false;
        config.general.server_ip = Ipv4Addr::LOCALHOST;
        config.general.server_port = 0;
        let localhost = IpAddr::from(Ipv4Addr::LOCALHOST);
        crate::raknet::tests::run(async {
            let (inputs, mut queue) = input_queue(1);
            let (controls, control_queue) = control_queue();
            let packs =
                Offer::load(&[], Vec::new(), config.general.force_resources.into()).unwrap();
            let network = start(Arc::new(config), packs, &dir, inputs, control_queue)
                .await
                .unwrap();
            let raknet: SocketAddr = network.udp.local_addrs()[0];
            let signaling: SocketAddr = network.nethernet.signaling_addrs()[0];
            let socket = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            // A new name: the pong and the join status show it.
            controls
                .send(Control::Motd("Renamed server".to_owned()))
                .await
                .unwrap();
            shows(&socket, raknet, "MCPE;Renamed server;").await;
            assert!(
                join_status(signaling)
                    .await
                    .starts_with("{\"name\":\"Renamed server\"")
            );
            // Blocked: the connection is closed at once. The name after it shows that the
            // networking has read the block.
            let block = Control::Block {
                ip: localhost,
                duration: Duration::MAX,
            };
            controls.send(block).await.unwrap();
            controls
                .send(Control::Motd("Blocked".to_owned()))
                .await
                .unwrap();
            shows(&socket, raknet, "MCPE;Blocked;").await;
            let end = clock::now() + Duration::from_secs(10);
            assert!(
                crate::raknet::tests::disconnected_before(raknet, || clock::now() >= end).await
            );
            // Unblocked: the connection gets a session, which the next report counts.
            controls.send(Control::Unblock(localhost)).await.unwrap();
            controls
                .send(Control::Motd("Open".to_owned()))
                .await
                .unwrap();
            shows(&socket, raknet, "MCPE;Open;").await;
            // The connection stays until the first report has counted it; the deadline only
            // catches a server that never reports.
            let mut counts = Vec::new();
            let end = clock::now() + Duration::from_secs(10);
            let reported = || {
                while let Ok(input) = queue.try_recv() {
                    if let InputKind::Measure(Measure::Connections { all, logged_in }) = input.kind
                    {
                        assert_eq!(input.session, SessionId::NETWORK);
                        counts.push((all, logged_in));
                    }
                }
                !counts.is_empty() || clock::now() >= end
            };
            assert!(!crate::raknet::tests::disconnected_before(raknet, reported).await);
            assert_eq!(counts.first(), Some(&(1, 0)), "{counts:?}");
            drop(controls);
            network.stop().await;
        });
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }
}
