//! The reliability layer of one RakNet connection, without sockets or clocks: datagram sequence
//! numbers, ACK and NAK, resends on an RTT-based timeout, a congestion window, splitting and
//! reassembly, and ordering. The server listener and the tests' client both run on it.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tracing::trace;

use super::wire::{
    self, CHANNELS, Cursor, DATAGRAM_HEADER_BYTES, FLAG_ACK, FLAG_DATAGRAM, FLAG_NAK,
    FLAG_NEEDS_B_AND_AS, Header, MAX_FRAME_HEADER_BYTES, Reliability, Split, U24_MASK,
    UDP_HEADER_BYTES,
};
use super::{Error, MAX_UNSENT_BYTES, Result};

/// Pending ACK ranges that trigger an ACK before the next update.
const ACK_BATCH: usize = 64;
/// Delay a peer may add before it ACKs: go-raknet ACKs every 100 ms. Added to the smoothed RTT
/// in place of a smaller variation term (the clock granularity G of RFC 6298), so that samples
/// clustered at the ACK interval do not trigger spurious resends.
const ACK_DELAY: Duration = Duration::from_millis(100);
/// Datagram numbers remembered for dropping duplicates. A later arrival than this many newer
/// datagrams is dropped as a duplicate; its reliable frames come again in the resend. A number
/// this far ahead of the next one expected or more is dropped too.
const DATAGRAM_HISTORY: usize = 1024;
/// Memory one queued frame costs besides its payload, charged against `MAX_UNSENT_BYTES` so that
/// many tiny messages cannot exceed the cap's memory.
const FRAME_COST_BYTES: usize = size_of::<OutFrame>() + size_of::<OutMessage>();
/// Bytes held in reassemblies and ordering queues at most before `NewIncomingConnection`: every
/// message of the handshake fits one datagram and arrives about in order, so a few datagrams
/// cover reordering. Anyone can open a connection to this point (there is no cookie).
const HANDSHAKE_HELD_BYTES: usize = 4 * MAX_MTU as usize;
/// First congestion window, in datagrams carrying reliable frames.
const INITIAL_CWND: usize = 32;
/// Resend timeout before the first RTT sample (RFC 6298 uses 1 s; RakNet links are faster).
const INITIAL_RTO: Duration = Duration::from_millis(500);
/// Split messages being reassembled at once (go-raknet `maxConcurrentSplits`). Honest senders
/// finish one before most of the next arrives; only loss keeps a few open.
const MAX_ASSEMBLIES: usize = 16;
/// Datagrams sent per flush at most. Flushes run every update (10 ms) and after every received
/// datagram, so this paces a large window instead of bursting it into the peer's socket buffer
/// (go-raknet uses the OS default, which drops a burst of a few hundred datagrams). It still
/// allows about 3,200 datagrams (4.7 MB) per second from updates alone.
const MAX_BURST: usize = 32;
/// Reliable datagrams in flight at most (the resend window). go-raknet receivers accept a 2048
/// datagram gap; more in flight could exceed it after one loss.
const MAX_CWND: usize = 2048;
/// The largest MTU (go-raknet `maxMTUSize`), also the top of `max-mtu-size`.
pub(crate) const MAX_MTU: u16 = 1492;
/// Separate gaps in the reliable indices at most. Each costs a map entry; an honest sender has
/// at most one per frame it is resending.
const MAX_RELIABLE_GAPS: usize = 8192;
/// Highest resend timeout, well inside the idle timeout so a slow link still resends.
const MAX_RTO: Duration = Duration::from_secs(2);
/// How far a reliable index may run ahead, and how far the oldest missing reliable index may lag:
/// far inside the 2^23 where 24-bit serial order becomes ambiguous.
const MAX_SPAN: u32 = 1 << 22;
/// The lowest congestion window, so that a lossy link keeps moving.
const MIN_CWND: usize = 4;
/// The smallest full fragment: a datagram at `MIN_MTU` without IP/UDP, datagram and frame
/// headers. Dividing the message limit by it bounds the fragments of any honest sender.
const MIN_FRAGMENT_BYTES: usize =
    MIN_MTU as usize - UDP_HEADER_BYTES - DATAGRAM_HEADER_BYTES - MAX_FRAME_HEADER_BYTES;
/// The smallest MTU a client may negotiate: go-raknet's server floor (`minMTUSize`). Its dialer
/// never asks for less than 576; a request below 400 gets no reply instead of a silent clamp.
pub(super) const MIN_MTU: u16 = 400;
/// Lowest resend timeout.
const MIN_RTO: Duration = Duration::from_millis(100);
/// Order indices a message may run ahead of the next one due (go-raknet `maxWindowSize`).
const ORDER_WINDOW: u32 = 2048;
/// Memory one held fragment or early ordered message costs besides its payload: its map slot
/// and `Vec` header, with the allocator's smallest block for the payload. Charged to the held
/// bytes so that many tiny ones cannot exceed the cap's memory, as `FRAME_COST_BYTES` does
/// outbound.
const PART_COST_BYTES: usize = 64;

/// One end of a RakNet connection after the offline handshake.
#[derive(Debug)]
pub(super) struct Peer {
    mtu: u16,
    limits: Limits,
    last_received: Duration,
    received_any: bool,
    // Receiving.
    datagrams: Datagrams,
    reliable: Reliables,
    assemblies: BTreeMap<u16, Assembly>,
    channels: [Channel; CHANNELS],
    held: usize,
    acks: Vec<(u32, u32)>,
    naks: Vec<(u32, u32)>,
    // Sending.
    next_seq: u64,
    next_message: u32,
    next_order: u32,
    next_split: u16,
    queue: VecDeque<OutFrame>,
    resend: VecDeque<OutFrame>,
    in_flight: BTreeMap<u64, Sent>,
    unsent: usize,
    rtt: Rtt,
    cwnd: usize,
    ssthresh: usize,
    acked: usize,
    recovery: u64,
    buf: Vec<u8>,
}

impl Peer {
    pub(super) fn new(now: Duration, mtu: u16, limits: Limits) -> Result<Self> {
        if !(MIN_MTU..=MAX_MTU).contains(&mtu) {
            return Err(Error::Malformed("mtu"));
        }
        Ok(Self {
            mtu,
            limits,
            last_received: now,
            received_any: false,
            datagrams: Datagrams::default(),
            reliable: Reliables::default(),
            assemblies: BTreeMap::new(),
            channels: std::array::from_fn(|_| Channel::default()),
            held: 0,
            acks: Vec::new(),
            naks: Vec::new(),
            next_seq: 0,
            next_message: 0,
            next_order: 0,
            next_split: 0,
            queue: VecDeque::new(),
            resend: VecDeque::new(),
            in_flight: BTreeMap::new(),
            unsent: 0,
            rtt: Rtt::default(),
            cwnd: INITIAL_CWND,
            ssthresh: MAX_CWND,
            acked: 0,
            recovery: 0,
            buf: Vec::with_capacity(usize::from(mtu)),
        })
    }

    pub(super) fn mtu(&self) -> u16 {
        self.mtu
    }

    pub(super) fn last_received(&self) -> Duration {
        self.last_received
    }

    /// Whether any datagram, ACK or NAK arrived yet.
    pub(super) fn has_received_any(&self) -> bool {
        self.received_any
    }

    /// The smoothed round-trip time, zero before the first sample.
    pub(super) fn rtt(&self) -> Duration {
        self.rtt.smoothed.unwrap_or_default()
    }

    /// Bytes queued, waiting for a resend or unacknowledged, with the frame cost
    /// `MAX_UNSENT_BYTES` counts.
    pub(super) fn unsent(&self) -> usize {
        self.unsent
    }

    /// Whether nothing is queued, waiting for a resend or unacknowledged.
    pub(super) fn is_settled(&self) -> bool {
        self.queue.is_empty() && self.resend.is_empty() && self.in_flight.is_empty()
    }

