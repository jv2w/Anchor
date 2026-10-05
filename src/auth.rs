//! Authorization environment: discovery, OpenID configuration and JWKS, cached in `data/auth.json`.
//!
//! Start (PLAN.md 10.2): a cache younger than an hour is used as is; otherwise the documents are
//! fetched, falling back to the cache when that fails. With neither the server does not start. A
//! corrupt cache stops the start and is left untouched. While running, everything is fetched
//! again every hour, and a token with an unknown `kid` may trigger one JWKS fetch per five
//! minutes. Sessions read the newest [`Keys`] from a `watch` channel.

mod document;
mod fetch;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::crypto::RsaPublicKey;
use crate::fs::{self, Access};
use document::Cache;
use fetch::Client;

/// Base URL of the Minecraft service discovery.
const DISCOVERY_BASE: &str = "https://client.discovery.minecraft-services.net";
/// The cache file inside the data directory.
const CACHE_FILE: &str = "auth.json";
/// A cache younger than this many seconds is used at start without fetching.
const FRESH_SECONDS: i64 = 3600;
/// Time between full refreshes while running.
const REFRESH: Duration = Duration::from_secs(3600);
/// Least time between JWKS fetches caused by unknown key IDs.
const KID_COOLDOWN: Duration = Duration::from_secs(300);
/// Limit on one fetch, shared by its requests, from connecting to the end of the last body (DNS
/// lookups excluded: the OS bounds them).
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response body and largest cache file. The documents are a few KiB.
const MAX_DOCUMENT: u64 = 1024 * 1024;

/// The authorization keys a token is verified with.
#[derive(Debug)]
pub(crate) struct Keys {
    /// The OpenID `issuer`, compared with the token's `iss` as a string.
    issuer: String,
    by_kid: BTreeMap<String, RsaPublicKey>,
}

impl Keys {
    /// The expected token issuer.
    pub(crate) fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The key with this `kid`.
    pub(crate) fn key(&self, kid: &str) -> Option<&RsaPublicKey> {
        self.by_kid.get(kid)
    }
}

