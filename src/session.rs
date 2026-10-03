//! Connection sessions: batches, encryption, the login sequence and the tick queues.
//!
//! [`start`] opens the transports and spawns the accept task. The accept task numbers new
//! connections in arrival order (the one place that allocates [`SessionId`]s, so no shared
//! counter is needed), spawns a session task for each and keeps the server-list pong current.
//! A session task owns one connection: it decodes batches, runs the login sequence of PLAN.md
//! 10.2 and then relays packets between the connection and the tick.

mod connection;

use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, warn};

use crate::auth::Auth;
use crate::clock;
use crate::config::{Config, GameMode};
use crate::crypto::{self, ServerKey};
use crate::error::{Error, Result};
use crate::protocol::batch;
use crate::protocol::packet::{GAME_VERSION, PROTOCOL_VERSION};
use crate::raknet;
use crate::tick::queue::{InputSender, SessionId};

/// New connections waiting for the accept task; when full, RakNet closes the newest.
const ACCEPT_QUEUE: usize = 64;
/// Session events waiting for the accept task. A session sends at most two (logged in, ended)
/// and only after taking a permit with `send().await`, so a full queue only delays it.
const EVENT_QUEUE: usize = 256;
/// How long [`Network::stop`] waits for sessions to send what they still hold.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Bytes a RakNet message may add to a compressed batch: the 8-byte checksum.
const CHECKSUM_BYTES: usize = 8;

/// Running networking: the transports, the accept task and every session.
#[derive(Debug)]
pub(crate) struct Network {
    raknet: raknet::Handle,
    stop: oneshot::Sender<()>,
    accept: JoinHandle<()>,
}

/// What every session shares, read-only.
#[derive(Debug)]
struct Shared {
    batch: batch::Settings,
    auth: Option<Auth>,
    server_key: ServerKey,
    inputs: InputSender,
    events: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
}

/// A session's report to the accept task.
#[derive(Debug, Clone, Copy)]
enum Event {
    /// The session passed login verification.
    LoggedIn,
    /// The session ended; `logged_in` tells whether it had sent [`Event::LoggedIn`].
    Ended { logged_in: bool },
}

/// Starts networking on the current runtime; sessions queue their inputs on `inputs`.
///
/// With `xbox-auth = true` the authorization keys load first (PLAN.md 10.2), and a server
/// without keys does not start.
pub(crate) async fn start(
    config: Arc<Config>,
    data_dir: &Path,
    inputs: InputSender,
) -> Result<Network> {
    let batch = config.batch_settings()?;
    let auth = if config.general.xbox_auth {
        let auth = Auth::start(data_dir, unix_now)
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
    let settings = raknet::Settings {
        v4: Some(SocketAddrV4::new(general.server_ip, general.server_port)),
        v6: general
            .enable_ipv6
            .then(|| SocketAddrV6::new(general.server_ipv6, general.server_port, 0, 0)),
        max_mtu: config.developer.max_mtu_size,
        max_message: max_message(&batch),
        accept: config.developer.raknet,
        guid,
    };
    let status = Status::new(&config, guid);
    let (pong_tx, pong_rx) = watch::channel(status.pong(0));
    let (accepted_tx, accepted_rx) = mpsc::channel(ACCEPT_QUEUE);
    let raknet = raknet::spawn(settings, pong_rx, accepted_tx).map_err(Error::Network)?;
    info!(addresses = ?raknet.local_addrs(), "raknet listening");

    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        batch,
        auth,
        server_key,
        inputs,
        events: events_tx,
        shutdown: shutdown_rx,
    });
    let (stop_tx, stop_rx) = oneshot::channel();
    let accept = Accept {
        shared,
        accepted: accepted_rx,
        events: events_rx,
        sessions: JoinSet::new(),
        next_session: 0,
        logged_in: 0,
        status,
        pong: pong_tx,
        shutdown: shutdown_tx,
    };
    let accept = tokio::spawn(accept.run(stop_rx));
    Ok(Network {
        raknet,
        stop: stop_tx,
        accept,
    })
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
            warn!(error = %e, "accept task failed");
        }
        self.raknet.shutdown().await;
    }
}