    /// Replaces the receive limits. Bytes already held count against the new ones.
    pub(super) fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Handles one connected packet (data datagram, ACK or NAK) and appends the messages it
    /// completes to `out`, in delivery order. NAKs go out at once, ACKs in batches. A malformed
    /// ACK or NAK, or a datagram number too far ahead, drops that datagram only.
    pub(super) fn receive(
        &mut self,
        now: Duration,
        datagram: &[u8],
        out: &mut Vec<Message>,
        transmit: &mut impl FnMut(&[u8]),
    ) -> Result<()> {
        let mut cur = Cursor::new(datagram);
        let flags = cur.u8()?;
        if flags & FLAG_ACK != 0 {
            let acknowledged =
                wire::read_ranges(cur.rest(), |first, last| self.acknowledge(now, first, last));
            return self.heard_ranges(now, acknowledged);
        }
        if flags & FLAG_NAK != 0 {
            let negated = wire::read_ranges(cur.rest(), |first, last| self.negative(first, last));
            return self.heard_ranges(now, negated);
        }
        if flags & FLAG_DATAGRAM == 0 {
            return Err(Error::Malformed("not a datagram"));
        }
        let seq = cur.u24()?;
        match self.datagrams.insert(seq) {
            Arrival::Jump => {
                trace!(seq, "raknet datagram number too far ahead; dropped");
                return Ok(());
            }
            Arrival::Duplicate => {
                self.hear(now);
                return Ok(());
            }
            Arrival::New { gap } => {
                self.hear(now);
                push_index(&mut self.acks, seq);
                if let Some(gap) = gap {
                    self.naks.push(gap);
                }
            }
        }
        while !cur.is_empty() {
            let (header, body) = Header::read(&mut cur)?;
            self.frame(header, body, out)?;
        }
        self.flush_ranges(FLAG_NAK, transmit);
        if self.acks.len() >= ACK_BATCH {
            self.flush_ranges(FLAG_ACK, transmit);
        }
        Ok(())
    }

    /// Queues a message, to be sent reliably and in order; a message larger than one datagram is
    /// split.
    pub(super) fn send(&mut self, id: u8, body: Vec<u8>) -> Result<()> {
        let reliability = Reliability::ReliableOrdered;
        let total = body.len().saturating_add(1);
        let room = self.max_datagram().saturating_sub(DATAGRAM_HEADER_BYTES);
        let whole = total + Header::encoded_len(reliability, false) <= room;
        let piece = if whole {
            total
        } else {
            room.saturating_sub(Header::encoded_len(reliability, true))
        };
        let count = total.div_ceil(piece.max(1));
        let cost = count
            .checked_mul(FRAME_COST_BYTES)
            .and_then(|cost| cost.checked_add(total))
            .and_then(|cost| cost.checked_add(self.unsent))
            .filter(|&unsent| unsent <= MAX_UNSENT_BYTES)
            .ok_or(Error::Limit("outbound bytes"))?;
        let split_count = u32::try_from(count).map_err(|_| Error::Limit("outbound bytes"))?;
        let order_index = self.next_order;
        self.next_order = order_index.wrapping_add(1) & U24_MASK;
        let split_id = (!whole).then(|| {
            let id = self.next_split;
            self.next_split = id.wrapping_add(1);
            id
        });
        let message = Arc::new(OutMessage { id, body });
        self.queue.reserve(count);
        for (index, start) in (0..split_count).zip((0..total).step_by(piece.max(1))) {
            let len = piece.min(total - start);
            let message_index = self.next_message;
            self.next_message = message_index.wrapping_add(1) & U24_MASK;
            self.queue.push_back(OutFrame {
                header: Header {
                    reliability,
                    message_index,
                    sequence_index: 0,
                    order_index,
                    channel: 0,
                    split: split_id.map(|id| Split {
                        count: split_count,
                        id,
                        index,
                    }),
                },
                message: Arc::clone(&message),
                start,
                len: u16::try_from(len).map_err(|_| Error::Invariant("fragment length"))?,
            });
        }
        self.unsent = cost;
        Ok(())
    }

    /// Moves datagrams unacknowledged past the resend timeout back to the resend queue, and
    /// doubles the timeout once per round of losses a timeout starts (a NAK's round does not):
    /// the other datagrams sent before that round share its doubling, or one update's many
    /// timeouts would jump straight to `MAX_RTO`.
    pub(super) fn update(&mut self, now: Duration) {
        let timeout = self.rtt.timeout();
        while let Some(entry) = self.in_flight.first_entry() {
            if now.saturating_sub(entry.get().at) < timeout {
                break;
            }
            let seq = *entry.key();
            // Checked before `lost` starts the next round.
            if seq >= self.recovery {
                self.rtt.back_off();
            }
            self.resend.extend(entry.remove().frames);
            self.lost(seq);
        }
    }

    /// Sends queued frames, resends first, as far as the congestion window allows and at most
    /// `MAX_BURST` datagrams per call.
    pub(super) fn flush(&mut self, now: Duration, transmit: &mut impl FnMut(&[u8])) -> Result<()> {
        let max = self.max_datagram();
        for _ in 0..MAX_BURST {
            if self.in_flight.len() >= self.cwnd
                || (self.resend.is_empty() && self.queue.is_empty())
            {
                break;
            }
            self.buf.clear();
            self.buf.push(FLAG_DATAGRAM | FLAG_NEEDS_B_AND_AS);
            wire::put_u24(&mut self.buf, wire::low24(self.next_seq));
            let mut frames = Vec::new();
            while let Some(frame) = self.pop_fitting(max.saturating_sub(self.buf.len())) {
                frame.header.write(&mut self.buf, frame.len)?;
                frame
                    .message
                    .put(&mut self.buf, frame.start, usize::from(frame.len))?;
                frames.push(frame);
            }
            if self.buf.len() == DATAGRAM_HEADER_BYTES {
                return Err(Error::Invariant("frame larger than a datagram"));
            }
            transmit(&self.buf);
            self.in_flight
                .insert(self.next_seq, Sent { at: now, frames });
            self.next_seq += 1;
        }
        Ok(())
    }

    /// Sends the pending ACKs.
    pub(super) fn flush_acks(&mut self, transmit: &mut impl FnMut(&[u8])) {
        self.flush_ranges(FLAG_ACK, transmit);
    }

    /// Sends one small unreliable message at once, outside the queue and the window (the
    /// connected ping and pong, and the disconnect notification): it must neither wait behind
    /// reliable data nor be dropped because some waits.
    pub(super) fn send_now(
        &mut self,
        id: u8,
        body: &[u8],
        transmit: &mut impl FnMut(&[u8]),
    ) -> Result<()> {
        self.buf.clear();
        self.buf.push(FLAG_DATAGRAM | FLAG_NEEDS_B_AND_AS);
        wire::put_u24(&mut self.buf, wire::low24(self.next_seq));
        let header = Header {
            reliability: Reliability::Unreliable,
            message_index: 0,
            sequence_index: 0,
            order_index: 0,
            channel: 0,
            split: None,
        };
        let len = u16::try_from(body.len().saturating_add(1))
            .map_err(|_| Error::Invariant("message longer than a datagram"))?;
        header.write(&mut self.buf, len)?;
        self.buf.push(id);
        self.buf.extend_from_slice(body);
        transmit(&self.buf);
        self.next_seq += 1;
        Ok(())
    }

    fn max_datagram(&self) -> usize {
        usize::from(self.mtu).saturating_sub(UDP_HEADER_BYTES)
    }

    /// The datagrams an inclusive 24-bit range of our sequence numbers can name, as the lowest
    /// and highest number; `None` before any datagram was sent or when the range lies before the
    /// oldest number 24 bits can still tell apart.
    fn sent_range(&self, first: u32, last: u32) -> Result<Option<(u64, u64)>> {
        let len = wire::u24_ahead(last, first).ok_or(Error::Malformed("acknowledgement range"))?;
        let Some(newest) = self.next_seq.checked_sub(1) else {
            return Ok(None);
        };
        let back = wire::low24(newest).wrapping_sub(first) & U24_MASK;
        Ok(newest
            .checked_sub(u64::from(back))
            .map(|low| (low, low.saturating_add(u64::from(len)))))
    }

    /// Ends the handling of an ACK or NAK whose ranges gave `result`. A malformed one is dropped
    /// and does not count as heard: nothing authenticates a datagram, so anyone who knows the
    /// client's address could send one to close its connection or keep it from timing out.
    fn heard_ranges(&mut self, now: Duration, result: Result<()>) -> Result<()> {
        if let Err(Error::Malformed(what)) = result {
            trace!(what, "raknet malformed acknowledgement dropped");
            return Ok(());
        }
        self.hear(now);
        result
    }

