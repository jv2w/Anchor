//! SDP for NetherNet signaling: the client's offer parser and Anchor's answer
//! writer, in the layout the official server 1.26.52 answers with.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::str::FromStr;

use serde::Deserialize;

use crate::crypto::{STD, crc32, max_jws_text};
use crate::login::MAX_TOKEN_PAYLOAD_BYTES;

/// The bytes in a SHA-256 fingerprint.
const FINGERPRINT_BYTES: usize = 32;
/// The priority of the answer's host candidate, as the official server writes it.
const HOST_PRIORITY: u32 = 2_122_260_223;
/// The length of `ice-pwd` in characters (RFC 8839 5.4).
const ICE_PWD_LEN_RANGE: RangeInclusive<usize> = 22..=256;
/// The length of `ice-ufrag` in characters (RFC 8839 5.4).
const ICE_UFRAG_LEN_RANGE: RangeInclusive<usize> = 4..=256;
/// The most usable candidates. A client offers one per interface and address family.
const MAX_CANDIDATES: usize = 32;
/// The longest identity provider domain, in bytes.
const MAX_DOMAIN_BYTES: usize = 256;
/// The longest detached fingerprint JWS text, in bytes. An ES384 one is 150.
const MAX_FINGERPRINTS_BYTES: usize = 1024;
/// The largest decoded `a=identity` JSON, in bytes.
const MAX_IDENTITY_JSON_BYTES: usize = 32 * 1024;
/// The most non-empty lines in an offer. A real offer has about 20.
const MAX_LINES: usize = 1024;
/// The `a=max-message-size` Anchor answers with, as the official server: one 262,143-byte fragment and its
/// 1-byte header.
pub(crate) const MAX_MESSAGE_BYTES: u32 = 262_144;
/// The largest offer body, in bytes. A real offer is about 2 KiB, most of it the identity token.
pub(crate) const MAX_OFFER_BYTES: usize = 64 * 1024;
/// The longest identity token text, in bytes: the longest the Login token check accepts.
const MAX_TOKEN_BYTES: usize = max_jws_text(MAX_TOKEN_PAYLOAD_BYTES);
/// The smallest `max-message-size` accepted, in bytes.
const MIN_MESSAGE_BYTES: u32 = 1024;
/// The only SCTP port accepted: the RFC 8841 default, which go-nethernet and the official server write.
const SCTP_PORT: &str = "5000";

