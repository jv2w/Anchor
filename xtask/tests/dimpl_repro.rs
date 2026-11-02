//! Reproduction tests for the fixed dimpl copy in `vendor/dimpl`: each drives
//! dimpl's public API in memory the way str0m does (a DTLS 1.2 client with the aws-lc-rs
//! backend against a dimpl server or hand-made records) and fails on the published 0.7.5 with a
//! panic.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dimpl::certificate::generate_self_signed_certificate;
use dimpl::{Config, Dtls, DtlsCertificate, Error, Output};

/// The DTLS record content type of `ChangeCipherSpec`.
const CONTENT_CHANGE_CIPHER_SPEC: u8 = 20;
/// The DTLS record content type of a handshake message.
const CONTENT_HANDSHAKE: u8 = 22;
/// The DTLS record content type of application data.
const CONTENT_APPLICATION_DATA: u8 = 23;
/// Steps before a handshake counts as stuck: 5 simulated seconds, past several retransmissions.
const MAX_STEPS: usize = 500;
/// DTLS record header length.
const RECORD_HEADER: usize = 13;
/// Simulated time between two steps of a handshake.
const STEP: Duration = Duration::from_millis(10);
/// The output buffer str0m polls dimpl with (`Rtc::dtls_buf`).
const STR0M_BUFFER: usize = 2000;

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the simulated clock starts at the real time and then moves by fixed steps"
)]
fn start() -> Instant {
    Instant::now()
}

fn certificate() -> DtlsCertificate {
    generate_self_signed_certificate().expect("generate a certificate")
}

/// A DTLS 1.2 client with the default configuration.
fn new_client(certificate: DtlsCertificate, now: Instant) -> Dtls {
    let mut dtls = Dtls::new_12(Arc::new(Config::default()), certificate, now);
    dtls.set_active(true);
    dtls
}

/// A DTLS 1.2 server with the default configuration.
fn new_server(certificate: DtlsCertificate, now: Instant) -> Dtls {
    let mut dtls = Dtls::new_12(Arc::new(Config::default()), certificate, now);
    dtls.set_active(false);
    dtls
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Runs `f`, turning a panic inside dimpl into a test failure that names `what`.
fn no_panic<T>(what: &str, f: impl FnOnce() -> T) -> T {
    panic::catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|payload| panic!("{what} panicked: {}", panic_message(payload.as_ref())))
}

/// One DTLS record.
fn record(content_type: u8, epoch: u16, sequence: u64, fragment: &[u8]) -> Vec<u8> {
    let mut out = vec![content_type, 0xfe, 0xfd];
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(&sequence.to_be_bytes()[2..]);
    out.extend_from_slice(
        &u16::try_from(fragment.len())
            .expect("fits u16")
            .to_be_bytes(),
    );
    out.extend_from_slice(fragment);
    out
}

/// The records of a datagram, each with its header.
fn records(datagram: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = datagram;
    while !rest.is_empty() {
        let len = RECORD_HEADER + usize::from(u16::from_be_bytes([rest[11], rest[12]]));
        out.push(rest[..len].to_vec());
        rest = &rest[len..];
    }
    out
}

/// What one endpoint produced while polled.
#[derive(Default)]
struct Drained {
    packets: Vec<Vec<u8>>,
    connected: bool,
    peer_cert: Option<Vec<u8>>,
    /// The size the last `BufferTooSmall` asked for.
    too_small: Option<usize>,
}

/// Polls `dtls` until it waits for a timer. On `BufferTooSmall` the buffer grows to the asked
/// size, as dimpl documents, and the request is recorded.
fn drain(dtls: &mut Dtls, buffer: &mut Vec<u8>) -> Drained {
    let mut out = Drained::default();
    loop {
        let needed = match dtls.poll_output(buffer) {
            Output::Packet(p) => {
                out.packets.push(p.to_vec());
                continue;
            }
            Output::Connected => {
                out.connected = true;
                continue;
            }
            Output::PeerCert(der) => {
                out.peer_cert = Some(der.to_vec());
                continue;
            }
            Output::BufferTooSmall { needed } => needed,
            Output::Timeout(_) => return out,
            _ => continue,
        };
        out.too_small = Some(needed);
        buffer.resize(needed, 0);
    }
}