/// An authorization failure.
#[derive(Debug)]
pub(crate) enum Error {
    /// An HTTP request failed (connection, TLS, status, timeout or body limit).
    Http { url: String, source: ureq::Error },
    /// A fetched document is not the expected JSON.
    Json {
        url: String,
        source: serde_json::Error,
    },
    /// A fetched document has unusable values.
    Document { url: String, reason: &'static str },
    /// The cache file could not be read or written.
    Io { path: PathBuf, source: io::Error },
    /// The cache file is corrupt. It is left as it is; fix or delete it to start.
    CorruptCache {
        path: PathBuf,
        reason: &'static str,
        source: Option<serde_json::Error>,
    },
    /// Nothing could be fetched and there is no cache.
    Unavailable { source: Box<Error> },
    /// A blocking task ended without a result.
    Task(tokio::task::JoinError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { url, .. } => write!(f, "request to {url} failed"),
            Self::Json { url, .. } => write!(f, "malformed json from {url}"),
            Self::Document { url, reason } => write!(f, "unusable document from {url}: {reason}"),
            Self::Io { path, .. } => write!(f, "cannot access {}", path.display()),
            Self::CorruptCache { path, reason, .. } => write!(
                f,
                "corrupt authorization cache {} ({reason}); fix or delete it",
                path.display()
            ),
            Self::Unavailable { .. } => {
                f.write_str("authorization keys unavailable: fetch failed and no cache")
            }
            Self::Task(_) => f.write_str("authorization task failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::CorruptCache { source, .. } => source
                .as_ref()
                .map(|e| e as &(dyn std::error::Error + 'static)),
            Self::Unavailable { source } => Some(source.as_ref()),
            Self::Task(source) => Some(source),
            Self::Document { .. } => None,
        }
    }
}

/// An authorization result.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Where the documents come from and how often; tests point it at a local server.
#[derive(Debug, Clone)]
struct Source {
    cache: PathBuf,
    discovery: String,
    https_only: bool,
    timeout: Duration,
    refresh: Duration,
    kid_cooldown: Duration,
}

impl Source {
    fn client(&self) -> Client {
        Client::new(self.https_only, self.timeout)
    }
}

/// A handle to the authorization keys. Cloning is cheap; the background task ends when the last
/// handle is dropped.
#[derive(Debug, Clone)]
pub(crate) struct Auth {
    keys: watch::Receiver<Arc<Keys>>,
    refresh: mpsc::Sender<()>,
}

impl Auth {
    /// Loads the keys as the start rule says and spawns the refresh task on the current tokio
    /// runtime. `clock` returns wall-clock Unix seconds.
    pub(crate) async fn start(data_dir: &Path, clock: fn() -> i64) -> Result<Self> {
        let source = Source {
            cache: data_dir.join(CACHE_FILE),
            discovery: DISCOVERY_BASE.to_owned(),
            https_only: true,
            timeout: HTTP_TIMEOUT,
            refresh: REFRESH,
            kid_cooldown: KID_COOLDOWN,
        };
        Self::start_from(source, clock).await
    }

    async fn start_from(source: Source, clock: fn() -> i64) -> Result<Self> {
        let now = clock();
        let cached = load_cache(&source.cache).await?;
        let (cache, keys) = match cached {
            Some((cache, keys)) if is_fresh(cache.fetched_at, now) => (cache, keys),
            cached => match fetch_all(&source, now).await {
                Ok((cache, keys)) => {
                    store_cache(&source.cache, &cache).await;
                    (cache, keys)
                }
                Err(e) => {
                    let Some((cache, keys)) = cached else {
                        return Err(Error::Unavailable {
                            source: Box::new(e),
                        });
                    };
                    tracing::warn!(error = %e, "authorization fetch failed; using the cache");
                    (cache, keys)
                }
            },
        };
        tracing::info!(
            keys = keys.by_kid.len(),
            fetched_at = cache.fetched_at,
            "authorization keys loaded"
        );
        let age = now.saturating_sub(cache.fetched_at).clamp(0, FRESH_SECONDS);
        let first_refresh = source
            .refresh
            .saturating_sub(Duration::from_secs(age.unsigned_abs()));
        let (keys_tx, keys_rx) = watch::channel(Arc::new(keys));
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let task = Refresher {
            source,
            clock,
            cache,
            keys: keys_tx,
            requests: refresh_rx,
        };
        tokio::spawn(task.run(first_refresh));
        Ok(Self {
            keys: keys_rx,
            refresh: refresh_tx,
        })
    }

    /// The newest keys.
    pub(crate) fn keys(&self) -> Arc<Keys> {
        Arc::clone(&self.keys.borrow())
    }

    /// Asks for a JWKS fetch after a token named an unknown `kid`, and waits for the answer: new
    /// keys, or the current ones when the fetch failed or ran less than five minutes ago. `None`
    /// when the refresh task is gone. Callers bound the wait with their login deadline.
    pub(crate) async fn refresh_keys(&self) -> Option<Arc<Keys>> {
        let mut keys = self.keys.clone();
        keys.mark_unchanged();
        if let Err(TrySendError::Closed(())) = self.refresh.try_send(()) {
            return None;
        }
        // A full queue means a request is pending, and its answer wakes this waiter too.
        keys.changed().await.ok()?;
        Some(Arc::clone(&keys.borrow_and_update()))
    }
}

fn is_fresh(fetched_at: i64, now: i64) -> bool {
    (0..FRESH_SECONDS).contains(&now.saturating_sub(fetched_at))
}

/// The background task: hourly refreshes and rate-limited JWKS fetches for unknown key IDs.
struct Refresher {
    source: Source,
    clock: fn() -> i64,
    cache: Cache,
    keys: watch::Sender<Arc<Keys>>,
    requests: mpsc::Receiver<()>,
}

impl Refresher {
    async fn run(mut self, first_refresh: Duration) {
        let timer = sleep(first_refresh);
        let cooldown = sleep(Duration::ZERO);
        tokio::pin!(timer, cooldown);
        let mut cooling = false;
        loop {
            tokio::select! {
                () = &mut timer => {
                    self.refresh_all().await;
                    timer.as_mut().set(sleep(self.source.refresh));
                }
                () = &mut cooldown, if cooling => cooling = false,
                request = self.requests.recv() => {
                    if request.is_none() {
                        break;
                    }
                    if cooling {
                        tracing::debug!("unknown key id; jwks fetched recently");
                        self.keys.send_modify(|_| {});
                    } else {
                        cooling = true;
                        cooldown.as_mut().set(sleep(self.source.kid_cooldown));
                        self.refresh_jwks().await;
                    }
                }
            }
        }
    }

    async fn refresh_all(&mut self) {
        match fetch_all(&self.source, (self.clock)()).await {
            Ok((cache, keys)) => {
                store_cache(&self.source.cache, &cache).await;
                self.cache = cache;
                tracing::debug!(keys = keys.by_kid.len(), "authorization keys refreshed");
                self.keys.send_replace(Arc::new(keys));
            }
            Err(e) => tracing::warn!(error = %e, "authorization refresh failed; keeping the keys"),
        }
    }

    async fn refresh_jwks(&mut self) {
        let client = self.source.client();
        let openid = self.cache.openid.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let jwks = client.jwks(&openid.jwks_uri)?;
            let keys = document::keys(&openid, &jwks).map_err(|reason| Error::Document {
                url: openid.jwks_uri.clone(),
                reason,
            })?;
            Ok((jwks, keys))
        })
        .await
        .map_err(Error::Task)
        .and_then(|r| r);
        match fetched {
            Ok((jwks, keys)) => {
                self.cache.jwks = jwks;
                store_cache(&self.source.cache, &self.cache).await;
                tracing::debug!(
                    keys = keys.by_kid.len(),
                    "jwks fetched for an unknown key id"
                );
                self.keys.send_replace(Arc::new(keys));
            }
            Err(e) => {
                tracing::warn!(error = %e, "jwks fetch failed; keeping the keys");
                self.keys.send_modify(|_| {});
            }
        }
    }
}

/// Fetches discovery, OpenID configuration and JWKS on the blocking pool.
async fn fetch_all(source: &Source, now: i64) -> Result<(Cache, Keys)> {
    let client = source.client();
    let discovery = source.discovery.clone();
    tokio::task::spawn_blocking(move || {
        let (auth, openid, jwks) = client.documents(&discovery)?;
        let keys = document::keys(&openid, &jwks).map_err(|reason| Error::Document {
            url: openid.jwks_uri.clone(),
            reason,
        })?;
        let cache = Cache {
            auth,
            openid,
            jwks,
            fetched_at: now,
        };
        Ok((cache, keys))
    })
    .await
    .map_err(Error::Task)?
}

/// Reads and checks the cache. `None` when there is no file.
async fn load_cache(path: &Path) -> Result<Option<(Cache, Keys)>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let Some(bytes) = read_file(&path)? else {
            return Ok(None);
        };
        let corrupt = |reason, source| Error::CorruptCache {
            path: path.clone(),
            reason,
            source,
        };
        let cache: Cache = serde_json::from_slice(&bytes)
            .map_err(|e| corrupt("not the expected json", Some(e)))?;
        let keys =
            document::keys(&cache.openid, &cache.jwks).map_err(|reason| corrupt(reason, None))?;
        Ok(Some((cache, keys)))
    })
    .await
    .map_err(Error::Task)?
}