/// Why an offer was rejected. The offer task answers every one with code 13, the shape of the
/// `a=identity` value included (go-nethernet parses it with the rest of the offer).
#[derive(Debug)]
pub(crate) enum Error {
    /// The body is longer than [`MAX_OFFER_BYTES`].
    TooLarge,
    /// The body has more than [`MAX_LINES`] lines.
    TooManyLines,
    /// A line breaks the SDP grammar.
    Malformed(&'static str),
    /// The media sections are not exactly one data channel section.
    Media(&'static str),
    /// A required attribute is absent.
    Missing(&'static str),
    /// An attribute allowed once appears again.
    Duplicate(&'static str),
    /// The named attribute's value breaks a rule.
    Invalid(&'static str),
    /// More than [`MAX_CANDIDATES`] usable candidates.
    TooManyCandidates,
    /// The `a=identity` value breaks a rule.
    Identity(&'static str),
    /// The `a=identity` JSON is malformed.
    IdentityJson(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("offer too large"),
            Self::TooManyLines => f.write_str("offer has too many lines"),
            Self::Malformed(reason) => write!(f, "malformed sdp: {reason}"),
            Self::Media(reason) => write!(f, "unexpected media: {reason}"),
            Self::Missing(name) => write!(f, "missing attribute {name}"),
            Self::Duplicate(name) => write!(f, "duplicate attribute {name}"),
            Self::Invalid(name) => write!(f, "invalid attribute {name}"),
            Self::TooManyCandidates => f.write_str("too many candidates"),
            Self::Identity(reason) => write!(f, "invalid identity: {reason}"),
            Self::IdentityJson(_) => f.write_str("malformed identity json"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IdentityJson(e) => Some(e),
            _ => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// A client offer: what the server needs to answer and connect.
#[derive(Debug)]
pub(crate) struct Offer {
    /// The client's ICE username fragment.
    pub(crate) ice_ufrag: String,
    /// The client's ICE password.
    pub(crate) ice_pwd: String,
    /// The client's DTLS certificate fingerprint.
    pub(crate) fingerprint: Fingerprint,
    /// Largest message the client receives.
    pub(crate) max_message_bytes: u32,
    /// Usable candidates in offer order: UDP, component 1, IP literal addresses.
    pub(crate) candidates: Vec<SocketAddr>,
    /// The session-level `a=identity`, shape-checked but not verified.
    pub(crate) identity: Option<Assertion>,
}

/// A SHA-256 DTLS certificate fingerprint (`a=fingerprint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    /// The hash name as written (`sha-256` in any case). The identity signature covers it.
    pub(crate) algorithm: String,
    /// The hex digest as written (any case). The identity signature covers it.
    pub(crate) digest: String,
    /// The digest bytes.
    pub(crate) bytes: [u8; FINGERPRINT_BYTES],
}

/// The `a=identity` assertion (go-nethernet `identityData`), shape-checked but not verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assertion {
    /// The identity JWT, a compact JWS.
    pub(crate) token: String,
    /// A detached JWS (`header..signature`) over the fingerprint JSON.
    pub(crate) fingerprints: String,
}

/// Anchor's answer. [`write_answer`] lays it out as the official server 1.26.52 does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Answer<'a> {
    /// The `o=` session ID, random per answer as the official server draws it.
    pub(crate) session_id: u64,
    /// The `a=identity` value: base64 of the server assertion.
    pub(crate) identity: &'a str,
    /// The advertised host candidate. An IPv4-mapped IPv6 address is written as IPv4.
    pub(crate) addr: SocketAddr,
    /// The server's ICE username fragment.
    pub(crate) ice_ufrag: &'a str,
    /// The server's ICE password.
    pub(crate) ice_pwd: &'a str,
    /// The server's DTLS fingerprint as [`fingerprint_text`] writes it.
    pub(crate) fingerprint: &'a str,
}

/// Attributes collected while reading an offer.
#[derive(Default)]
struct Attributes<'a> {
    ice_ufrag: Option<&'a str>,
    ice_pwd: Option<&'a str>,
    fingerprint: Option<Fingerprint>,
    setup: Option<()>,
    max_message_bytes: Option<u32>,
    sctp_port: Option<()>,
    candidates: Vec<SocketAddr>,
    identity: Option<Assertion>,
}

/// The JSON of an `a=identity` value.
#[derive(Deserialize)]
struct IdentityJson {
    /// JSON text of [`AssertionJson`].
    assertion: String,
    idp: IdentityProvider,
}

/// The identity provider of an `a=identity` value.
#[derive(Deserialize)]
struct IdentityProvider {
    domain: String,
    protocol: String,
}

/// The assertion inside an `a=identity` value.
#[derive(Deserialize)]
struct AssertionJson {
    fingerprints: String,
    token: String,
}

/// Parses a client offer. Lines end in CRLF or LF; empty lines and unknown line
/// types and attributes are skipped. ICE, DTLS, SCTP attributes and candidates may sit at the
/// session or the media level; `a=identity` only at the session level.
pub(crate) fn parse_offer(text: &str) -> Result<Offer> {
    if text.len() > MAX_OFFER_BYTES {
        return Err(Error::TooLarge);
    }
    let mut found = Attributes::default();
    let mut lines = 0_usize;
    let mut media = 0_usize;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        lines += 1;
        if lines > MAX_LINES {
            return Err(Error::TooManyLines);
        }
        if line.bytes().any(|b| b.is_ascii_control()) {
            return Err(Error::Malformed("control character"));
        }
        if lines == 1 {
            if line != "v=0" {
                return Err(Error::Malformed("first line is not v=0"));
            }
            continue;
        }
        let Some((kind, value)) = line.split_once('=') else {
            return Err(Error::Malformed("line without '='"));
        };
        match kind {
            "m" => {
                media += 1;
                if media > 1 {
                    return Err(Error::Media("more than one media section"));
                }
                check_media(value)?;
            }
            "a" => {
                let (name, value) = match value.split_once(':') {
                    Some((name, value)) => (name, Some(value)),
                    None => (value, None),
                };
                attribute(&mut found, name, value, media > 0)?;
            }
            _ if kind.len() == 1 && kind.bytes().all(|b| b.is_ascii_lowercase()) => {}
            _ => return Err(Error::Malformed("line type is not one lowercase letter")),
        }
    }
    if lines == 0 {
        return Err(Error::Malformed("empty"));
    }
    if media == 0 {
        return Err(Error::Media("no media section"));
    }
    found.setup.ok_or(Error::Missing("setup"))?;
    Ok(Offer {
        ice_ufrag: found
            .ice_ufrag
            .ok_or(Error::Missing("ice-ufrag"))?
            .to_owned(),
        ice_pwd: found.ice_pwd.ok_or(Error::Missing("ice-pwd"))?.to_owned(),
        fingerprint: found.fingerprint.ok_or(Error::Missing("fingerprint"))?,
        max_message_bytes: found
            .max_message_bytes
            .ok_or(Error::Missing("max-message-size"))?,
        candidates: found.candidates,
        identity: found.identity,
    })
}

/// Checks the `m=` value: `application <port> UDP/DTLS/SCTP webrtc-datachannel`.
fn check_media(value: &str) -> Result<()> {
    let mut fields = value.split(' ');
    match (
        fields.next(),
        fields.next().and_then(decimal::<u16>),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        (Some("application"), Some(_), Some("UDP/DTLS/SCTP"), Some("webrtc-datachannel"), None) => {
            Ok(())
        }
        _ => Err(Error::Media("not one UDP/DTLS/SCTP data channel section")),
    }
}

fn attribute<'a>(
    found: &mut Attributes<'a>,
    name: &str,
    value: Option<&'a str>,
    media_level: bool,
) -> Result<()> {
    match name {
        "ice-ufrag" => once(
            &mut found.ice_ufrag,
            ice_text(value, ICE_UFRAG_LEN_RANGE, "ice-ufrag")?,
            "ice-ufrag",
        ),
        "ice-pwd" => once(
            &mut found.ice_pwd,
            ice_text(value, ICE_PWD_LEN_RANGE, "ice-pwd")?,
            "ice-pwd",
        ),
        "fingerprint" => once(&mut found.fingerprint, fingerprint(value)?, "fingerprint"),
        "setup" => match value {
            Some("actpass" | "passive") => once(&mut found.setup, (), "setup"),
            _ => Err(Error::Invalid("setup")),
        },
        "max-message-size" => {
            let size = value
                .and_then(decimal::<u32>)
                .filter(|&size| size >= MIN_MESSAGE_BYTES)
                .ok_or(Error::Invalid("max-message-size"))?;
            once(&mut found.max_message_bytes, size, "max-message-size")
        }
        "sctp-port" if value == Some(SCTP_PORT) => once(&mut found.sctp_port, (), "sctp-port"),
        "sctp-port" => Err(Error::Invalid("sctp-port")),
        "candidate" => {
            if let Some(candidate) = candidate(value)? {
                if found.candidates.len() == MAX_CANDIDATES {
                    return Err(Error::TooManyCandidates);
                }
                found.candidates.push(candidate);
            }
            Ok(())
        }
        "identity" if media_level => Err(Error::Identity("not at the session level")),
        "identity" => once(&mut found.identity, identity(value)?, "identity"),
        _ => Ok(()),
    }
}

fn once<T>(slot: &mut Option<T>, value: T, name: &'static str) -> Result<()> {
    if slot.is_some() {
        return Err(Error::Duplicate(name));
    }
    *slot = Some(value);
    Ok(())
}

/// An `ice-ufrag` or `ice-pwd` value: ICE characters (RFC 8839 `ice-char`) within `len`.
fn ice_text<'a>(
    value: Option<&'a str>,
    len: RangeInclusive<usize>,
    name: &'static str,
) -> Result<&'a str> {
    value
        .filter(|v| len.contains(&v.len()) && v.bytes().all(is_ice_char))
        .ok_or(Error::Invalid(name))
}

/// `sha-256 <32 hex bytes separated by colons>`.
fn fingerprint(value: Option<&str>) -> Result<Fingerprint> {
    let (algorithm, digest) = value
        .and_then(|v| v.split_once(' '))
        .ok_or(Error::Invalid("fingerprint"))?;
    if !algorithm.eq_ignore_ascii_case("sha-256") {
        return Err(Error::Invalid("fingerprint algorithm"));
    }
    let bytes = parse_digest(digest).ok_or(Error::Invalid("fingerprint"))?;
    Ok(Fingerprint {
        algorithm: algorithm.to_owned(),
        digest: digest.to_owned(),
        bytes,
    })
}

fn parse_digest(digest: &str) -> Option<[u8; FINGERPRINT_BYTES]> {
    if digest.len() != FINGERPRINT_BYTES * 3 - 1 {
        return None;
    }
    let hex = |b: u8| {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let mut out = [0_u8; FINGERPRINT_BYTES];
    for (i, (byte, chunk)) in out.iter_mut().zip(digest.as_bytes().chunks(3)).enumerate() {
        let (hi, lo) = match *chunk {
            [hi, lo, b':'] if i + 1 < FINGERPRINT_BYTES => (hi, lo),
            [hi, lo] if i + 1 == FINGERPRINT_BYTES => (hi, lo),
            _ => return None,
        };
        *byte = hex(hi)? << 4 | hex(lo)?;
    }
    Some(out)
}

/// A candidate value (RFC 8839 5.1), with the `candidate:` prefix once or twice (go-nethernet
/// writes it twice), as its address. `None` for a candidate Anchor cannot use: not UDP, not
/// component 1, or an address that is not an IP literal (such as an mDNS `.local` name).
fn candidate(value: Option<&str>) -> Result<Option<SocketAddr>> {
    let text = value.ok_or(Error::Invalid("candidate"))?;
    let text = text.strip_prefix("candidate:").unwrap_or(text);
    let mut fields = text.split(' ');
    let mut next = || {
        fields
            .next()
            .filter(|f| !f.is_empty())
            .ok_or(Error::Invalid("candidate"))
    };
    let (foundation, component, transport, priority) = (next()?, next()?, next()?, next()?);
    let (address, port, typ, kind) = (next()?, next()?, next()?, next()?);
    let (Some(component), Some(_), Some(port)) = (
        decimal::<u32>(component),
        decimal::<u32>(priority),
        decimal::<u16>(port),
    ) else {
        return Err(Error::Invalid("candidate"));
    };
    if foundation.len() > 32
        || !foundation.bytes().all(is_ice_char)
        || typ != "typ"
        || !kind.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(Error::Invalid("candidate"));
    }
    if component != 1 || !transport.eq_ignore_ascii_case("udp") {
        return Ok(None);
    }
    Ok(address
        .parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port)))
}