    /// Counts the datagram just accepted as proof the peer is alive, for the idle timeout.
    fn hear(&mut self, now: Duration) {
        self.last_received = now;
        self.received_any = true;
    }

    fn frame(&mut self, header: Header, body: &[u8], out: &mut Vec<Message>) -> Result<()> {
        if header.reliability.is_reliable() && !self.reliable.insert(header.message_index)? {
            return Ok(());
        }
        let (header, message) = if let Some(split) = header.split {
            let Some(done) = self.reassemble(header, split, body)? else {
                return Ok(());
            };
            done
        } else {
            let (&id, body) = body.split_first().ok_or(Error::Malformed("empty frame"))?;
            let message = Message {
                id,
                body: body.to_vec(),
            };
            (header, message)
        };
        self.order(header, message, out)
    }

    fn reassemble(
        &mut self,
        header: Header,
        split: Split,
        body: &[u8],
    ) -> Result<Option<(Header, Message)>> {
        if split.count == 0 || split.count > self.limits.splits {
            return Err(Error::Limit("split count"));
        }
        if split.index >= split.count {
            return Err(Error::Malformed("split index"));
        }
        let open = self.assemblies.len();
        let assembly = match self.assemblies.entry(split.id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                if open >= MAX_ASSEMBLIES {
                    return Err(Error::Limit("concurrent split messages"));
                }
                entry.insert(Assembly {
                    header,
                    count: split.count,
                    bytes: 0,
                    parts: BTreeMap::new(),
                })
            }
        };
        if assembly.count != split.count {
            return Err(Error::Malformed("split count changed"));
        }
        if assembly.parts.contains_key(&split.index) {
            return Ok(None);
        }
        let bytes = assembly.bytes.saturating_add(body.len());
        if bytes > self.limits.message {
            return Err(Error::Limit("message size"));
        }
        let held = self
            .held
            .saturating_add(body.len())
            .saturating_add(PART_COST_BYTES);
        if held > self.limits.held {
            return Err(Error::Limit("held bytes"));
        }
        assembly.parts.insert(split.index, body.to_vec());
        assembly.bytes = bytes;
        self.held = held;
        if u32::try_from(assembly.parts.len()) != Ok(assembly.count) {
            return Ok(None);
        }
        let Some(assembly) = self.assemblies.remove(&split.id) else {
            return Err(Error::Invariant("finished split message missing"));
        };
        let parts_cost = assembly.parts.len().saturating_mul(PART_COST_BYTES);
        self.held = self
            .held
            .saturating_sub(assembly.bytes.saturating_add(parts_cost));
        let mut parts = assembly.parts.into_values();
        let first = parts.next().unwrap_or_default();
        let (&id, rest) = first.split_first().ok_or(Error::Malformed("empty frame"))?;
        let mut body = Vec::with_capacity(assembly.bytes.saturating_sub(1));
        body.extend_from_slice(rest);
        for part in parts {
            body.extend_from_slice(&part);
        }
        Ok(Some((assembly.header, Message { id, body })))
    }

    fn order(&mut self, header: Header, message: Message, out: &mut Vec<Message>) -> Result<()> {
        if !header.reliability.has_order() {
            out.push(message);
            return Ok(());
        }
        let channel = self
            .channels
            .get_mut(usize::from(header.channel))
            .ok_or(Error::Malformed("ordering channel"))?;
        if header.reliability.is_sequenced() {
            // Sequenced frames belong to the current order index; older or stale ones drop.
            if header.order_index == channel.next
                && wire::u24_ahead(header.sequence_index, channel.sequence).is_some()
            {
                channel.sequence = header.sequence_index.wrapping_add(1) & U24_MASK;
                out.push(message);
            }
            return Ok(());
        }
        let Some(ahead) = wire::u24_ahead(header.order_index, channel.next) else {
            return Ok(());
        };
        if ahead >= ORDER_WINDOW {
            return Err(Error::Limit("ordering window"));
        }
        if ahead > 0 {
            if let Entry::Vacant(entry) = channel.waiting.entry(header.order_index) {
                let held = self
                    .held
                    .saturating_add(message.body.len())
                    .saturating_add(PART_COST_BYTES);
                if held > self.limits.held {
                    return Err(Error::Limit("held bytes"));
                }
                self.held = held;
                entry.insert(message);
            }
            return Ok(());
        }
        out.push(message);
        loop {
            channel.next = channel.next.wrapping_add(1) & U24_MASK;
            channel.sequence = 0;
            let Some(message) = channel.waiting.remove(&channel.next) else {
                return Ok(());
            };
            self.held = self
                .held
                .saturating_sub(message.body.len().saturating_add(PART_COST_BYTES));
            out.push(message);
        }
    }

    fn acknowledge(&mut self, now: Duration, first: u32, last: u32) -> Result<()> {
        let Some((mut low, high)) = self.sent_range(first, last)? else {
            return Ok(());
        };
        // `range` panics on an empty range, so the bound is checked first.
        while low <= high
            && let Some(seq) = self.in_flight.range(low..=high).next().map(|(&seq, _)| seq)
        {
            low = seq.saturating_add(1);
            let Some(sent) = self.in_flight.remove(&seq) else {
                break;
            };
            self.rtt.sample(now.saturating_sub(sent.at));
            for frame in &sent.frames {
                self.unsent = self.unsent.saturating_sub(frame.cost());
            }
            if self.cwnd < self.ssthresh {
                self.cwnd += 1;
            } else {
                self.acked += 1;
                if self.acked >= self.cwnd {
                    self.acked = 0;
                    self.cwnd += 1;
                }
            }
            self.cwnd = self.cwnd.min(MAX_CWND);
        }
        Ok(())
    }

    fn negative(&mut self, first: u32, last: u32) -> Result<()> {
        let Some((mut low, high)) = self.sent_range(first, last)? else {
            return Ok(());
        };
        // `range` panics on an empty range, so the bound is checked first.
        while low <= high
            && let Some(seq) = self.in_flight.range(low..=high).next().map(|(&seq, _)| seq)
        {
            low = seq.saturating_add(1);
            if let Some(sent) = self.in_flight.remove(&seq) {
                self.resend.extend(sent.frames);
                self.lost(seq);
            }
        }
        Ok(())
    }

    /// Shrinks the window to 3/4 once per round trip of losses. Halving (Reno) kept the window
    /// near 50 datagrams against go-raknet, whose 100 ms ACK interval makes additive growth slow:
    /// a largest message took 30 s on loopback, 6 to 14 s with 3/4 (CUBIC uses 0.7).
    fn lost(&mut self, seq: u64) {
        if seq >= self.recovery {
            self.ssthresh = (self.cwnd * 3 / 4).max(MIN_CWND);
            self.cwnd = self.ssthresh;
            self.recovery = self.next_seq;
        }
    }

    fn pop_fitting(&mut self, room: usize) -> Option<OutFrame> {
        if let Some(frame) = self.resend.front() {
            return (frame.encoded_len() <= room)
                .then(|| self.resend.pop_front())
                .flatten();
        }
        self.queue
            .front()
            .is_some_and(|frame| frame.encoded_len() <= room)
            .then(|| self.queue.pop_front())
            .flatten()
    }

    fn flush_ranges(&mut self, flag: u8, transmit: &mut impl FnMut(&[u8])) {
        let max = self.max_datagram();
        let ranges = if flag == FLAG_ACK {
            &mut self.acks
        } else {
            &mut self.naks
        };
        let mut rest = ranges.as_slice();
        while !rest.is_empty() {
            self.buf.clear();
            let written = wire::put_ranges(&mut self.buf, flag, rest, max);
            rest = rest.get(written..).unwrap_or_default();
            transmit(&self.buf);
        }
        ranges.clear();
    }
}

/// Receive limits derived from the largest message.
#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    /// Largest message in bytes, its first byte included.
    message: usize,
    /// Most fragments one message may declare: `message / MIN_FRAGMENT_BYTES`, rounded up.
    splits: u32,
    /// Bytes in unfinished reassemblies and ordering queues together, `PART_COST_BYTES` included
    /// per entry: two largest messages in the smallest full fragments and a full send window, so
    /// a largest message may still be missing a fragment while the next one arrives.
    held: usize,
}