/// The client's view of a handshake.
#[derive(Default)]
struct Session {
    client_connected: bool,
    server_connected: bool,
    peer_cert: Option<Vec<u8>>,
    too_small: Option<usize>,
}

/// Runs a handshake for at most [`MAX_STEPS`]; `route` turns each step's server datagrams into
/// the datagrams the client receives. The client polls with str0m's buffer size.
fn handshake(
    client: &mut Dtls,
    server: &mut Dtls,
    now: &mut Instant,
    mut route: impl FnMut(Vec<Vec<u8>>) -> Vec<Vec<u8>>,
) -> Session {
    let mut session = Session::default();
    let mut client_buffer = vec![0; STR0M_BUFFER];
    let mut server_buffer = vec![0; STR0M_BUFFER];
    for _ in 0..MAX_STEPS {
        client.handle_timeout(*now).expect("client timer");
        server.handle_timeout(*now).expect("server timer");
        let from_client = drain(client, &mut client_buffer);
        let from_server = drain(server, &mut server_buffer);
        session.client_connected |= from_client.connected;
        session.server_connected |= from_server.connected;
        session.peer_cert = from_client.peer_cert.or(session.peer_cert.take());
        session.too_small = from_client.too_small.or(session.too_small);
        for datagram in &from_client.packets {
            server
                .handle_packet(datagram)
                .expect("server takes the datagram");
        }
        for datagram in route(from_server.packets) {
            client
                .handle_packet(&datagram)
                .expect("client takes the datagram");
        }
        if session.client_connected && session.server_connected {
            return session;
        }
        *now += STEP;
    }
    session
}

/// P1: a duplicate epoch-1 record that arrives before `ChangeCipherSpec` hit
/// `assert_eq!(seq_current.epoch, 0)` (engine.rs). One forged or duplicated UDP datagram is
/// enough; the copy is now dropped like any replayed record.
#[test]
fn duplicate_epoch_one_record_before_change_cipher_spec_is_dropped() {
    let mut now = start();
    let mut client = new_client(certificate(), now);
    let mut buffer = vec![0; STR0M_BUFFER];
    client.handle_timeout(now).expect("start the handshake");
    drain(&mut client, &mut buffer);

    let forged = record(CONTENT_APPLICATION_DATA, 1, 7, &[0x5a; 48]);
    for copy in 1..=2 {
        no_panic("an epoch-1 record before ChangeCipherSpec", || {
            client.handle_packet(&forged)
        })
        .unwrap_or_else(|error| panic!("copy {copy} is dropped silently, got {error}"));
    }
    now += STEP;
    client.handle_timeout(now).expect("client timer");
    assert!(drain(&mut client, &mut buffer).too_small.is_none());
}

/// P1 with real traffic: the server's encrypted Finished arrives twice before its
/// `ChangeCipherSpec` (a duplicated and reordered datagram). The handshake still completes.
#[test]
fn duplicated_server_finished_before_change_cipher_spec_still_connects() {
    let mut now = start();
    let mut client = new_client(certificate(), now);
    let mut server = new_server(certificate(), now);
    let mut reordered = false;
    let session = no_panic("a duplicated server Finished", || {
        handshake(&mut client, &mut server, &mut now, |datagrams| {
            let all: Vec<Vec<u8>> = datagrams.iter().flat_map(|d| records(d)).collect();
            if reordered || !all.iter().any(|r| r[0] == CONTENT_CHANGE_CIPHER_SPEC) {
                return datagrams;
            }
            reordered = true;
            let (ccs, encrypted): (Vec<_>, Vec<_>) = all
                .into_iter()
                .partition(|r| r[0] == CONTENT_CHANGE_CIPHER_SPEC);
            assert!(
                encrypted
                    .iter()
                    .all(|r| r[0] == CONTENT_HANDSHAKE && r[3..5] == [0, 1])
            );
            let mut out = encrypted.clone();
            out.extend(encrypted);
            out.extend(ccs);
            out
        })
    });
    assert!(reordered, "the server sent its ChangeCipherSpec");
    assert!(session.client_connected && session.server_connected);
}