/// The `a=identity` value: padded standard base64 (go-nethernet `base64.StdEncoding`, and the official server) of
/// `{"assertion":"<JSON text of {fingerprints, token}>","idp":{"domain":..,"protocol":"default"}}`.
fn identity(value: Option<&str>) -> Result<Assertion> {
    let text = value.ok_or(Error::Identity("empty"))?;
    let json = STD
        .decode(text, MAX_IDENTITY_JSON_BYTES)
        .map_err(|_| Error::Identity("not padded standard base64 within the size cap"))?;
    let outer: IdentityJson = serde_json::from_slice(&json).map_err(Error::IdentityJson)?;
    let inner: AssertionJson =
        serde_json::from_str(&outer.assertion).map_err(Error::IdentityJson)?;
    if outer.idp.protocol != "default" {
        return Err(Error::Identity("protocol is not default"));
    }
    let domain = &outer.idp.domain;
    if domain.is_empty() || domain.len() > MAX_DOMAIN_BYTES || domain.chars().any(char::is_control)
    {
        return Err(Error::Identity("domain must be 1 to 256 bytes of text"));
    }
    if inner.token.len() > MAX_TOKEN_BYTES || !is_compact_jws(&inner.token, false) {
        return Err(Error::Identity("token is not a compact jws"));
    }
    if inner.fingerprints.len() > MAX_FINGERPRINTS_BYTES
        || !is_compact_jws(&inner.fingerprints, true)
    {
        return Err(Error::Identity("fingerprints is not a detached jws"));
    }
    Ok(Assertion {
        token: inner.token,
        fingerprints: inner.fingerprints,
    })
}