/// The largest RakNet message a client may send: the reassembly bound of PLAN.md 3 (the worst
/// Snappy expansion of the largest batch and its header byte) plus the encryption checksum.
fn max_message(batch: &batch::Settings) -> usize {
    let max = batch.max_batch_bytes();
    max.saturating_add(max / 6)
        .saturating_add(42)
        .saturating_add(CHECKSUM_BYTES)
}

/// Wall-clock Unix seconds for token checks; before 1970 reads as 0 and fails every check.
fn unix_now() -> i64 {
    clock::unix_now()
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(0)
}

/// The accept task: numbers connections, spawns sessions, reaps them and keeps the pong current.
struct Accept {
    shared: Arc<Shared>,
    accepted: mpsc::Receiver<raknet::Connection>,
    events: mpsc::Receiver<Event>,
    sessions: JoinSet<()>,
    next_session: u64,
    logged_in: usize,
    status: Status,
    pong: watch::Sender<Arc<[u8]>>,
    shutdown: watch::Sender<bool>,
}

impl Accept {
    async fn run(mut self, mut stop: oneshot::Receiver<()>) {
        // `false` once the transport ends, so a closed queue is not polled again in a loop.
        let mut accepting = true;
        loop {
            tokio::select! {
                _ = &mut stop => break,
                conn = self.accepted.recv(), if accepting => {
                    if let Some(conn) = conn {
                        self.spawn(conn);
                    } else {
                        error!("the raknet transport stopped; no new connections are accepted");
                        accepting = false;
                    }
                }
                // `Shared` keeps a sender, so this queue never closes while the task runs.
                Some(event) = self.events.recv() => self.on_event(event),
                Some(ended) = self.sessions.join_next(), if !self.sessions.is_empty() => {
                    if let Err(e) = ended {
                        // A panic in one session ends only that connection (CLAUDE.md 4.6).
                        warn!(error = %e, "session task failed");
                    }
                }
            }
        }
        self.drain().await;
    }

    fn spawn(&mut self, conn: raknet::Connection) {
        let id = SessionId::new(self.next_session);
        // A u64 counter does not wrap in a process lifetime; saturating keeps it panic-free.
        self.next_session = self.next_session.saturating_add(1);
        debug!(session = id.get(), addr = %conn.addr(), "connection accepted");
        self.sessions
            .spawn(connection::run(id, conn, Arc::clone(&self.shared)));
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::LoggedIn => self.logged_in = self.logged_in.saturating_add(1),
            Event::Ended { logged_in: true } => self.logged_in = self.logged_in.saturating_sub(1),
            Event::Ended { logged_in: false } => return,
        }
        self.pong.send_replace(self.status.pong(self.logged_in));
    }

    /// Tells sessions to end and waits for them, at most [`STOP_GRACE`]; the rest are aborted.
    async fn drain(&mut self) {
        self.shutdown.send_replace(true);
        self.accepted.close();
        let grace = tokio::time::sleep(STOP_GRACE);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                () = &mut grace => {
                    warn!(sessions = self.sessions.len(), "sessions did not end in time");
                    self.sessions.shutdown().await;
                    return;
                }
                // Sessions may still report while ending; keep their queue moving.
                Some(_) = self.events.recv() => {}
                ended = self.sessions.join_next() => match ended {
                    None => return,
                    Some(Err(e)) => warn!(error = %e, "session task failed"),
                    Some(Ok(())) => {}
                },
            }
        }
    }
}

/// The server-list pong in the field order BDS 1.26.52 answers with (P6):
/// `MCPE;motd;protocol;version;players;max;guid;level;mode;1;port v4;port v6;0;1;1;`. BDS sends
/// `Survival;1`; what it sends after other modes is unverified, so the `1` is kept for all.
#[derive(Debug)]
struct Status {
    motd: String,
    max_players: u32,
    guid: u64,
    level_name: String,
    mode: &'static str,
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
            mode: match general.gamemode {
                GameMode::Survival => "Survival",
                GameMode::Creative => "Creative",
                GameMode::Adventure => "Adventure",
                GameMode::Spectator => "Spectator",
            },
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
        let text = format!(
            "MCPE;{motd};{PROTOCOL_VERSION};{GAME_VERSION};{players};{max_players};{guid};\
             {level_name};{mode};1;{port};{port};0;1;1;"
        );
        Arc::from(text.into_bytes())
    }
}
