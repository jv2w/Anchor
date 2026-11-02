//! One NetherNet peer: a str0m WebRTC session without sockets or clocks. ICE controlled, DTLS
//! client (active), SCTP client; the remote opens `ReliableDataChannel` and
//! `UnreliableDataChannel`, and reliable messages travel as segments (`message`).
//!
//! Every call into str0m for a peer, from building its session on, runs under `catch_unwind`:
//! one driver task owns all peers, and a panic in str0m or its DTLS and SCTP crates must close
//! only the peer that caused it. Only the plain values str0m returns are read outside. A peer
//! that panicked is never touched again. Dropping a peer needs no guard: str0m, str0m-proto,
//! str0m-aws-lc-rs, dimpl and sctp-proto define no `Drop`, so dropping runs none of their code.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::config::{CryptoProvider, DtlsCert, DtlsVersion};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceCreds, Input, Rtc, RtcConfig, RtcError};
use tracing::{debug, trace};

use super::message::{self, Reassembler};
use super::sdp;
use super::{STUN_HEADER_LEN, STUN_MAGIC_COOKIE};
use crate::raknet::MAX_UNSENT_BYTES;

/// The ICE `ice-char` set (RFC 8445 15.4): letters, digits, `+` and `/`. 64 characters, so one
/// random byte masked to 6 bits picks one uniformly.
const ICE_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// The silence after which an open peer is dead. pion sends an ICE keepalive every 2 s and str0m
/// checks the nominated pair at least every 3 s (its maximum STUN RTO), so this is three missed
/// rounds. str0m alone never gives up: its SCTP retransmits forever and ICE only reports
/// `Disconnected`.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest datagram passed to str0m, in bytes: an Ethernet MTU. Honest peers stay below it
/// (pion sends SCTP at 1200 bytes and DTLS flights at its MTU); str0m itself reads up to 2000.
pub(crate) const MAX_DATAGRAM: usize = 1500;
/// The time from the peer's creation until both channels are open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// The memory one queued message costs besides its bytes, in bytes, charged against
/// `MAX_UNSENT_BYTES` so that many tiny messages cannot exceed the cap's memory.
const PENDING_COST_BYTES: usize = size_of::<Pending>();
/// The length of the local ICE password, as BDS sends.
const PWD_LEN: usize = 24;
/// The label and parameters of the reliable channel (go-nethernet `MessageReliabilityReliable`).
const RELIABLE_LABEL: &str = "ReliableDataChannel";
/// How often str0m reports the round trip time of its latest ICE consent check (RFC 7675), the
/// connection's ping.
const RTT_INTERVAL: Duration = Duration::from_secs(2);
/// The bytes str0m holds for one peer until acknowledged: four of the largest messages, so a
/// segment always fits, and about 1 MiB per round trip, the receive window go-nethernet's pion
/// offers. str0m's 128 KiB default can never take a 262,144-byte message.
const SCTP_SEND_BUFFER_BYTES: usize = 4 * SEND_MESSAGE_LIMIT_BYTES;
/// The largest data channel message sent, in bytes: BDS's `a=max-message-size`. BDS sends
/// segments of this minus the header byte, 262,143 bytes (P6); a remote that announces less gets
/// smaller ones.
const SEND_MESSAGE_LIMIT_BYTES: usize = 256 * 1024;
/// The type of a STUN binding request.
const STUN_BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
/// The type of the STUN USERNAME attribute.
const STUN_USERNAME: u16 = 0x0006;
/// The length of the local ICE username fragment, as BDS sends.
const UFRAG_LEN: usize = 4;
/// The label of the unreliable channel: unordered with no retransmits.
const UNRELIABLE_LABEL: &str = "UnreliableDataChannel";

/// A NetherNet peer failure. Each one closes the peer.
#[derive(Debug)]
pub(crate) enum Error {
    /// The peer was closed locally, or is not open: still connecting, closing or closed.
    Closed,
    /// The remote closed the session or one of its channels.
    Remote,
    /// The channels did not open in time, or the remote fell silent.
    Timeout(&'static str),
    /// The remote opened a channel NetherNet does not use.
    Channel(&'static str),
    /// A reliable message broke the segment rules.
    Message(message::Error),
    /// A limit was exceeded.
    Limit(&'static str),
    /// The local address is not usable as an ICE candidate.
    Address(str0m::error::IceError),
    /// ICE, DTLS or SCTP failed, including a remote certificate that does not match the offer.
    Webrtc(RtcError),
    /// str0m panicked; the peer is dropped without touching it again.
    Panicked,
    /// The DTLS certificate could not be generated.
    Certificate,
    /// Randomness for the ICE credentials failed.
    Crypto(crate::crypto::Error),
}

/// A peer result.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("nethernet peer closed"),
            Self::Remote => f.write_str("nethernet peer closed by the remote"),
            Self::Timeout(what) => write!(f, "nethernet peer timed out: {what}"),
            Self::Channel(what) => write!(f, "invalid nethernet data channel: {what}"),
            Self::Message(_) => f.write_str("invalid nethernet message"),
            Self::Limit(what) => write!(f, "nethernet limit exceeded: {what}"),
            Self::Address(_) => f.write_str("unusable local nethernet address"),
            Self::Webrtc(_) => f.write_str("webrtc failure"),
            Self::Panicked => f.write_str("webrtc panicked"),
            Self::Certificate => f.write_str("dtls certificate generation failed"),
            Self::Crypto(_) => f.write_str("ice credential generation failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Message(e) => Some(e),
            Self::Address(e) => Some(e),
            Self::Webrtc(e) => Some(e),
            Self::Crypto(e) => Some(e),
            _ => None,
        }
    }
}

/// The DTLS certificate and crypto provider all peers share: made once per server start, so no
/// peer pays for key generation and str0m never reads its process-global provider.
pub(crate) struct Certificate {
    provider: Arc<CryptoProvider>,
    dtls: DtlsCert,
    fingerprint: [u8; 32],
}

impl Certificate {
    /// A new self-signed ECDSA P-256 certificate (str0m's aws-lc-rs provider, through rcgen).
    pub(crate) fn generate() -> Result<Self> {
        let provider = str0m::crypto::from_feature_flags();
        let dtls = provider
            .dtls_provider
            .generate_certificate()
            .ok_or(Error::Certificate)?;
        let fingerprint = crate::crypto::sha256(&[&dtls.certificate]);
        Ok(Self {
            provider: Arc::new(provider),
            dtls,
            fingerprint,
        })
    }