/// Whether `text` is `header.payload.signature` in base64url, with an empty payload when
/// `detached` and a present one otherwise. go-nethernet `identityData.Valid` only counts dots.
fn is_compact_jws(text: &str, detached: bool) -> bool {
    let mut parts = text.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let url = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    !header.is_empty()
        && !signature.is_empty()
        && payload.is_empty() == detached
        && url(header)
        && url(payload)
        && url(signature)
}

/// Whether `b` is an ICE character (RFC 8839 `ice-char`).
fn is_ice_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

/// The number in `text` when it is decimal digits only (no sign) and fits `T`.
fn decimal<T: FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Writes the answer with CRLF line ends. The candidate foundation is libwebrtc's classic one,
/// the decimal CRC-32 of `"local" + ip + "udp"`: stable per address, the shape the official server writes.
pub(crate) fn write_answer(answer: &Answer<'_>) -> String {
    let ip = answer.addr.ip().to_canonical();
    let family = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let ip = ip.to_string();
    let port = answer.addr.port();
    let foundation = crc32(&[b"local", ip.as_bytes(), b"udp"]);
    format!(
        "v=0\r\n\
         o=- {session_id} 2 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0\r\n\
         a=extmap-allow-mixed\r\n\
         a=msid-semantic: WMS\r\n\
         a=identity:{identity}\r\n\
         m=application {port} UDP/DTLS/SCTP webrtc-datachannel\r\n\
         c=IN {family} {ip}\r\n\
         a=candidate:{foundation} 1 udp {HOST_PRIORITY} {ip} {port} typ host generation 0 network-id 1\r\n\
         a=ice-ufrag:{ice_ufrag}\r\n\
         a=ice-pwd:{ice_pwd}\r\n\
         a=ice-options:trickle\r\n\
         a=fingerprint:sha-256 {fingerprint}\r\n\
         a=setup:active\r\n\
         a=mid:0\r\n\
         a=sctp-port:{SCTP_PORT}\r\n\
         a=max-message-size:{MAX_MESSAGE_BYTES}\r\n",
        session_id = answer.session_id,
        identity = answer.identity,
        ice_ufrag = answer.ice_ufrag,
        ice_pwd = answer.ice_pwd,
        fingerprint = answer.fingerprint,
    )
}

/// A fingerprint as the official server writes it: uppercase hex bytes separated by colons.
pub(crate) fn fingerprint_text(bytes: &[u8; FINGERPRINT_BYTES]) -> String {
    let mut out = String::with_capacity(FINGERPRINT_BYTES * 3 - 1);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push(hex_digit(b >> 4));
        out.push(hex_digit(b & 0x0f));
    }
    out
}

/// The uppercase hex digit of `nibble`.
#[expect(clippy::expect_used, reason = "invariant: the callers pass a nibble")]
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16)
        .expect("a nibble is below 16")
        .to_ascii_uppercase()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::protocol::{self, fuzz};

    /// A captured go-nethernet offer (offline, self-signed ES384 token). The
    /// candidate's `ufrag` extension differs from the capture; extensions are ignored.
    pub(crate) const OFFER: &str = "v=0
