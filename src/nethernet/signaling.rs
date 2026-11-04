//! NetherNet signaling: a minimal HTTP/1.1 server over TCP, with optional TLS.
//!
//! It answers `GET /v1/join` with the server-list status and hands `POST /v1/join/{networkId}`
//! offers to the offer consumer, within the connection, size and time limits below. It never
//! reads the SDP.
//! Responses are the official server's: each one closes the connection, and a failed negotiation
//! is still `200` with the error code as an `application/sdp` body.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::TryCurrentError;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, trace, warn};

use crate::raknet::ip_key;

/// The pause after an accept error such as running out of file descriptors, which would
/// otherwise repeat at once and spin the task.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// The connections the kernel queues before they are accepted.
const BACKLOG: i32 = 128;
/// What the official server sends to a TLS client hello on plain HTTP: an alert record (TLS 1.2,
/// length 2) holding a fatal `handshake_failure`.
const HANDSHAKE_FAILURE: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28];
/// The largest request body after chunked decoding, in bytes: the SDP offer limit.
const MAX_BODY_BYTES: usize = super::sdp::MAX_OFFER_BYTES;
/// The most open signaling connections at once.
const MAX_CONNECTIONS: usize = 64;
/// The most open signaling connections from one IPv4 address or one IPv6 /64. A /64 is the
/// smallest block one IPv6 host is commonly given, so it counts as one address.
const MAX_CONNECTIONS_PER_IP: u8 = 4;
/// The most chunked framing (size lines, extensions, trailers) in a request, in bytes, so tiny
/// chunks cannot stretch a body read far past [`MAX_BODY_BYTES`]. A client writing 4 KiB chunks
/// spends about 8 bytes per chunk.
const MAX_FRAMING_BYTES: usize = 8 * 1024;
/// The largest request line and headers, the closing empty line included, in bytes.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// The longest DNS name a `Host` header may give, in bytes, without a final dot (RFC 1035).
const MAX_HOST_NAME_BYTES: usize = 253;
/// The longest label of a DNS name, in bytes (RFC 1035).
const MAX_LABEL_BYTES: usize = 63;
/// The digits of a decimal u64: the longest valid network id.
const MAX_NETWORK_ID_DIGITS: usize = 20;
/// The largest certificate chain or key file, in bytes.
const MAX_PEM_BYTES: u64 = 1024 * 1024;
/// The bytes read from a connection at once.
const READ_CHUNK_BYTES: usize = 4096;
/// One connection from accept to close: TLS handshake, reading, waiting for the answer and
/// writing. When it passes, the connection is dropped without a response.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// The first byte of a TLS handshake record: a client trying HTTPS first.
const TLS_HANDSHAKE: u8 = 0x16;

/// A signaling failure: a listener that cannot be opened, or a TLS certificate or key that
/// cannot be loaded.
#[derive(Debug)]
pub(crate) enum Error {
    /// A listener cannot be opened.
    Bind {
        /// The address.
        addr: SocketAddr,
        /// The cause.
        source: io::Error,
    },
    /// The server is spawned outside a tokio runtime.
    Runtime(TryCurrentError),
    /// The settings give no address to listen on.
    NoAddress,
    /// A file cannot be read.
    Read {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// A file is larger than [`MAX_PEM_BYTES`].
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// A file is not PEM or holds no usable item of the expected kind.
    Pem {
        /// The file.
        path: PathBuf,
        /// The expected kind of item.
        item: &'static str,
        /// The cause.
        source: pem::Error,
    },
    /// rustls refused the certificate chain or the key.
    Tls(rustls::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind { addr, .. } => write!(f, "cannot listen on {addr}"),
            Self::Runtime(_) => f.write_str("signaling needs a tokio runtime"),
            Self::NoAddress => f.write_str("signaling needs an address"),
            Self::Read { path, .. } => write!(f, "cannot read {}", path.display()),
            Self::TooLarge { path } => {
                write!(f, "{} is larger than {MAX_PEM_BYTES} bytes", path.display())
            }
            Self::Pem { path, item, .. } => {
                write!(f, "{} holds no usable pem {item}", path.display())
            }
            Self::Tls(_) => f.write_str("invalid tls certificate or key"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind { source, .. } | Self::Read { source, .. } => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Pem { source, .. } => Some(source),
            Self::Tls(source) => Some(source),
            Self::NoAddress | Self::TooLarge { .. } => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// Socket settings for `spawn`.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// IPv4 address to bind, if any.
    pub(crate) v4: Option<SocketAddrV4>,
    /// IPv6 address to bind (IPv6 only), if any.
    pub(crate) v6: Option<SocketAddrV6>,
    /// `Some`: every connection is HTTPS. `None`: plain HTTP.
    pub(crate) tls: Option<Arc<rustls::ServerConfig>>,
}

/// An SDP offer from `POST /v1/join/{networkId}`, waiting for its reply.
pub(crate) struct Offer {
    /// The client's network id from the path.
    pub(crate) network_id: u64,
    /// The offer: non-empty UTF-8 of at most 64 KiB, otherwise unchecked.
    pub(crate) sdp: String,
    /// The client's TCP address (IPv4-mapped IPv6 as IPv4).
    pub(crate) peer: SocketAddr,
    /// The server's TCP address the client reached (IPv4-mapped IPv6 as IPv4).
    pub(crate) local: SocketAddr,
    /// The host of the request's `Host` header: the address the client was given. `None`
    /// without one, with more than one, or with one that is not an IP literal or a DNS name.
    pub(crate) host: Option<Host>,
    /// Where the answer goes. It is dropped when the answer is written or the request deadline
    /// passes.
    pub(crate) reply: oneshot::Sender<Reply>,
}

impl fmt::Debug for Offer {
    // The SDP carries the client's identity token, which must stay out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Offer")
            .field("network_id", &self.network_id)
            .field("sdp_len", &self.sdp.len())
            .field("peer", &self.peer)
            .field("local", &self.local)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// The host part of a `Host` header (RFC 9110 7.2), its port left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    /// An IP literal.
    Ip(IpAddr),
    /// A DNS name: dot-separated labels of letters, digits and inner hyphens, at most
    /// [`MAX_HOST_NAME_BYTES`] bytes.
    Name(String),
}

/// The error code of a failed negotiation (go-nethernet `ErrorCode…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    /// `FailedToCreateAnswer`: no answer can be made.
    Internal,
    /// `FailedToSetRemoteDescription`: the offer cannot be used. An empty or non-UTF-8 body
    /// here, a malformed SDP in the offer task.
    BadOffer,
    /// `IdentityVerificationFailed`: the offer identity is missing where required or does not
    /// verify.
    Identity,
}

impl Code {
    /// The number the response body carries.
    fn number(self) -> u32 {
        match self {
            Self::Internal => 11,
            Self::BadOffer => 13,
            Self::Identity => 37,
        }
    }
}

/// The answer to an [`Offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// `200`, `application/sdp`, the answer SDP.
    Answer(String),
    /// `200`, `application/sdp`, the negotiation error code in decimal.
    Code(Code),
    /// `503 Service Unavailable`: too many peers before login.
    Busy,
}

/// Controls the signaling task.
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