    /// SHA-256 of the certificate's DER: the `a=fingerprint` of the answer.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// What the offer says about the remote.
#[derive(Debug, Clone)]
pub(crate) struct Remote {
    pub(crate) ice_ufrag: String,
    pub(crate) ice_pwd: String,
    /// SHA-256 fingerprint of the remote DTLS certificate; DTLS fails if the certificate differs.
    pub(crate) fingerprint: [u8; 32],
    /// The remote's host candidates. Addresses ICE cannot use (unspecified, multicast, IPv4
    /// link-local) are skipped.
    pub(crate) candidates: Vec<SocketAddr>,
    /// The remote's `a=max-message-size`.
    pub(crate) max_message_size: u32,
}

/// What the answer says about this peer.
#[derive(Debug, Clone)]
pub(crate) struct Local {
    pub(crate) ice_ufrag: String,
    pub(crate) ice_pwd: String,
    pub(crate) fingerprint: [u8; 32],
}

/// The next thing a peer needs from its driver or has for it.
#[derive(Debug)]
pub(crate) enum Output {
    /// Send `data` to `to` from the NetherNet socket of the same address family.
    Transmit { to: SocketAddr, data: Vec<u8> },
    /// Both channels are open; messages may be sent.
    Open,
    /// A whole message from the reliable channel.
    Message(Vec<u8>),
    /// The round trip time of the latest ICE consent check on the selected pair.
    Rtt(Duration),
    /// Nothing to do before this time: call `poll` again then, or after the next `receive` or
    /// `send`.
    Timeout(Instant),
    /// The peer is closed for this reason; every later poll returns `Closed(Error::Closed)`.
    Closed(Error),
}

#[derive(Debug)]
enum State {
    Connecting,
    Open,
    /// str0m is sending its close output; the reason follows it.
    Closing(Error),
    /// Closed without close output to send; the next poll reports the reason.
    Failed(Error),
    Closed,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Reliable,
    Unreliable,
}

/// A message whose segments str0m did not all take yet. The `sent` bytes of `rest` end at a
/// segment boundary, so splitting the rest again by the same payload size gives the same
/// segments.
#[derive(Debug)]
struct Pending {
    rest: Vec<u8>,
    /// The bytes of `rest` that str0m took.
    sent: usize,
    /// The header of the next segment.
    header: u8,
}

/// One NetherNet peer. The driver feeds it datagrams and time and sends what it returns; it
/// calls [`Peer::poll`] after each `receive`, `send` and timeout until it returns `Timeout` or
/// `Closed`.
pub(crate) struct Peer {
    rtc: Rtc,
    local: Local,
    local_addr: SocketAddr,
    state: State,
    reliable: Option<ChannelId>,
    unreliable: Option<ChannelId>,
    reassembler: Reassembler,
    /// Messages that came on the reliable channel before the unreliable one opened, reported
    /// after `Open`. go-nethernet's dialer sends as soon as it has asked for both channels, so
    /// data can overtake the other channel's opening; the messages wait instead of failing.
    early: VecDeque<Vec<u8>>,
    /// Bytes in `early`, at most `max_message`: before the open only `RequestNetworkSettings`
    /// is due.
    early_bytes: usize,
    max_message: usize,
    /// Outbound segment payload: the smaller message limit of the two sides, minus the header.
    max_payload: usize,
    queue: VecDeque<Pending>,
    /// Bytes in `queue`, `PENDING_COST_BYTES` included per message.
    queued: usize,
    /// One segment with its header, reused for every write.
    scratch: Vec<u8>,
    /// When str0m wants its next timeout; `None` runs its timers on the next poll.
    due: Option<Instant>,
    open_deadline: Option<Instant>,
    /// When the last datagram str0m accepted arrived.
    last_receive: Instant,
    /// Where the last DTLS datagram str0m accepted came from.
    remote_addr: Option<SocketAddr>,
    /// Messages read and dropped from the unreliable channel.
    unreliable_dropped: u64,
}

impl Peer {
    /// A peer answering `remote`, reachable at `local_addr` (the advertised host candidate), that
    /// reassembles messages of at most `max_message` bytes.
    pub(crate) fn new(
        remote: Remote,
        local_addr: SocketAddr,
        cert: &Certificate,
        max_message: usize,
        now: Instant,
    ) -> Result<Self> {
        let send_limit = usize::try_from(remote.max_message_size)
            .unwrap_or(usize::MAX)
            .min(SEND_MESSAGE_LIMIT_BYTES);
        let max_payload = send_limit.saturating_sub(1);
        if max_payload == 0 {
            return Err(Error::Limit("remote max message size"));
        }
        let local = Local {
            ice_ufrag: ice_string::<UFRAG_LEN>()?,
            ice_pwd: ice_string::<PWD_LEN>()?,
            fingerprint: cert.fingerprint,
        };
        let credentials = IceCreds {
            ufrag: local.ice_ufrag.clone(),
            pass: local.ice_pwd.clone(),
        };
        let offer = str0m_offer(&remote);
        let rtc = catch_unwind(AssertUnwindSafe(move || -> Result<Rtc> {
            let candidate = Candidate::host(local_addr, "udp").map_err(Error::Address)?;
            let mut rtc = RtcConfig::new()
                .set_crypto_provider(Arc::clone(&cert.provider))
                // The default, kept explicit: DTLS 1.2 fixed.
                .set_dtls_version(DtlsVersion::Dtls12)
                .set_sctp_max_buffered_amount(SCTP_SEND_BUFFER_BYTES)
                .set_stats_interval(Some(RTT_INTERVAL))
                .set_dtls_cert(cert.dtls.clone())
                .set_local_ice_credentials(credentials)
                .build(now);
            rtc.add_local_candidate(candidate);
            let offer =
                SdpOffer::from_sdp_string(&offer).map_err(|e| Error::Webrtc(RtcError::Sdp(e)))?;
            // Only str0m reads its answer; the client gets the BDS-shaped one from `sdp`.
            #[expect(
                clippy::disallowed_methods,
                reason = "consistency: the SDP API is str0m's only way to learn the remote \
                          max-message-size; its answer is never sent"
            )]
            let answer = rtc.sdp_api().accept_offer(offer).map_err(Error::Webrtc)?;
            drop(answer);
            for addr in remote.candidates {
                match Candidate::host(addr, "udp") {
                    Ok(candidate) => rtc.add_remote_candidate(candidate),
                    Err(e) => trace!(
                        %addr,
                        error = &e as &(dyn std::error::Error + 'static),
                        "skipped remote nethernet candidate"
                    ),
                }
            }
            Ok(rtc)
        }))
        .map_err(|_| Error::Panicked)??;
        Ok(Self {
            rtc,
            local,
            local_addr,
            state: State::Connecting,
            reliable: None,
            unreliable: None,
            reassembler: Reassembler::new(max_message),
            early: VecDeque::new(),
            early_bytes: 0,
            max_message,
            max_payload,
            queue: VecDeque::new(),
            queued: 0,
            scratch: Vec::with_capacity(send_limit),
            due: None,
            open_deadline: now.checked_add(OPEN_TIMEOUT),
            last_receive: now,
            remote_addr: None,
            unreliable_dropped: 0,
        })
    }

    /// ICE credentials and DTLS fingerprint for the answer.
    pub(crate) fn local(&self) -> &Local {
        &self.local
    }

    /// The client address the channels run on: where the last DTLS datagram str0m accepted
    /// came from. str0m takes those only from an address whose ICE check succeeded, which a
    /// spoofed source cannot pass.
    pub(crate) fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Whether everything sent has been delivered. A str0m panic leaves nothing that could
    /// still be delivered.
    pub(crate) fn is_drained(&mut self) -> bool {
        !matches!(self.undelivered(), Ok(bytes) if bytes > 0)
    }

    /// The bytes sent that are not delivered yet, as `MAX_UNSENT_BYTES` counts them: queued here
    /// or unacknowledged in SCTP (sctp-proto releases buffered bytes on acknowledgement). A peer
    /// that is not open has nothing more to deliver. On a str0m panic the peer closes, and `poll`
    /// reports the reason.
    pub(crate) fn undelivered(&mut self) -> Result<usize> {
        if !matches!(self.state, State::Open) {
            return Ok(0);
        }
        let Some(id) = self.reliable else {
            return Ok(self.queued);
        };
        match buffered_bytes(&mut self.rtc, id) {
            Ok(buffered) => Ok(self.queued.saturating_add(buffered.unwrap_or(0))),
            Err(e) => {
                self.fail(e);
                Err(Error::Closed)
            }
        }
    }

    /// Takes a datagram from `from`. Datagrams that are not STUN or DTLS, too large, or not
    /// accepted by ICE are dropped; a fatal one closes the peer, which `poll` reports.
    ///
    /// Call `poll` before the next `receive`: the DTLS layer holds at most 30 unread datagrams
    /// (dimpl `max_queue_rx`) and fails the session when more arrive unpolled.
    pub(crate) fn receive(&mut self, now: Instant, from: SocketAddr, datagram: &[u8]) {
        if !matches!(self.state, State::Connecting | State::Open) {
            return;
        }
        if datagram.len() > MAX_DATAGRAM || !matches!(datagram.first(), Some(0..=1 | 20..=63)) {
            trace!(%from, len = datagram.len(), "dropped nethernet datagram");
            return;
        }
        // ICE drops requests not addressed to a local candidate; the socket is bound to every
        // interface, so the destination is the advertised candidate by definition.
        let local_addr = self.local_addr;
        let result = guard(&mut self.rtc, |rtc| {
            let Ok(receive) = Receive::new(Protocol::Udp, from, local_addr, datagram) else {
                return Ok(false);
            };
            let input = Input::Receive(now, receive);
            if !rtc.accepts(&input) {
                return Ok(false);
            }
            rtc.handle_input(input).map(|()| true)
        });
        match result {
            Ok(Ok(true)) => {
                self.last_receive = now;
                // STUN (first byte 0 or 1) is accepted from any source that knows the ICE
                // credentials, DTLS only from a checked one.
                if !matches!(datagram.first(), Some(0..=1)) {
                    self.remote_addr = Some(from);
                }
            }
            Ok(Ok(false)) => trace!(%from, "nethernet datagram not accepted"),
            Ok(Err(e)) => self.fail(Error::Webrtc(e)),
            Err(e) => self.fail(e),
        }
    }