o=- 2327969700533983999 2 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0
a=extmap-allow-mixed
a=msid-semantic: WMS
a=identity:eyJhc3NlcnRpb24iOiJ7XCJmaW5nZXJwcmludHNcIjpcImV5SmhiR2NpT2lKRlV6TTROQ0o5Li55aENJZXFRLU5VelBNQWhtcmN1UzJiRl9LTkJOdFdEY05DcjZ3S1ZvMWd5WHB2WGlkbzhsaHBiZUdIVnpFb1lvNW5TelVvdHB5dU92X3BHVXVEZ2Qyb3Nrek5DeEVIazJ5cXg2aEt1M2hrd1VEQkJoem5BTHM1RWtUaC1UdXhnWVwiLFwidG9rZW5cIjpcImV5SmhiR2NpT2lKRlV6TTROQ0o5LmV5SmpjR3NpT2lKTlNGbDNSVUZaU0V0dldrbDZhakJEUVZGWlJrczBSVVZCUTBsRVdXZEJSVWxDVFhOMldXWnJVbTUxYmpabVN6WlFiRlp3Y0hkYVVHZFZNbGhVV25rNFRHMVJRM0p5U2xremJ6SjJPVUpZYkVoWk4zWnllak5KVUdVMVN6bDFSRkJ6UlVJdlJXdGpOMUZrTm01bmJUUlFRVXBUTm5wNGRuSkthMGhXVlVjM2JFWkRNM0Z5VWpSTE5sUjVlWE5rVW1wd1l6SlhWVEJXV2l0a1dtZFFaVUkwSWl3aVpYaHdJam94Tnprd09UazNNRFU1TENKcFlYUWlPakUzT1RBNU9UTTBOVGw5LnNqcDMtSlIxbW5HVjl6ZUlzMURVN2p1RElsVWRXU0ZDcmY5Tm5VdHZoN2ZqY25qT2ZQTVBhdmNOSjRXTGJMUGNuckhoT1VvUUthcXROTDdiZzVjak9acFh1cVdOSGlCMy1tU2VuVldXUTU4UG1UZllzVmxUdENLWHBQT3d6bnJPXCJ9IiwiaWRwIjp7ImRvbWFpbiI6Imh0dHBzOi8vZXhhbXBsZS5pbnZhbGlkLyIsInByb3RvY29sIjoiZGVmYXVsdCJ9fQ==
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=candidate:candidate:3775397255 1 udp 2130706431 172.25.96.1 63448 typ host generation 0 ufrag skFedTtcOXfRrcUS network-id 0 network-cost 0
a=ice-ufrag:skFedTtcOXfRrcUS
a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoeM
a=ice-options:trickle
a=fingerprint:sha-256 8a:74:fb:69:94:62:1e:0c:53:90:18:b6:dc:d4:3c:fc:75:64:24:e1:42:f3:95:55:fb:f9:52:2d:e5:ee:63:f2
a=setup:actpass
a=mid:0
a=sctp-port:5000
a=max-message-size:262144
";

    fn crlf(text: &str) -> String {
        text.replace('\n', "\r\n")
    }

    #[test]
    fn the_go_nethernet_offer_parses() {
        for text in [OFFER.to_owned(), crlf(OFFER), OFFER.trim_end().to_owned()] {
            let offer = parse_offer(&text).unwrap();
            assert_eq!(offer.ice_ufrag, "skFedTtcOXfRrcUS");
            assert_eq!(offer.ice_pwd, "iDzxHsFcUUTBszTHPTqncZCykyXAVoeM");
            assert_eq!(offer.fingerprint.algorithm, "sha-256");
            assert!(offer.fingerprint.digest.starts_with("8a:74:fb:69:"));
            assert_eq!(offer.fingerprint.bytes[..3], [0x8a, 0x74, 0xfb]);
            assert_eq!(offer.fingerprint.bytes[31], 0xf2);
            assert_eq!(
                fingerprint_text(&offer.fingerprint.bytes),
                offer.fingerprint.digest.to_ascii_uppercase()
            );
            assert_eq!(offer.max_message_bytes, 262_144);
            assert_eq!(
                offer.candidates,
                ["172.25.96.1:63448".parse::<SocketAddr>().unwrap()]
            );
            let identity = offer.identity.unwrap();
            assert!(
                identity
                    .token
                    .starts_with("eyJhbGciOiJFUzM4NCJ9.eyJjcGsiOiJNSFl3")
            );
            assert!(identity.fingerprints.starts_with("eyJhbGciOiJFUzM4NCJ9.."));
        }
    }

    /// The candidate line of [`OFFER`].
    const CANDIDATE: &str = "a=candidate:candidate:3775397255 1 udp 2130706431 172.25.96.1 63448 typ host generation 0 ufrag skFedTtcOXfRrcUS network-id 0 network-cost 0";

    /// [`OFFER`] with one line replaced (the line must be there).
    fn offer_with(from: &str, to: &str) -> String {
        assert!(OFFER.contains(from), "{from}");
        OFFER.replacen(from, to, 1)
    }

    /// The `a=identity` line of [`OFFER`].
    pub(crate) fn identity_line() -> &'static str {
        OFFER
            .lines()
            .find(|line| line.starts_with("a=identity:"))
            .unwrap()
    }

    #[test]
    fn accepted_variants_parse() {
        // Moves a media-level line to the session level.
        let at_session = |line: &str| {
            offer_with(&format!("{line}\n"), "").replacen(
                "a=group:BUNDLE 0\n",
                &format!("a=group:BUNDLE 0\n{line}\n"),
                1,
            )
        };
        let fingerprint_line = OFFER
            .lines()
            .find(|l| l.starts_with("a=fingerprint"))
            .unwrap();
        let cases = [
            // The candidate prefix once, as the standard writes it.
            offer_with("a=candidate:candidate:", "a=candidate:"),
            offer_with("sha-256", "SHA-256"),
            offer_with("8a:74:fb", "8A:74:FB"),
            offer_with("a=setup:actpass", "a=setup:passive"),
            offer_with("a=sctp-port:5000\n", ""),
            offer_with("a=max-message-size:262144", "a=max-message-size:1024"),
            offer_with("a=mid:0", "a=mid:0\na=unknown-thing:1 2 3\nb=AS:30\na=flag"),
            at_session(fingerprint_line),
            at_session("a=ice-ufrag:skFedTtcOXfRrcUS"),
            at_session(CANDIDATE),
            offer_with("a=ice-ufrag:skFedTtcOXfRrcUS", "a=ice-ufrag:a+/b"),
            offer_with("s=-\n", "s=-\n\n\r\n"),
        ];
        for text in cases {
            let offer = parse_offer(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
            assert_eq!(offer.fingerprint.bytes[0], 0x8a);
        }
        // An offer without an identity parses; online mode rejects it later.
        let without = offer_with(&format!("{}\n", identity_line()), "");
        assert!(parse_offer(&without).unwrap().identity.is_none());
        // Unusable candidates are skipped, IPv6 literals kept.
        let candidates = [
            "a=candidate:1 1 tcp 1 10.0.0.1 9 typ host tcptype active",
            "a=candidate:2 1 udp 1 abcd.local 5000 typ host",
            "a=candidate:3 2 udp 1 10.0.0.1 5000 typ host",
            "a=candidate:4 1 UDP 7 2001:db8::1 5000 typ srflx raddr 0.0.0.0 rport 0",
        ];
        let text = offer_with(
            "a=ice-ufrag",
            &format!("{}\na=ice-ufrag", candidates.join("\n")),
        );
        let offer = parse_offer(&text).unwrap();
        assert_eq!(offer.candidates.len(), 2);
        assert_eq!(
            offer.candidates[1],
            "[2001:db8::1]:5000".parse::<SocketAddr>().unwrap()
        );
    }

    type Check = fn(&Error) -> bool;

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one table lists every rejected rule"
    )]
    fn each_rule_is_rejected() {
        let candidate = "a=candidate:1 1 udp 1 10.0.0.1 5000 typ host\n";
        let identity = identity_line();
        let cases: Vec<(String, Check)> = vec![
            (
                format!("{OFFER}a=x:{}\n", "y".repeat(MAX_OFFER_BYTES)),
                |e| matches!(e, Error::TooLarge),
            ),
            (format!("{OFFER}{}", "a=x\n".repeat(MAX_LINES)), |e| {
                matches!(e, Error::TooManyLines)
            }),
            (
                format!("{OFFER}{}", candidate.repeat(MAX_CANDIDATES)),
                |e| matches!(e, Error::TooManyCandidates),
            ),
            (String::new(), |e| matches!(e, Error::Malformed(_))),
            (offer_with("v=0", "v=1"), |e| {
                matches!(e, Error::Malformed(_))
            }),
            (offer_with("s=-", "s=-\u{1}"), |e| {
                matches!(e, Error::Malformed(_))
            }),
            (offer_with("s=-", "s-"), |e| {
                matches!(e, Error::Malformed(_))
            }),
            (offer_with("s=-", "S=-"), |e| {
                matches!(e, Error::Malformed(_))
            }),
            (
                offer_with("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\n", ""),
                |e| matches!(e, Error::Media(_)),
            ),
            (
                format!("{OFFER}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\n"),
                |e| matches!(e, Error::Media(_)),
            ),
            (offer_with("m=application", "m=audio"), |e| {
                matches!(e, Error::Media(_))
            }),
            (offer_with("UDP/DTLS/SCTP", "DTLS/SCTP"), |e| {
                matches!(e, Error::Media(_))
            }),
            (
                offer_with("webrtc-datachannel", "webrtc-datachannel 5000"),
                |e| matches!(e, Error::Media(_)),
            ),
            (offer_with("application 9", "application 70000"), |e| {
                matches!(e, Error::Media(_))
            }),
            (
                offer_with("a=ice-ufrag:skFedTtcOXfRrcUS", "a=ice-ufrag:abc"),
                |e| matches!(e, Error::Invalid("ice-ufrag")),
            ),
            (
                offer_with(
                    "a=ice-ufrag:skFedTtcOXfRrcUS",
                    &format!("a=ice-ufrag:{}", "a".repeat(257)),
                ),
                |e| matches!(e, Error::Invalid("ice-ufrag")),
            ),
            (
                offer_with(
                    "a=ice-ufrag:skFedTtcOXfRrcUS",
                    "a=ice-ufrag:skFed-tcOXfRrcUS",
                ),
                |e| matches!(e, Error::Invalid("ice-ufrag")),
            ),
            (offer_with("a=ice-ufrag:skFedTtcOXfRrcUS\n", ""), |e| {
                matches!(e, Error::Missing("ice-ufrag"))
            }),
            (
                offer_with(
                    "a=ice-ufrag:skFedTtcOXfRrcUS",
                    "a=ice-ufrag:skFedTtcOXfRrcUS\na=ice-ufrag:skFedTtcOXfRrcUS",
                ),
                |e| matches!(e, Error::Duplicate("ice-ufrag")),
            ),
            (
                offer_with(
                    "a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoeM",
                    "a=ice-pwd:iDzxHsFcUUTBszTHPTqnc",
                ),
                |e| matches!(e, Error::Invalid("ice-pwd")),
            ),
            (
                offer_with(
                    "a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoeM",
                    &format!("a=ice-pwd:{}", "a".repeat(257)),
                ),
                |e| matches!(e, Error::Invalid("ice-pwd")),
            ),
            (
                offer_with(
                    "a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoeM",
                    "a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoe=",
                ),
                |e| matches!(e, Error::Invalid("ice-pwd")),
            ),
            (
                offer_with("a=ice-pwd:iDzxHsFcUUTBszTHPTqncZCykyXAVoeM\n", ""),
                |e| matches!(e, Error::Missing("ice-pwd")),
            ),
            (offer_with("sha-256", "sha-1"), |e| {
                matches!(e, Error::Invalid("fingerprint algorithm"))
            }),
            (offer_with(":63:f2", ":63"), |e| {
                matches!(e, Error::Invalid("fingerprint"))
            }),
            (offer_with(":63:f2", ":63:f2:00"), |e| {
                matches!(e, Error::Invalid("fingerprint"))
            }),
            (offer_with(":63:f2", ":63:g2"), |e| {
                matches!(e, Error::Invalid("fingerprint"))
            }),
            (offer_with("8a:74:", "8a-74:"), |e| {
                matches!(e, Error::Invalid("fingerprint"))
            }),
            (offer_with("sha-256 ", "sha-256  "), |e| {
                matches!(e, Error::Invalid("fingerprint"))
            }),
            (
                offer_with(
                    "a=setup:actpass",
                    "a=fingerprint:sha-256 8a:74:fb:69:94:62:1e:0c:53:90:18:b6:dc:d4:3c:fc:75:64:24:e1:42:f3:95:55:fb:f9:52:2d:e5:ee:63:f2\na=setup:actpass",
                ),
                |e| matches!(e, Error::Duplicate("fingerprint")),
            ),
            (offer_with("a=fingerprint:", "a=x-fingerprint:"), |e| {
                matches!(e, Error::Missing("fingerprint"))
            }),
            (offer_with("a=setup:actpass", "a=setup:active"), |e| {
                matches!(e, Error::Invalid("setup"))
            }),
            (offer_with("a=setup:actpass", "a=setup:holdconn"), |e| {
                matches!(e, Error::Invalid("setup"))
            }),
            (offer_with("a=setup:actpass", "a=setup"), |e| {
                matches!(e, Error::Invalid("setup"))
            }),
            (offer_with("a=setup:actpass\n", ""), |e| {
                matches!(e, Error::Missing("setup"))
            }),
            (offer_with("a=max-message-size:262144\n", ""), |e| {
                matches!(e, Error::Missing("max-message-size"))
            }),
            (
                offer_with("a=max-message-size:262144", "a=max-message-size:1023"),
                |e| matches!(e, Error::Invalid("max-message-size")),
            ),
            (
                offer_with("a=max-message-size:262144", "a=max-message-size:+262144"),
                |e| matches!(e, Error::Invalid("max-message-size")),
            ),
            (
                offer_with("a=max-message-size:262144", "a=max-message-size:4294967296"),
                |e| matches!(e, Error::Invalid("max-message-size")),
            ),
            (
                offer_with("a=max-message-size:262144", "a=max-message-size:"),
                |e| matches!(e, Error::Invalid("max-message-size")),
            ),
            (offer_with("a=sctp-port:5000", "a=sctp-port:5001"), |e| {
                matches!(e, Error::Invalid("sctp-port"))
            }),
            (
                offer_with("a=sctp-port:5000", "a=sctp-port:5000\na=sctp-port:5000"),
                |e| matches!(e, Error::Duplicate("sctp-port")),
            ),
            (
                offer_with(" typ host generation", " typx host generation"),
                |e| matches!(e, Error::Invalid("candidate")),
            ),
            (offer_with("172.25.96.1 63448", "172.25.96.1 70000"), |e| {
                matches!(e, Error::Invalid("candidate"))
            }),
            (offer_with("2130706431", "-1"), |e| {
                matches!(e, Error::Invalid("candidate"))
            }),
            (offer_with(" 1 udp", "  1 udp"), |e| {
                matches!(e, Error::Invalid("candidate"))
            }),
            (
                offer_with(
                    CANDIDATE,
                    "a=candidate:3775397255 1 udp 2130706431 172.25.96.1 63448 typ",
                ),
                |e| matches!(e, Error::Invalid("candidate")),
            ),
            (
                offer_with(CANDIDATE, "a=candidate:1 1 udp 1 10.0.0.1 1 type host"),
                |e| matches!(e, Error::Invalid("candidate")),
            ),
            (
                offer_with(
                    CANDIDATE,
                    &format!("a=candidate:{} 1 udp 1 10.0.0.1 1 typ host", "f".repeat(33)),
                ),
                |e| matches!(e, Error::Invalid("candidate")),
            ),
            (offer_with(CANDIDATE, "a=candidate:"), |e| {
                matches!(e, Error::Invalid("candidate"))
            }),
            (offer_with(CANDIDATE, "a=candidate"), |e| {
                matches!(e, Error::Invalid("candidate"))
            }),
            (offer_with("a=mid:0", "a=mid:0\na=identity:e30="), |e| {
                matches!(e, Error::Identity("not at the session level"))
            }),
            (offer_with("a=identity:eyJ", "a=identity:!eyJ"), |e| {
                matches!(e, Error::Identity(_))
            }),
            (offer_with("fQ==\n", "fQ\n"), |e| {
                matches!(e, Error::Identity(_))
            }),
            (
                offer_with(
                    "a=extmap-allow-mixed",
                    &format!("a=extmap-allow-mixed\n{identity}"),
                ),
                |e| matches!(e, Error::Duplicate("identity")),
            ),
        ];
        for (text, expected) in cases {
            let err = parse_offer(&text).unwrap_err();
            assert!(expected(&err), "{err:?} for\n{text}");
        }
    }

    /// The offer with its identity JSON replaced by `json`.
    fn offer_with_identity(json: impl AsRef<[u8]>) -> String {
        offer_with(
            identity_line(),
            &format!("a=identity:{}", STD.encode(json.as_ref())),
        )
    }

    fn identity_json(fingerprints: &str, token: &str, domain: &str, protocol: &str) -> String {
        let assertion =
            serde_json::json!({"fingerprints": fingerprints, "token": token}).to_string();
        serde_json::json!({"assertion": assertion, "idp": {"domain": domain, "protocol": protocol}})
            .to_string()
    }

    #[test]
    fn identity_rules_are_checked() {
        let token = "eyJh.eyJj.c2ln";
        let detached = "eyJh..c2ln";
        let ok = identity_json(detached, token, "self", "default");
        let identity = parse_offer(&offer_with_identity(&ok))
            .unwrap()
            .identity
            .unwrap();
        assert_eq!(
            identity,
            Assertion {
                token: token.to_owned(),
                fingerprints: detached.to_owned(),
            }
        );
        let long_token = format!("eyJh.{}.c2ln", "a".repeat(MAX_TOKEN_BYTES));
        let long_domain = "d".repeat(MAX_DOMAIN_BYTES + 1);
        for json in [
            identity_json(detached, token, "self", "other"),
            identity_json(detached, token, "", "default"),
            identity_json(detached, token, &long_domain, "default"),
            identity_json(detached, token, "se\u{7}lf", "default"),
            identity_json(detached, "eyJh.eyJj", "self", "default"),
            identity_json(detached, "eyJh..c2ln", "self", "default"),
            identity_json(detached, "eyJh.eyJj.c2ln.x", "self", "default"),
            identity_json(detached, "eyJh.ey+j.c2ln", "self", "default"),
            identity_json(detached, &long_token, "self", "default"),
            identity_json(token, token, "self", "default"),
            identity_json("..c2ln", token, "self", "default"),
            identity_json("eyJh..", token, "self", "default"),
            r#"{"assertion":"{}","idp":{"domain":"self","protocol":"default"}}"#.to_owned(),
            r#"{"assertion":{},"idp":{"domain":"self","protocol":"default"}}"#.to_owned(),
            r#"{"assertion":"[]"}"#.to_owned(),
            "[]".to_owned(),
        ] {
            let err = parse_offer(&offer_with_identity(&json)).unwrap_err();
            assert!(
                matches!(err, Error::Identity(_) | Error::IdentityJson(_)),
                "{err:?} for {json}"
            );
        }
        let huge = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_IDENTITY_JSON_BYTES));
        assert!(matches!(
            parse_offer(&offer_with_identity(&huge)),
            Err(Error::Identity(_))
        ));
    }

    /// The token cap is the longest token the Login check accepts, so the offer refuses no
    /// token the login would take, and none longer.
    #[test]
    fn the_identity_token_cap_is_the_login_token_cap() {
        let detached = "eyJh..c2ln";
        let token = |len: usize| format!("eyJh.{}.c2ln", "a".repeat(len - 10));
        let longest = max_jws_text(MAX_TOKEN_PAYLOAD_BYTES);
        let json = identity_json(detached, &token(longest), "self", "default");
        let identity = parse_offer(&offer_with_identity(&json)).unwrap().identity;
        assert_eq!(identity.unwrap().token.len(), longest);
        let json = identity_json(detached, &token(longest + 1), "self", "default");
        assert!(matches!(
            parse_offer(&offer_with_identity(&json)),
            Err(Error::Identity(_))
        ));
    }

    fn answer() -> Answer<'static> {
        Answer {
            session_id: 1_234_567_890_123,
            identity: "SUQ=",
            addr: "192.0.2.7:19133".parse().unwrap(),
            ice_ufrag: "Ab+/",
            ice_pwd: "abcdefghijklmnopqrstuvwx",
            fingerprint: "7D:C0:D8:7A:F6:1F:11:CF:20:96:15:45:82:89:DC:2E:E9:12:99:44:CD:60:C7:25:D5:7C:85:7E:19:AE:41:D9",
        }
    }

    #[test]
    fn answers_follow_the_official_layout() {
        let expected = "v=0\r
o=- 1234567890123 2 IN IP4 127.0.0.1\r
s=-\r
t=0 0\r
a=group:BUNDLE 0\r
a=extmap-allow-mixed\r
a=msid-semantic: WMS\r
a=identity:SUQ=\r
m=application 19133 UDP/DTLS/SCTP webrtc-datachannel\r
c=IN IP4 192.0.2.7\r
a=candidate:4154560954 1 udp 2122260223 192.0.2.7 19133 typ host generation 0 network-id 1\r
a=ice-ufrag:Ab+/\r
a=ice-pwd:abcdefghijklmnopqrstuvwx\r
a=ice-options:trickle\r
a=fingerprint:sha-256 7D:C0:D8:7A:F6:1F:11:CF:20:96:15:45:82:89:DC:2E:E9:12:99:44:CD:60:C7:25:D5:7C:85:7E:19:AE:41:D9\r
a=setup:active\r
a=mid:0\r
a=sctp-port:5000\r
a=max-message-size:262144\r
";
        assert_eq!(write_answer(&answer()), expected);
        let v6 = Answer {
            addr: "[2001:db8::7]:19133".parse().unwrap(),
            ..answer()
        };
        let text = write_answer(&v6);
        assert!(text.contains("\r\nc=IN IP6 2001:db8::7\r\n"));
        assert!(text.contains("a=candidate:391553795 1 udp 2122260223 2001:db8::7 19133 typ host"));
        let mapped = Answer {
            addr: "[::ffff:192.0.2.7]:19133".parse().unwrap(),
            ..answer()
        };
        assert_eq!(write_answer(&mapped), expected);
    }

    #[test]
    fn written_answers_parse_back_except_for_the_active_role() {
        let text = write_answer(&answer());
        // "SUQ=" is not an identity assertion; with a real one the answer parses (identity.rs).
        let err = parse_offer(&text).unwrap_err();
        assert!(matches!(err, Error::IdentityJson(_)), "{err:?}");
        let text = text.replace("a=identity:SUQ=\r\n", "");
        assert!(matches!(parse_offer(&text), Err(Error::Invalid("setup"))));
        let offer = parse_offer(&text.replace("a=setup:active", "a=setup:actpass")).unwrap();
        assert_eq!(offer.ice_ufrag, "Ab+/");
        assert_eq!(offer.ice_pwd, "abcdefghijklmnopqrstuvwx");
        assert_eq!(offer.fingerprint.digest, answer().fingerprint);
        assert_eq!(
            fingerprint_text(&offer.fingerprint.bytes),
            answer().fingerprint
        );
        assert_eq!(offer.max_message_bytes, MAX_MESSAGE_BYTES);
        assert_eq!(offer.candidates, [answer().addr]);
    }

    #[test]
    fn fingerprints_match_references() {
        let mut bytes = [0_u8; FINGERPRINT_BYTES];
        bytes[0] = 0x0a;
        bytes[31] = 0xf0;
        let text = fingerprint_text(&bytes);
        assert_eq!(text.len(), 95);
        assert!(text.starts_with("0A:00:"));
        assert!(text.ends_with(":00:F0"));
        assert_eq!(parse_digest(&text), Some(bytes));
    }

    #[test]
    fn mutated_offers_never_panic() {
        let check = |data: &[u8]| -> protocol::Result<()> {
            let text = std::str::from_utf8(data).map_err(|_| protocol::Error::InvalidUtf8)?;
            parse_offer(text)
                .map(|_| ())
                .map_err(|_| protocol::Error::Size)
        };
        assert!(fuzz(OFFER.as_bytes(), 10_000, check) > 0);
        // Mutations inside the identity JSON reach the JSON and JWS shape checks.
        let json = identity_json("eyJh..c2ln", "eyJh.eyJj.c2ln", "self", "default");
        let passed = fuzz(json.as_bytes(), 3000, |data| {
            check(offer_with_identity(data).as_bytes())
        });
        assert!(passed > 0);
    }
}