/// Grows a certificate by `extra` bytes of signature padding. dimpl reads only the public key
/// and does no PKI check, so the certificate stays usable.
fn inflate(der: &[u8], extra: usize) -> Vec<u8> {
    /// Header length and content length of the DER element at `at`.
    fn header(der: &[u8], at: usize) -> (usize, usize) {
        let first = der[at + 1];
        if first < 0x80 {
            return (2, usize::from(first));
        }
        let n = usize::from(first & 0x7f);
        let len = der[at + 2..at + 2 + n]
            .iter()
            .fold(0, |len, &b| (len << 8) | usize::from(b));
        (2 + n, len)
    }
    fn element(tag: u8, content: &[u8]) -> Vec<u8> {
        let len = content.len();
        let mut out = vec![tag];
        if len < 0x80 {
            out.push(u8::try_from(len).expect("short form"));
        } else {
            let bytes = u32::try_from(len).expect("fits u32").to_be_bytes();
            let skip = bytes.iter().take_while(|&&b| b == 0).count();
            out.push(0x80 | u8::try_from(4 - skip).expect("at most 4"));
            out.extend_from_slice(&bytes[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    let (outer, _) = header(der, 0);
    let mut at = outer;
    for _ in 0..2 {
        let (h, len) = header(der, at);
        at += h + len;
    }
    assert_eq!(der[at], 0x03, "the signature is a BIT STRING");
    let (h, len) = header(der, at);
    let mut signature = der[at + h..at + h + len].to_vec();
    signature.resize(len + extra, 0);
    let mut content = der[outer..at].to_vec();
    content.extend(element(0x03, &signature));
    element(0x30, &content)
}

/// P2: a peer certificate larger than the poll buffer hit `assert!(l <= buf.len())`
/// (client.rs `LocalEvent::into_output`). str0m polls with 2000 bytes, so any peer could abort
/// it. dimpl now answers `BufferTooSmall` and keeps the event, and the whole certificate comes
/// out once the buffer is large enough.
#[test]
fn peer_certificate_larger_than_the_poll_buffer_asks_for_a_larger_buffer() {
    let mut now = start();
    let mut big = certificate();
    big.certificate = inflate(&big.certificate, 2 * STR0M_BUFFER);
    let expected = big.certificate.clone();
    let mut client = new_client(certificate(), now);
    let mut server = new_server(big, now);
    let session = no_panic("a large peer certificate", || {
        handshake(&mut client, &mut server, &mut now, |datagrams| datagrams)
    });
    assert_eq!(session.too_small, Some(expected.len()));
    assert!(
        session.peer_cert == Some(expected),
        "the whole certificate, not a prefix"
    );
    assert!(session.client_connected && session.server_connected);
}

/// P3: in auto mode a `ServerHello` whose last extension is a `supported_versions` header with no
/// data read past the end of the message (auto.rs). It is now an unrecognized response.
#[test]
fn truncated_supported_versions_in_auto_mode_is_rejected() {
    let now = start();
    let mut client = Dtls::new_auto(Arc::new(Config::default()), certificate(), now);
    client.set_active(true);
    client.handle_timeout(now).expect("start the handshake");
    drain(&mut client, &mut vec![0; STR0M_BUFFER]);

    let mut body = vec![0xfe, 0xfd];
    body.extend_from_slice(&[0x11; 32]); // random
    body.push(0); // session ID length
    body.extend_from_slice(&[0xc0, 0x2b, 0]); // cipher suite, compression
    body.extend_from_slice(&[0, 4, 0, 0x2b, 0, 2]); // extensions: supported_versions, length 2
    let len = u32::try_from(body.len()).expect("fits u32").to_be_bytes();
    let mut message = vec![2]; // ServerHello
    message.extend_from_slice(&len[1..]);
    message.extend_from_slice(&[0, 0, 0, 0, 0]); // message_seq, fragment_offset
    message.extend_from_slice(&len[1..]);
    message.extend_from_slice(&body);
    let datagram = record(CONTENT_HANDSHAKE, 0, 0, &message);

    let result = no_panic("a truncated supported_versions", || {
        client.handle_packet(&datagram)
    });
    assert!(
        matches!(result, Err(Error::UnexpectedMessage(_))),
        "{result:?}"
    );
}
