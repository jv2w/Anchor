//! Connection sessions: batches, encryption, the login sequence and the tick queues.
//!
//! [`start`] opens both transports (RakNet and NetherNet at once) and spawns the accept task. The
//! accept task numbers new connections of both in arrival order (the one place that allocates
//! [`SessionId`]s, so no shared counter is needed), spawns a session task for each and keeps the
//! server-list status current (the RakNet pong and the NetherNet JSON).
//! A session task owns one connection: it decodes batches, runs the login sequence of PLAN.md
//! 10.2 and then relays packets between the connection and the tick.

mod connection;
mod transport;

use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, warn};

use crate::auth::Auth;
use crate::clock;
use crate::config::{Config, GameMode};
use crate::crypto::{self, ServerKey};
use crate::error::{Error, Result};
use crate::nethernet;
use crate::protocol::batch;
use crate::protocol::packet::{GAME_VERSION, PROTOCOL_VERSION};
use crate::raknet;
use crate::tick::queue::{InputSender, SessionId};

use transport::Transport;

/// New connections waiting for the accept task, per transport; when full, the transport closes
/// the newest.
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
    nethernet: nethernet::Handle,
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
/// without keys does not start. NetherNet loads its identity key from `data_dir` (PLAN.md 10.3).
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
    let v4 = Some(general.server_ip);
    let v6 = general.enable_ipv6.then_some(general.server_ipv6);
    let settings = raknet::Settings {
        v4: v4.map(|ip| SocketAddrV4::new(ip, general.server_port)),
        v6: v6.map(|ip| SocketAddrV6::new(ip, general.server_port, 0, 0)),
        max_mtu: config.developer.max_mtu_size,
        max_message: max_message(&batch).saturating_add(CHECKSUM_BYTES),
        accept: config.developer.raknet,
        guid,
    };
    let status = Status::new(&config, guid);
    let (pong_tx, pong_rx) = watch::channel(status.pong(0));
    let (json_tx, json_rx) = watch::channel(status.json(0));
    let (raknet_tx, raknet_rx) = mpsc::channel(ACCEPT_QUEUE);
    let raknet = raknet::spawn(settings, pong_rx, raknet_tx).map_err(Error::Network)?;
    info!(addresses = ?raknet.local_addrs(), "raknet listening");

    let settings = nethernet::Settings {
        signaling_v4: v4.map(|ip| SocketAddrV4::new(ip, general.server_port)),
        signaling_v6: v6.map(|ip| SocketAddrV6::new(ip, general.server_port, 0, 0)),
        udp_v4: v4.map(|ip| SocketAddrV4::new(ip, general.nethernet_port)),
        udp_v6: v6.map(|ip| SocketAddrV6::new(ip, general.nethernet_port, 0, 0)),
        public_ip: general.public_ip,
        max_message: max_message(&batch),
        tls: config
            .developer
            .nethernet_tls_cert
            .clone()
            .zip(config.developer.nethernet_tls_key.clone()),
    };
    let (nethernet_tx, nethernet_rx) = mpsc::channel(ACCEPT_QUEUE);
    let nethernet =
        match nethernet::start(settings, data_dir, json_rx, auth.clone(), nethernet_tx).await {
            Ok(nethernet) => nethernet,
            Err(e) => {
                raknet.shutdown().await;
                return Err(e.into());
            }
        };
    info!(
        signaling = ?nethernet.signaling_addrs(),
        udp = ?nethernet.udp_addrs(),
        "nethernet listening"
    );

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
        raknet,
        nethernet,
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
        self.nethernet.shutdown().await;
    }
}

/// The largest game message a client may send, without transport framing: the reassembly bound
/// of PLAN.md 3 (the worst Snappy expansion of the largest batch and its header byte).
fn max_message(batch: &batch::Settings) -> usize {
    let max = batch.max_batch_bytes();
    max.saturating_add(max / 6).saturating_add(42)
}

/// Wall-clock Unix seconds for token checks; before 1970 reads as 0 and fails every check.
fn unix_now() -> i64 {
    clock::unix_now()
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(0)
}

/// The accept task: numbers connections, spawns sessions, reaps them and keeps the status
/// current.
struct Accept {
    shared: Arc<Shared>,
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
        loop {
            tokio::select! {
                _ = &mut stop => break,
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
                        // A panic in one session ends only that connection (CLAUDE.md 4.6).
                        warn!(error = %e, "session task failed");
                    }
                }
            }
        }
        self.drain().await;
    }

    fn spawn(&mut self, conn: Transport) {
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
        self.json.send_replace(self.status.json(self.logged_in));
    }

    /// Tells sessions to end and waits for them, at most [`STOP_GRACE`]; the rest are aborted.
    async fn drain(&mut self) {
        self.shutdown.send_replace(true);
        self.raknet.close();
        self.nethernet.close();
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

/// The server-list status. The RakNet pong uses the field order BDS 1.26.52 answers with (P6):
/// `MCPE;motd;protocol;version;players;max;guid;level;mode;1;port v4;port v6;0;1;1;`. BDS sends
/// `Survival;1`; what it sends after other modes is unverified, so the `1` is kept for all. The
/// NetherNet JSON uses the key order of BDS's `GET /v1/join` (PLAN.md 10.3).
#[derive(Debug)]
struct Status {
    motd: String,
    max_players: u32,
    guid: u64,
    level_name: String,
    mode: GameMode,
    port: u16,
}

/// The body of `GET /v1/join`, keys in BDS order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Join<'a> {
    name: &'a str,
    protocol: i32,
    version: &'a str,
    level: &'a str,
    players: usize,
    max_players: u32,
    game_type: u8,
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
            game_type: match self.mode {
                GameMode::Survival => 0,
                GameMode::Creative => 1,
                GameMode::Adventure => 2,
                GameMode::Spectator => 6,
            },
        };
        // Serializing a struct of strings and integers into memory cannot fail; the fallback
        // keeps that a non-panicking path.
        Arc::from(serde_json::to_vec(&join).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_join_status_has_the_bds_keys_in_order() {
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
}