    /// Sends `message` on the reliable channel as segments. Segments str0m cannot buffer yet are
    /// queued here. On any error the peer closes, and `poll` reports the reason.
    pub(crate) fn send(&mut self, message: Vec<u8>) -> Result<()> {
        if !matches!(self.state, State::Open) {
            return Err(Error::Closed);
        }
        match self.try_send(message) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.fail(e);
                Err(Error::Closed)
            }
        }
    }

    /// The next output. Call it until it returns `Timeout` or `Closed`.
    pub(crate) fn poll(&mut self, now: Instant) -> Output {
        loop {
            let output = match std::mem::replace(&mut self.state, State::Closed) {
                State::Connecting => {
                    self.state = State::Connecting;
                    self.step(now)
                }
                State::Open if !self.early.is_empty() => {
                    self.state = State::Open;
                    self.early.pop_front().map(|message| {
                        self.early_bytes = self.early_bytes.saturating_sub(message.len());
                        Output::Message(message)
                    })
                }
                State::Open => {
                    self.state = State::Open;
                    self.step(now)
                }
                State::Closing(reason) => self.drain(reason),
                State::Failed(reason) => return Output::Closed(reason),
                State::Closed => return Output::Closed(Error::Closed),
            };
            if let Some(output) = output {
                return output;
            }
        }
    }

    /// Closes the peer: str0m sends its close output through `poll`, which then returns `Closed`.
    pub(crate) fn close(&mut self) {
        if matches!(self.state, State::Connecting | State::Open) {
            self.fail(Error::Closed);
        }
    }

    /// When the peer times out by its own rules, if ever.
    fn deadline(&self) -> Option<Instant> {
        if matches!(self.state, State::Open) {
            self.last_receive.checked_add(IDLE_TIMEOUT)
        } else {
            self.open_deadline
        }
    }

    fn try_send(&mut self, message: Vec<u8>) -> Result<()> {
        let mut header = message::segments(&message, self.max_payload)
            .map_err(Error::Message)?
            .header();
        let id = self.reliable.ok_or(Error::Closed)?;
        let buffered = buffered_bytes(&mut self.rtc, id)?.unwrap_or(0);
        let cost = message.len().saturating_add(PENDING_COST_BYTES);
        if self.queued.saturating_add(buffered).saturating_add(cost) > MAX_UNSENT_BYTES {
            return Err(Error::Limit("unsent bytes"));
        }
        // Behind queued messages this one waits whole; otherwise as much as str0m takes goes
        // now.
        let mut sent = 0;
        if self.queue.is_empty() {
            while sent < message.len() {
                let rest = message.get(sent..).unwrap_or_default();
                let payload = rest.get(..self.max_payload).unwrap_or(rest);
                if !write(&mut self.rtc, id, &mut self.scratch, header, payload)? {
                    break;
                }
                sent = sent.saturating_add(payload.len());
                header = header.saturating_sub(1);
            }
        }
        if sent < message.len() {
            self.queued = self
                .queued
                .saturating_add(message.len().saturating_sub(sent))
                .saturating_add(PENDING_COST_BYTES);
            self.queue.push_back(Pending {
                rest: message,
                sent,
                header,
            });
        }
        Ok(())
    }

    /// Moves queued segments into str0m while it has room.
    fn flush(&mut self) -> Result<()> {
        let Some(id) = self.reliable else {
            return Ok(());
        };
        while let Some(pending) = self.queue.front_mut() {
            let rest = pending.rest.get(pending.sent..).unwrap_or_default();
            let payload = rest.get(..self.max_payload).unwrap_or(rest);
            if !write(
                &mut self.rtc,
                id,
                &mut self.scratch,
                pending.header,
                payload,
            )? {
                return Ok(());
            }
            let len = payload.len();
            pending.sent = pending.sent.saturating_add(len);
            pending.header = pending.header.saturating_sub(1);
            self.queued = self.queued.saturating_sub(len);
            if pending.sent >= pending.rest.len() {
                self.queue.pop_front();
                self.queued = self.queued.saturating_sub(PENDING_COST_BYTES);
            }
        }
        Ok(())
    }

    /// One output while connecting or open, or `None` to look again.
    fn step(&mut self, now: Instant) -> Option<Output> {
        let deadline = self.deadline();
        if deadline.is_some_and(|deadline| now >= deadline) {
            let what = if matches!(self.state, State::Open) {
                "no datagrams"
            } else {
                "channels not open"
            };
            self.fail(Error::Timeout(what));
            return None;
        }
        if self.due.is_none_or(|due| now >= due) {
            self.due = None;
            if let Err(e) = guard_rtc(&mut self.rtc, |rtc| rtc.handle_input(Input::Timeout(now))) {
                self.fail(e);
                return None;
            }
        }
        if matches!(self.state, State::Open)
            && let Err(e) = self.flush()
        {
            self.fail(e);
            return None;
        }
        let output = match guard_rtc(&mut self.rtc, Rtc::poll_output) {
            Ok(output) => output,
            Err(e) => {
                self.fail(e);
                return None;
            }
        };
        match output {
            str0m::Output::Transmit(transmit) => Some(Output::Transmit {
                to: transmit.destination,
                data: transmit.contents.into(),
            }),
            str0m::Output::Timeout(at) => {
                self.due = Some(at);
                Some(Output::Timeout(
                    deadline.map_or(at, |deadline| at.min(deadline)),
                ))
            }
            str0m::Output::Event(event) => self.event(event),
        }
    }

    /// The close drain: str0m's transmits until it is quiet, then the reason.
    fn drain(&mut self, reason: Error) -> Option<Output> {
        match guard_rtc(&mut self.rtc, Rtc::poll_output) {
            Ok(str0m::Output::Transmit(transmit)) => {
                self.state = State::Closing(reason);
                Some(Output::Transmit {
                    to: transmit.destination,
                    data: transmit.contents.into(),
                })
            }
            Ok(str0m::Output::Event(_)) => {
                self.state = State::Closing(reason);
                None
            }
            Ok(str0m::Output::Timeout(_)) | Err(_) => Some(Output::Closed(reason)),
        }
    }

    fn event(&mut self, event: Event) -> Option<Output> {
        match event {
            Event::ChannelOpen(id, _) => self.open_channel(id),
            Event::ChannelData(data) => self.channel_data(data),
            Event::ChannelClose(id) if Some(id) == self.reliable || Some(id) == self.unreliable => {
                self.fail(Error::Remote);
                None
            }
            Event::Closed => {
                self.fail(Error::Remote);
                None
            }
            Event::IceConnectionStateChange(state) => {
                debug!(?state, "nethernet ice state");
                None
            }
            Event::PeerStats(stats) => stats
                .selected_candidate_pair
                .and_then(|pair| pair.current_round_trip_time)
                .map(Output::Rtt),
            _ => None,
        }
    }

    fn open_channel(&mut self, id: ChannelId) -> Option<Output> {
        let kind = match guard(&mut self.rtc, |rtc| {
            rtc.channel(id)
                .and_then(|channel| channel.config().and_then(channel_kind))
        }) {
            Ok(Some(kind)) => kind,
            Ok(None) => {
                self.fail(Error::Channel("unknown label or parameters"));
                return None;
            }
            Err(e) => {
                self.fail(e);
                return None;
            }
        };
        let slot = match kind {
            Kind::Reliable => &mut self.reliable,
            Kind::Unreliable => &mut self.unreliable,
        };
        if slot.is_some() {
            self.fail(Error::Channel("opened twice"));
            return None;
        }
        *slot = Some(id);
        if self.reliable.is_some() && self.unreliable.is_some() {
            self.state = State::Open;
            return Some(Output::Open);
        }
        None
    }

    fn channel_data(&mut self, data: ChannelData) -> Option<Output> {
        if Some(data.id) == self.unreliable {
            self.unreliable_dropped = self.unreliable_dropped.saturating_add(1);
            trace!(
                len = data.data.len(),
                dropped = self.unreliable_dropped,
                "dropped unreliable nethernet message"
            );
            return None;
        }
        if Some(data.id) != self.reliable {
            self.fail(Error::Channel("data on no open channel"));
            return None;
        }
        let message = match self.reassembler.push(data.data) {
            Ok(message) => message?,
            Err(e) => {
                self.fail(Error::Message(e));
                return None;
            }
        };
        if matches!(self.state, State::Open) {
            return Some(Output::Message(message));
        }
        let bytes = self.early_bytes.saturating_add(message.len());
        if bytes > self.max_message {
            self.fail(Error::Limit("data before both channels opened"));
            return None;
        }
        self.early_bytes = bytes;
        self.early.push_back(message);
        None
    }

    /// Starts closing for `reason`. After a panic str0m is not touched again.
    fn fail(&mut self, reason: Error) {
        if !matches!(self.state, State::Connecting | State::Open) {
            return;
        }
        self.queue = VecDeque::new();
        self.queued = 0;
        if matches!(reason, Error::Panicked) {
            self.state = State::Failed(reason);
            return;
        }
        self.state = match guard_rtc(&mut self.rtc, Rtc::close) {
            Ok(()) => State::Closing(reason),
            Err(e) => {
                debug!(
                    error = &e as &(dyn std::error::Error + 'static),
                    "nethernet peer close failed"
                );
                State::Failed(reason)
            }
        };
    }
}

impl fmt::Debug for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Peer")
            .field("local_addr", &self.local_addr)
            .field("state", &self.state)
            .field("queued", &self.queued)
            .finish_non_exhaustive()
    }
}

/// The offer as str0m reads it: the remote's ICE credentials (`sdp` checked their characters),
/// fingerprint and message size, and `setup:passive`. A NetherNet client offers `actpass`, for
/// which str0m would take the DTLS server role; BDS takes the client role, and so does str0m
/// told the remote is passive. str0m then also runs SCTP as the client and ICE as controlled.
/// Candidates are added to str0m directly.
fn str0m_offer(remote: &Remote) -> String {
    format!(
        "v=0\r\no=- 0 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
         m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\n\
         a=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:sha-256 {}\r\na=setup:passive\r\n\
         a=mid:0\r\na=sctp-port:5000\r\na=max-message-size:{}\r\n",
        remote.ice_ufrag,
        remote.ice_pwd,
        sdp::fingerprint_text(&remote.fingerprint),
        remote.max_message_size,
    )
}

/// The server ICE username fragment of a STUN binding request: USERNAME is `<server
/// ufrag>:<client ufrag>`. `None` for anything else. Reads only within `datagram`.
pub(crate) fn stun_server_ufrag(datagram: &[u8]) -> Option<&str> {
    let (header, mut attributes) = datagram.split_at_checked(STUN_HEADER_LEN)?;
    let [t0, t1, l0, l1, c0, c1, c2, c3] = <[u8; 8]>::try_from(header.get(..8)?).ok()?;
    if [t0, t1] != STUN_BINDING_REQUEST || [c0, c1, c2, c3] != STUN_MAGIC_COOKIE {
        return None;
    }
    if usize::from(u16::from_be_bytes([l0, l1])) != attributes.len() {
        return None;
    }
    while let Some((head, rest)) = attributes.split_at_checked(4) {
        let [k0, k1, n0, n1] = <[u8; 4]>::try_from(head).ok()?;
        let len = usize::from(u16::from_be_bytes([n0, n1]));
        let value = rest.get(..len)?;
        if u16::from_be_bytes([k0, k1]) == STUN_USERNAME {
            let (server, client) = std::str::from_utf8(value).ok()?.split_once(':')?;
            return (!server.is_empty() && !client.is_empty()).then_some(server);
        }
        attributes = rest.get(len.next_multiple_of(4)..)?;
    }
    None
}

/// Writes one segment; `false` if str0m has no room for it yet. Room is checked before the
/// segment is copied: the driver polls a busy peer thousands of times a second, and copying a
/// 262,143-byte segment only to have it refused cost 9 µs per poll. Only the
/// reliable channel is written, so its buffered amount is str0m's whole send buffer use.
fn write(
    rtc: &mut Rtc,
    id: ChannelId,
    scratch: &mut Vec<u8>,
    header: u8,
    payload: &[u8],
) -> Result<bool> {
    let buffered = buffered_bytes(rtc, id)?.ok_or(Error::Closed)?;
    if buffered.saturating_add(payload.len()) >= SCTP_SEND_BUFFER_BYTES {
        return Ok(false);
    }
    scratch.clear();
    scratch.push(header);
    scratch.extend_from_slice(payload);
    let segment: &[u8] = scratch;
    match guard(rtc, |rtc| {
        rtc.channel(id)
            .map(|mut channel| channel.write(true, segment))
    })? {
        Some(result) => result.map_err(Error::Webrtc),
        None => Err(Error::Closed),
    }
}