impl Limits {
    /// Limits for messages of at most `max_message_bytes` bytes after their first byte.
    pub(super) fn new(max_message_bytes: usize) -> Result<Self> {
        let invalid = Error::Limit("max message size");
        let message = max_message_bytes.checked_add(1).ok_or(invalid)?;
        let splits = u32::try_from(message.div_ceil(MIN_FRAGMENT_BYTES)).map_err(|_| invalid)?;
        if splits >= MAX_SPAN {
            return Err(invalid);
        }
        let parts = usize::try_from(splits)
            .map_err(|_| invalid)?
            .checked_mul(2 * PART_COST_BYTES)
            .ok_or(invalid)?;
        let held = message
            .checked_mul(2)
            .and_then(|held| held.checked_add(MAX_CWND * usize::from(MAX_MTU)))
            .and_then(|held| held.checked_add(parts))
            .ok_or(invalid)?;
        Ok(Self {
            message,
            splits,
            held,
        })
    }

    /// These limits for a connection that has not finished the handshake: held bytes cut to
    /// [`HANDSHAKE_HELD_BYTES`].
    pub(super) fn to_handshake(self) -> Self {
        Self {
            held: self.held.min(HANDSHAKE_HELD_BYTES),
            ..self
        }
    }
}

/// A whole received message: its first byte (a RakNet message ID, or 0xfe for game data) and
/// the rest.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Message {
    pub(super) id: u8,
    pub(super) body: Vec<u8>,
}

/// Recently received datagram numbers. A skipped number gets a NAK at once and is not waited
/// for: the sender resends its frames under a new number (go-raknet also forgets a number once
/// it sent a NAK for it).
/// `recent[i]` is number `next - recent.len() + i`.
#[derive(Debug, Default)]
struct Datagrams {
    next: u32,
    recent: VecDeque<bool>,
}

impl Datagrams {
    /// Records datagram number `seq` as received. A number further ahead than the history holds
    /// is not recorded: the datagram is dropped and the numbers stay where they were, so a
    /// datagram nobody can vouch for cannot push the real ones out of the window.
    fn insert(&mut self, seq: u32) -> Arrival {
        let Some(ahead) = wire::u24_ahead(seq, self.next) else {
            let back = self.next.wrapping_sub(1).wrapping_sub(seq) & U24_MASK;
            let len = self.recent.len();
            let Some(at) = usize::try_from(back)
                .ok()
                .and_then(|back| len.checked_sub(back + 1))
            else {
                return Arrival::Duplicate;
            };
            return match self.recent.get_mut(at) {
                Some(seen) if !*seen => {
                    *seen = true;
                    Arrival::New { gap: None }
                }
                _ => Arrival::Duplicate,
            };
        };
        let skipped = match usize::try_from(ahead) {
            Ok(skipped) if skipped < DATAGRAM_HISTORY => skipped,
            _ => return Arrival::Jump,
        };
        let gap = (ahead > 0).then(|| (self.next, seq.wrapping_sub(1) & U24_MASK));
        let keep = DATAGRAM_HISTORY - 1 - skipped;
        if let Some(excess) = self.recent.len().checked_sub(keep) {
            self.recent.drain(..excess);
        }
        self.recent.extend(std::iter::repeat_n(false, skipped));
        self.recent.push_back(true);
        self.next = seq.wrapping_add(1) & U24_MASK;
        Arrival::New { gap }
    }
}

enum Arrival {
    /// Further ahead than the history holds.
    Jump,
    Duplicate,
    /// New; `gap` is the inclusive range of numbers this arrival skipped over.
    New {
        gap: Option<(u32, u32)>,
    },
}

/// Received reliable message indices: all below `next` except the `missing` ranges, which the
/// sender resends under the same index until they are acknowledged. Indices are unwrapped.
#[derive(Debug, Default)]
struct Reliables {
    next: u64,
    missing: BTreeMap<u64, u64>,
}

impl Reliables {
    /// Records `index` as received; `false` when it was received before.
    fn insert(&mut self, index: u32) -> Result<bool> {
        let base = wire::low24(self.next);
        let index = if let Some(ahead) = wire::u24_ahead(index, base) {
            if ahead >= MAX_SPAN {
                return Err(Error::Limit("reliable index jump"));
            }
            self.next.saturating_add(u64::from(ahead))
        } else {
            let back = u64::from(base.wrapping_sub(index) & U24_MASK);
            let Some(index) = self.next.checked_sub(back) else {
                return Ok(false);
            };
            index
        };
        if index >= self.next {
            if index > self.next {
                self.missing.insert(self.next, index - 1);
            }
            self.next = index.saturating_add(1);
        } else {
            let Some((&first, &last)) = self.missing.range(..=index).next_back() else {
                return Ok(false);
            };
            if index > last {
                return Ok(false);
            }
            self.missing.remove(&first);
            if first < index {
                self.missing.insert(first, index - 1);
            }
            if index < last {
                self.missing.insert(index.saturating_add(1), last);
            }
        }
        if self.missing.len() > MAX_RELIABLE_GAPS {
            return Err(Error::Limit("reliable index gaps"));
        }
        if let Some((&oldest, _)) = self.missing.first_key_value()
            && self.next.saturating_sub(oldest) > u64::from(MAX_SPAN)
        {
            return Err(Error::Limit("reliable index span"));
        }
        Ok(true)
    }
}

/// A split message being reassembled. Parts live in a map, so a large declared count allocates
/// nothing until fragments arrive.
#[derive(Debug)]
struct Assembly {
    header: Header,
    count: u32,
    bytes: usize,
    parts: BTreeMap<u32, Vec<u8>>,
}

/// One ordering channel: the next order index due, the next sequence index accepted for
/// sequenced frames, and ordered messages that arrived early.
#[derive(Debug, Default)]
struct Channel {
    next: u32,
    sequence: u32,
    waiting: BTreeMap<u32, Message>,
}

/// A frame waiting to be sent, or sent and waiting for its ACK.
#[derive(Debug)]
struct OutFrame {
    header: Header,
    message: Arc<OutMessage>,
    start: usize,
    len: u16,
}

impl OutFrame {
    fn encoded_len(&self) -> usize {
        Header::encoded_len(self.header.reliability, self.header.split.is_some())
            + usize::from(self.len)
    }

    fn cost(&self) -> usize {
        usize::from(self.len) + FRAME_COST_BYTES
    }
}

/// A message to send. It is shared by its fragments and freed with the last one.
#[derive(Debug)]
struct OutMessage {
    id: u8,
    body: Vec<u8>,
}

impl OutMessage {
    /// Appends bytes `start..start + len` of the message (its ID byte, then its body).
    fn put(&self, buf: &mut Vec<u8>, start: usize, len: usize) -> Result<()> {
        let end = start
            .checked_add(len)
            .and_then(|end| end.checked_sub(1))
            .ok_or(Error::Invariant("empty fragment"))?;
        let from = if let Some(from) = start.checked_sub(1) {
            from
        } else {
            buf.push(self.id);
            0
        };
        let part = self
            .body
            .get(from..end)
            .ok_or(Error::Invariant("fragment outside its message"))?;
        buf.extend_from_slice(part);
        Ok(())
    }
}

/// A datagram in flight with its reliable frames.
#[derive(Debug)]
struct Sent {
    at: Duration,
    frames: Vec<OutFrame>,
}

/// Round-trip time estimate from acknowledged datagrams (RFC 6298). Every datagram is sent once (a
/// resend gets a new sequence number), so every ACK is a clean sample. An ACK after the timeout
/// finds its datagram gone and gives no sample, so a round trip above the timeout could never
/// raise it: each round of timeouts doubles it instead (RFC 6298 backoff), until ACKs come back
/// in time and the next sample resets it.
#[derive(Debug, Default)]
struct Rtt {
    smoothed: Option<Duration>,
    variation: Duration,
    /// Doublings of the timeout since the last sample. The timeout stops doubling at `MAX_RTO`,
    /// so this stays below 6 (`MIN_RTO` doubled 5 times passes it).
    backoff: u32,
}