    /// Stops accepting, aborts every open connection and waits for the tasks to end. Dropping
    /// the handle does the same without waiting.
    pub(crate) async fn shutdown(self) {
        if self.stop.send(()).is_err() {
            debug!("signaling task already stopped");
        }
        if let Err(error) = self.task.await {
            error!(
                error = &error as &(dyn std::error::Error + 'static),
                "signaling task failed"
            );
        }
    }
}

/// What every connection task reads.
#[derive(Debug)]
struct Shared {
    tls: Option<Arc<rustls::ServerConfig>>,
    status: watch::Receiver<Arc<[u8]>>,
    offers: mpsc::Sender<Offer>,
    /// The `503` response, written to connections over the limit without a task.
    unavailable: Vec<u8>,
}

/// The connection budget key: an IPv4 address or an IPv6 /64, as RakNet counts them.
type Group = u128;

/// Open connections, in total and per [`Group`].
#[derive(Debug, Default)]
struct Limits {
    total: usize,
    groups: BTreeMap<Group, u8>,
}

impl Limits {
    /// Takes a connection that `try_add` counted off the counts.
    #[expect(
        clippy::expect_used,
        reason = "invariant: `serve` removes only a group `try_add` counted"
    )]
    fn remove(&mut self, group: Group) {
        let count = self
            .groups
            .get_mut(&group)
            .expect("a removed connection's group is counted");
        *count -= 1;
        if *count == 0 {
            self.groups.remove(&group);
        }
        self.total -= 1;
    }

    /// Counts a new connection, or returns `false` if it would pass a limit.
    fn try_add(&mut self, group: Group) -> bool {
        if self.total >= MAX_CONNECTIONS {
            return false;
        }
        let count = self.groups.entry(group).or_default();
        if *count >= MAX_CONNECTIONS_PER_IP {
            return false;
        }
        *count += 1;
        self.total += 1;
        true
    }
}

/// A response status. Except `200`, the reason phrase is also the `text/plain` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    BadRequest,
    /// The official server puts this message in the status line.
    BadNetworkId,
    NotFound,
    PayloadTooLarge,
    HeaderFieldsTooLarge,
    Unavailable,
}

impl Status {
    fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::BadRequest | Self::BadNetworkId => 400,
            Self::NotFound => 404,
            Self::PayloadTooLarge => 413,
            Self::HeaderFieldsTooLarge => 431,
            Self::Unavailable => 503,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::BadRequest => "Bad Request",
            Self::BadNetworkId => "Bad Request: the path needs a network ID",
            Self::NotFound => "Not Found",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::HeaderFieldsTooLarge => "Request Header Fields Too Large",
            Self::Unavailable => "Service Unavailable",
        }
    }
}

/// A response body type.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Json,
    Sdp,
    Text,
}

impl Kind {
    fn mime(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Sdp => "application/sdp",
            Self::Text => "text/plain",
        }
    }
}

/// What a request asks for.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Status,
    Offer(u64),
    Reject(Status),
}

/// A request method; only GET and POST are routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Other,
}

/// A whole request.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: Method,
    /// The request target up to `?`: visible ASCII starting with `/`.
    path: String,
    /// The `Host` header's host, as [`Offer::host`] holds it.
    host: Option<Host>,
    /// At most [`MAX_BODY_BYTES`] bytes.
    body: Vec<u8>,
}

/// Where the parser is.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Reading the head; its first `scanned` bytes hold no CRLF CRLF.
    Head {
        scanned: usize,
    },
    /// `remaining` bytes of a `Content-Length` body follow.
    Body {
        remaining: usize,
    },
    Chunked(Chunk),
    /// The request was returned; later bytes are ignored.
    Done,
}

/// Where the chunked decoder is.
#[derive(Debug, Clone, Copy)]
enum Chunk {
    /// Reading the hex size; `digits` says whether one was seen.
    Size {
        size: usize,
        digits: bool,
    },
    /// Skipping a chunk extension.
    Extension {
        size: usize,
    },
    /// After the CR ending the size line.
    SizeLf {
        size: usize,
    },
    Data {
        remaining: usize,
    },
    DataCr,
    DataLf,
    /// At the start of a trailer line or the final empty line.
    TrailerStart,
    Trailer,
    TrailerLf,
    /// After the CR of the final empty line.
    EndLf,
}

/// An incremental HTTP/1.x request parser over bytes: no sockets, no clocks. Every byte is
/// looked at once, and every buffer is bounded before it grows.
#[derive(Debug)]
struct Parser {
    head: Vec<u8>,
    state: State,
    method: Method,
    path: String,
    host: Option<Host>,
    body: Vec<u8>,
    /// Chunked framing bytes so far, against [`MAX_FRAMING_BYTES`].
    framing: usize,
}

impl Parser {
    /// Feeds the next bytes. `Ok(Some)` once the request is whole, `Ok(None)` while more is
    /// needed, `Err` with the status to refuse with.
    fn push(&mut self, data: &[u8]) -> Result<Option<Request>, Status> {
        let data = match self.state {
            State::Head { scanned } => {
                let Some(rest) = self.push_head(scanned, data)? else {
                    return Ok(None);
                };
                rest
            }
            State::Body { .. } | State::Chunked(_) => data,
            State::Done => return Ok(None),
        };
        let done = match self.state {
            State::Head { .. } | State::Done => false,
            State::Body { remaining } => {
                let take = remaining.min(data.len());
                self.body
                    .extend_from_slice(data.get(..take).unwrap_or_default());
                let remaining = remaining.saturating_sub(take);
                self.state = State::Body { remaining };
                remaining == 0
            }
            State::Chunked(chunk) => match self.push_chunked(chunk, data)? {
                Some(chunk) => {
                    self.state = State::Chunked(chunk);
                    false
                }
                None => true,
            },
        };
        if !done {
            return Ok(None);
        }
        self.state = State::Done;
        Ok(Some(Request {
            method: self.method,
            path: std::mem::take(&mut self.path),
            host: self.host.take(),
            body: std::mem::take(&mut self.body),
        }))
    }