/// Which NetherNet channel `config` is, if any: the label and every parameter must match
/// go-nethernet's `MessageReliability.Parameters`.
fn channel_kind(config: &ChannelConfig) -> Option<Kind> {
    if config.negotiated.is_some() || !config.protocol.is_empty() {
        return None;
    }
    match (config.label.as_str(), config.ordered, config.reliability) {
        (RELIABLE_LABEL, true, Reliability::Reliable) => Some(Kind::Reliable),
        (UNRELIABLE_LABEL, false, Reliability::MaxRetransmits { retransmits: 0 }) => {
            Some(Kind::Unreliable)
        }
        _ => None,
    }
}

/// `N` random ICE characters from the crypto module (str0m's own credentials come from
/// `fastrand`, which is not a cryptographic generator).
fn ice_string<const N: usize>() -> Result<String> {
    let mut bytes = [0_u8; N];
    crate::crypto::fill_random(&mut bytes).map_err(Error::Crypto)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a byte masked to 6 bits indexes the 64-entry table"
    )]
    Ok(bytes
        .iter()
        .map(|byte| char::from(ICE_CHARS[usize::from(byte & 63)]))
        .collect())
}

/// The bytes str0m holds on channel `id` until they are acknowledged; `None` when the channel
/// is gone.
fn buffered_bytes(rtc: &mut Rtc, id: ChannelId) -> Result<Option<usize>> {
    guard(rtc, |rtc| {
        rtc.channel(id).map(|mut channel| channel.buffered_amount())
    })
}

/// [`guard`] for str0m calls that fail with [`RtcError`].
fn guard_rtc<T>(rtc: &mut Rtc, f: impl FnOnce(&mut Rtc) -> Result<T, RtcError>) -> Result<T> {
    guard(rtc, f)?.map_err(Error::Webrtc)
}