/// Writes the cache atomically. A failure is logged: the keys in memory stay valid.
async fn store_cache(path: &Path, cache: &Cache) {
    let path = path.to_owned();
    let result = match serde_json::to_vec_pretty(cache) {
        Ok(bytes) => tokio::task::spawn_blocking(move || {
            let written = fs::write_atomic(&path, &bytes, Access::Shared);
            written.map_err(|source| Error::Io { path, source })
        })
        .await
        .map_err(Error::Task)
        .and_then(|r| r),
        Err(e) => Err(Error::Json {
            url: CACHE_FILE.to_owned(),
            source: e,
        }),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "cannot write the authorization cache");
    }
}

/// Reads at most [`MAX_DOCUMENT`] bytes of `path`; `None` when it does not exist.
fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read_bounded(path, MAX_DOCUMENT) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Err(Error::CorruptCache {
            path: path.to_owned(),
            reason: "larger than 1 MiB",
            source: None,
        }),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::net::{TcpListener, TcpStream};

    use super::testing::{ISSUER, Issuer, KID};
    use super::*;

    const NOW: i64 = 1_790_000_000;
    const DISCOVERY_PATH: &str = "/api/v1.0/discovery/MinecraftPE/builds/1.26.50";

    fn now() -> i64 {
        NOW
    }

    /// Runs a test on its own current-thread runtime.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: each test owns a small runtime"
    )]
    fn run<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Like `run`, with time paused: it jumps ahead only while every task waits and no blocking
    /// work (fetch, cache write) is in flight, so a slow machine cannot run a timeout out.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: each test owns a small runtime"
    )]
    fn run_paused<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(future)
    }

    #[derive(Clone)]
    enum Reply {
        Json(String),
        Status(u16),
        Bytes(usize),
        Hang,
        /// JSON after [`SLOW`].
        Slow(String),
    }

    /// How long a [`Reply::Slow`] waits before it answers.
    const SLOW: Duration = Duration::from_millis(800);

    /// A local HTTP server with canned replies per path; every request path is reported.
    struct Server {
        base: String,
        routes: watch::Sender<BTreeMap<String, Reply>>,
        hits: mpsc::Receiver<String>,
    }

    impl Server {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (routes, routes_rx) = watch::channel(BTreeMap::new());
            let (hits_tx, hits) = mpsc::channel(1024);
            tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(serve(stream, routes_rx.clone(), hits_tx.clone()));
                }
            });
            Self { base, routes, hits }
        }

        fn route(&self, path: &str, reply: Reply) {
            self.routes.send_modify(|r| {
                r.insert(path.to_owned(), reply);
            });
        }

        /// Serves the test issuer's discovery, OpenID configuration and JWKS (`kid`).
        fn serve_issuer(&self, kid: &str) {
            let issuer = Issuer::get();
            let discovery = json!({"result": {"serviceEnvironments": {
                "auth": {"prod": {"serviceUri": self.base, "issuer": self.base, "playfabTitleId": "20CA2"}},
                "signaling": {"prod": {"serviceUri": "wss://x"}},
            }}});
            self.route(DISCOVERY_PATH, Reply::Json(discovery.to_string()));
            let openid = Issuer::openid(&format!("{}/keys", self.base));
            self.route(
                "/.well-known/openid-configuration",
                Reply::Json(openid.to_string()),
            );
            self.route("/keys", Reply::Json(issuer.jwks(kid).to_string()));
        }

        /// Makes every JSON route answer [`SLOW`] late.
        fn slow_down(&self) {
            self.routes.send_modify(|r| {
                for reply in r.values_mut() {
                    if let Reply::Json(body) = reply {
                        *reply = Reply::Slow(std::mem::take(body));
                    }
                }
            });
        }

        fn take_hits(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(path) = self.hits.try_recv() {
                out.push(path);
            }
            out
        }
    }

    async fn read_some(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            stream.readable().await?;
            match stream.try_read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
        }
    }

    async fn write_all(stream: &TcpStream, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            stream.writable().await?;
            match stream.try_write(data) {
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn serve(
        stream: TcpStream,
        routes: watch::Receiver<BTreeMap<String, Reply>>,
        hits: mpsc::Sender<String>,
    ) {
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match read_some(&stream, &mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
        let text = String::from_utf8_lossy(&head);
        let path = text.split_whitespace().nth(1).unwrap().to_owned();
        hits.try_send(path.clone()).unwrap();
        let reply = routes.borrow().get(&path).cloned();
        let (status, body) = match reply.unwrap_or(Reply::Status(404)) {
            Reply::Json(body) => (200, body.into_bytes()),
            Reply::Status(status) => (status, Vec::new()),
            Reply::Bytes(len) => (200, vec![b' '; len]),
            Reply::Hang => {
                sleep(Duration::from_secs(3600)).await;
                return;
            }
            Reply::Slow(body) => {
                sleep(SLOW).await;
                (200, body.into_bytes())
            }
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        if write_all(&stream, head.as_bytes()).await.is_ok() {
            write_all(&stream, &body).await.unwrap_or(());
        }
    }

    /// A per-test directory under the system temp directory, removed on drop.
    struct TempDir(PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-auth-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).unwrap_or(());
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn cache(&self) -> PathBuf {
            self.0.join(CACHE_FILE)
        }

        fn write(&self, bytes: &[u8]) {
            std::fs::write(self.cache(), bytes).unwrap();
        }

        fn read(&self) -> Vec<u8> {
            std::fs::read(self.cache()).unwrap()
        }
    }

    impl Drop for TempDir {
        #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap_or(());
        }
    }

    fn source(dir: &TempDir, base: &str) -> Source {
        Source {
            cache: dir.cache(),
            discovery: base.to_owned(),
            https_only: false,
            // Generous: a loaded test machine can stall loopback for a moment; the timeout tests
            // build their own short clients.
            timeout: Duration::from_secs(5),
            refresh: Duration::from_secs(3600),
            kid_cooldown: Duration::from_secs(3600),
        }
    }

    fn cache_json(fetched_at: i64, kid: &str) -> Vec<u8> {
        let issuer = Issuer::get();
        let cache = json!({
            "auth": {"serviceUri": "https://auth.test", "issuer": "https://auth.test"},
            "openid": Issuer::openid("https://issuer.test/keys"),
            "jwks": issuer.jwks(kid),
            "fetched_at": fetched_at,
        });
        serde_json::to_vec(&cache).unwrap()
    }

    #[test]
    fn a_fresh_cache_is_used_without_fetching() {
        run(async {
            let dir = TempDir::new("fresh");
            let mut server = Server::start().await;
            dir.write(&cache_json(NOW - 3599, KID));
            let auth = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap();
            assert_eq!(auth.keys().issuer(), ISSUER);
            assert!(auth.keys().key(KID).is_some());
            assert!(auth.keys().key("ec-key").is_none());
            assert!(server.take_hits().is_empty());
        });
    }

    #[test]
    fn the_production_entry_reads_the_data_directory_cache() {
        run(async {
            let dir = TempDir::new("production");
            dir.write(&cache_json(NOW - 10, KID));
            let auth = Auth::start(&dir.0, now).await.unwrap();
            assert!(auth.keys().key(KID).is_some());
        });
    }

    #[test]
    fn a_stale_cache_is_fetched_again_and_rewritten() {
        run(async {
            let dir = TempDir::new("stale");
            let mut server = Server::start().await;
            server.serve_issuer("fresh-kid");
            dir.write(&cache_json(NOW - 3600, KID));
            let auth = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap();
            assert!(auth.keys().key("fresh-kid").is_some());
            assert!(auth.keys().key(KID).is_none());
            let hits = server.take_hits();
            assert_eq!(
                hits,
                [DISCOVERY_PATH, "/.well-known/openid-configuration", "/keys"]
            );
            let written: Cache = serde_json::from_slice(&dir.read()).unwrap();
            assert_eq!(written.fetched_at, NOW);
            assert_eq!(written.auth.issuer, server.base);
            assert_eq!(written.openid.issuer, ISSUER);
            // A cache from the future (the clock moved back) is not fresh either.
            dir.write(&cache_json(NOW + 10, KID));
            let auth = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap();
            assert!(auth.keys().key("fresh-kid").is_some());
        });
    }

    #[test]
    fn a_failed_fetch_falls_back_to_any_cache() {
        run(async {
            let dir = TempDir::new("fallback");
            let server = Server::start().await;
            server.route(DISCOVERY_PATH, Reply::Status(500));
            dir.write(&cache_json(NOW - 100_000, KID));
            let before = dir.read();
            let auth = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap();
            assert!(auth.keys().key(KID).is_some());
            assert_eq!(dir.read(), before);
        });
    }

    #[test]
    fn no_cache_and_no_fetch_refuses_to_start() {
        run(async {
            let dir = TempDir::new("none");
            let server = Server::start().await;
            let err = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Unavailable { .. }), "{err}");
        });
    }

    #[test]
    fn a_corrupt_cache_refuses_to_start_and_stays_untouched() {
        run(async {
            let dir = TempDir::new("corrupt");
            let server = Server::start().await;
            server.serve_issuer(KID);
            let mut bad_key: serde_json::Value =
                serde_json::from_slice(&cache_json(NOW, KID)).unwrap();
            bad_key["jwks"]["keys"][1]["n"] = json!("AQAB");
            let cases = [
                b"not json".to_vec(),
                b"{}".to_vec(),
                br#"{"auth":{},"openid":{},"jwks":{"keys":[]},"fetched_at":1}"#.to_vec(),
                serde_json::to_vec(&bad_key).unwrap(),
                vec![b' '; 2 * 1024 * 1024],
            ];
            for bytes in cases {
                dir.write(&bytes);
                let err = Auth::start_from(source(&dir, &server.base), now)
                    .await
                    .unwrap_err();
                assert!(matches!(err, Error::CorruptCache { .. }), "{err}");
                assert!(err.to_string().contains(CACHE_FILE), "{err}");
                assert_eq!(dir.read(), bytes);
            }
        });
    }

    #[test]
    fn the_cache_round_trips() {
        run(async {
            let dir = TempDir::new("round-trip");
            let server = Server::start().await;
            server.serve_issuer(KID);
            let first = Auth::start_from(source(&dir, &server.base), now)
                .await
                .unwrap();
            server.route(DISCOVERY_PATH, Reply::Status(503));
            let second = Auth::start_from(source(&dir, &server.base), || NOW + 60)
                .await
                .unwrap();
            assert_eq!(first.keys().issuer(), second.keys().issuer());
            assert!(second.keys().key(KID).is_some());
            let (cache, keys) = load_cache(&dir.cache()).await.unwrap().unwrap();
            assert_eq!(cache.fetched_at, NOW);
            assert_eq!(keys.by_kid.len(), 1);
        });
    }

    #[test]
    fn oversized_slow_and_malformed_responses_fail() {
        run(async {
            let server = Server::start().await;
            server.route("/big", Reply::Bytes(2 * 1024 * 1024));
            server.route("/limit", Reply::Bytes(1024 * 1024));
            server.route("/hang", Reply::Hang);
            let missing = r#"{"keys":[{"kid":"a"}]}"#.to_owned();
            server.route("/missing-field", Reply::Json(missing));
            let too_many = json!({"keys": vec![json!({"kty": "EC"}); 65]}).to_string();
            server.route("/too-many", Reply::Json(too_many));
            let base = server.base.clone();
            let results = tokio::task::spawn_blocking(move || {
                let client = Client::new(false, Duration::from_millis(300));
                [
                    "/big",
                    "/limit",
                    "/hang",
                    "/missing-field",
                    "/too-many",
                    "/absent",
                ]
                .map(|path| client.jwks(&format!("{base}{path}")).map(|_| ()))
            })
            .await
            .unwrap();
            for result in &results[..3] {
                assert!(matches!(result, Err(Error::Http { .. })), "{result:?}");
            }
            for result in &results[3..5] {
                assert!(matches!(result, Err(Error::Json { .. })), "{result:?}");
            }
            assert!(matches!(results[5], Err(Error::Http { .. })));
            // Production clients refuse plain HTTP.
            let base = server.base.clone();
            let https = tokio::task::spawn_blocking(move || {
                Client::new(true, Duration::from_millis(300)).jwks(&format!("{base}/keys"))
            })
            .await
            .unwrap();
            assert!(matches!(https, Err(Error::Http { .. })));
        });
    }

    /// Starts with the test issuer's keys under `kid-1` and `cooldown` between JWKS fetches for
    /// unknown key IDs, with no hits counted yet.
    async fn kid_auth(name: &str, cooldown: Duration) -> (TempDir, Server, Auth) {
        let dir = TempDir::new(name);
        let mut server = Server::start().await;
        server.serve_issuer("kid-1");
        let mut source = source(&dir, &server.base);
        source.kid_cooldown = cooldown;
        let auth = Auth::start_from(source, now).await.unwrap();
        server.take_hits();
        (dir, server, auth)
    }

    #[test]
    fn unknown_kid_refreshes_wait_out_the_cooldown() {
        run(async {
            let (_dir, mut server, auth) = kid_auth("kid-cooling", Duration::from_secs(3600)).await;
            server.route(
                "/keys",
                Reply::Json(Issuer::get().jwks("kid-2").to_string()),
            );
            let keys = auth.refresh_keys().await.unwrap();
            assert!(keys.key("kid-2").is_some());
            assert_eq!(server.take_hits(), ["/keys"]);
            assert!(auth.keys().key("kid-2").is_some());

            server.route(
                "/keys",
                Reply::Json(Issuer::get().jwks("kid-3").to_string()),
            );
            let keys = auth.refresh_keys().await.unwrap();
            assert!(keys.key("kid-3").is_none());
            assert!(server.take_hits().is_empty());
        });
    }

    #[test]
    fn unknown_kid_refreshes_fetch_again_after_the_cooldown() {
        run(async {
            let (dir, mut server, auth) = kid_auth("kid-cooled", Duration::ZERO).await;
            for kid in ["kid-2", "kid-3"] {
                server.route("/keys", Reply::Json(Issuer::get().jwks(kid).to_string()));
                let keys = auth.refresh_keys().await.unwrap();
                assert!(keys.key(kid).is_some(), "{kid}");
                assert_eq!(server.take_hits(), ["/keys"], "{kid}");
            }
            let written: Cache = serde_json::from_slice(&dir.read()).unwrap();
            assert_eq!(written.jwks.keys[1].kid, "kid-3");

            // A failed fetch still answers, with the keys unchanged.
            server.route("/keys", Reply::Status(500));
            let keys = auth.refresh_keys().await.unwrap();
            assert!(keys.key("kid-3").is_some());
            assert_eq!(server.take_hits(), ["/keys"]);
        });
    }

    #[test]
    fn a_refresh_in_progress_ends_within_its_timeout() {
        run(async {
            let dir = TempDir::new("deadline");
            let server = Server::start().await;
            server.serve_issuer(KID);
            server.slow_down();
            let mut source = source(&dir, &server.base);
            source.timeout = Duration::from_secs(1);
            // Each document alone comes within the timeout; the three together do not.
            let err = fetch_all(&source, NOW)
                .await
                .expect_err("the refresh outlived its timeout");
            assert!(matches!(err, Error::Http { .. }), "{err}");
        });
    }

    #[test]
    fn keys_refresh_on_schedule_and_the_task_ends_with_the_handles() {
        run_paused(async {
            let dir = TempDir::new("schedule");
            let mut server = Server::start().await;
            server.serve_issuer("kid-1");
            let mut source = source(&dir, &server.base);
            source.refresh = Duration::from_millis(300);
            let auth = Auth::start_from(source, now).await.unwrap();
            server.take_hits();
            server.route(
                "/keys",
                Reply::Json(Issuer::get().jwks("kid-2").to_string()),
            );
            let mut watcher = auth.keys.clone();
            tokio::time::timeout(Duration::from_secs(5), watcher.changed())
                .await
                .unwrap()
                .unwrap();
            assert!(auth.keys().key("kid-2").is_some());
            // The next refresh waits for time to move, and time stands still while this task runs.
            assert_eq!(
                server.take_hits(),
                [DISCOVERY_PATH, "/.well-known/openid-configuration", "/keys"]
            );
            let clone = auth.clone();
            drop(auth);
            drop(clone);
            let closed = async { while watcher.changed().await.is_ok() {} };
            tokio::time::timeout(Duration::from_secs(5), closed)
                .await
                .unwrap();
        });
    }
}