    /// Adds head bytes. Once the head is whole, parses it, sets the body state and returns
    /// the bytes after it.
    fn push_head<'a>(
        &mut self,
        scanned: usize,
        data: &'a [u8],
    ) -> Result<Option<&'a [u8]>, Status> {
        let before = self.head.len();
        let take = MAX_HEAD_BYTES.saturating_sub(before).min(data.len());
        self.head
            .extend_from_slice(data.get(..take).unwrap_or_default());
        // CRLF CRLF may straddle the last push.
        let from = scanned.saturating_sub(3);
        let found = self
            .head
            .get(from..)
            .and_then(|tail| tail.windows(4).position(|window| window == b"\r\n\r\n"));
        let Some(end) = found.and_then(|at| at.checked_add(from + 4)) else {
            if self.head.len() >= MAX_HEAD_BYTES {
                return Err(Status::HeaderFieldsTooLarge);
            }
            self.state = State::Head {
                scanned: self.head.len(),
            };
            return Ok(None);
        };
        // The terminator was not in the bytes scanned before, so it ends in the new ones.
        let used = end.checked_sub(before).ok_or(Status::BadRequest)?;
        let head = self.head.get(..end).ok_or(Status::BadRequest)?;
        let (method, path, host, framing) = parse_head(head)?;
        self.head = Vec::new();
        self.method = method;
        self.path = path;
        self.host = host;
        self.state = match framing {
            Framing::Length(length) => {
                self.body.reserve_exact(length);
                State::Body { remaining: length }
            }
            Framing::Chunked => State::Chunked(Chunk::Size {
                size: 0,
                digits: false,
            }),
        };
        Ok(Some(data.get(used..).unwrap_or_default()))
    }

    /// Decodes chunked bytes. Returns the next state, or `None` at the end of the body.
    fn push_chunked(&mut self, mut chunk: Chunk, mut data: &[u8]) -> Result<Option<Chunk>, Status> {
        loop {
            if let Chunk::Data { remaining } = chunk {
                if data.is_empty() {
                    return Ok(Some(chunk));
                }
                let take = remaining.min(data.len());
                let (bytes, rest) = data.split_at_checked(take).ok_or(Status::BadRequest)?;
                self.body.extend_from_slice(bytes);
                data = rest;
                chunk = match remaining.saturating_sub(take) {
                    0 => Chunk::DataCr,
                    remaining => Chunk::Data { remaining },
                };
                continue;
            }
            let Some((&byte, rest)) = data.split_first() else {
                return Ok(Some(chunk));
            };
            data = rest;
            self.framing = self.framing.saturating_add(1);
            if self.framing > MAX_FRAMING_BYTES {
                return Err(Status::PayloadTooLarge);
            }
            chunk = match (chunk, byte) {
                (Chunk::Size { size, .. }, byte) if byte.is_ascii_hexdigit() => {
                    let digit = char::from(byte).to_digit(16).ok_or(Status::BadRequest)?;
                    let room = MAX_BODY_BYTES.saturating_sub(self.body.len());
                    let size = size
                        .checked_mul(16)
                        .and_then(|size| size.checked_add(usize::try_from(digit).ok()?))
                        .filter(|&size| size <= room)
                        .ok_or(Status::PayloadTooLarge)?;
                    Chunk::Size { size, digits: true }
                }
                (Chunk::Size { size, digits: true }, b' ' | b'\t' | b';') => {
                    Chunk::Extension { size }
                }
                (Chunk::Size { size, digits: true } | Chunk::Extension { size }, b'\r') => {
                    Chunk::SizeLf { size }
                }
                (Chunk::Extension { size }, byte) if is_field_byte(byte) => {
                    Chunk::Extension { size }
                }
                (Chunk::SizeLf { size: 0 } | Chunk::TrailerLf, b'\n') => Chunk::TrailerStart,
                (Chunk::SizeLf { size }, b'\n') => {
                    // The body grows by doubling within its limit: reserving only this chunk
                    // would copy the body once per chunk. `size` is within `room` (checked
                    // when it was read), so the chunk always fits.
                    let spare = self.body.capacity().saturating_sub(self.body.len());
                    if spare < size {
                        let room = MAX_BODY_BYTES.saturating_sub(self.body.len());
                        self.body.reserve_exact(size.max(self.body.len()).min(room));
                    }
                    Chunk::Data { remaining: size }
                }
                (Chunk::DataCr, b'\r') => Chunk::DataLf,
                (Chunk::DataLf, b'\n') => Chunk::Size {
                    size: 0,
                    digits: false,
                },
                (Chunk::TrailerStart, b'\r') => Chunk::EndLf,
                (Chunk::EndLf, b'\n') => return Ok(None),
                (Chunk::TrailerStart | Chunk::Trailer, byte) if is_field_byte(byte) => {
                    Chunk::Trailer
                }
                (Chunk::Trailer, b'\r') => Chunk::TrailerLf,
                _ => return Err(Status::BadRequest),
            };
        }
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            head: Vec::new(),
            state: State::Head { scanned: 0 },
            method: Method::Other,
            path: String::new(),
            host: None,
            body: Vec::new(),
            framing: 0,
        }
    }
}

/// How a request body is framed.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// `Content-Length`, at most [`MAX_BODY_BYTES`]; also 0 when neither header is present.
    Length(usize),
    Chunked,
}

/// Reads a PEM certificate chain (leaf first) and a PEM private key into a TLS configuration
/// on the aws-lc-rs provider. Blocking: call it once at start on the blocking pool.
pub(crate) fn load_tls(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let pem_error = |path: &Path, item: &'static str| {
        let path = path.to_owned();
        move |source| Error::Pem { path, item, source }
    };
    let chain = CertificateDer::pem_slice_iter(&read_pem(cert)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(pem_error(cert, "certificate"))?;
    if chain.is_empty() {
        return Err(pem_error(cert, "certificate")(pem::Error::NoItemsFound));
    }
    let key = PrivateKeyDer::from_pem_slice(&read_pem(key)?).map_err(pem_error(
        key,
        "private key (pkcs#8, pkcs#1 or sec1, unencrypted)",
    ))?;
    // An explicit provider: rustls panics when none is given and no process default is set.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(Error::Tls)?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(Error::Tls)?;
    // One request per connection: resumption only costs tickets and a session cache.
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    Ok(Arc::new(config))
}

/// Reads at most [`MAX_PEM_BYTES`] of `path`.
fn read_pem(path: &Path) -> Result<Vec<u8>> {
    crate::fs::read_bounded(path, MAX_PEM_BYTES).map_err(|source| {
        if source.kind() == io::ErrorKind::FileTooLarge {
            Error::TooLarge {
                path: path.to_owned(),
            }
        } else {
            Error::Read {
                path: path.to_owned(),
                source,
            }
        }
    })
}

/// Binds the listeners on the current tokio runtime and serves them in tasks. `status` holds
/// the whole `GET /v1/join` body; `offers` receives each offer (when it is full, the request
/// gets `503`).
pub(crate) fn spawn(
    settings: Settings,
    status: watch::Receiver<Arc<[u8]>>,
    offers: mpsc::Sender<Offer>,
) -> Result<Handle> {
    let runtime = tokio::runtime::Handle::try_current().map_err(Error::Runtime)?;
    let _guard = runtime.enter();
    let v4 = settings.v4.map(|addr| bind(addr.into())).transpose()?;
    let v6 = settings.v6.map(|addr| bind(addr.into())).transpose()?;
    let local_addrs: Vec<SocketAddr> = v4
        .iter()
        .chain(&v6)
        .map(|(_, local_addr)| *local_addr)
        .collect();
    if local_addrs.is_empty() {
        return Err(Error::NoAddress);
    }
    let shared = Shared {
        tls: settings.tls,
        status,
        offers,
        unavailable: encode_status(Status::Unavailable),
    };
    let listeners = [v4, v6].map(|bound| bound.map(|(listener, _)| listener));
    let (stop, stopped) = oneshot::channel();
    let task = runtime.spawn(serve(listeners, Arc::new(shared), stopped));
    Ok(Handle {
        local_addrs,
        stop,
        task,
    })
}

/// Opens a listener on `addr`, with the address it is bound to.
fn bind(addr: SocketAddr) -> Result<(TcpListener, SocketAddr)> {
    open(addr)
        .and_then(|listener| {
            let local_addr = listener.local_addr()?;
            Ok((listener, local_addr))
        })
        .map_err(|source| Error::Bind { addr, source })
}

fn open(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        // Lets the IPv4 socket bind the same port on every OS.
        socket.set_only_v6(true)?;
    }
    // As std does on Unix: a restart may bind while closed connections sit in TIME_WAIT. On
    // Windows the option lets another socket take a bound port, so it stays off there.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    TcpListener::from_std(socket.into())
}