/// Runs `f` on `rtc`, turning a panic into [`Error::Panicked`].
fn guard<T>(rtc: &mut Rtc, f: impl FnOnce(&mut Rtc) -> T) -> Result<T> {
    catch_unwind(AssertUnwindSafe(|| f(rtc))).map_err(|_| Error::Panicked)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::net::{IpAddr, Ipv4Addr};

    use str0m::change::SdpAnswer;
    use str0m::config::Fingerprint;
    use str0m::{DATAGRAM_MTU_TARGET, DATAGRAM_MTU_TARGET_MIN, DATAGRAM_MTU_WARN};

    use crate::nethernet::message::segments;

    /// The number of messages of the bulk phase of the cost measurement.
    const BULK: u32 = 16;
    const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 50_000);
    /// The test client drives str0m's direct API, which sends at most 64 KiB messages: so the
    /// peer also receives 65,535-byte segments, which BDS accepts too (P6).
    const CLIENT_PAYLOAD_BYTES: usize = 64 * 1024 - 1;
    /// The bytes of a DTLS record header.
    const DTLS_RECORD_HEADER_BYTES: usize = 13;
    pub(crate) const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
    /// The bytes of a mebibyte.
    const MIB_BYTES: usize = 1024 * 1024;
    /// The number of handshakes per loss percentage.
    const RUNS: u64 = 300;
    const SERVER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 19_133);
    /// The datagrams one side may receive before the peer opens; far above the 10 to 60 a
    /// faulty handshake takes, so only a retransmission storm reaches it.
    const STORM: usize = 1000;
    /// The number of ticks of the game-like phase of the cost measurement.
    const TICKS: u32 = 1200;

    pub(crate) fn reliable() -> ChannelConfig {
        ChannelConfig {
            label: RELIABLE_LABEL.to_owned(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        }
    }

    pub(crate) fn unreliable() -> ChannelConfig {
        ChannelConfig {
            label: UNRELIABLE_LABEL.to_owned(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| i.to_le_bytes()[0] ^ seed).collect()
    }

    /// xorshift64: a fixed-seed generator for the fuzz test.
    fn next(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn below(state: &mut u64, n: usize) -> usize {
        usize::try_from(next(state) % u64::try_from(n.max(1)).unwrap()).unwrap()
    }

    /// A NetherNet client on str0m: ICE controlling, DTLS server, SCTP server. It opens its
    /// channels in the order given; the first one carries its messages.
    pub(crate) struct Client {
        pub(crate) rtc: Rtc,
        channels: Vec<ChannelId>,
        open: Vec<ChannelId>,
        reassembler: Reassembler,
        pub(crate) received: Vec<Vec<u8>>,
        outbound: VecDeque<Vec<u8>>,
        pub(crate) timeout: Instant,
    }

    impl Client {
        /// A client at `addr` that packs its datagrams to at most `mtu` bytes, and its offer,
        /// which claims `fingerprint`.
        pub(crate) fn new(
            cert: &Certificate,
            addr: SocketAddr,
            mtu: usize,
            fingerprint: [u8; 32],
            now: Instant,
        ) -> (Self, Remote) {
            #[expect(
                clippy::disallowed_methods,
                reason = "invariant: tests pass str0m's default MTU or its minimum"
            )]
            let mut rtc = RtcConfig::new()
                .set_crypto_provider(Arc::clone(&cert.provider))
                .set_dtls_cert(cert.dtls.clone())
                .set_mtu(mtu..=DATAGRAM_MTU_WARN)
                .build(now);
            let creds = rtc.direct_api().local_ice_credentials();
            let remote = Remote {
                ice_ufrag: creds.ufrag,
                ice_pwd: creds.pass,
                fingerprint,
                candidates: vec![addr],
                max_message_size: 262_144,
            };
            let client = Self {
                rtc,
                channels: Vec::new(),
                open: Vec::new(),
                reassembler: Reassembler::new(MAX_MESSAGE_BYTES),
                received: Vec::new(),
                outbound: VecDeque::new(),
                timeout: now,
            };
            (client, remote)
        }

        /// A client on str0m's SDP API, as a WebRTC client negotiates: its offer reaches a new
        /// peer through [`sdp::parse_offer`], and it takes the answer [`sdp::write_answer`]
        /// makes, so it sends messages up to the answer's `max-message-size`.
        fn over_sdp(server: &Certificate, cert: &Certificate, now: Instant) -> (Self, Peer) {
            let mut rtc = RtcConfig::new()
                .set_crypto_provider(Arc::clone(&cert.provider))
                .set_dtls_cert(cert.dtls.clone())
                .set_sctp_max_buffered_amount(SCTP_SEND_BUFFER_BYTES)
                .build(now);
            rtc.add_local_candidate(Candidate::host(CLIENT, "udp").unwrap());
            #[expect(
                clippy::disallowed_methods,
                reason = "consistency: the test client negotiates by SDP, as a WebRTC client does"
            )]
            let mut api = rtc.sdp_api();
            let channels = vec![
                api.add_channel_with_config(reliable()),
                api.add_channel_with_config(unreliable()),
            ];
            let (offer, pending) = api.apply().unwrap();
            let offer = offer.to_sdp_string();
            let parsed = sdp::parse_offer(&offer).unwrap();
            let remote = Remote {
                ice_ufrag: parsed.ice_ufrag,
                ice_pwd: parsed.ice_pwd,
                fingerprint: parsed.fingerprint.bytes,
                candidates: parsed.candidates,
                max_message_size: parsed.max_message_size,
            };
            let peer = Peer::new(remote, SERVER, server, MAX_MESSAGE_BYTES, now).unwrap();
            let local = peer.local();
            let answer = sdp::write_answer(&sdp::Answer {
                session_id: 1,
                identity: "SUQ=",
                addr: SERVER,
                ice_ufrag: &local.ice_ufrag,
                ice_pwd: &local.ice_pwd,
                fingerprint: &sdp::fingerprint_text(&local.fingerprint),
            });
            // str0m names its media section at random; a NetherNet client names it 0.
            let mid = offer
                .lines()
                .find_map(|line| line.strip_prefix("a=mid:"))
                .unwrap();
            let answer = answer
                .replace("a=group:BUNDLE 0\r\n", &format!("a=group:BUNDLE {mid}\r\n"))
                .replace("a=mid:0\r\n", &format!("a=mid:{mid}\r\n"));
            let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
            #[expect(
                clippy::disallowed_methods,
                reason = "consistency: the test client negotiates by SDP, as a WebRTC client does"
            )]
            rtc.sdp_api().accept_answer(pending, answer).unwrap();
            let client = Self {
                rtc,
                channels,
                open: Vec::new(),
                reassembler: Reassembler::new(MAX_MESSAGE_BYTES),
                received: Vec::new(),
                outbound: VecDeque::new(),
                timeout: now,
            };
            (client, peer)
        }

        /// Takes the peer's answer, `local` at `server`, and asks for `channels`.
        pub(crate) fn answer(
            &mut self,
            local: &Local,
            addr: SocketAddr,
            server: SocketAddr,
            channels: &[ChannelConfig],
        ) {
            let mut api = self.rtc.direct_api();
            api.set_ice_controlling(true);
            api.set_remote_ice_credentials(IceCreds {
                ufrag: local.ice_ufrag.clone(),
                pass: local.ice_pwd.clone(),
            });
            api.set_remote_fingerprint(Fingerprint {
                hash_func: "sha-256".to_owned(),
                bytes: local.fingerprint.to_vec(),
            });
            api.start_dtls(false).unwrap();
            api.start_sctp(false);
            self.channels = channels
                .iter()
                .map(|config| api.create_data_channel(config.clone()))
                .collect();
            self.rtc
                .add_local_candidate(Candidate::host(addr, "udp").unwrap());
            self.rtc
                .add_remote_candidate(Candidate::host(server, "udp").unwrap());
        }

        fn send(&mut self, message: &[u8]) {
            for (header, payload) in segments(message, CLIENT_PAYLOAD_BYTES).unwrap() {
                let mut segment = vec![header];
                segment.extend_from_slice(payload);
                self.outbound.push_back(segment);
            }
        }

        fn flush(&mut self) {
            let Some(&id) = self.channels.first() else {
                return;
            };
            if !self.open.contains(&id) {
                return;
            }
            while let Some(segment) = self.outbound.front() {
                let Some(mut channel) = self.rtc.channel(id) else {
                    return;
                };
                if !channel.write(true, segment).unwrap() {
                    return;
                }
                self.outbound.pop_front();
            }
        }

        pub(crate) fn event(&mut self, event: Event) {
            match event {
                Event::ChannelOpen(id, _) => self.open.push(id),
                Event::ChannelData(data) if self.channels.first() == Some(&data.id) => {
                    if let Some(message) = self.reassembler.push(data.data).unwrap() {
                        self.received.push(message);
                    }
                }
                _ => {}
            }
        }
    }

    /// What the in-memory network does to datagrams in both directions until the peer opens.
    #[derive(Debug, Clone, Copy)]
    enum Fault {
        /// Every DTLS record travels in a datagram of its own (as a sender may send them), and
        /// each datagram is a random one of those in flight: flights arrive shuffled.
        Reorder,
        /// Half of the datagrams arrive a second time, after the others in flight.
        Duplicate,
        /// The first copy of each plaintext DTLS handshake datagram is lost, so every flight
        /// needs a retransmission.
        LoseFirst,
        /// This percentage of datagrams is lost.
        Lose(usize),
        /// The peer's first application data datagram, its SCTP INIT, is lost.
        LoseInit,
        /// The records inside each DTLS datagram arrive in reverse order. No network does this;
        /// only a sender could (dimpl #168).
        ReverseRecords,
        /// As `ReverseRecords` for the first copy of each DTLS datagram of several records; the
        /// retransmissions arrive intact.
        ReverseFirst,
    }

    /// Each DTLS record of `datagram` as a datagram of its own, as a sender may send them;
    /// anything else unchanged.
    fn dtls_records(datagram: Vec<u8>) -> Vec<Vec<u8>> {
        if !matches!(datagram.first(), Some(20..=63)) {
            return vec![datagram];
        }
        let mut out = Vec::new();
        let mut rest = datagram.as_slice();
        while rest.len() >= DTLS_RECORD_HEADER_BYTES {
            let len =
                DTLS_RECORD_HEADER_BYTES + usize::from(u16::from_be_bytes([rest[11], rest[12]]));
            let (record, tail) = rest.split_at(len.min(rest.len()));
            out.push(record.to_vec());
            rest = tail;
        }
        out
    }

    /// A [`Fault`] with its generator and what it saw.
    struct Network {
        fault: Fault,
        rng: u64,
        start: Instant,
        /// Datagrams delivered to the peer and to the client before the peer opened.
        delivered: [usize; 2],
        /// When the peer finished its DTLS handshake: its first application data record.
        dtls: Option<Duration>,
        /// For `LoseFirst` and `LoseInit`: (to the peer, content type, handshake type) of
        /// datagrams lost once.
        lost: Vec<(bool, u8, u8)>,
    }

    impl Network {
        fn new(fault: Fault, seed: u64, start: Instant) -> Self {
            Self {
                fault,
                // xorshift needs a nonzero state.
                rng: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
                start,
                delivered: [0; 2],
                dtls: None,
                lost: Vec::new(),
            }
        }
    }

    /// A peer and a client joined by an in-memory network with a simulated clock.
    struct Harness {
        now: Instant,
        peer: Peer,
        client: Client,
        to_client: VecDeque<Vec<u8>>,
        /// Datagrams on their way to the peer; used only with a faulty network, otherwise the
        /// peer takes each one as the client sends it.
        to_peer: VecDeque<Vec<u8>>,
        network: Option<Network>,
        peer_timeout: Instant,
        opened: bool,
        received: Vec<Vec<u8>>,
        /// The round trip times the peer reported.
        rtts: Vec<Duration>,
        closed: Option<Error>,
        /// Every datagram the client sent, when recording.
        record: Option<Vec<Vec<u8>>>,
        /// The peer's own work, when measuring.
        meter: Option<Meter>,
    }

    /// Time spent inside the peer's `receive` and `poll` calls (the driver task's share), with
    /// the datagrams it took and gave. Each call adds two clock reads.
    #[derive(Debug, Default)]
    struct Meter {
        busy: Duration,
        received: usize,
        sent: usize,
    }

    impl Harness {
        fn new(
            server: &Certificate,
            client: &Certificate,
            channels: &[ChannelConfig],
            fingerprint: [u8; 32],
        ) -> Self {
            Self::with_client_mtu(server, client, channels, fingerprint, DATAGRAM_MTU_TARGET)
        }

        /// A harness whose client packs its datagrams to at most `client_mtu` bytes.
        fn with_client_mtu(
            server: &Certificate,
            client: &Certificate,
            channels: &[ChannelConfig],
            fingerprint: [u8; 32],
            client_mtu: usize,
        ) -> Self {
            let now = crate::clock::now();
            let (mut client, remote) = Client::new(client, CLIENT, client_mtu, fingerprint, now);
            let peer = Peer::new(remote, SERVER, server, MAX_MESSAGE_BYTES, now).unwrap();
            client.answer(peer.local(), CLIENT, SERVER, channels);
            Self::joined(peer, client, now)
        }

        /// A harness for a peer and a client that already know each other's description.
        fn joined(peer: Peer, client: Client, now: Instant) -> Self {
            Self {
                now,
                peer,
                client,
                to_client: VecDeque::new(),
                to_peer: VecDeque::new(),
                network: None,
                meter: None,
                peer_timeout: now,
                opened: false,
                received: Vec::new(),
                rtts: Vec::new(),
                closed: None,
                record: None,
            }
        }

        fn honest(server: &Certificate, client: &Certificate) -> Self {
            Self::new(
                server,
                client,
                &[reliable(), unreliable()],
                client.fingerprint,
            )
        }

        /// Moves outputs that are ready and one datagram to the client; `true` if anything moved.
        /// Both sides are polled after every datagram they take, as str0m requires.
        fn step(&mut self) -> bool {
            let mut progress = self.poll_peer();
            self.client.flush();
            loop {
                match self.client.rtc.poll_output() {
                    Ok(str0m::Output::Transmit(transmit)) => {
                        if let Some(record) = &mut self.record {
                            record.push(transmit.contents.to_vec());
                        }
                        if self.network.is_some() {
                            self.to_peer.push_back(transmit.contents.to_vec());
                        } else {
                            self.deliver_to_peer(&transmit.contents);
                        }
                    }
                    Ok(str0m::Output::Event(event)) => self.client.event(event),
                    Ok(str0m::Output::Timeout(at)) => {
                        self.client.timeout = at;
                        break;
                    }
                    Err(_) => {
                        self.client.timeout = self.now + Duration::from_secs(3600);
                        break;
                    }
                }
                progress = true;
            }
            if let Some(data) = self.next_datagram(true) {
                self.deliver_to_peer(&data);
                progress = true;
            }
            if let Some(data) = self.next_datagram(false) {
                let receive = Receive::new(Protocol::Udp, SERVER, CLIENT, &data).unwrap();
                let delivered = self
                    .client
                    .rtc
                    .handle_input(Input::Receive(self.now, receive));
                assert!(
                    delivered.is_ok() || self.closed.is_some(),
                    "client failed: {delivered:?}"
                );
                progress = true;
            }
            progress
        }

        /// The next datagram for the peer (`to_peer`) or the client, after the network's fault
        /// while the peer is not open.
        fn next_datagram(&mut self, to_peer: bool) -> Option<Vec<u8>> {
            let queue = if to_peer {
                &mut self.to_peer
            } else {
                &mut self.to_client
            };
            let Some(network) = self.network.as_mut().filter(|_| !self.opened) else {
                return queue.pop_front();
            };
            let datagram = match network.fault {
                Fault::Reorder => {
                    *queue = queue.drain(..).flat_map(dtls_records).collect();
                    let at = below(&mut network.rng, queue.len());
                    queue.remove(at)?
                }
                Fault::Duplicate => {
                    let datagram = queue.pop_front()?;
                    if below(&mut network.rng, 2) == 0 {
                        queue.push_back(datagram.clone());
                    }
                    datagram
                }
                Fault::LoseFirst => loop {
                    let datagram = queue.pop_front()?;
                    let key = match datagram.get(..14) {
                        Some(&[kind @ (20 | 22), _, _, 0, 0, .., handshake]) => {
                            (to_peer, kind, handshake)
                        }
                        _ => break datagram,
                    };
                    if network.lost.contains(&key) {
                        break datagram;
                    }
                    network.lost.push(key);
                },
                Fault::Lose(percent) => loop {
                    let datagram = queue.pop_front()?;
                    if below(&mut network.rng, 100) >= percent {
                        break datagram;
                    }
                },
                Fault::LoseInit => loop {
                    let datagram = queue.pop_front()?;
                    let key = (false, 23, 0);
                    if to_peer || datagram.first() != Some(&23) || network.lost.contains(&key) {
                        break datagram;
                    }
                    network.lost.push(key);
                },
                Fault::ReverseRecords => {
                    let mut records = dtls_records(queue.pop_front()?);
                    records.reverse();
                    records.concat()
                }
                Fault::ReverseFirst => {
                    let datagram = queue.pop_front()?;
                    let mut records = dtls_records(datagram.clone());
                    let key = (to_peer, datagram[0], datagram.get(13).copied().unwrap_or(0));
                    if records.len() < 2 || network.lost.contains(&key) {
                        datagram
                    } else {
                        network.lost.push(key);
                        records.reverse();
                        records.concat()
                    }
                }
            };
            let delivered = &mut network.delivered[usize::from(!to_peer)];
            *delivered += 1;
            assert!(*delivered < STORM, "retransmission storm");
            Some(datagram)
        }

        /// Gives the peer one datagram from the client and polls it.
        fn deliver_to_peer(&mut self, datagram: &[u8]) {
            let start = self.meter.is_some().then(crate::clock::now);
            self.peer.receive(self.now, CLIENT, datagram);
            if let (Some(meter), Some(start)) = (&mut self.meter, start) {
                meter.busy += crate::clock::now() - start;
                meter.received += 1;
            }
            self.poll_peer();
        }

        fn poll_peer(&mut self) -> bool {
            let mut progress = false;
            while self.closed.is_none() {
                let start = self.meter.is_some().then(crate::clock::now);
                let output = self.peer.poll(self.now);
                if let (Some(meter), Some(start)) = (&mut self.meter, start) {
                    meter.busy += crate::clock::now() - start;
                    meter.sent += usize::from(matches!(output, Output::Transmit { .. }));
                }
                match output {
                    Output::Transmit { to, data } => {
                        assert_eq!(to, CLIENT);
                        if let Some(network) = &mut self.network
                            && network.dtls.is_none()
                            && data.first() == Some(&23)
                        {
                            network.dtls = Some(self.now - network.start);
                        }
                        self.to_client.push_back(data);
                    }
                    Output::Open => {
                        assert!(!self.opened);
                        self.opened = true;
                    }
                    Output::Message(message) => self.received.push(message),
                    Output::Rtt(rtt) => self.rtts.push(rtt),
                    Output::Timeout(at) => {
                        self.peer_timeout = at;
                        break;
                    }
                    Output::Closed(reason) => self.closed = Some(reason),
                }
                progress = true;
            }
            progress
        }

        /// Runs until `done` or `limit` of simulated time passes; `true` if `done`.
        fn run(&mut self, limit: Duration, done: impl Fn(&Self) -> bool) -> bool {
            let end = self.now + limit;
            loop {
                if done(self) {
                    return true;
                }
                if self.step() {
                    continue;
                }
                if self.now >= end {
                    return false;
                }
                self.now = self
                    .peer_timeout
                    .min(self.client.timeout)
                    .min(end)
                    .max(self.now + Duration::from_millis(1));
                if self
                    .client
                    .rtc
                    .handle_input(Input::Timeout(self.now))
                    .is_err()
                {
                    self.client.timeout = self.now + Duration::from_secs(3600);
                }
            }
        }

        fn open(&mut self) {
            assert!(
                self.run(Duration::from_secs(5), |h| h.opened
                    && h.client.open.len() == 2),
                "channels did not open: {:?}",
                self.closed
            );
        }

        fn run_until_closed(&mut self, limit: Duration) -> Error {
            assert!(
                self.run(limit, |h| h.closed.is_some()),
                "peer did not close"
            );
            self.closed.take().unwrap()
        }
    }

    pub(crate) fn certificates() -> (Certificate, Certificate) {
        (
            Certificate::generate().unwrap(),
            Certificate::generate().unwrap(),
        )
    }

    /// `cert` with `extra` zero bytes after its signature: a larger certificate whose DTLS flight
    /// spans datagrams. dimpl reads only the public key and str0m checks only the fingerprint,
    /// so it stays usable.
    fn inflated(cert: &Certificate, extra: usize) -> Certificate {
        /// Header and content length of the DER element at the start of `der`.
        fn header(der: &[u8]) -> (usize, usize) {
            match der[1] {
                short @ 0..0x80 => (2, usize::from(short)),
                long => {
                    let n = usize::from(long & 0x7f);
                    let len = der[2..2 + n]
                        .iter()
                        .fold(0, |len, &b| (len << 8) | usize::from(b));
                    (2 + n, len)
                }
            }
        }
        fn element(tag: u8, content: &[u8]) -> Vec<u8> {
            let len = content.len().to_be_bytes();
            let skip = len.iter().take_while(|&&b| b == 0).count();
            let mut out = vec![tag];
            if content.len() < 0x80 {
                out.push(len[len.len() - 1]);
            } else {
                out.push(0x80 | u8::try_from(len.len() - skip).unwrap());
                out.extend_from_slice(&len[skip..]);
            }
            out.extend_from_slice(content);
            out
        }
        // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature BIT STRING }
        let der = &cert.dtls.certificate;
        let (outer, _) = header(der);
        let mut at = outer;
        for _ in 0..2 {
            let (h, len) = header(&der[at..]);
            at += h + len;
        }
        assert_eq!(der[at], 0x03, "the signature is a BIT STRING");
        let (h, len) = header(&der[at..]);
        let mut signature = der[at + h..at + h + len].to_vec();
        signature.resize(len + extra, 0);
        let mut content = der[outer..at].to_vec();
        content.extend(element(0x03, &signature));
        let mut dtls = cert.dtls.clone();
        dtls.certificate = element(0x30, &content);
        Certificate {
            provider: Arc::clone(&cert.provider),
            fingerprint: crate::crypto::sha256(&[&dtls.certificate]),
            dtls,
        }
    }

    #[test]
    fn local_credentials_match_bds_shape() {
        let (server, client) = certificates();
        let h = Harness::honest(&server, &client);
        let local = h.peer.local();
        assert_eq!(local.ice_ufrag.len(), UFRAG_LEN);
        assert_eq!(local.ice_pwd.len(), PWD_LEN);
        assert!(
            local
                .ice_ufrag
                .bytes()
                .chain(local.ice_pwd.bytes())
                .all(|c| ICE_CHARS.contains(&c))
        );
        assert_eq!(&local.fingerprint, server.fingerprint());
    }

    /// An open peer reports the round trip time of its ICE consent checks, the connection's
    /// ping (PMMP `NetworkSession::getPing`), every [`RTT_INTERVAL`].
    #[test]
    fn an_open_peer_reports_its_round_trip_time() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        let before = h.rtts.len();
        assert!(
            h.run(RTT_INTERVAL * 5, |h| h.rtts.len() >= before + 2),
            "no round trip time reported"
        );
        // The simulated link delivers at once.
        assert!(h.rtts.iter().all(|rtt| *rtt < Duration::from_millis(100)));
    }

    /// A message counts as undelivered from its send until SCTP has it acknowledged.
    #[test]
    fn sent_bytes_stay_undelivered_until_acknowledged() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        assert_eq!(h.peer.undelivered().unwrap(), 0);
        let down = pattern(2 * 1024 * 1024, 4);
        h.peer.send(down.clone()).unwrap();
        assert!(h.peer.undelivered().unwrap() >= down.len());
        assert!(
            h.run(Duration::from_secs(60), |h| !h.client.received.is_empty()),
            "the message did not arrive: {:?}",
            h.closed
        );
        // A second more for the last acknowledgement.
        assert!(!h.run(Duration::from_secs(1), |_| false));
        assert_eq!(h.peer.undelivered().unwrap(), 0);
    }

    #[test]
    fn certificate_fingerprint_matches_str0m() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        let str0m = h.peer.rtc.direct_api().local_dtls_fingerprint().clone();
        assert_eq!(str0m.hash_func, "sha-256");
        assert_eq!(str0m.bytes, server.fingerprint().to_vec());
    }

    #[test]
    fn channels_open_and_messages_round_trip_both_ways() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        h.peer.send(Vec::new()).unwrap();
        for (len, seed) in [(1, 1), (1024 * 1024, 2), (MAX_MESSAGE_BYTES, 3)] {
            let down = pattern(len, seed);
            let up = pattern(len, seed ^ 0x55);
            h.peer.send(down.clone()).unwrap();
            h.client.send(&up);
            assert!(
                h.run(Duration::from_secs(60), |h| !h.received.is_empty()
                    && !h.client.received.is_empty()),
                "message of {len} bytes did not arrive: {:?}",
                h.closed
            );
            assert_eq!(h.client.received.pop().unwrap(), down);
            assert_eq!(h.received.pop().unwrap(), up);
            assert!(h.received.is_empty() && h.client.received.is_empty());
        }
        assert!(h.closed.is_none());
        assert_eq!(h.peer.queue.len(), 0);
        assert_eq!(h.peer.queued, 0);
    }

    /// Reproduces the str0m 0.24.0 defects fixed in 0.24.1 (#1062): the send buffer was fixed at 128 KiB, so a 262,144-byte message never left, and
    /// polling recursed once per queued SCTP packet, which overflowed the stack on large sends
    /// (see also `channels_open_and_messages_round_trip_both_ways`).
    #[test]
    fn a_segment_of_the_remote_max_message_size_is_sent() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        let down = pattern(SEND_MESSAGE_LIMIT_BYTES - 1, 9);
        h.peer.send(down.clone()).unwrap();
        assert!(
            h.run(Duration::from_secs(10), |h| !h.client.received.is_empty()),
            "a {} byte segment did not leave: {:?}",
            down.len(),
            h.closed
        );
        assert_eq!(h.client.received.pop().unwrap(), down);
    }

    /// The `max-message-size` the answer advertises is one str0m receives: a client that sends
    /// a data channel message of that size is heard.
    #[test]
    fn a_message_of_the_advertised_max_message_size_is_received() {
        let (server, client) = certificates();
        let now = crate::clock::now();
        let (client, peer) = Client::over_sdp(&server, &client, now);
        let mut h = Harness::joined(peer, client, now);
        h.open();
        let size = usize::try_from(sdp::MAX_MESSAGE_BYTES).unwrap();
        let up = pattern(size - 1, 5);
        let mut segment = vec![0];
        segment.extend_from_slice(&up);
        h.client.outbound.push_back(segment);
        assert!(
            h.run(Duration::from_secs(10), |h| !h.received.is_empty()),
            "a {size} byte message did not arrive: {:?}",
            h.closed
        );
        assert_eq!(h.received.pop().unwrap(), up);
    }

    #[test]
    fn reliable_data_before_the_unreliable_channel_waits_for_the_open() {
        let (server, client) = certificates();
        let mut h = Harness::new(&server, &client, &[reliable()], client.fingerprint);
        let early = pattern(100, 7);
        h.client.send(&early);
        assert!(
            h.run(Duration::from_secs(5), |h| h.client.open.len() == 1
                && h.client.outbound.is_empty()),
            "{:?}",
            h.closed
        );
        // Delivery time for the message; the result is the state checked below.
        h.run(Duration::from_millis(500), |_| false);
        assert!(!h.opened && h.received.is_empty() && h.closed.is_none());
        let unreliable = h.client.rtc.direct_api().create_data_channel(unreliable());
        h.client.channels.push(unreliable);
        assert!(
            h.run(Duration::from_secs(5), |h| h.opened
                && !h.received.is_empty()),
            "{:?}",
            h.closed
        );
        assert_eq!(h.received, vec![early]);
    }

    #[test]
    fn more_than_one_message_of_data_before_the_open_closes_the_peer() {
        let (server, client) = certificates();
        let mut h = Harness::new(&server, &client, &[reliable()], client.fingerprint);
        h.client.send(&pattern(MAX_MESSAGE_BYTES, 1));
        h.client.send(&[1]);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(60)),
            Error::Limit(_)
        ));
    }

    #[test]
    fn message_over_the_limit_closes_the_peer() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        h.client.send(&pattern(MAX_MESSAGE_BYTES + 1, 0));
        let reason = h.run_until_closed(Duration::from_secs(60));
        assert!(
            matches!(reason, Error::Message(message::Error::TooLarge)),
            "{reason:?}"
        );
    }

    #[test]
    fn unknown_label_closes_the_peer() {
        let (server, client) = certificates();
        let other = ChannelConfig {
            label: "OtherChannel".to_owned(),
            ..reliable()
        };
        let mut h = Harness::new(&server, &client, &[reliable(), other], client.fingerprint);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(5)),
            Error::Channel(_)
        ));
        assert!(!h.opened);
    }

    #[test]
    fn wrong_reliability_closes_the_peer() {
        let (server, client) = certificates();
        let ordered = ChannelConfig {
            ordered: true,
            ..unreliable()
        };
        let mut h = Harness::new(&server, &client, &[reliable(), ordered], client.fingerprint);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(5)),
            Error::Channel(_)
        ));
        assert!(!h.opened);
    }

    #[test]
    fn third_channel_closes_the_peer() {
        let (server, client) = certificates();
        let channels = [reliable(), unreliable(), reliable()];
        let mut h = Harness::new(&server, &client, &channels, client.fingerprint);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(5)),
            Error::Channel("opened twice")
        ));
    }

    #[test]
    fn wrong_fingerprint_never_opens() {
        let (server, client) = certificates();
        let mut h = Harness::new(&server, &client, &[reliable(), unreliable()], [7; 32]);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(15)),
            Error::Webrtc(_)
        ));
        assert!(!h.opened);
    }

    #[test]
    fn channels_not_open_in_time_close_the_peer() {
        let (server, client) = certificates();
        let mut h = Harness::new(&server, &client, &[reliable()], client.fingerprint);
        let start = h.now;
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(15)),
            Error::Timeout(_)
        ));
        assert!(h.now >= start + OPEN_TIMEOUT);
        assert!(!h.opened);
    }

    #[test]
    fn silent_remote_times_out() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        let silent = h.now + IDLE_TIMEOUT;
        loop {
            match h.peer.poll(silent) {
                Output::Closed(reason) => {
                    assert!(matches!(reason, Error::Timeout(_)));
                    break;
                }
                Output::Timeout(_) => panic!("peer kept waiting"),
                _ => {}
            }
        }
        assert!(matches!(h.peer.poll(silent), Output::Closed(Error::Closed)));
    }

    #[test]
    fn remote_channel_close_closes_the_peer() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        let id = h.client.channels[0];
        h.client.rtc.direct_api().close_data_channel(id);
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(5)),
            Error::Remote
        ));
    }

    /// A closing connection waits for this before the peer goes (`nethernet.rs`), so the last
    /// packets, such as a Disconnect, reach the client.
    #[test]
    fn sent_messages_are_drained_once_acknowledged() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        assert!(h.peer.is_drained());
        let down = pattern(1024 * 1024, 4);
        h.peer.send(down.clone()).unwrap();
        assert!(!h.peer.is_drained(), "nothing is acknowledged yet");
        let mut drained = false;
        for _ in 0..1000 {
            h.run(Duration::from_millis(10), |_| false);
            if h.peer.is_drained() {
                drained = true;
                break;
            }
        }
        assert!(drained, "acknowledged bytes were not released");
        assert_eq!(h.client.received.pop().unwrap(), down);
    }

    #[test]
    fn local_close_drains_and_reports_closed() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        h.peer.close();
        assert!(matches!(h.peer.send(b"late".to_vec()), Err(Error::Closed)));
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(1)),
            Error::Closed
        ));
    }

    #[test]
    fn unsent_bytes_over_the_cap_close_the_peer() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();
        let message = pattern(12 * 1024 * 1024, 9);
        h.peer.send(message.clone()).unwrap();
        h.peer.send(message.clone()).unwrap();
        assert!(matches!(h.peer.send(message), Err(Error::Closed)));
        assert!(matches!(
            h.run_until_closed(Duration::from_secs(1)),
            Error::Limit("unsent bytes")
        ));
    }

    #[test]
    fn stun_server_ufrag_reads_binding_requests_only() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.record = Some(Vec::new());
        h.open();
        let record = h.record.take().unwrap();
        let ufrag = h.peer.local().ice_ufrag.clone();
        let requests: Vec<_> = record
            .iter()
            .filter_map(|datagram| stun_server_ufrag(datagram))
            .collect();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|found| *found == ufrag));
        let request = record
            .iter()
            .find(|datagram| stun_server_ufrag(datagram).is_some())
            .unwrap();
        for len in 0..request.len() {
            assert_eq!(stun_server_ufrag(&request[..len]), None);
        }
        assert_eq!(stun_server_ufrag(&[0x16; 64]), None);
    }

    #[test]
    fn the_remote_address_is_where_accepted_dtls_came_from() {
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.record = Some(Vec::new());
        h.open();
        assert_eq!(h.peer.remote_addr(), Some(CLIENT));
        // The client's own valid STUN request, with a spoofed source.
        let record = h.record.take().unwrap();
        let request = record
            .iter()
            .find(|datagram| stun_server_ufrag(datagram).is_some())
            .unwrap();
        let spoofed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 40_000);
        let before = h.peer.last_receive;
        h.now += Duration::from_millis(1);
        h.peer.receive(h.now, spoofed, request);
        // The STUN response goes to the spoofed source, past the harness.
        loop {
            match h.peer.poll(h.now) {
                Output::Timeout(_) => break,
                Output::Closed(reason) => panic!("peer closed: {reason:?}"),
                _ => {}
            }
        }
        assert!(h.peer.last_receive > before, "str0m accepted the request");
        assert_eq!(h.peer.remote_addr(), Some(CLIENT));
    }

    /// Certificates whose DTLS flights span datagrams: the server's grows past one datagram,
    /// and `faulty_handshake` packs the client's to the smallest MTU str0m allows.
    fn split_certificates() -> (Certificate, Certificate) {
        let (server, client) = certificates();
        (inflated(&server, 900), client)
    }

    /// What a handshake through a faulty network came to, in simulated time from its start.
    struct Handshake {
        /// When the peer and the client both had both channels open.
        opened: Option<Duration>,
        /// When the peer finished DTLS.
        dtls: Option<Duration>,
        closed: Option<Error>,
    }

    fn faulty_handshake(
        server: &Certificate,
        client: &Certificate,
        fault: Fault,
        seed: u64,
    ) -> Handshake {
        let mut h = Harness::with_client_mtu(
            server,
            client,
            &[reliable(), unreliable()],
            client.fingerprint,
            DATAGRAM_MTU_TARGET_MIN,
        );
        let start = h.now;
        h.network = Some(Network::new(fault, seed, start));
        let open = |h: &Harness| h.opened && h.client.open.len() == 2;
        h.run(OPEN_TIMEOUT + Duration::from_secs(1), |h| {
            open(h) || h.closed.is_some()
        });
        Handshake {
            opened: open(&h).then(|| h.now - start),
            dtls: h.network.as_ref().and_then(|network| network.dtls),
            closed: h.closed.take(),
        }
    }

    /// Reordering, down to every DTLS record in a datagram of its own (Finished before
    /// `ChangeCipherSpec`, certificate fragments swapped), never stalls the handshake: dimpl
    /// queues records by handshake sequence (dimpl #168).
    #[test]
    fn handshake_completes_with_reordered_records() {
        let (server, client) = split_certificates();
        for seed in 1..=20 {
            let result = faulty_handshake(&server, &client, Fault::Reorder, seed);
            assert!(
                result.opened.is_some_and(|after| after < OPEN_TIMEOUT),
                "seed {seed}: {:?}",
                result.closed
            );
        }
    }

    /// Duplicates trigger flight resends on both sides (both are dimpl here), yet the
    /// handshake completes without a storm (`STORM`, dimpl #169).
    #[test]
    fn handshake_completes_with_duplicated_datagrams() {
        let (server, client) = split_certificates();
        for seed in 1..=20 {
            let result = faulty_handshake(&server, &client, Fault::Duplicate, seed);
            assert!(
                result.opened.is_some_and(|after| after < OPEN_TIMEOUT),
                "seed {seed}: {:?}",
                result.closed
            );
        }
    }

    /// Every flight lost once is retransmitted on dimpl's timer (1 s, doubling) within the
    /// establishment deadline.
    #[test]
    fn handshake_completes_when_every_flight_is_lost_once() {
        let (server, client) = split_certificates();
        let result = faulty_handshake(&server, &client, Fault::LoseFirst, 1);
        assert!(
            result.opened.is_some_and(|after| after < OPEN_TIMEOUT),
            "{:?}",
            result.closed
        );
    }

    /// A lost SCTP INIT is sent again after RTO.Initial, 1 s as RFC 9260 recommends, rather than
    /// sctp-proto's RFC 4960 default of 3 s, which costs a lossy handshake its deadline.
    #[test]
    fn a_lost_sctp_init_is_sent_again_after_one_second() {
        let (server, client) = split_certificates();
        let result = faulty_handshake(&server, &client, Fault::LoseInit, 1);
        // The peer sends its INIT as soon as DTLS finishes; the rest of the handshake takes no
        // simulated time.
        let init = result.dtls.expect("DTLS finished");
        let opened = result.opened.expect("channels opened");
        let resent = opened.checked_sub(init).expect("opened after DTLS");
        assert!(resent < Duration::from_secs(2), "{resent:?}");
    }

    /// A datagram with its records out of order is discarded (dimpl 0.7.5, #169; 0.7.4 stalled
    /// for good, #168). Only a sender can cause it; if every copy is out of order, the deadline
    /// closes just that peer.
    #[test]
    fn records_reversed_inside_a_datagram_time_out_the_peer() {
        let (server, client) = split_certificates();
        let result = faulty_handshake(&server, &client, Fault::ReverseRecords, 1);
        assert!(result.opened.is_none() && result.dtls.is_none());
        assert!(
            matches!(result.closed, Some(Error::Timeout("channels not open"))),
            "{:?}",
            result.closed
        );
    }

    /// A datagram whose records arrive out of order once is discarded and its retransmission
    /// completes the handshake (dimpl 0.7.5, #169; 0.7.4 stalled until the deadline).
    #[test]
    fn records_reversed_inside_one_copy_of_a_datagram_are_recovered_by_retransmission() {
        let (server, client) = split_certificates();
        let result = faulty_handshake(&server, &client, Fault::ReverseFirst, 1);
        assert!(
            result.opened.is_some_and(|after| after < OPEN_TIMEOUT),
            "{:?}",
            result.closed
        );
    }

    /// Handshakes under random loss: how many open within the deadline, and how
    /// many of the rest failed in DTLS rather than in the SCTP association after it. Run:
    /// `cargo test --release --lib nethernet::peer::tests::handshake_under_random_loss --
    /// --ignored --nocapture`
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        reason = "performance: a measurement reports its numbers"
    )]
    fn handshake_under_random_loss() {
        let (server, client) = split_certificates();
        for percent in [5, 10, 30] {
            let (mut opened, mut dtls_failed, mut slowest_dtls, mut slowest_open) =
                (0, 0, Duration::ZERO, Duration::ZERO);
            for seed in 1..=RUNS {
                let result = faulty_handshake(&server, &client, Fault::Lose(percent), seed);
                match result.dtls {
                    Some(after) => slowest_dtls = slowest_dtls.max(after),
                    None => dtls_failed += 1,
                }
                if let Some(after) = result.opened {
                    opened += 1;
                    slowest_open = slowest_open.max(after);
                }
            }
            println!(
                "{percent}% loss: {opened}/{RUNS} open within {OPEN_TIMEOUT:?} (slowest \
                 {slowest_open:?}); {dtls_failed} never finished DTLS (slowest finish \
                 {slowest_dtls:?}); the other {} failed in SCTP",
                RUNS - opened - dtls_failed
            );
        }
    }

    /// The driver task's cost for one peer: time inside the peer's calls for
    /// bulk transfer each way, for a game-like exchange of one message each way per server tick,
    /// and while idle. Run: `cargo test --release --lib
    /// nethernet::peer::tests::driver_cost_per_peer -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        reason = "performance: a measurement reports its numbers"
    )]
    fn driver_cost_per_peer() {
        fn report(what: &str, meter: &Meter, per: &str, count: u32) {
            let count = count.max(1);
            println!(
                "{what}: {:?} busy, {:?} per {per}; {} datagrams in ({:?} each), {} out \
                 ({:?} each)",
                meter.busy,
                meter.busy / count,
                meter.received,
                meter.busy / u32::try_from(meter.received.max(1)).unwrap(),
                meter.sent,
                meter.busy / u32::try_from(meter.sent.max(1)).unwrap(),
            );
        }
        let (server, client) = certificates();
        let mut h = Harness::honest(&server, &client);
        h.open();

        let message = pattern(MIB_BYTES, 1);
        h.meter = Some(Meter::default());
        let start = crate::clock::now();
        for _ in 0..BULK {
            h.peer.send(message.clone()).unwrap();
        }
        h.meter.as_mut().unwrap().busy += crate::clock::now() - start;
        assert!(h.run(Duration::from_secs(600), |h| h.client.received.len()
            == BULK as usize));
        report("down 16 x 1 MiB", &h.meter.take().unwrap(), "MiB", BULK);
        h.client.received.clear();

        h.meter = Some(Meter::default());
        for _ in 0..BULK {
            h.client.send(&message);
        }
        assert!(h.run(Duration::from_secs(600), |h| h.received.len()
            == BULK as usize));
        report("up 16 x 1 MiB", &h.meter.take().unwrap(), "MiB", BULK);
        h.received.clear();

        let down = pattern(2560, 2);
        let up = pattern(256, 3);
        h.meter = Some(Meter::default());
        for _ in 0..TICKS {
            let start = crate::clock::now();
            h.peer.send(down.clone()).unwrap();
            h.meter.as_mut().unwrap().busy += crate::clock::now() - start;
            h.client.send(&up);
            h.run(crate::tick::TICK, |_| false);
        }
        assert!(h.run(Duration::from_secs(1), |h| h.received.len()
            == TICKS as usize
            && h.client.received.len() == TICKS as usize));
        report(
            "ticks of 2,560 B down and 256 B up",
            &h.meter.take().unwrap(),
            "tick",
            TICKS,
        );

        h.meter = Some(Meter::default());
        h.run(Duration::from_secs(60), |_| false);
        report("idle 60 s", &h.meter.take().unwrap(), "second", 60);
        assert!(h.closed.is_none());
    }

    fn mutate(datagram: &mut Vec<u8>, rng: &mut u64) {
        match below(rng, 4) {
            0 => {
                for _ in 0..=below(rng, 4) {
                    let at = below(rng, datagram.len());
                    if let Some(byte) = datagram.get_mut(at) {
                        *byte ^= next(rng).to_le_bytes()[0] | 1;
                    }
                }
            }
            1 => datagram.truncate(below(rng, datagram.len() + 1)),
            2 => {
                for _ in 0..=below(rng, 64) {
                    datagram.push(next(rng).to_le_bytes()[0]);
                }
            }
            _ => {
                let start = below(rng, datagram.len());
                for byte in &mut datagram[start..] {
                    *byte = next(rng).to_le_bytes()[0];
                }
            }
        }
    }

    /// Feeds `datagram` to the peer and polls it; the reason if the peer closed.
    fn feed(h: &mut Harness, datagram: &[u8]) -> Option<Error> {
        h.peer.receive(h.now, CLIENT, datagram);
        loop {
            match h.peer.poll(h.now) {
                Output::Timeout(_) => return None,
                Output::Closed(reason) => return Some(reason),
                _ => {}
            }
        }
    }

    /// A peer past ICE that has taken the client's first DTLS flight.
    fn handshaking(server: &Certificate, client: &Certificate) -> Harness {
        let mut h = Harness::honest(server, client);
        h.record = Some(Vec::new());
        let dtls = |h: &Harness| {
            h.record
                .iter()
                .flatten()
                .any(|datagram| matches!(datagram.first(), Some(20..=63)))
        };
        assert!(h.run(Duration::from_secs(5), dtls));
        assert!(!h.opened);
        h
    }

    #[test]
    fn mutated_datagrams_never_panic() {
        let (server, client) = certificates();
        let mut open = Harness::honest(&server, &client);
        open.record = Some(Vec::new());
        open.open();
        open.peer.send(pattern(100_000, 1)).unwrap();
        open.client.send(&pattern(100_000, 2));
        assert!(open.run(Duration::from_secs(5), |h| !h.received.is_empty()));
        let record = open.record.take().unwrap();
        let mut early = handshaking(&server, &client);
        let mut rng = 0x9e37_79b9_7f4a_7c15;
        let (mut panicked, mut closed) = (0, 0);
        for round in 0..10_000 {
            let mut datagram = record[below(&mut rng, record.len())].clone();
            mutate(&mut datagram, &mut rng);
            if let Some(ufrag) = stun_server_ufrag(&datagram) {
                assert!(datagram.len() > 20 + ufrag.len());
            }
            let target = if round % 2 == 0 {
                &mut open
            } else {
                &mut early
            };
            let Some(reason) = feed(target, &datagram) else {
                continue;
            };
            panicked += usize::from(matches!(reason, Error::Panicked));
            closed += 1;
            if round % 2 == 0 {
                open = Harness::honest(&server, &client);
                open.open();
            } else {
                early = handshaking(&server, &client);
            }
        }
        assert_eq!(panicked, 0);
        assert!(closed < 10_000, "every mutation closed the peer");
    }
}