impl Rtt {
    fn timeout(&self) -> Duration {
        let base = self.smoothed.map_or(INITIAL_RTO, |smoothed| {
            smoothed
                .saturating_add(self.variation.saturating_mul(4).max(ACK_DELAY))
                .clamp(MIN_RTO, MAX_RTO)
        });
        base.saturating_mul(1 << self.backoff).min(MAX_RTO)
    }

    fn sample(&mut self, rtt: Duration) {
        self.backoff = 0;
        if let Some(smoothed) = self.smoothed {
            self.variation = self
                .variation
                .saturating_mul(3)
                .saturating_add(smoothed.abs_diff(rtt))
                / 4;
            self.smoothed = Some(smoothed.saturating_mul(7).saturating_add(rtt) / 8);
        } else {
            self.smoothed = Some(rtt);
            self.variation = rtt / 2;
        }
    }

    fn back_off(&mut self) {
        if self.timeout() < MAX_RTO {
            self.backoff += 1;
        }
    }
}

/// Adds `index` to ascending inclusive ranges, extending the last range when it follows it.
fn push_index(ranges: &mut Vec<(u32, u32)>, index: u32) {
    if let Some(last) = ranges.last_mut()
        && last.1.wrapping_add(1) & U24_MASK == index
    {
        last.1 = index;
    } else {
        ranges.push((index, index));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;
    /// A small message limit: 1001 bytes with the ID, 3 fragments, and held bytes for two such
    /// messages besides a full send window.
    const SMALL: usize = 1000;
    /// A message limit of about 19 MiB, as large as the default settings give.
    const DEFAULT_MESSAGE: usize = 19_573_468;

    pub(in crate::raknet) fn header(reliability: Reliability) -> Header {
        Header {
            reliability,
            message_index: 0,
            sequence_index: 0,
            order_index: 0,
            channel: 0,
            split: None,
        }
    }

    /// A data datagram with sequence number `seq` holding `frames`.
    pub(in crate::raknet) fn datagram(seq: u32, frames: &[(Header, &[u8])]) -> Vec<u8> {
        let mut buf = vec![FLAG_DATAGRAM | FLAG_NEEDS_B_AND_AS];
        wire::put_u24(&mut buf, seq);
        for (header, body) in frames {
            header
                .write(&mut buf, u16::try_from(body.len()).unwrap())
                .unwrap();
            buf.extend_from_slice(body);
        }
        buf
    }

    fn peer(max_message_bytes: usize) -> Peer {
        Peer::new(T0, MAX_MTU, Limits::new(max_message_bytes).unwrap()).unwrap()
    }

    fn receive(peer: &mut Peer, data: &[u8]) -> Result<Vec<Message>> {
        let mut out = Vec::new();
        peer.receive(T0, data, &mut out, &mut |_| {})?;
        Ok(out)
    }

    fn fragment(message_index: u32, id: u16, count: u32, index: u32) -> Header {
        Header {
            message_index,
            split: Some(Split { count, id, index }),
            ..header(Reliability::ReliableOrdered)
        }
    }

    fn ordered(message_index: u32, order_index: u32) -> Header {
        Header {
            message_index,
            order_index,
            ..header(Reliability::ReliableOrdered)
        }
    }

    #[test]
    fn limits_follow_the_message_size() {
        let limits = Limits::new(DEFAULT_MESSAGE).unwrap();
        assert_eq!(limits.message, DEFAULT_MESSAGE + 1);
        assert_eq!(MIN_FRAGMENT_BYTES, 345);
        assert_eq!(
            limits.splits,
            u32::try_from((DEFAULT_MESSAGE + 1).div_ceil(345)).unwrap()
        );
        assert_eq!(
            limits.held,
            2 * (DEFAULT_MESSAGE + 1)
                + 2048 * 1492
                + 2 * usize::try_from(limits.splits).unwrap() * PART_COST_BYTES
        );
        assert!(Limits::new(usize::MAX).is_err());
        assert!(Limits::new(1 << 40).is_err());
    }

    #[test]
    fn mtu_outside_the_supported_range_is_refused() {
        let limits = Limits::new(SMALL).unwrap();
        assert!(Peer::new(T0, MIN_MTU - 1, limits).is_err());
        assert!(Peer::new(T0, MAX_MTU + 1, limits).is_err());
        assert!(Peer::new(T0, MIN_MTU, limits).is_ok());
    }

    #[test]
    fn split_count_over_the_limit_is_an_error() {
        let mut peer = peer(SMALL);
        let over = datagram(0, &[(fragment(0, 0, 4, 0), b"x")]);
        assert_eq!(receive(&mut peer, &over), Err(Error::Limit("split count")));
        let mut peer = self::peer(SMALL);
        let zero = datagram(0, &[(fragment(0, 0, 0, 0), b"x")]);
        assert_eq!(receive(&mut peer, &zero), Err(Error::Limit("split count")));
        let mut peer = self::peer(SMALL);
        let index = datagram(0, &[(fragment(0, 0, 3, 3), b"x")]);
        assert_eq!(
            receive(&mut peer, &index),
            Err(Error::Malformed("split index"))
        );
    }

    #[test]
    fn huge_declared_split_count_allocates_nothing_up_front() {
        // The largest count a 20 MiB message allows: the parts map holds only what arrived.
        let mut peer = peer(20 << 20);
        let count = peer.limits.splits;
        let data = datagram(0, &[(fragment(0, 0, count, 5), b"x")]);
        assert_eq!(receive(&mut peer, &data), Ok(Vec::new()));
        assert_eq!(peer.assemblies.get(&0).map(|a| a.parts.len()), Some(1));
    }

    #[test]
    fn more_concurrent_split_messages_than_the_limit_is_an_error() {
        let mut peer = peer(SMALL);
        for id in 0..16 {
            let index = u32::from(id);
            let data = datagram(index, &[(fragment(index, id, 2, 0), b"x")]);
            assert_eq!(receive(&mut peer, &data), Ok(Vec::new()));
        }
        let data = datagram(16, &[(fragment(16, 16, 2, 0), b"x")]);
        assert_eq!(
            receive(&mut peer, &data),
            Err(Error::Limit("concurrent split messages"))
        );
    }

    #[test]
    fn reassembled_message_over_the_limit_is_an_error() {
        let mut peer = peer(SMALL);
        let part = [0xfe; 400];
        for index in 0..2 {
            let data = datagram(index, &[(fragment(index, 0, 3, index), &part)]);
            assert_eq!(receive(&mut peer, &data), Ok(Vec::new()));
        }
        let data = datagram(2, &[(fragment(2, 0, 3, 2), &part)]);
        assert_eq!(receive(&mut peer, &data), Err(Error::Limit("message size")));
    }

    #[test]
    fn split_count_that_changes_is_an_error() {
        let mut peer = peer(SMALL);
        let first = datagram(0, &[(fragment(0, 0, 3, 0), b"x")]);
        assert_eq!(receive(&mut peer, &first), Ok(Vec::new()));
        let second = datagram(1, &[(fragment(1, 0, 2, 1), b"x")]);
        assert_eq!(
            receive(&mut peer, &second),
            Err(Error::Malformed("split count changed"))
        );
    }

    #[test]
    fn held_bytes_over_the_limit_is_an_error() {
        // 2 x 1,000,001 + 2048 x 1492 + 2 x 2899 x 64 bytes: the sixth unfinished message of
        // 714 x (1400 + 64) bytes goes over.
        let mut peer = peer(1_000_000);
        let part = [0xfe; 1400];
        let mut index = 0;
        let error = 'sets: {
            for id in 0..16 {
                for fragment_index in 0..714 {
                    let header = fragment(index, id, 715, fragment_index);
                    let data = datagram(index, &[(header, &part)]);
                    index += 1;
                    if let Err(error) = receive(&mut peer, &data) {
                        break 'sets (id, error);
                    }
                }
            }
            panic!("no error");
        };
        assert_eq!(error, (5, Error::Limit("held bytes")));
    }

    /// Before the handshake is done a peer holds `HANDSHAKE_HELD_BYTES` at most, and the full
    /// limits come back with `set_limits`.
    #[test]
    fn a_handshaking_peer_holds_only_a_few_datagrams() {
        let limits = Limits::new(DEFAULT_MESSAGE).unwrap();
        assert!(limits.held > HANDSHAKE_HELD_BYTES);
        let mut peer = Peer::new(T0, MAX_MTU, limits.to_handshake()).unwrap();
        let part = [0xfe; 1000];
        let unreliable = |index| Header {
            split: Some(Split {
                count: 1000,
                id: 0,
                index,
            }),
            ..header(Reliability::Unreliable)
        };
        let mut index = 0;
        let error = loop {
            let data = datagram(index, &[(unreliable(index), &part)]);
            if let Err(error) = receive(&mut peer, &data) {
                break error;
            }
            assert!(peer.held <= HANDSHAKE_HELD_BYTES);
            index += 1;
        };
        assert_eq!(error, Error::Limit("held bytes"));
        assert_eq!(
            usize::try_from(index).unwrap(),
            HANDSHAKE_HELD_BYTES / (part.len() + PART_COST_BYTES)
        );
        peer.set_limits(limits);
        let data = datagram(index + 1, &[(unreliable(index), &part)]);
        assert_eq!(receive(&mut peer, &data), Ok(Vec::new()));
    }

    #[test]
    fn tiny_fragments_count_against_held_bytes() {
        // Every assembly declares the largest count and misses one fragment, so none finishes:
        // 1-byte fragments cost far more memory than their payload.
        let mut peer = peer(DEFAULT_MESSAGE);
        let count = peer.limits.splits;
        let unreliable = |id, index| Header {
            split: Some(Split { count, id, index }),
            ..header(Reliability::Unreliable)
        };
        let mut seq = 0;
        let mut stored = 0_usize;
        let error = 'fill: {
            for id in 0..16 {
                for first in (1..count).step_by(100) {
                    let frames: Vec<_> = (first..count.min(first + 100))
                        .map(|index| (unreliable(id, index), &b"\xfe"[..]))
                        .collect();
                    let data = datagram(seq, &frames);
                    seq += 1;
                    let result = receive(&mut peer, &data);
                    assert!(peer.held <= peer.limits.held);
                    match result {
                        Ok(_) => stored += frames.len(),
                        Err(error) => break 'fill error,
                    }
                }
            }
            panic!("{stored} one-byte fragments held without an error");
        };
        assert_eq!(error, Error::Limit("held bytes"));
        assert!(
            stored <= peer.limits.held / PART_COST_BYTES,
            "{stored} fragments held"
        );
    }

    #[test]
    fn honest_largest_messages_still_fit_with_part_cost() {
        // Two largest messages in the smallest full fragments, each missing its last one.
        let mut peer = peer(DEFAULT_MESSAGE);
        let count = peer.limits.splits;
        let full = [0xfe; MIN_FRAGMENT_BYTES];
        let last = peer.limits.message - MIN_FRAGMENT_BYTES * (usize::try_from(count).unwrap() - 1);
        let unreliable = |id, index| Header {
            split: Some(Split { count, id, index }),
            ..header(Reliability::Unreliable)
        };
        let mut seq = 0;
        for id in 0..2 {
            for first in (0..count - 1).step_by(4) {
                let frames: Vec<_> = (first..(count - 1).min(first + 4))
                    .map(|index| (unreliable(id, index), &full[..]))
                    .collect();
                assert_eq!(receive(&mut peer, &datagram(seq, &frames)), Ok(Vec::new()));
                seq += 1;
            }
        }
        for id in 0..2 {
            let data = datagram(seq, &[(unreliable(id, count - 1), &full[..last])]);
            seq += 1;
            let done = receive(&mut peer, &data).unwrap();
            assert_eq!(done.len(), 1);
            assert_eq!(done[0].body.len(), DEFAULT_MESSAGE);
        }
        assert!(peer.assemblies.is_empty());
        assert_eq!(peer.held, 0);
    }

    #[test]
    fn order_index_past_the_window_is_an_error() {
        let mut peer = peer(SMALL);
        let inside = datagram(0, &[(ordered(0, ORDER_WINDOW - 1), b"\xfe")]);
        assert_eq!(receive(&mut peer, &inside), Ok(Vec::new()));
        let outside = datagram(1, &[(ordered(1, ORDER_WINDOW), b"\xfe")]);
        assert_eq!(
            receive(&mut peer, &outside),
            Err(Error::Limit("ordering window"))
        );
    }

    /// A datagram number past the history is dropped without moving the numbers, so the honest
    /// datagrams after it are delivered, and it does not count as heard.
    #[test]
    fn datagram_number_jump_past_the_history_is_dropped() {
        let mut peer = peer(SMALL);
        let unreliable = header(Reliability::Unreliable);
        let later = Duration::from_secs(3);
        let jump = datagram(MAX_SPAN - 1, &[(unreliable, b"\xfe")]);
        let mut out = Vec::new();
        peer.receive(later, &jump, &mut out, &mut |_| {}).unwrap();
        assert_eq!(out, Vec::new());
        assert_eq!(peer.last_received(), T0);
        assert!(!peer.has_received_any());
        let honest = datagram(0, &[(unreliable, b"\xfe")]);
        assert_eq!(receive(&mut peer, &honest).map(|out| out.len()), Ok(1));
        // The last number the history holds is accepted, the next one is not.
        let history = u32::try_from(DATAGRAM_HISTORY).unwrap();
        let inside = datagram(history, &[(unreliable, b"\xfe")]);
        assert_eq!(receive(&mut peer, &inside).map(|out| out.len()), Ok(1));
        let outside = datagram(2 * history + 1, &[(unreliable, b"\xfe")]);
        assert_eq!(receive(&mut peer, &outside), Ok(Vec::new()));
    }

    #[test]
    fn reliable_index_limits_are_errors() {
        let reliable = |index| Header {
            message_index: index,
            ..header(Reliability::Reliable)
        };
        let mut peer = peer(SMALL);
        let jump = datagram(0, &[(reliable(MAX_SPAN), b"\xfe")]);
        assert_eq!(
            receive(&mut peer, &jump),
            Err(Error::Limit("reliable index jump"))
        );
        // Every other index: one gap each.
        let mut peer = self::peer(SMALL);
        let gaps = u32::try_from(MAX_RELIABLE_GAPS).unwrap();
        for index in 0..gaps {
            let data = datagram(index, &[(reliable(2 * index + 1), b"\xfe")]);
            assert_eq!(receive(&mut peer, &data).map(|out| out.len()), Ok(1));
        }
        let data = datagram(gaps, &[(reliable(2 * gaps + 1), b"\xfe")]);
        assert_eq!(
            receive(&mut peer, &data),
            Err(Error::Limit("reliable index gaps"))
        );
        // A gap that stays open while newer indices run far ahead.
        let mut peer = self::peer(SMALL);
        let mut seq = 0;
        for index in [1, MAX_SPAN - 1] {
            let data = datagram(seq, &[(reliable(index), b"\xfe")]);
            seq += 1;
            assert_eq!(receive(&mut peer, &data).map(|out| out.len()), Ok(1));
        }
        let data = datagram(seq, &[(reliable(MAX_SPAN), b"\xfe")]);
        assert_eq!(
            receive(&mut peer, &data),
            Err(Error::Limit("reliable index span"))
        );
    }

    #[test]
    fn duplicate_datagrams_and_reliable_frames_are_dropped() {
        let mut peer = peer(SMALL);
        let reliable = |index| Header {
            message_index: index,
            ..header(Reliability::Reliable)
        };
        let first = datagram(0, &[(reliable(0), b"\xfea")]);
        assert_eq!(receive(&mut peer, &first).map(|out| out.len()), Ok(1));
        assert_eq!(receive(&mut peer, &first), Ok(Vec::new()));
        // The same frame resent under a new datagram number.
        let resent = datagram(1, &[(reliable(0), b"\xfea")]);
        assert_eq!(receive(&mut peer, &resent), Ok(Vec::new()));
        // A late datagram inside the history still counts; one older than it does not.
        let skip = datagram(5, &[(reliable(2), b"\xfec")]);
        assert_eq!(receive(&mut peer, &skip).map(|out| out.len()), Ok(1));
        let late = datagram(3, &[(reliable(1), b"\xfeb")]);
        assert_eq!(receive(&mut peer, &late).map(|out| out.len()), Ok(1));
        let far = u32::try_from(DATAGRAM_HISTORY).unwrap() + 4;
        let jump = datagram(far, &[(reliable(3), b"\xfed")]);
        assert_eq!(receive(&mut peer, &jump).map(|out| out.len()), Ok(1));
        let old = datagram(4, &[(reliable(4), b"\xfee")]);
        assert_eq!(receive(&mut peer, &old), Ok(Vec::new()));
    }

    /// An ACK or NAK that cannot be read is dropped, and neither closes the connection nor counts
    /// as heard: the datagram is unauthenticated.
    #[test]
    fn malformed_acknowledgements_are_dropped() {
        let mut peer = peer(SMALL);
        let later = Duration::from_secs(3);
        let mut backwards = vec![FLAG_DATAGRAM | FLAG_ACK, 0, 1, 0];
        wire::put_u24(&mut backwards, 10);
        wire::put_u24(&mut backwards, 5);
        let record = vec![FLAG_DATAGRAM | FLAG_NAK, 0, 1, 2, 0, 0, 0];
        // A count larger than the records present is a truncation, not a long loop.
        let count = vec![FLAG_DATAGRAM | FLAG_ACK, 0xff, 0xff, 1, 0, 0, 0];
        for data in [backwards, record, count] {
            let mut out = Vec::new();
            peer.receive(later, &data, &mut out, &mut |_| {}).unwrap();
            assert_eq!(out, Vec::new());
            assert_eq!(peer.last_received(), T0);
        }
        // A readable one still counts.
        let readable = [FLAG_DATAGRAM | FLAG_ACK, 0, 0];
        peer.receive(later, &readable, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert_eq!(peer.last_received(), later);
    }

    #[test]
    fn outbound_bytes_over_the_cap_is_an_error() {
        let mut peer = peer(SMALL);
        let mut sent = 0;
        let error = loop {
            match peer.send(wire::ID_GAME, vec![0; 1 << 20]) {
                Ok(()) => sent += 1,
                Err(error) => break error,
            }
        };
        assert_eq!(error, Error::Limit("outbound bytes"));
        // 32 MiB less the frame cost of about 720 fragments per message.
        assert_eq!(sent, 30);
        // Tiny messages reach the cap through their frame cost, not 32 MiB of payload.
        let mut peer = self::peer(SMALL);
        let mut sent = 0_usize;
        while peer.send(1, Vec::new()).is_ok() {
            sent += 1;
        }
        assert_eq!(sent, MAX_UNSENT_BYTES / (1 + FRAME_COST_BYTES));
    }

    #[test]
    fn duplicate_fragment_after_reassembly_is_ignored() {
        let mut peer = peer(SMALL);
        let first = datagram(0, &[(fragment(0, 7, 2, 0), b"\xfeab")]);
        let second = datagram(1, &[(fragment(1, 7, 2, 1), b"cd")]);
        assert_eq!(receive(&mut peer, &first), Ok(Vec::new()));
        let done = receive(&mut peer, &second).unwrap();
        assert_eq!(
            done,
            vec![Message {
                id: 0xfe,
                body: b"abcd".to_vec()
            }]
        );
        // go-raknet opens a new reassembly here that never finishes.
        let again = datagram(2, &[(fragment(0, 7, 2, 0), b"\xfeab")]);
        assert_eq!(receive(&mut peer, &again), Ok(Vec::new()));
        assert!(peer.assemblies.is_empty());
        assert_eq!(peer.held, 0);
    }

    #[test]
    fn ordered_messages_wait_for_missing_ones() {
        let mut peer = peer(SMALL);
        let later = datagram(0, &[(ordered(0, 1), b"\xfe2")]);
        assert_eq!(receive(&mut peer, &later), Ok(Vec::new()));
        let earlier = datagram(1, &[(ordered(1, 0), b"\xfe1")]);
        let out = receive(&mut peer, &earlier).unwrap();
        let bodies: Vec<_> = out.into_iter().map(|message| message.body).collect();
        assert_eq!(bodies, [b"1".to_vec(), b"2".to_vec()]);
        assert_eq!(peer.held, 0);
        // Each channel has its own order.
        let other = Header {
            channel: 3,
            ..ordered(2, 0)
        };
        let data = datagram(2, &[(other, b"\xfe3")]);
        assert_eq!(receive(&mut peer, &data).map(|out| out.len()), Ok(1));
    }

    #[test]
    fn sequenced_messages_drop_older_ones() {
        let mut peer = peer(SMALL);
        let sequenced = |message_index, sequence_index| Header {
            message_index,
            sequence_index,
            ..header(Reliability::ReliableSequenced)
        };
        let newer = datagram(0, &[(sequenced(0, 5), b"\xfe5")]);
        assert_eq!(receive(&mut peer, &newer).map(|out| out.len()), Ok(1));
        let older = datagram(1, &[(sequenced(1, 4), b"\xfe4")]);
        assert_eq!(receive(&mut peer, &older), Ok(Vec::new()));
        let next = datagram(2, &[(sequenced(2, 6), b"\xfe6")]);
        assert_eq!(receive(&mut peer, &next).map(|out| out.len()), Ok(1));
    }

    #[test]
    fn ack_receipt_reliabilities_read_as_plain_ones() {
        let mut data = datagram(0, &[(ordered(0, 0), b"\xfe")]);
        // Reliability 3 (reliable ordered) becomes 7 (reliable ordered with ACK receipt).
        data[4] |= 0x80;
        let mut peer = peer(SMALL);
        assert_eq!(receive(&mut peer, &data).map(|out| out.len()), Ok(1));
        assert_eq!(peer.channels[0].next, 1);
    }

    #[test]
    fn numbers_wrap_around_24_bits() {
        let mut datagrams = Datagrams {
            next: U24_MASK - 1,
            recent: VecDeque::new(),
        };
        for seq in [U24_MASK - 1, U24_MASK, 0, 1] {
            assert!(matches!(datagrams.insert(seq), Arrival::New { gap: None }));
        }
        assert!(matches!(datagrams.insert(U24_MASK), Arrival::Duplicate));
        assert_eq!(datagrams.next, 2);
        assert!(matches!(
            datagrams.insert(5),
            Arrival::New { gap: Some((2, 4)) }
        ));
        let mut reliables = Reliables {
            next: u64::from(U24_MASK),
            missing: BTreeMap::new(),
        };
        assert_eq!(reliables.insert(1), Ok(true));
        assert_eq!(reliables.next, u64::from(U24_MASK) + 3);
        assert_eq!(reliables.insert(U24_MASK), Ok(true));
        assert_eq!(reliables.insert(0), Ok(true));
        assert_eq!(reliables.insert(0), Ok(false));
        assert!(reliables.missing.is_empty());
    }

    #[test]
    fn skipped_datagrams_are_nacked_at_once() {
        let mut peer = peer(SMALL);
        let mut sent = Vec::new();
        let mut out = Vec::new();
        let unreliable = header(Reliability::Unreliable);
        for seq in [0, 3] {
            let data = datagram(seq, &[(unreliable, b"\xfe")]);
            peer.receive(T0, &data, &mut out, &mut |d| sent.push(d.to_vec()))
                .unwrap();
        }
        let mut nak = vec![FLAG_DATAGRAM | FLAG_NAK, 0, 1, 0];
        wire::put_u24(&mut nak, 1);
        wire::put_u24(&mut nak, 2);
        assert_eq!(sent, [nak]);
        peer.flush_acks(&mut |d| sent.push(d.to_vec()));
        let mut ack = vec![FLAG_DATAGRAM | FLAG_ACK, 0, 2, 1];
        wire::put_u24(&mut ack, 0);
        ack.push(1);
        wire::put_u24(&mut ack, 3);
        assert_eq!(sent.get(1), Some(&ack));
    }

    #[test]
    fn unacknowledged_datagrams_are_resent_after_the_timeout() {
        let mut peer = peer(SMALL);
        peer.send(wire::ID_GAME, b"x".to_vec()).unwrap();
        let mut sent = Vec::new();
        peer.flush(T0, &mut |d| sent.push(d.to_vec())).unwrap();
        peer.update(Duration::from_millis(499));
        peer.flush(T0, &mut |d| sent.push(d.to_vec())).unwrap();
        assert_eq!(sent.len(), 1);
        peer.update(INITIAL_RTO);
        peer.flush(INITIAL_RTO, &mut |d| sent.push(d.to_vec()))
            .unwrap();
        assert_eq!(sent.len(), 2);
        // The same frame under a new sequence number.
        assert_eq!(sent[0][4..], sent[1][4..]);
        assert_ne!(sent[0][1..4], sent[1][1..4]);
        // The ACK of the resend settles it.
        let mut ack = vec![FLAG_DATAGRAM | FLAG_ACK, 0, 1, 1];
        wire::put_u24(&mut ack, 1);
        peer.receive(INITIAL_RTO, &ack, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert!(peer.is_settled());
        assert_eq!(peer.unsent, 0);
    }

    #[test]
    fn congestion_window_shrinks_on_loss_and_grows_on_acks() {
        let mut peer = peer(SMALL);
        for _ in 0..100 {
            peer.send(wire::ID_GAME, vec![0; 1000]).unwrap();
        }
        let mut sent = 0;
        peer.flush(T0, &mut |_| sent += 1).unwrap();
        assert_eq!(sent, INITIAL_CWND);
        let mut nak = vec![FLAG_DATAGRAM | FLAG_NAK, 0, 1, 1];
        wire::put_u24(&mut nak, 0);
        peer.receive(T0, &nak, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert_eq!(peer.cwnd, INITIAL_CWND * 3 / 4);
        let mut ack = vec![FLAG_DATAGRAM | FLAG_ACK, 0, 1, 0];
        wire::put_u24(&mut ack, 1);
        wire::put_u24(&mut ack, 31);
        peer.receive(T0, &ack, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert!(peer.cwnd > INITIAL_CWND * 3 / 4);
        assert!(peer.in_flight.is_empty());
    }

    #[test]
    fn resend_timeout_doubles_once_per_round_of_timeouts_until_a_sample() {
        // A NAK is a loss but not a timeout.
        let mut naked = peer(SMALL);
        naked.send(wire::ID_GAME, vec![0; 1000]).unwrap();
        let first = wire::low24(naked.next_seq);
        naked.flush(T0, &mut |_| {}).unwrap();
        let mut nak = vec![FLAG_DATAGRAM | FLAG_NAK, 0, 1, 1];
        wire::put_u24(&mut nak, first);
        naked
            .receive(T0, &nak, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert!(naked.in_flight.is_empty());
        assert_eq!(naked.rtt.timeout(), INITIAL_RTO);
        let mut peer = peer(SMALL);
        for _ in 0..100 {
            peer.send(wire::ID_GAME, vec![0; 1000]).unwrap();
        }
        let mut sent = 0;
        peer.flush(T0, &mut |_| sent += 1).unwrap();
        assert_eq!(sent, INITIAL_CWND);
        // All 32 datagrams time out in one update: one doubling, not one per datagram.
        peer.update(INITIAL_RTO);
        assert_eq!(peer.rtt.timeout(), 2 * INITIAL_RTO);
        peer.flush(INITIAL_RTO, &mut |_| {}).unwrap();
        peer.update(INITIAL_RTO + Duration::from_millis(999));
        assert_eq!(peer.in_flight.len(), INITIAL_CWND * 3 / 4);
        peer.update(3 * INITIAL_RTO);
        assert_eq!(peer.rtt.timeout(), MAX_RTO);
        // The cap holds through further rounds.
        peer.flush(3 * INITIAL_RTO, &mut |_| {}).unwrap();
        peer.update(3 * INITIAL_RTO + MAX_RTO);
        assert!(peer.in_flight.is_empty());
        assert_eq!(peer.rtt.timeout(), MAX_RTO);
        // The first sample (100 ms, variation 50 ms) sets the timeout from the estimate again.
        let now = 3 * INITIAL_RTO + MAX_RTO;
        let first = wire::low24(peer.next_seq);
        peer.flush(now, &mut |_| {}).unwrap();
        let mut ack = vec![FLAG_DATAGRAM | FLAG_ACK, 0, 1, 1];
        wire::put_u24(&mut ack, first);
        let now = now + Duration::from_millis(100);
        peer.receive(now, &ack, &mut Vec::new(), &mut |_| {})
            .unwrap();
        assert_eq!(peer.rtt.timeout(), Duration::from_millis(300));
    }

    /// A message sent at once leaves even when the congestion window is full and frames still
    /// wait, and it takes no place in the window.
    #[test]
    fn a_message_sent_at_once_skips_the_queue_and_the_window() {
        let mut peer = peer(SMALL);
        for _ in 0..100 {
            peer.send(wire::ID_GAME, vec![0; 1000]).unwrap();
        }
        let mut sent = Vec::new();
        peer.flush(T0, &mut |d| sent.push(d.to_vec())).unwrap();
        assert_eq!(sent.len(), INITIAL_CWND);
        peer.send_now(wire::ID_CONNECTED_PING, &[1, 2], &mut |d| {
            sent.push(d.to_vec());
        })
        .unwrap();
        assert_eq!(sent.len(), INITIAL_CWND + 1);
        assert_eq!(peer.in_flight.len(), INITIAL_CWND);
        let mut receiver = self::peer(SMALL);
        let out = receive(&mut receiver, sent.last().unwrap()).unwrap();
        assert_eq!(
            out,
            [Message {
                id: wire::ID_CONNECTED_PING,
                body: vec![1, 2]
            }]
        );
    }

    /// Sends 400 KB of ordered messages from one peer to another, five every 10 ms, over a link
    /// without loss whose one-way delay is `delay(sent at)`, and asserts that all of them arrive
    /// in order and the sender settles.
    fn stream_arrives_over(delay: impl Fn(Duration) -> Duration) {
        let messages: Vec<Vec<u8>> = (0..400)
            .map(|index| vec![u8::try_from(index % 251).unwrap(); SMALL])
            .collect();
        let deadline = Duration::from_secs(60);
        let mut sender = peer(SMALL);
        let mut receiver = peer(SMALL);
        // (arrival, to the receiver, datagram). A delay that never falls keeps arrivals in
        // sending order.
        let mut flying = VecDeque::new();
        let mut arrived = Vec::new();
        let mut next = 0;
        let mut now = T0;
        while arrived.len() < messages.len() || !sender.is_settled() {
            now += Duration::from_millis(1);
            assert!(
                now < deadline,
                "stalled at {} of {} messages with window {}",
                arrived.len(),
                messages.len(),
                sender.cwnd
            );
            let mut to_receiver = Vec::new();
            let mut to_sender = Vec::new();
            while flying.front().is_some_and(|(at, _, _)| *at <= now) {
                let (_, forward, data): (Duration, bool, Vec<u8>) = flying.pop_front().unwrap();
                if forward {
                    receiver
                        .receive(now, &data, &mut arrived, &mut |d| {
                            to_sender.push(d.to_vec());
                        })
                        .unwrap();
                } else {
                    sender
                        .receive(now, &data, &mut Vec::new(), &mut |_| {})
                        .unwrap();
                    sender
                        .flush(now, &mut |d| to_receiver.push(d.to_vec()))
                        .unwrap();
                }
            }
            if now.as_millis().is_multiple_of(10) {
                for message in messages.iter().skip(next).take(5) {
                    sender.send(wire::ID_GAME, message.clone()).unwrap();
                }
                next += 5;
                sender.update(now);
                sender
                    .flush(now, &mut |d| to_receiver.push(d.to_vec()))
                    .unwrap();
                receiver.flush_acks(&mut |d| to_sender.push(d.to_vec()));
            }
            let at = now + delay(now);
            flying.extend(to_receiver.into_iter().map(|data| (at, true, data)));
            flying.extend(to_sender.into_iter().map(|data| (at, false, data)));
        }
        assert!(
            arrived
                .iter()
                .map(|message| &message.body)
                .eq(messages.iter())
        );
    }

    #[test]
    fn stream_arrives_when_the_round_trip_exceeds_the_first_resend_timeout() {
        // 600 ms round trip, as over geostationary satellite.
        stream_arrives_over(|_| Duration::from_millis(300));
    }

    #[test]
    fn stream_arrives_after_the_round_trip_steps_far_past_the_resend_timeout() {
        // 30 ms, then 300 ms from 300 ms on, as when an uplink queue fills.
        stream_arrives_over(|sent| {
            if sent < Duration::from_millis(300) {
                Duration::from_millis(15)
            } else {
                Duration::from_millis(150)
            }
        });
    }
}