/// The accept task: owns the listeners, the limits and every connection task.
async fn serve(
    listeners: [Option<TcpListener>; 2],
    shared: Arc<Shared>,
    mut stopped: oneshot::Receiver<()>,
) {
    let [v4, v6] = listeners;
    let mut tasks = JoinSet::new();
    let mut groups = BTreeMap::<Id, Group>::new();
    let mut limits = Limits::default();
    loop {
        let accepted = tokio::select! {
            accepted = accept(v4.as_ref()) => accepted,
            accepted = accept(v6.as_ref()) => accepted,
            Some(joined) = tasks.join_next_with_id() => {
                let id = match joined {
                    Ok((id, ())) => id,
                    Err(error) => {
                        if error.is_panic() {
                            error!(
                                error = &error as &(dyn std::error::Error + 'static),
                                "signaling connection task panicked"
                            );
                        }
                        error.id()
                    }
                };
                if let Some(group) = groups.remove(&id) {
                    limits.remove(group);
                }
                continue;
            }
            // A stop request or a dropped handle.
            _ = &mut stopped => break,
        };
        match accepted {
            Ok((stream, peer)) => {
                let group = ip_key(peer.ip());
                if !limits.try_add(group) {
                    debug!(addr = %peer, "signaling connection over the limit");
                    // A TLS client cannot read a plain response, so it only sees the close.
                    if shared.tls.is_none() {
                        refuse(&stream, &shared.unavailable, peer);
                    }
                    continue;
                }
                let task = tasks.spawn(connection(stream, peer, Arc::clone(&shared)));
                groups.insert(task.id(), group);
            }
            Err(error) => match error.kind() {
                // The client gave up before the accept: nothing is wrong with the listener.
                io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::Interrupted => trace!(
                    error = &error as &(dyn std::error::Error + 'static),
                    "signaling accept skipped"
                ),
                _ => {
                    warn!(
                        error = &error as &(dyn std::error::Error + 'static),
                        "signaling accept failed; pausing"
                    );
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
        }
    }
    tasks.shutdown().await;
    debug!("signaling task stopped");
}

/// Accepts a connection on `listener`, or waits forever without one.
async fn accept(listener: Option<&TcpListener>) -> io::Result<(TcpStream, SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// Writes `response` to a connection over the limit without a task; the caller then drops it
/// unread. Best effort: if the client's request has already arrived, closing with unread bytes
/// sends a reset, which the client may see instead of the response.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the accepted socket is non-blocking, so the send returns at once"
)]
fn refuse(stream: &TcpStream, response: &[u8], peer: SocketAddr) {
    // `TcpStream::try_write` would refuse here: tokio has no write readiness for a socket it
    // has not polled yet. The empty send buffer takes the whole response.
    if let Err(error) = socket2::SockRef::from(stream).send(response) {
        trace!(
            addr = %peer,
            error = &error as &(dyn std::error::Error + 'static),
            "signaling 503 not sent"
        );
    }
}

/// One connection: one request, one response, then close.
async fn connection(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let peer = canonical(peer);
    debug!(addr = %peer, "signaling connection opened");
    match tokio::time::timeout(REQUEST_DEADLINE, exchange(stream, peer, &shared)).await {
        Ok(Ok(())) => debug!(addr = %peer, "signaling connection closed"),
        Ok(Err(error)) => debug!(
            addr = %peer,
            error = &error as &(dyn std::error::Error + 'static),
            "signaling connection failed"
        ),
        Err(_) => debug!(addr = %peer, "signaling request timed out"),
    }
}

/// Serves the request on `stream`, through TLS when configured.
async fn exchange(stream: TcpStream, peer: SocketAddr, shared: &Shared) -> io::Result<()> {
    let local = canonical(stream.local_addr()?);
    match &shared.tls {
        Some(config) => {
            let stream = TlsAcceptor::from(Arc::clone(config)).accept(stream).await?;
            respond(stream, peer, local, shared).await
        }
        None => respond(stream, peer, local, shared).await,
    }
}

/// Reads one request, writes its response and closes the stream.
async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    shared: &Shared,
) -> io::Result<()> {
    let mut parser = Parser::default();
    let mut buf = [0; READ_CHUNK_BYTES];
    let mut first = shared.tls.is_none();
    let parsed = loop {
        let read = stream.read(&mut buf).await?;
        let Some(data) = buf.get(..read).filter(|data| !data.is_empty()) else {
            trace!(addr = %peer, "signaling client closed before a whole request");
            return Ok(());
        };
        if first && data.first() == Some(&TLS_HANDSHAKE) {
            trace!(addr = %peer, "signaling refused a tls handshake on plain http");
            stream.write_all(&HANDSHAKE_FAILURE).await?;
            return stream.shutdown().await;
        }
        first = false;
        match parser.push(data) {
            Ok(None) => {}
            Ok(Some(request)) => break Ok(request),
            Err(status) => break Err(status),
        }
    };
    let response = match parsed {
        Ok(request) => answer(request, peer, local, shared).await,
        Err(status) => {
            trace!(addr = %peer, code = status.code(), "signaling request refused");
            encode_status(status)
        }
    };
    stream.write_all(&response).await?;
    stream.shutdown().await
}

/// Answers a parsed request: the status, or the offer's reply.
async fn answer(request: Request, peer: SocketAddr, local: SocketAddr, shared: &Shared) -> Vec<u8> {
    match route(request.method, &request.path) {
        Route::Status => {
            trace!(addr = %peer, "signaling status request");
            let status = Arc::clone(&shared.status.borrow());
            encode(Status::Ok, Kind::Json, &status)
        }
        Route::Offer(network_id) => {
            trace!(addr = %peer, network_id, "signaling offer");
            let Ok(sdp) = String::from_utf8(request.body) else {
                return encode_code(Code::BadOffer);
            };
            if sdp.is_empty() {
                return encode_code(Code::BadOffer);
            }
            let (reply, replied) = oneshot::channel();
            let offer = Offer {
                network_id,
                sdp,
                peer,
                local,
                host: request.host,
                reply,
            };
            match shared.offers.try_send(offer) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return encode_status(Status::Unavailable),
                Err(TrySendError::Closed(_)) => return encode_code(Code::Internal),
            }
            match replied.await {
                Ok(Reply::Answer(sdp)) => encode(Status::Ok, Kind::Sdp, sdp.as_bytes()),
                Ok(Reply::Code(code)) => encode_code(code),
                Ok(Reply::Busy) => encode_status(Status::Unavailable),
                Err(_) => encode_code(Code::Internal),
            }
        }
        Route::Reject(status) => {
            trace!(addr = %peer, code = status.code(), "signaling request refused");
            encode_status(status)
        }
    }
}

/// `addr` with an IPv4-mapped IPv6 address written as IPv4.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// The route of a request, like the official server: an unknown path, or a known path with
/// another method, is `404`.
fn route(method: Method, path: &str) -> Route {
    if method == Method::Get && path == "/v1/join" {
        return Route::Status;
    }
    if method == Method::Post
        && let Some(id) = path.strip_prefix("/v1/join/")
    {
        return network_id(id).map_or(Route::Reject(Status::BadNetworkId), Route::Offer);
    }
    Route::Reject(Status::NotFound)
}

/// A decimal u64 of 1 to 20 digits, nothing else (`parse` alone would take a `+` sign).
fn network_id(id: &str) -> Option<u64> {
    let digits = (1..=MAX_NETWORK_ID_DIGITS).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_digit());
    digits.then(|| id.parse().ok()).flatten()
}

/// A refusal with its reason phrase as the body.
fn encode_status(status: Status) -> Vec<u8> {
    encode(status, Kind::Text, status.reason().as_bytes())
}

/// A failed negotiation: `200` with the code as the SDP body.
fn encode_code(code: Code) -> Vec<u8> {
    encode(Status::Ok, Kind::Sdp, code.number().to_string().as_bytes())
}

/// A whole response, closing the connection.
fn encode(status: Status, kind: Kind, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status.code(),
        status.reason(),
        kind.mime(),
        body.len()
    );
    let mut response = Vec::with_capacity(head.len().saturating_add(body.len()));
    response.extend_from_slice(head.as_bytes());
    response.extend_from_slice(body);
    response
}

/// Parses a head ending in CRLF CRLF.
fn parse_head(head: &[u8]) -> Result<(Method, String, Option<Host>, Framing), Status> {
    // Without the final CRLF every line, the request line included, ends in CRLF.
    let head = head.strip_suffix(b"\r\n").ok_or(Status::BadRequest)?;
    let mut lines = head.split_inclusive(|&byte| byte == b'\n').map(|line| {
        line.strip_suffix(b"\r\n")
            .filter(|line| !line.contains(&b'\r'))
            .ok_or(Status::BadRequest)
    });
    let request_line = lines.next().ok_or(Status::BadRequest)??;
    let (method, path, http11) = parse_request_line(request_line)?;
    let mut length = None;
    let mut chunked = false;
    let mut transfer_encoding = false;
    let mut host = None;
    let mut hosts = 0_usize;
    for line in lines {
        let line = line?;
        let colon = line
            .iter()
            .position(|&byte| byte == b':')
            .ok_or(Status::BadRequest)?;
        let (name, value) = line.split_at_checked(colon).ok_or(Status::BadRequest)?;
        let value = trim_whitespace(value.get(1..).unwrap_or_default());
        if !is_token(name) || !value.iter().all(|&byte| is_field_byte(byte)) {
            return Err(Status::BadRequest);
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            if length.is_some() || value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(Status::BadRequest);
            }
            // Saturation is refused below as too large.
            length = Some(value.iter().fold(0_usize, |length, &digit| {
                length
                    .saturating_mul(10)
                    .saturating_add(usize::from(digit - b'0'))
            }));
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if transfer_encoding || !value.eq_ignore_ascii_case(b"chunked") {
                return Err(Status::BadRequest);
            }
            transfer_encoding = true;
            chunked = true;
        } else if name.eq_ignore_ascii_case(b"host") {
            // Only a lone Host names the client's address; a request is never refused for it.
            hosts = hosts.saturating_add(1);
            host = if hosts == 1 { parse_host(value) } else { None };
        }
    }
    let framing = match (length, chunked) {
        // Both framings at once is the request-smuggling shape (RFC 9112 6.3).
        (Some(_), true) => return Err(Status::BadRequest),
        (None, true) if !http11 => return Err(Status::BadRequest),
        (None, true) => Framing::Chunked,
        (Some(length), false) if length > MAX_BODY_BYTES => return Err(Status::PayloadTooLarge),
        (Some(length), false) => Framing::Length(length),
        (None, false) => Framing::Length(0),
    };
    Ok((method, path, host, framing))
}

/// The host of a `Host` value (`host [":" port]`, an IPv6 literal in brackets), or `None` when
/// it is neither an IP literal nor a DNS name.
fn parse_host(value: &[u8]) -> Option<Host> {
    let text = std::str::from_utf8(value).ok()?;
    let is_port = |port: &str| port.bytes().all(|byte| byte.is_ascii_digit());
    if let Some(rest) = text.strip_prefix('[') {
        let (literal, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        return literal
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| Host::Ip(ip.into()));
    }
    let name = match text.rsplit_once(':') {
        Some((name, port)) if is_port(port) => name,
        Some(_) => return None,
        None => text,
    };
    if let Ok(ip) = name.parse::<Ipv4Addr>() {
        return Some(Host::Ip(ip.into()));
    }
    is_dns_name(name).then(|| Host::Name(name.to_owned()))
}

/// Whether `name` is a DNS name: 1 to [`MAX_HOST_NAME_BYTES`] bytes of dot-separated labels,
/// each 1 to [`MAX_LABEL_BYTES`] letters, digits and hyphens, not starting or ending with one.
fn is_dns_name(name: &str) -> bool {
    (1..=MAX_HOST_NAME_BYTES).contains(&name.len())
        && name.split('.').all(|label| {
            (1..=MAX_LABEL_BYTES).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Parses `METHOD SP /target SP HTTP/1.x`; returns whether the version is 1.1.
fn parse_request_line(line: &[u8]) -> Result<(Method, String, bool), Status> {
    let mut parts = line.split(|&byte| byte == b' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Status::BadRequest);
    };
    let method = match method {
        b"GET" => Method::Get,
        b"POST" => Method::Post,
        method if is_token(method) => Method::Other,
        _ => return Err(Status::BadRequest),
    };
    let http11 = match version {
        b"HTTP/1.1" => true,
        b"HTTP/1.0" => false,
        _ => return Err(Status::BadRequest),
    };
    if target.first() != Some(&b'/') || !target.iter().all(u8::is_ascii_graphic) {
        return Err(Status::BadRequest);
    }
    let path = target
        .split(|&byte| byte == b'?')
        .next()
        .unwrap_or_default();
    let path = String::from_utf8(path.to_vec()).map_err(|_| Status::BadRequest)?;
    Ok((method, path, http11))
}

/// A non-empty RFC 9110 token: a method or a header name.
fn is_token(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// `bytes` without HTTP whitespace (space and tab) at either end.
fn trim_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = bytes {
        bytes = rest;
    }
    bytes
}

/// A byte of a header value or chunk extension: tab, visible ASCII, space or obs-text.
fn is_field_byte(byte: u8) -> bool {
    byte == b'\t' || byte == b' ' || byte.is_ascii_graphic() || byte >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use tokio_rustls::TlsConnector;

    use crate::protocol::Rng;
    use crate::raknet::tests::{run, run_paused};

    const WAIT: Duration = Duration::from_secs(10);
    const STATUS: &[u8] = br#"{"name":"Anchor","protocol":1}"#;

    /// A server on an IPv4 localhost port with `offers` as its queue.
    fn server(
        tls: Option<Arc<rustls::ServerConfig>>,
        offers: mpsc::Sender<Offer>,
    ) -> (Handle, SocketAddr, watch::Sender<Arc<[u8]>>) {
        let (status, statuses) = watch::channel(Arc::<[u8]>::from(STATUS));
        let settings = Settings {
            v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            v6: None,
            tls,
        };
        let handle = spawn(settings, statuses, offers).unwrap();
        let addr = handle.local_addrs()[0];
        (handle, addr, status)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test client reads the whole reply of the server under test"
    )]
    async fn read_all<S: AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
        let mut response = Vec::new();
        tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    }

    /// Sends `request` on a new connection and reads until the server closes it.
    async fn send(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        read_all(&mut stream).await
    }

    fn response(head: &str, body: &str) -> Vec<u8> {
        let (code, kind) = head.rsplit_once(' ').unwrap();
        format!(
            "HTTP/1.1 {code}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn text(code: &str) -> Vec<u8> {
        let reason = code.split_once(' ').unwrap().1;
        response(&format!("{code} text/plain"), reason)
    }

    fn sdp(body: &str) -> Vec<u8> {
        response("200 OK application/sdp", body)
    }

    fn post(network_id: &str, body: &str) -> Vec<u8> {
        format!(
            "POST /v1/join/{network_id} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Answers offers by network id: 1 answers, 2 fails with 37, 3 is busy, others drop.
    async fn consume(mut offers: mpsc::Receiver<Offer>) {
        while let Some(offer) = offers.recv().await {
            assert_eq!(offer.peer.ip(), Ipv4Addr::LOCALHOST);
            assert_eq!(offer.local.ip(), Ipv4Addr::LOCALHOST);
            let reply = match offer.network_id {
                1 => Reply::Answer(format!("answer to {}", offer.sdp)),
                2 => Reply::Code(Code::Identity),
                3 => Reply::Busy,
                _ => continue,
            };
            offer.reply.send(reply).unwrap();
        }
    }

    #[test]
    fn requests_get_the_official_responses() {
        run(requests_get_responses());
    }

    async fn requests_get_responses() {
        let (offers, received) = mpsc::channel(8);
        let consumer = tokio::spawn(consume(received));
        let (handle, addr, status) = server(None, offers);
        let json = |body: &[u8]| {
            response(
                "200 OK application/json",
                std::str::from_utf8(body).unwrap(),
            )
        };

        let get = b"GET /v1/join HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(send(addr, get).await, json(STATUS));
        status.send(Arc::from(&b"{}"[..])).unwrap();
        assert_eq!(send(addr, get).await, json(b"{}"));
        let query = b"GET /v1/join?x=1 HTTP/1.0\r\n\r\n";
        assert_eq!(send(addr, query).await, json(b"{}"));

        assert_eq!(
            send(addr, &post("1", "offer")).await,
            sdp("answer to offer")
        );
        assert_eq!(send(addr, &post("2", "offer")).await, sdp("37"));
        assert_eq!(
            send(addr, &post("3", "offer")).await,
            text("503 Service Unavailable")
        );
        assert_eq!(send(addr, &post("4", "offer")).await, sdp("11"));
        assert_eq!(send(addr, &post("1", "")).await, sdp("13"));
        let latin1 = b"POST /v1/join/1 HTTP/1.1\r\nContent-Length: 1\r\n\r\n\xff";
        assert_eq!(send(addr, latin1).await, sdp("13"));
        let chunked = b"POST /v1/join/1 HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n\
            3;ext=1\r\noff\r\n2\r\ner\r\n0\r\nTrailer: x\r\n\r\n";
        assert_eq!(send(addr, chunked).await, sdp("answer to offer"));

        let bad_id = text("400 Bad Request: the path needs a network ID");
        for id in ["", "abc", "+1", "-1", "1a", "18446744073709551616", "1/2"] {
            assert_eq!(send(addr, &post(id, "offer")).await, bad_id, "{id}");
        }
        assert_eq!(
            send(addr, &post("18446744073709551615", "")).await,
            sdp("13")
        );

        let not_found = text("404 Not Found");
        for request in [
            &b"GET / HTTP/1.1\r\n\r\n"[..],
            b"GET /v1/joined HTTP/1.1\r\n\r\n",
            b"GET /v1/join/1 HTTP/1.1\r\n\r\n",
            b"PUT /v1/join HTTP/1.1\r\n\r\n",
            b"POST /v1/join HTTP/1.1\r\n\r\n",
            b"get /v1/join HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(send(addr, request).await, not_found);
        }

        let bad = text("400 Bad Request");
        for request in [
            &b"GET /v1/join HTTP/2.0\r\n\r\n"[..],
            b"GET  /v1/join HTTP/1.1\r\n\r\n",
            b"GET /v1/join HTTP/1.1\r\nNo colon\r\n\r\n",
            b"GET /v1/join HTTP/1.1\r\nBad name: x\r\n\r\n",
            b"GET /v1/join HTTP/1.1\r\nX: a\rb\r\n\r\n",
            b"GET /v1/join HTTP/1.1\nHost: x\r\n\r\n",
            b"POST /v1/join/1 HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST /v1/join/1 HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            b"POST /v1/join/1 HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST /v1/join/1 HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            b"POST /v1/join/1 HTTP/1.1\r\nContent-Length: +1\r\n\r\nx",
            b"POST /v1/join/1 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nx\r\n",
        ] {
            assert_eq!(send(addr, request).await, bad, "{}", request.escape_ascii());
        }

        handle.shutdown().await;
        consumer.await.unwrap();
    }

    #[test]
    fn limits_refuse_oversized_requests() {
        run(refuse_oversized_requests());
    }

    async fn refuse_oversized_requests() {
        let (offers, received) = mpsc::channel(8);
        let consumer = tokio::spawn(consume(received));
        let (handle, addr, _status) = server(None, offers);

        // Exactly the limit, so the server reads every byte sent and closes without a reset.
        let mut head = b"GET /v1/join HTTP/1.1\r\n".to_vec();
        while head.len() < MAX_HEAD_BYTES {
            head.extend_from_slice(b"X-Filler: 0123456789abcdef\r\n");
        }
        head.truncate(MAX_HEAD_BYTES);
        assert_eq!(
            send(addr, &head).await,
            text("431 Request Header Fields Too Large")
        );

        let largest = "x".repeat(MAX_BODY_BYTES);
        let answer = sdp(&format!("answer to {largest}"));
        assert_eq!(send(addr, &post("1", &largest)).await, answer);
        let over = format!(
            "POST /v1/join/1 HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(
            send(addr, over.as_bytes()).await,
            text("413 Payload Too Large")
        );

        let mut chunked =
            b"POST /v1/join/1 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..MAX_BODY_BYTES / 4096 {
            chunked.extend_from_slice(b"1000\r\n");
            chunked.extend_from_slice(&[b'x'; 4096]);
            chunked.extend_from_slice(b"\r\n");
        }
        let mut over = chunked.clone();
        chunked.extend_from_slice(b"0\r\n\r\n");
        assert_eq!(send(addr, &chunked).await, answer);
        // The refusal comes at this last byte, so nothing is left unread.
        over.extend_from_slice(b"1");
        assert_eq!(send(addr, &over).await, text("413 Payload Too Large"));

        handle.shutdown().await;
        consumer.await.unwrap();
    }

    #[test]
    fn full_offer_queue_is_unavailable() {
        run(async {
            let (offers, received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers);
            // The first offer fills the queue and waits for a reply that never comes.
            let mut waiting = TcpStream::connect(addr).await.unwrap();
            waiting.write_all(&post("1", "offer")).await.unwrap();
            while received.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let unavailable = text("503 Service Unavailable");
            assert_eq!(send(addr, &post("1", "offer")).await, unavailable);
            handle.shutdown().await;
            // Shutdown aborted the waiting connection without a response.
            assert!(read_all(&mut waiting).await.is_empty());
        });
    }

    #[test]
    fn closed_offer_queue_fails_internally() {
        run(async {
            let (offers, received) = mpsc::channel(1);
            drop(received);
            let (handle, addr, _status) = server(None, offers);
            assert_eq!(send(addr, &post("1", "offer")).await, sdp("11"));
            handle.shutdown().await;
        });
    }

    #[test]
    fn slow_request_is_dropped_at_the_deadline() {
        run_paused(async {
            let (offers, _received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers);
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /v1/join HTTP/1.1\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            let early = REQUEST_DEADLINE
                .checked_sub(Duration::from_secs(1))
                .unwrap();
            assert!(
                tokio::time::timeout(early, stream.read(&mut byte))
                    .await
                    .is_err(),
                "closed before the deadline"
            );
            let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await;
            // Closed without a response.
            assert_eq!(read.unwrap().unwrap(), 0);
            handle.shutdown().await;
        });
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test client reads the whole reply of the server under test"
    )]
    fn connections_over_the_ip_limit_get_503() {
        run(async {
            let (offers, _received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers);
            let mut held = Vec::new();
            for _ in 0..MAX_CONNECTIONS_PER_IP {
                held.push(TcpStream::connect(addr).await.unwrap());
            }
            let mut over = TcpStream::connect(addr).await.unwrap();
            assert_eq!(read_all(&mut over).await, text("503 Service Unavailable"));

            // A closed connection frees its place once the server sees the close. Until then a
            // request is refused, and a refused request left unread may come back as a reset.
            drop(held.pop());
            let served = async {
                loop {
                    let mut stream = TcpStream::connect(addr).await.unwrap();
                    let mut response = Vec::new();
                    if stream
                        .write_all(b"GET /v1/join HTTP/1.1\r\n\r\n")
                        .await
                        .is_ok()
                        && stream.read_to_end(&mut response).await.is_ok()
                        && response.starts_with(b"HTTP/1.1 200 OK")
                    {
                        break;
                    }
                }
            };
            tokio::time::timeout(WAIT, served).await.unwrap();
            handle.shutdown().await;
        });
    }

    #[test]
    fn limits_count_ipv4_hosts_and_ipv6_prefixes() {
        let mut limits = Limits::default();
        let v4 = ip_key(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        let mapped = ip_key(IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped()));
        assert_eq!(v4, mapped);
        let v6 = |last: u16| {
            ip_key(IpAddr::V6(Ipv6Addr::new(
                0x2001, 0xdb8, 1, 2, 0, 0, 0, last,
            )))
        };
        assert_eq!(v6(1), v6(2));
        assert_ne!(
            v6(1),
            ip_key(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1)))
        );

        for _ in 0..MAX_CONNECTIONS_PER_IP {
            assert!(limits.try_add(v6(1)));
        }
        assert!(!limits.try_add(v6(2)));
        limits.remove(v6(1));
        assert!(limits.try_add(v6(2)));

        // Fill the total from distinct IPv4 hosts.
        let mut hosts = (0..).map(|n: u32| ip_key(IpAddr::V4(Ipv4Addr::from_bits(n))));
        while limits.total < MAX_CONNECTIONS {
            assert!(limits.try_add(hosts.next().unwrap()));
        }
        assert!(!limits.try_add(hosts.next().unwrap()));
        limits.remove(v6(1));
        assert!(limits.try_add(v4));

        // Removing every connection empties the table.
        let groups: Vec<_> = limits
            .groups
            .iter()
            .map(|(&group, &count)| (group, count))
            .collect();
        for (group, count) in groups {
            for _ in 0..count {
                limits.remove(group);
            }
        }
        assert_eq!(limits.total, 0);
        assert!(limits.groups.is_empty());
    }

    #[test]
    fn client_hello_without_tls_gets_handshake_failure() {
        run(async {
            let (offers, _received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers);
            let hello = [0x16, 0x03, 0x01, 0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00];
            assert_eq!(send(addr, &hello).await, HANDSHAKE_FAILURE);
            handle.shutdown().await;
        });
    }

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/nethernet/testdata")
            .join(name)
    }

    #[test]
    fn https_serves_with_a_configured_certificate() {
        let config = load_tls(&testdata("cert.pem"), &testdata("key.pem")).unwrap();
        run(async {
            let (offers, received) = mpsc::channel(8);
            let consumer = tokio::spawn(consume(received));
            let (handle, addr, _status) = server(Some(config), offers);

            let pem = include_bytes!("testdata/cert.pem");
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from_pem_slice(pem).unwrap())
                .unwrap();
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let client = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(client));
            for (request, expected) in [
                (
                    b"GET /v1/join HTTP/1.1\r\n\r\n".to_vec(),
                    response(
                        "200 OK application/json",
                        std::str::from_utf8(STATUS).unwrap(),
                    ),
                ),
                (post("1", "offer"), sdp("answer to offer")),
            ] {
                let tcp = TcpStream::connect(addr).await.unwrap();
                let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                let mut tls = connector.connect(name, tcp).await.unwrap();
                tls.write_all(&request).await.unwrap();
                assert_eq!(read_all(&mut tls).await, expected);
            }
            handle.shutdown().await;
            consumer.await.unwrap();
        });
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn load_tls_refuses_bad_files() {
        let missing = testdata("missing.pem");
        let cert = testdata("cert.pem");
        let key = testdata("key.pem");
        assert!(matches!(load_tls(&missing, &key), Err(Error::Read { .. })));
        assert!(matches!(load_tls(&cert, &missing), Err(Error::Read { .. })));
        // A key file holds no certificate, and a certificate file no key.
        assert!(matches!(
            load_tls(&key, &key),
            Err(Error::Pem {
                item: "certificate",
                ..
            })
        ));
        assert!(matches!(load_tls(&cert, &cert), Err(Error::Pem { .. })));
        let huge = std::env::temp_dir().join(format!("anchor-huge-{}.pem", std::process::id()));
        std::fs::write(
            &huge,
            vec![b' '; usize::try_from(MAX_PEM_BYTES).unwrap() + 1],
        )
        .unwrap();
        let refused = load_tls(&huge, &key);
        std::fs::remove_file(&huge).unwrap();
        assert!(matches!(refused, Err(Error::TooLarge { .. })));
    }

    #[test]
    fn spawn_needs_a_runtime_and_an_address() {
        let (_, statuses) = watch::channel(Arc::<[u8]>::from(STATUS));
        let (offers, _) = mpsc::channel(1);
        let settings = Settings {
            v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            v6: None,
            tls: None,
        };
        assert!(matches!(
            spawn(settings.clone(), statuses.clone(), offers.clone()),
            Err(Error::Runtime(_))
        ));
        run(async {
            let none = Settings {
                v4: None,
                ..settings.clone()
            };
            assert!(matches!(
                spawn(none, statuses.clone(), offers.clone()),
                Err(Error::NoAddress)
            ));
            // IPv4 and IPv6 share one port.
            let probe = spawn(settings, statuses.clone(), offers.clone()).unwrap();
            let port = probe.local_addrs()[0].port();
            probe.shutdown().await;
            let both = Settings {
                v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
                v6: Some(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0)),
                tls: None,
            };
            let handle = spawn(both, statuses, offers).unwrap();
            let v6 = handle.local_addrs()[1];
            assert_eq!(v6.port(), port);
            let get = b"GET /v1/join HTTP/1.1\r\n\r\n";
            assert!(send(v6, get).await.starts_with(b"HTTP/1.1 200 OK"));
            handle.shutdown().await;
        });
    }

    /// A listener that cannot be opened is reported with its address.
    #[test]
    fn a_taken_address_is_reported_with_the_address() {
        run(async {
            let (offers, _received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers.clone());
            let (_, statuses) = watch::channel(Arc::<[u8]>::from(STATUS));
            let settings = Settings {
                v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port())),
                v6: None,
                tls: None,
            };
            let Err(Error::Bind { addr: failed, .. }) = spawn(settings, statuses, offers) else {
                panic!("the taken port was bound twice");
            };
            assert_eq!(failed, addr);
            handle.shutdown().await;
        });
    }

    /// The address of the next connection `listener` accepts.
    async fn accepted(listener: &TcpListener) -> SocketAddr {
        let accepted = tokio::time::timeout(WAIT, listener.accept()).await;
        accepted.unwrap().unwrap().1
    }

    /// The IPv4 and the IPv6 listener share one port and each accepts only its own family.
    #[test]
    fn ipv4_and_ipv6_listeners_share_a_port() {
        run(async {
            if !crate::raknet::tests::has_ipv6_loopback() {
                return;
            }
            let (ip4, ip6) = crate::raknet::tests::dual_stack_ips();
            let v4 = open(SocketAddr::from((ip4, 0))).unwrap();
            let port = v4.local_addr().unwrap().port();
            let v6 = open(SocketAddr::from((ip6, port))).unwrap();
            assert!(socket2::SockRef::from(&v6).only_v6().unwrap());
            // The IPv4 client connects first: an IPv6 listener taking IPv4 connections would
            // accept it first.
            let client4 = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            let client6 = TcpStream::connect((Ipv6Addr::LOCALHOST, port))
                .await
                .unwrap();
            assert_eq!(accepted(&v4).await, client4.local_addr().unwrap());
            assert_eq!(accepted(&v6).await, client6.local_addr().unwrap());
        });
    }

    /// A restarted server binds its port at once, though the connection it closed last still
    /// holds that port in `TIME_WAIT`.
    #[test]
    fn a_restart_binds_the_port_its_closed_connection_holds() {
        run(async {
            let (offers, _received) = mpsc::channel(1);
            let (handle, addr, _status) = server(None, offers.clone());
            let get = b"GET /v1/join HTTP/1.1\r\n\r\n";
            // The server closes first, so its end of the connection waits in `TIME_WAIT`.
            assert!(send(addr, get).await.starts_with(b"HTTP/1.1 200 OK"));
            handle.shutdown().await;

            let (_status, statuses) = watch::channel(Arc::<[u8]>::from(STATUS));
            let settings = Settings {
                v4: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port())),
                v6: None,
                tls: None,
            };
            let handle = spawn(settings, statuses, offers).unwrap();
            assert!(send(addr, get).await.starts_with(b"HTTP/1.1 200 OK"));
            handle.shutdown().await;
        });
    }

    /// Parses `bytes` fed in pieces of `piece` bytes.
    fn parse(bytes: &[u8], piece: usize) -> Result<Option<Request>, Status> {
        let mut parser = Parser::default();
        for data in bytes.chunks(piece.max(1)) {
            if let Some(request) = parser.push(data)? {
                return Ok(Some(request));
            }
        }
        Ok(None)
    }

    #[test]
    fn parser_reads_requests_in_any_pieces() {
        let request = b"POST /v1/join/7?a HTTP/1.1\r\ncontent-length: 5\r\n\r\nhello extra";
        for piece in 1..request.len() {
            let expected = Request {
                method: Method::Post,
                path: "/v1/join/7".to_owned(),
                host: None,
                body: b"hello".to_vec(),
            };
            assert_eq!(parse(request, piece), Ok(Some(expected)));
        }
        assert_eq!(parse(b"GET /v1/join HTTP/1.1\r\n", 3), Ok(None));
        let chunked =
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n00005\r\nhello\r\n0\r\n\r\n";
        for piece in 1..chunked.len() {
            let request = parse(chunked, piece).unwrap().unwrap();
            assert_eq!(request.body, b"hello");
        }
        let huge =
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffffffffff\r\n";
        assert_eq!(parse(huge, 4), Err(Status::PayloadTooLarge));
        let huge = b"POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999999\r\n\r\n";
        assert_eq!(parse(huge, 4), Err(Status::PayloadTooLarge));
        let mut tiny = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..MAX_FRAMING_BYTES {
            tiny.extend_from_slice(b"1\r\nx\r\n");
        }
        assert_eq!(parse(&tiny, 4096), Err(Status::PayloadTooLarge));
    }

    /// Many small chunks grow the body by doubling and never past its limit, so the body is
    /// not copied once per chunk.
    #[test]
    fn small_chunks_grow_the_body_by_doubling() {
        let mut parser = Parser::default();
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parser.push(head), Ok(None));
        let mut capacities = Vec::new();
        for _ in 0..1000 {
            assert_eq!(parser.push(b"1\r\nx\r\n"), Ok(None));
            if capacities.last() != Some(&parser.body.capacity()) {
                capacities.push(parser.body.capacity());
            }
        }
        assert_eq!(parser.body.len(), 1000);
        assert!(capacities.len() <= 16, "{capacities:?}");
        assert!(parser.body.capacity() <= MAX_BODY_BYTES);
        let request = parser.push(b"0\r\n\r\n").unwrap().unwrap();
        assert_eq!(request.body, vec![b'x'; 1000]);
    }

    /// A lone `Host` header gives the client's host without its port. One that is neither an IP
    /// literal nor a DNS name, or a second one, gives none, and the request still goes through.
    #[test]
    fn a_lone_host_header_names_the_client_host() {
        let host = |value: &str| parse_host(value.as_bytes());
        let ip = |text: &str| Some(Host::Ip(text.parse().unwrap()));
        let name = |text: &str| Some(Host::Name(text.to_owned()));
        assert_eq!(host("203.0.113.7:19132"), ip("203.0.113.7"));
        assert_eq!(host("203.0.113.7"), ip("203.0.113.7"));
        assert_eq!(host("203.0.113.7:"), ip("203.0.113.7"));
        assert_eq!(host("[2001:db8::1]:19132"), ip("2001:db8::1"));
        assert_eq!(host("[2001:db8::1]"), ip("2001:db8::1"));
        assert_eq!(host("play.example.net:19132"), name("play.example.net"));
        assert_eq!(host("localhost"), name("localhost"));
        let longest = format!("{}a", "a.".repeat(126));
        assert_eq!(host(&longest), name(&longest));
        for bad in [
            String::new(),
            ":19132".to_owned(),
            "2001:db8::1".to_owned(),
            "[2001:db8::1".to_owned(),
            "[2001:db8::1]x".to_owned(),
            "[2001:db8::1]:x".to_owned(),
            "[203.0.113.7]".to_owned(),
            "203.0.113.7:+1".to_owned(),
            "example.net.".to_owned(),
            "a..b".to_owned(),
            "-a.b".to_owned(),
            "a-.b".to_owned(),
            "a_b".to_owned(),
            "a b".to_owned(),
            "\u{e9}.net".to_owned(),
            "a".repeat(64),
            format!("{longest}a"),
        ] {
            assert_eq!(host(&bad), None, "{bad}");
        }

        let request = |headers: &str| {
            let head = format!("GET /v1/join HTTP/1.1\r\n{headers}\r\n");
            parse(head.as_bytes(), 1).unwrap().unwrap().host
        };
        assert_eq!(request("Host: 203.0.113.7:19132\r\n"), ip("203.0.113.7"));
        assert_eq!(
            request("host:  example.net:19132 \r\n"),
            name("example.net")
        );
        assert_eq!(request(""), None);
        assert_eq!(request("Host: a_b\r\n"), None);
        let twice = "Host: 203.0.113.7\r\nHost: 203.0.113.7\r\n";
        assert_eq!(request(twice), None);
        assert_eq!(request(&format!("{twice}Host: 203.0.113.7\r\n")), None);
    }

    #[test]
    fn routes_follow_the_official_server() {
        assert_eq!(route(Method::Get, "/v1/join"), Route::Status);
        assert_eq!(route(Method::Post, "/v1/join/0"), Route::Offer(0));
        assert_eq!(
            route(Method::Post, "/v1/join/00000000000000000001"),
            Route::Offer(1)
        );
        let bad = Route::Reject(Status::BadNetworkId);
        assert_eq!(route(Method::Post, "/v1/join/000000000000000000001"), bad);
        assert_eq!(route(Method::Post, "/v1/join/ 1"), bad);
        let not_found = Route::Reject(Status::NotFound);
        assert_eq!(route(Method::Other, "/v1/join"), not_found);
        assert_eq!(route(Method::Get, "/v1/join/1"), not_found);
        assert_eq!(route(Method::Post, "/v1/join"), not_found);
    }

    #[test]
    fn mutated_requests_never_panic() {
        let seeds: [&[u8]; 4] = [
            b"GET /v1/join HTTP/1.1\r\nHost: 127.0.0.1:19132\r\n\r\n",
            b"POST /v1/join/123 HTTP/1.1\r\nContent-Length: 11\r\nContent-Type: application/sdp\r\n\r\nv=0\r\no=- 1 2",
            b"POST /v1/join/9 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4;x=y\r\nv=0\n\r\n3\r\nabc\r\n0\r\nT: 1\r\n\r\n",
            b"POST /v1/join/1 HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
        ];
        let mut rng = Rng(0x00a1_c401);
        let mut done = 0;
        for _ in 0..10_000 {
            let mut bytes = seeds[rng.below(seeds.len())].to_vec();
            for _ in 0..=rng.below(8) {
                let at = rng.below(bytes.len() + 1);
                let byte = u8::try_from(rng.next() & 0xff).unwrap();
                match rng.below(4) {
                    0 if at < bytes.len() => bytes[at] ^= 1 << rng.below(8),
                    1 => bytes.insert(at, byte),
                    2 if at < bytes.len() => drop(bytes.remove(at)),
                    _ => bytes.truncate(at),
                }
            }
            let whole = parse(&bytes, bytes.len());
            // Any split of the same bytes gives the same result.
            let pieces = parse(&bytes, 1 + rng.below(16));
            assert_eq!(whole, pieces, "{}", bytes.escape_ascii());
            if let Ok(Some(request)) = whole {
                assert!(request.body.len() <= MAX_BODY_BYTES);
                let _ = route(request.method, &request.path);
                done += 1;
            }
        }
        // The mutations leave some requests whole, so the success paths are fuzzed too.
        assert!(done > 100, "{done}");
    }
}
