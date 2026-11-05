//! The input log: every input each tick applied, in order, so that a crash can be replayed from a
//! saved state.
//!
//! The tick moves each input into the log before it applies it, and applies the logged value, so
//! recording copies nothing large and the log of a tick that panics holds the input that did. A
//! join is the one exception: the tick logs a copy of its identity, whose skin is shared, and
//! applies the identity itself, which the new player keeps. Packets are encoded only when the log
//! is written, on the main thread after the tick thread ended. The log is a list of segments: one
//! from the start of the process and one per save snapshot, each starting at the tick its state
//! was taken with a checkpoint of that state ([`super::checkpoint`]), which a replay starts from
//! with the server file of the same tick.
//!
//! The bound ([`MAX_BYTES`]) counts each input by its size on the wire or, for a packet, what
//! decoding it allocated if that is larger, and every list the log holds by the room it takes:
//! a tick, an input, an answer and a forgotten session each take their own size in their list,
//! whose room is counted as it grows ([`growth`]), so the bound counts the memory the log holds
//! and many empty inputs or ticks are bounded too. Recording stops when the inputs alone would
//! pass it. Committed segments are kept while there is room: the oldest go only when
//! the inputs and the checkpoints of every kept segment would pass it, never the newest committed
//! one, which the crash log needs. A segment that goes is handed back
//! ([`InputLog::take_discarded`]) for another thread to free, since freeing it takes time
//! proportional to everything it holds. Checkpoints have no bound of their own. A segment whose
//! recording stops, at the bound or at a save thread answer that cannot be logged, is marked
//! truncated, and a replay ends there; the next snapshot starts a new segment. A quarter of the
//! bound recorded since the last commit asks for a snapshot ([`InputLog::wants_commit`]), so the
//! rest covers the commit's time and a segment is not truncated in normal running.
//!
//! The log chains every input the tick applies, recorded or not, into a commit identity
//! ([`InputLog::identity`]), from the identity of the state the server file held at the start. A
//! snapshot's commit writes it with the tick into the file, and each segment keeps the identity of
//! its state, so a replay starts a segment only from a file that holds that very state, not one
//! another run committed at the same tick. The chain takes each input's place and kind and the
//! values measured off the tick, not packet bodies: encoding every packet on the tick is what the
//! log avoids, and two runs from one state differ in their measured tick times, logged every tick.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::checkpoint::{Checkpoint, capacity, read_skin, skin_bytes};
use super::queue::{Client, Identity, JobOutcome, LeaveReason, Measure, SessionId};
use crate::clock;
use crate::nbt::Encoding;
use crate::protocol::{
    self, Writer,
    packet::{PROTOCOL_VERSION, Packet},
};
use crate::save::{Loaded, PlayerData, PlayerLoad, Response, WorldIndex};
use crate::storage::Checked;
use crate::storage::codec::MAX_PLAYER_EXTRA_BYTES;
use crate::storage::seal::Crc32c;
use crate::world::ChunkPos;

/// The tag of a missing address.
const ADDRESS_NONE: u8 = 0;
/// The tag of an IPv4 address.
const ADDRESS_V4: u8 = 4;
/// The tag of an IPv6 address.
const ADDRESS_V6: u8 = 6;
/// The tag of a chunk answer.
const ANSWER_CHUNK: u8 = 0;
/// The tag of a player answer.
const ANSWER_PLAYER: u8 = 1;
/// The tag of a commit answer.
const ANSWER_COMMITTED: u8 = 2;
/// The tag of a failed commit answer.
const ANSWER_COMMIT_FAILED: u8 = 3;
/// The tag of a data file check answer.
const ANSWER_CHECKED: u8 = 4;
/// The tag of a world file open failure answer.
const ANSWER_WORLD_OPEN_FAILED: u8 = 5;
/// What the commit identity chains before an answer.
const CHAIN_ANSWER: u8 = b'A';
/// What the commit identity chains before a forgotten session.
const CHAIN_DROPPED: u8 = b'D';
/// What the commit identity chains before an input.
const CHAIN_INPUT: u8 = b'I';
/// What the commit identity chains before a snapshot.
const CHAIN_SNAPSHOT: u8 = b'S';
/// What the commit identity chains before a tick.
const CHAIN_TICK: u8 = b'T';
/// The tag of a data file check that passed.
const CHECK_PASSED: u8 = 0;
/// The tag of a data file check that found damage.
const CHECK_DAMAGED: u8 = 1;
/// The tag of a data file check that could not finish.
const CHECK_FAILED: u8 = 2;
/// The input bytes recorded since the last commit at which the log asks for a snapshot.
const COMMIT_BYTES: usize = MAX_BYTES / 4;
/// The folder of crash reports in the data folder.
const CRASH_DIR: &str = "crash";
/// The bytes of the CRC-32C at the end of a written log.
const CRC_BYTES: usize = 4;
/// The written format's version: the layout of the log and of the checkpoints it holds, which
/// every rule module's checkpoint codec decides. [`InputLog::decode`] reads no other.
const FORMAT: u32 = 3;
/// The outcome byte of a job result that has its JSON; other values are
/// [`JobError::code`](crate::task::JobError::code)s.
const JOB_DONE: u8 = 0;
/// The tag of a join input.
const KIND_JOIN: u8 = 0;
/// The tag of a packet input.
const KIND_PACKET: u8 = 1;
/// The tag of a leave input.
const KIND_LEAVE: u8 = 2;
/// The tag of a console input.
const KIND_CONSOLE: u8 = 3;
/// The tag of a job input.
const KIND_JOB: u8 = 4;
/// The tag of a measure input.
const KIND_MEASURE: u8 = 5;
/// The tag of a leave because the client closed the connection.
const LEAVE_LEFT: u8 = 0;
/// The tag of a leave because the connection closed otherwise.
const LEAVE_CONNECTION_CLOSED: u8 = 1;
/// The tag of a leave because the connection timed out.
const LEAVE_TIMED_OUT: u8 = 2;
/// The first bytes of a written log.
const MAGIC: &[u8; 16] = b"ANCHOR-INPUT-LOG";
/// The most bytes the log counts: recording stops where its inputs would pass it, and old
/// committed segments go where its inputs and checkpoints would.
pub(crate) const MAX_BYTES: usize = 64 << 20;
/// The largest written log: [`write_crash_log`] writes no more ([`InputLog::encode_within`]) and
/// [`InputLog::decode`] reads no more. Checkpoints hold every loaded chunk and have no bound of
/// their own, so a log can be larger; it is then written from its newest committed segment, or
/// not at all.
pub(crate) const MAX_FILE_BYTES: usize = 1 << 30;
/// The tag of a tick time measure.
const MEASURE_TICK_TIME: u8 = 0;
/// The tag of a ping measure.
const MEASURE_PING: u8 = 1;
/// The tag of a connection count measure.
const MEASURE_CONNECTIONS: u8 = 2;
/// The tag of a process measure.
const MEASURE_PROCESS: u8 = 3;
/// The tag of a chunk read from the data files.
const ORIGIN_DISK: u8 = 0;
/// The tag of a chunk made by the generator.
const ORIGIN_GENERATED: u8 = 1;
/// The tag of a damaged chunk.
const ORIGIN_DAMAGED: u8 = 2;
/// The tag of a player load that found data.
const PLAYER_FOUND: u8 = 0;
/// The tag of a player load that found none.
const PLAYER_ABSENT: u8 = 1;
/// The tag of a player load that found damaged data.
const PLAYER_DAMAGED: u8 = 2;

/// The input log.
#[derive(Debug)]
pub(crate) struct InputLog {
    /// Oldest first; never empty. The first committed one or one after it is the newest
    /// committed segment.
    segments: VecDeque<Segment>,
    /// Segments dropped for room, not yet freed: freeing one releases every tick, packet and
    /// chunk copy it holds, which the tick must not do ([`Self::take_discarded`]).
    discarded: Vec<Segment>,
    /// The input bytes charged for every segment.
    bytes: usize,
    /// The checkpoint bytes of every segment ([`Checkpoint::size`]).
    checkpoint_bytes: usize,
    /// Whether a new segment waits for its checkpoint ([`Self::needs_checkpoint`]): only the
    /// tick's own log takes them (tests' logs replay from a known start).
    takes_checkpoints: bool,
    /// What the server was built with, which a replay must match.
    fingerprints: Fingerprints,
    /// A replay's log: the ticks the replayed log's snapshots started at, which decide
    /// [`Self::wants_commit`] instead of the bytes recorded (the replay does not have the bytes
    /// the live log had before its first segment).
    planned: Option<std::collections::BTreeSet<u64>>,
    /// The commit identity of the state after the inputs chained so far ([`Self::identity`]).
    chain: super::Fnv,
}

impl InputLog {
    /// Reads a written log of at most [`MAX_FILE_BYTES`], of [`FORMAT`], whose packets together
    /// decode within [`MAX_BYTES`]. Its versions are checked first, then its checksum, then every
    /// count against the bytes left before anything is allocated for it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Format("larger than a log can be"));
        }
        let Some((covered, crc)) = bytes.split_last_chunk::<CRC_BYTES>() else {
            return Err(Error::Format("ends early"));
        };
        let mut r = Read(covered);
        if r.take(MAGIC.len())? != MAGIC {
            return Err(Error::Format("magic"));
        }
        let format = r.u32()?;
        if format != FORMAT {
            return Err(Error::OtherFormat(format));
        }
        let protocol = r.i32()?;
        if protocol != PROTOCOL_VERSION {
            return Err(Error::OtherProtocol(protocol));
        }
        let mut check = Crc32c::new();
        check.update(covered);
        if check.finish() != u32::from_le_bytes(*crc) {
            return Err(Error::Checksum);
        }
        let fingerprints = Fingerprints {
            settings: r.array()?,
            registrations: r.array()?,
        };
        // A segment takes at least 15 bytes.
        let count = r.count(15)?;
        if count == 0 {
            return Err(Error::Format("no segment"));
        }
        let mut segments = VecDeque::with_capacity(capacity::<Segment>(count, r.0.len()));
        // The log counted each packet as at least what decoding it allocated, within the bound
        // for all of them (`InputLog::record`): an item's NBT takes a reader far more than its
        // bytes.
        let mut packet_budget = MAX_BYTES;
        for _ in 0..count {
            let start = r.u64()?;
            let committed = r.flag("committed flag")?;
            let truncated = r.flag("truncated flag")?;
            let identity = if r.flag("identity flag")? {
                Some(r.u64()?)
            } else {
                None
            };
            let checkpoint = if r.flag("checkpoint flag")? {
                Some(Point::Written(r.bytes()?.to_vec()))
            } else {
                None
            };
            let ticks = r.count(16)?;
            let mut segment = Segment {
                checkpoint,
                ticks: Vec::with_capacity(capacity::<Tick>(ticks, r.0.len())),
                truncated,
                committed,
                ..Segment::new(start, identity)
            };
            for _ in 0..ticks {
                segment.ticks.push(r.tick(&mut packet_budget)?);
            }
            segments.push_back(segment);
        }
        if !r.0.is_empty() {
            return Err(Error::Format("bytes after the last segment"));
        }
        Ok(Self {
            segments,
            discarded: Vec::new(),
            bytes: 0,
            checkpoint_bytes: 0,
            takes_checkpoints: false,
            fingerprints,
            planned: None,
            chain: super::Fnv(super::Fnv::OFFSET),
        })
    }

    /// The tick's log: it starts at `checkpoint`'s state, the one the server file holds at the
    /// start of the process with commit identity `identity`, and takes a checkpoint at every
    /// snapshot ([`Self::needs_checkpoint`]).
    pub(crate) fn new(
        checkpoint: Checkpoint,
        fingerprints: Fingerprints,
        identity: Option<u64>,
    ) -> Self {
        let mut log = Self {
            takes_checkpoints: true,
            fingerprints,
            ..Self::from_state(checkpoint.tick(), identity)
        };
        log.set_checkpoint(checkpoint);
        log
    }

    /// A replay's log: it starts at the restored `checkpoint`, the state the server file holds
    /// with commit identity `identity`, and takes a checkpoint at every snapshot, to compare with
    /// the replayed log's; the replayed log's snapshots started at the ticks of `planned`, which
    /// decide [`Self::wants_commit`].
    pub(crate) fn replaying(
        checkpoint: Checkpoint,
        identity: Option<u64>,
        planned: std::collections::BTreeSet<u64>,
    ) -> Self {
        Self {
            planned: Some(planned),
            ..Self::new(checkpoint, Fingerprints::default(), identity)
        }
    }

    /// A log whose state starts at tick `start` (which the server file holds), taking no
    /// checkpoints: tests replay it from a state they know.
    pub(crate) fn starting_at(start: u64) -> Self {
        Self::from_state(start, None)
    }

    /// What the run that wrote the log was built with.
    pub(crate) fn fingerprints(&self) -> Fingerprints {
        self.fingerprints
    }

    /// The commit identity of the state the inputs chained so far lead to: what the commit of a
    /// snapshot taken now writes into the server file.
    pub(crate) fn identity(&self) -> u64 {
        self.chain.0
    }

    /// Whether a replay of this log stops before its last tick: a kept segment stopped recording.
    pub(crate) fn is_truncated(&self) -> bool {
        self.segments.iter().any(|s| s.truncated)
    }

    /// Whether the newest segment waits for the checkpoint of its state: the tick takes it at
    /// the tick boundary that started the segment, right after the snapshot, before anything
    /// changes the state.
    pub(crate) fn needs_checkpoint(&self) -> bool {
        self.takes_checkpoints && self.last_segment().checkpoint.is_none()
    }

    /// The segment being recorded.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the log keeps at least one segment"
    )]
    pub(crate) fn last_segment(&self) -> &Segment {
        self.segments.back().expect("the log keeps a segment")
    }

    /// The segments, oldest first.
    pub(crate) fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter()
    }

    /// The tick of the oldest kept state.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the log keeps at least one segment"
    )]
    pub(crate) fn start(&self) -> u64 {
        self.segments
            .front()
            .expect("the log keeps a segment")
            .start
    }

    /// The ticks a replay from the oldest segment applies, oldest first.
    #[cfg(test)]
    pub(crate) fn ticks(&self) -> Vec<Tick> {
        self.replayable().cloned().collect()
    }

    /// Whether the inputs recorded since the state of the last commit reach a quarter of the
    /// bound: the tick then takes a snapshot even with periodic saves off (`/save off`), so
    /// recording does not stop at the bound while the commit is written. In a replay's log:
    /// whether the replayed log's snapshot started at `tick`, the tick of the boundary.
    pub(crate) fn wants_commit(&self, tick: u64) -> bool {
        if let Some(planned) = &self.planned {
            return planned.contains(&tick);
        }
        let since: usize = self
            .segments
            .iter()
            .skip(self.newest_committed())
            .map(|s| s.bytes)
            .sum();
        since >= COMMIT_BYTES
    }

    /// Starts the record of tick `number`; every tick counts against the bound, even an empty
    /// one, by the room it takes in its segment. Nothing is recorded in a truncated segment.
    pub(crate) fn begin(&mut self, number: u64) {
        self.chain.raw(&[CHAIN_TICK]);
        self.chain.u64(number);
        let room = growth(&self.last_segment().ticks);
        if self.charge(room_bytes::<Tick>(room)) == Charge::Fits {
            let ticks = &mut self.last_segment_mut().ticks;
            ticks.reserve_exact(room);
            ticks.push(Tick {
                number,
                inputs: Vec::new(),
                dropped: Vec::new(),
                responses: Vec::new(),
            });
        }
    }

    /// The snapshot taken before tick `start` is committed: its segment is the newest committed
    /// one. The segments before it stay while there is room. Does nothing if no segment starts
    /// at `start`.
    pub(crate) fn committed(&mut self, start: u64) {
        if let Some(segment) = self.segments.iter_mut().find(|s| s.start == start) {
            segment.committed = true;
            self.make_room(0);
        }
    }

    /// Records that the tick forgot `session` after its output.
    pub(crate) fn dropped(&mut self, session: SessionId) {
        self.chain.raw(&[CHAIN_DROPPED]);
        self.chain.u64(session.get());
        let room = self.last_tick_room(|t| growth(&t.dropped));
        if self.charge_entry(room_bytes::<SessionId>(room))
            && let Some(tick) = self.last_tick()
        {
            tick.dropped.reserve_exact(room);
            tick.dropped.push(session);
        }
    }

    /// Records an input of `size` bytes (a packet's
    /// [`InputKind::Packet`](super::queue::InputKind::Packet) size, a join's
    /// [`Identity::size`](super::queue::Identity::size), 0 for a leave) before the tick
    /// applies it, and gives it back for the tick to apply: the logged entry, or the input itself
    /// when the log no longer records.
    pub(crate) fn record(
        &mut self,
        session: SessionId,
        seq: u64,
        kind: EntryKind,
        size: usize,
    ) -> Cow<'_, EntryKind> {
        self.chain_input(session, seq, &kind);
        let room = self.last_tick_room(|t| growth(&t.inputs));
        if !self.charge_entry(size.saturating_add(room_bytes::<Entry>(room))) {
            return Cow::Owned(kind);
        }
        match self.last_tick() {
            Some(tick) => {
                tick.inputs.reserve_exact(room);
                Cow::Borrowed(&tick.inputs.push_mut(Entry { session, seq, kind }).kind)
            }
            None => Cow::Owned(kind),
        }
    }

    /// Records a save thread answer the tick applied. An answer that cannot be logged (a player
    /// extra that does not encode) ends the segment like the bound does: a replay must not apply
    /// a tick without it.
    pub(crate) fn response(&mut self, response: &Response) {
        self.chain.raw(&[
            CHAIN_ANSWER,
            match response {
                Response::Chunk { .. } => ANSWER_CHUNK,
                Response::Player { .. } => ANSWER_PLAYER,
                Response::Committed { .. } => ANSWER_COMMITTED,
                Response::CommitFailed { .. } => ANSWER_COMMIT_FAILED,
                Response::Checked(_) => ANSWER_CHECKED,
                Response::WorldOpenFailed { .. } => ANSWER_WORLD_OPEN_FAILED,
            },
        ]);
        if self.last_segment().truncated {
            return;
        }
        let (answer, size) = match Answer::of(response) {
            Ok(logged) => logged,
            Err(e) => {
                warn!(
                    error = &e as &(dyn std::error::Error + 'static),
                    "an answer cannot be logged; ticks until the next save are not recorded"
                );
                let segment = self.last_segment_mut();
                segment.truncated = true;
                segment.ticks.pop();
                return;
            }
        };
        let room = self.last_tick_room(|t| growth(&t.responses));
        if self.charge_entry(size.saturating_add(room_bytes::<Answer>(room)))
            && let Some(tick) = self.last_tick()
        {
            tick.responses.reserve_exact(room);
            tick.responses.push(answer);
        }
    }

    /// Gives the newest segment the checkpoint of its state, if it starts at the checkpoint's
    /// tick and has none.
    pub(crate) fn set_checkpoint(&mut self, checkpoint: Checkpoint) {
        let last = self.last_segment_mut();
        if last.start != checkpoint.tick() || last.checkpoint.is_some() {
            return;
        }
        let point = Point::Taken(Box::new(checkpoint));
        let size = point.size();
        last.checkpoint = Some(point);
        self.checkpoint_bytes = self.checkpoint_bytes.saturating_add(size);
        self.make_room(0);
    }

    /// Starts a segment at tick `start`, the tick a save snapshot is taken before, and chains the
    /// snapshot into the commit identity first, so the identity the snapshot commits differs from
    /// the previous snapshot's even at the same tick (a shutdown's last one). A snapshot takes
    /// [`Self::identity`] after this.
    pub(crate) fn snapshot(&mut self, start: u64) {
        self.chain.raw(&[CHAIN_SNAPSHOT]);
        if self.last_segment().start == start {
            return;
        }
        let identity = Some(self.identity());
        self.segments.push_back(Segment::new(start, identity));
    }

    /// The segments dropped for room since the last call, for the tick to hand to another thread
    /// to free.
    pub(crate) fn take_discarded(&mut self) -> Vec<Segment> {
        std::mem::take(&mut self.discarded)
    }

    /// The written form; tests compare and read it back.
    #[cfg(test)]
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.encode_from(0, usize::MAX)
    }

    /// A log whose state starts at tick `start` with commit identity `identity`, taking no
    /// checkpoints.
    fn from_state(start: u64, identity: Option<u64>) -> Self {
        Self {
            segments: VecDeque::from([Segment {
                committed: true,
                ..Segment::new(start, identity)
            }]),
            discarded: Vec::new(),
            bytes: 0,
            checkpoint_bytes: 0,
            takes_checkpoints: false,
            fingerprints: Fingerprints::default(),
            planned: None,
            chain: super::Fnv(identity.unwrap_or(super::Fnv::OFFSET)),
        }
    }

    /// The written form if it takes at most `limit` bytes; otherwise the written form from the
    /// newest committed segment on, which a replay starts from with the server file of the last
    /// commit, if that fits; otherwise [`Error::TooLarge`].
    fn encode_within(&self, limit: usize) -> Result<Vec<u8>> {
        match self.encode_from(0, limit) {
            Err(Error::TooLarge) if self.newest_committed() > 0 => {
                self.encode_from(self.newest_committed(), limit)
            }
            written => written,
        }
    }

    /// The written form of the segments from the one at place `from` on: the header with the
    /// fingerprints, then each segment (its start, marks, commit identity, checkpoint and ticks),
    /// then the CRC-32C of everything before it. Integers are little-endian; checkpoints are
    /// encoded here, on the main thread. [`Error::TooLarge`] as soon as it would take more than
    /// `limit` bytes.
    fn encode_from(&self, from: usize, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.bytes.saturating_add(128));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.extend_from_slice(&self.fingerprints.settings);
        out.extend_from_slice(&self.fingerprints.registrations);
        put_len(&mut out, self.segments.len().saturating_sub(from))?;
        let mut body = Vec::new();
        for segment in self.segments.iter().skip(from) {
            out.extend_from_slice(&segment.start.to_le_bytes());
            out.push(u8::from(segment.committed));
            out.push(u8::from(segment.truncated));
            match segment.identity {
                Some(identity) => {
                    out.push(1);
                    out.extend_from_slice(&identity.to_le_bytes());
                }
                None => out.push(0),
            }
            match &segment.checkpoint {
                Some(Point::Taken(c)) => {
                    out.push(1);
                    put_bytes(&mut out, &c.to_bytes())?;
                }
                Some(Point::Written(bytes)) => {
                    out.push(1);
                    put_bytes(&mut out, bytes)?;
                }
                None => out.push(0),
            }
            put_len(&mut out, segment.ticks.len())?;
            for tick in &segment.ticks {
                put_tick(&mut out, tick, &mut body)?;
            }
            if out.len().saturating_add(CRC_BYTES) > limit {
                return Err(Error::TooLarge);
            }
        }
        let mut crc = Crc32c::new();
        crc.update(&out);
        out.extend_from_slice(&crc.finish().to_le_bytes());
        Ok(out)
    }

    /// The items of room that `growth_of` one of the lists of the tick being recorded needs for
    /// its next item; none without a tick being recorded.
    fn last_tick_room(&self, growth_of: impl FnOnce(&Tick) -> usize) -> usize {
        self.last_segment().ticks.last().map_or(0, growth_of)
    }

    /// The place of the newest committed segment.
    fn newest_committed(&self) -> usize {
        self.segments.iter().rposition(|s| s.committed).unwrap_or(0)
    }

    /// The ticks a replay from the oldest segment applies, oldest first: every kept segment up
    /// to and including the first truncated one.
    #[cfg(test)]
    fn replayable(&self) -> impl Iterator<Item = &Tick> {
        let end = self
            .segments
            .iter()
            .position(|s| s.truncated)
            .map_or(self.segments.len(), |i| i.saturating_add(1));
        self.segments.iter().take(end).flat_map(|s| &s.ticks)
    }

    /// Chains an input into the commit identity: its place, its kind, a packet's id and the
    /// values a measure holds.
    fn chain_input(&mut self, session: SessionId, seq: u64, kind: &EntryKind) {
        let chain = &mut self.chain;
        chain.raw(&[CHAIN_INPUT]);
        chain.u64(session.get());
        chain.u64(seq);
        match kind {
            EntryKind::Join(_) => chain.raw(&[KIND_JOIN]),
            EntryKind::Packet(packet) => {
                chain.raw(&[KIND_PACKET]);
                chain.u64(u64::from(packet.id()));
            }
            EntryKind::Leave(reason) => chain.raw(&[KIND_LEAVE, leave_tag(*reason)]),
            EntryKind::Console(_) => chain.raw(&[KIND_CONSOLE]),
            EntryKind::Job(_) => chain.raw(&[KIND_JOB]),
            EntryKind::Measure(measure) => {
                chain.raw(&[KIND_MEASURE]);
                match *measure {
                    Measure::TickTime(v) | Measure::Ping(v) => chain.u64(u64::from(v)),
                    Measure::Connections { all, logged_in } => {
                        chain.u64(u64::from(all));
                        chain.u64(u64::from(logged_in));
                    }
                    Measure::Process { memory, uptime_ms } => {
                        chain.u64(memory);
                        chain.u64(uptime_ms);
                    }
                }
            }
        }
    }

    /// Counts an entry of the tick being recorded; `false` if it is not recorded. When the bound
    /// is reached in the middle of a tick, that tick is incomplete, so it goes too: a replay
    /// must not apply part of a tick.
    fn charge_entry(&mut self, size: usize) -> bool {
        match self.charge(size) {
            Charge::Fits => true,
            Charge::Full => {
                self.last_segment_mut().ticks.pop();
                false
            }
            Charge::Truncated => false,
        }
    }

    /// Counts `cost` bytes against the bound, for the segment being recorded. Old committed
    /// segments go first if the bound needs room.
    fn charge(&mut self, cost: usize) -> Charge {
        if self.last_segment().truncated {
            return Charge::Truncated;
        }
        self.make_room(cost);
        let bytes = self.bytes.saturating_add(cost);
        if bytes > MAX_BYTES {
            warn!(
                bytes = self.bytes,
                "input log reached its bound; ticks until the next save are not recorded"
            );
            self.last_segment_mut().truncated = true;
            return Charge::Full;
        }
        self.bytes = bytes;
        let segment = self.last_segment_mut();
        segment.bytes = segment.bytes.saturating_add(cost);
        Charge::Fits
    }

    /// The tick being recorded.
    fn last_tick(&mut self) -> Option<&mut Tick> {
        self.last_segment_mut().ticks.last_mut()
    }

    /// The segment being recorded, to change.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the log keeps at least one segment"
    )]
    fn last_segment_mut(&mut self) -> &mut Segment {
        self.segments.back_mut().expect("the log keeps a segment")
    }

    /// Drops the oldest segments before the newest committed one while the inputs, the
    /// checkpoints and `cost` more bytes exceed the bound.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a committed segment follows the one dropped"
    )]
    fn make_room(&mut self, cost: usize) {
        while self
            .bytes
            .saturating_add(self.checkpoint_bytes)
            .saturating_add(cost)
            > MAX_BYTES
            && self.newest_committed() > 0
        {
            let first = self
                .segments
                .pop_front()
                .expect("a committed segment follows the first");
            self.bytes = self.bytes.saturating_sub(first.bytes);
            let size = first.checkpoint.as_ref().map_or(0, Point::size);
            self.checkpoint_bytes = self.checkpoint_bytes.saturating_sub(size);
            self.discarded.push(first);
        }
    }
}

impl Default for InputLog {
    /// A log that starts at tick 0 and takes no checkpoints.
    fn default() -> Self {
        Self::starting_at(0)
    }
}

/// What a replay must have in common with the run that wrote the log: SHA-256 of the tick settings
/// and worlds, and of what the user registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Fingerprints {
    pub(crate) settings: [u8; 32],
    pub(crate) registrations: [u8; 32],
}

impl Fingerprints {
    /// The fingerprints of a server built with `settings`, `worlds` and `setup`, taken before it
    /// reads its server file: every field of each, by its `Debug` form, which names every
    /// registration (commands, custom items, blocks and entities, listeners per event, task
    /// periods, data types, hooks) without the closures.
    pub(crate) fn of(
        settings: &super::Settings,
        worlds: &[crate::save::WorldSpec],
        setup: &super::Setup,
    ) -> Self {
        Self {
            settings: crate::crypto::sha256(&[format!("{settings:?}{worlds:?}").as_bytes()]),
            registrations: crate::crypto::sha256(&[format!("{setup:?}").as_bytes()]),
        }
    }
}

/// The ticks recorded from one state on.
#[derive(Debug)]
pub(crate) struct Segment {
    /// The tick of the state: the first tick this segment can hold.
    start: u64,
    /// The state at `start`, once taken.
    checkpoint: Option<Point>,
    ticks: Vec<Tick>,
    /// The input bytes charged for this segment.
    bytes: usize,
    /// Recording stopped, at the bound or at an answer that cannot be logged; later ticks of
    /// this segment are missing.
    truncated: bool,
    /// The server file holds the state at `start`: the start of the process, or a commit the tick
    /// learnt of.
    committed: bool,
    /// The commit identity of the state at `start`; `None` if unknown (a server file that has
    /// none).
    identity: Option<u64>,
}

impl Segment {
    /// The written checkpoint of the segment's state, if any (logs read from a file).
    pub(crate) fn checkpoint(&self) -> Option<&[u8]> {
        match &self.checkpoint {
            Some(Point::Written(bytes)) => Some(bytes),
            Some(Point::Taken(_)) | None => None,
        }
    }

    /// The commit identity of the segment's state, if known.
    pub(crate) fn identity(&self) -> Option<u64> {
        self.identity
    }

    /// Whether recording stopped before the segment's last tick.
    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The tick of the segment's state.
    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    /// The checkpoint of the segment's state as this process took it, if any.
    pub(crate) fn taken(&self) -> Option<&Checkpoint> {
        match &self.checkpoint {
            Some(Point::Taken(c)) => Some(c),
            Some(Point::Written(_)) | None => None,
        }
    }

    /// The ticks recorded, in order.
    pub(crate) fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    /// A segment from tick `start`, of the state with commit identity `identity`, not committed.
    fn new(start: u64, identity: Option<u64>) -> Self {
        Self {
            start,
            checkpoint: None,
            ticks: Vec::new(),
            bytes: 0,
            truncated: false,
            committed: false,
            identity,
        }
    }
}

/// A segment's checkpoint: taken by this process, or read from a written log (its bytes, read
/// with the server's custom content by the replay).
#[derive(Debug)]
enum Point {
    Taken(Box<Checkpoint>),
    Written(Vec<u8>),
}

impl Point {
    fn size(&self) -> usize {
        match self {
            Self::Taken(c) => c.size(),
            Self::Written(b) => b.len(),
        }
    }
}

/// What one tick applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tick {
    /// The engine time (tick number) the inputs were applied at.
    pub(crate) number: u64,
    /// The inputs in the order the tick applied them.
    pub(crate) inputs: Vec<Entry>,
    /// Sessions the tick forgot after its output because their queue was full or closed, in
    /// order. They are inputs too: a replay must forget them at the same point.
    pub(crate) dropped: Vec<SessionId>,
    /// The save thread's answers the tick applied, in order, before the session inputs.
    pub(crate) responses: Vec<Answer>,
}

/// One applied input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) session: SessionId,
    pub(crate) seq: u64,
    pub(crate) kind: EntryKind,
}

/// What an input was.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EntryKind {
    Join(Identity),
    Packet(Packet),
    Leave(LeaveReason),
    /// A console line ([`InputKind::Console`](super::queue::InputKind::Console)).
    Console(String),
    /// A blocking job's result ([`InputKind::Job`](super::queue::InputKind::Job)); the entry's
    /// sequence number is the job's id.
    Job(JobOutcome),
    /// A value measured off the tick ([`InputKind::Measure`](super::queue::InputKind::Measure)).
    Measure(Measure),
}

/// A save thread answer as the log keeps it: what arrived and where from, not the chunk itself,
/// which a replay loads again from the same server file or generator, checking it against the
/// checksum.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    Chunk {
        world: WorldIndex,
        pos: ChunkPos,
        origin: Origin,
        /// [`Stored::checksum`](crate::save::Stored::checksum): what was read from the file for
        /// the chunk, terrain and extras (0 for a damaged chunk).
        checksum: u32,
    },
    Player {
        key: String,
        data: LoggedPlayer,
    },
    Committed(u64),
    CommitFailed(u64),
    Checked(Checked),
    /// The file of the world at this index did not open on the save thread.
    WorldOpenFailed(WorldIndex),
}

impl Answer {
    /// The logged form of an answer and the bytes it is charged.
    fn of(response: &Response) -> protocol::Result<(Self, usize)> {
        Ok(match response {
            Response::Chunk { world, pos, loaded } => {
                let (origin, checksum) = match loaded {
                    Loaded::Disk(_, stored) => (Origin::Disk, stored.checksum),
                    Loaded::Generated(_, stored) => (Origin::Generated, stored.checksum),
                    Loaded::Damaged => (Origin::Damaged, 0),
                };
                let answer = Self::Chunk {
                    world: *world,
                    pos: *pos,
                    origin,
                    checksum,
                };
                (answer, 0)
            }
            Response::Player { key, data } => {
                let data = LoggedPlayer::of(data)?;
                let size = data.size().saturating_add(key.as_str().len());
                let answer = Self::Player {
                    key: key.as_str().to_owned(),
                    data,
                };
                (answer, size)
            }
            Response::Committed { start } => (Self::Committed(*start), 0),
            Response::CommitFailed { start } => (Self::CommitFailed(*start), 0),
            Response::Checked(found) => {
                let size = match found {
                    Checked::Passed { .. } => 0,
                    Checked::Damaged { first, .. } => first.len(),
                    Checked::Failed(why) => why.len(),
                };
                (Self::Checked(found.clone()), size)
            }
            Response::WorldOpenFailed { world } => (Self::WorldOpenFailed(*world), 0),
        })
    }
}

/// Where a loaded chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Disk,
    Generated,
    Damaged,
}

/// A player load as the log keeps it: the extra compound as its little-endian NBT bytes, which
/// the log counts against its bound and copies no tree for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoggedPlayer {
    Found {
        world: String,
        position: [f64; 3],
        extra: Vec<u8>,
    },
    Absent,
    Damaged,
}

impl LoggedPlayer {
    /// The load this records, for a replay.
    pub(crate) fn to_load(&self) -> Result<PlayerLoad> {
        Ok(match self {
            Self::Found {
                world,
                position,
                extra,
            } => PlayerLoad::Found(PlayerData {
                world: world.clone(),
                position: *position,
                extra: read_extra(extra)?,
            }),
            Self::Absent => PlayerLoad::Absent,
            Self::Damaged => PlayerLoad::Damaged,
        })
    }

    /// The logged form of a load; fails only if the extra does not encode.
    fn of(load: &PlayerLoad) -> protocol::Result<Self> {
        Ok(match load {
            PlayerLoad::Found(data) => {
                let mut w = Writer::new(Vec::new(), 0);
                w.nbt_within(&data.extra, Encoding::LittleEndian, MAX_PLAYER_EXTRA_BYTES)?;
                Self::Found {
                    world: data.world.clone(),
                    position: data.position,
                    extra: w.into_inner(),
                }
            }
            PlayerLoad::Absent => Self::Absent,
            PlayerLoad::Damaged => Self::Damaged,
        })
    }

    /// The bytes the log charges for it.
    fn size(&self) -> usize {
        match self {
            Self::Found { world, extra, .. } => world.len().saturating_add(extra.len()),
            Self::Absent | Self::Damaged => 0,
        }
    }
}

/// What counting an entry against the bound gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charge {
    Fits,
    /// This entry reached the bound; the log is now truncated.
    Full,
    /// The log was truncated before.
    Truncated,
}

/// A log that cannot be written or read.
#[derive(Debug)]
pub(crate) enum Error {
    /// A logged packet or skin does not encode, or a written one does not decode.
    Packet(protocol::Error),
    /// The bytes are not an input log, or break a rule of its format.
    Format(&'static str),
    /// The log is of another format version ([`FORMAT`]): another Anchor wrote it.
    OtherFormat(u32),
    /// The log is of another protocol version: another Anchor wrote it.
    OtherProtocol(i32),
    /// The bytes do not match the log's checksum: the file is damaged.
    Checksum,
    /// The written log would be larger than a replay reads ([`MAX_FILE_BYTES`]), even from its
    /// newest committed segment on.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packet(_) => f.write_str("a logged packet does not encode or decode"),
            Self::Format(what) => write!(f, "not an input log: {what}"),
            Self::OtherFormat(found) => write!(f, "log format {found}, not {FORMAT}"),
            Self::OtherProtocol(found) => write!(f, "log protocol {found}, not {PROTOCOL_VERSION}"),
            Self::Checksum => f.write_str("the checksum does not match: the file is damaged"),
            Self::TooLarge => f.write_str("crash log over the replay limit; replay unavailable"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Packet(e) => Some(e),
            Self::Format(_)
            | Self::OtherFormat(_)
            | Self::OtherProtocol(_)
            | Self::Checksum
            | Self::TooLarge => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// A reader over a written log.
struct Read<'a>(&'a [u8]);

impl<'a> Read<'a> {
    /// A byte that is 0 or 1.
    fn flag(&mut self, what: &'static str) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Format(what)),
        }
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Format("fixed width"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(Error::Format("ends early"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    /// A tick of a written log, whose packets take what decoding them allocates from
    /// `packet_budget`.
    fn tick(&mut self, packet_budget: &mut usize) -> Result<Tick> {
        let number = self.u64()?;
        // An entry takes at least 18 bytes, a dropped session 8.
        let entries = self.count(18)?;
        let mut inputs = Vec::with_capacity(capacity::<Entry>(entries, self.0.len()));
        for _ in 0..entries {
            let session = SessionId::new(self.u64()?);
            let seq = self.u64()?;
            let kind = match self.u8()? {
                KIND_JOIN => {
                    let name = String::from_utf8(self.bytes()?.to_vec())
                        .map_err(|_| Error::Format("name"))?;
                    let has_xuid = self.flag("xuid flag")?;
                    let xuid = self.u64()?;
                    let uuid: [u8; 16] = self.array()?;
                    let device_os = self.i32()?;
                    let skin = read_skin(self.bytes()?).map_err(Error::Packet)?;
                    let address = self.address()?;
                    let client = self.client()?;
                    EntryKind::Join(Identity {
                        skin,
                        device_os,
                        address,
                        client,
                        name,
                        xuid: has_xuid.then_some(xuid),
                        uuid: protocol::Uuid::from_bytes(uuid),
                    })
                }
                KIND_PACKET => {
                    let id = self.u32()?;
                    let payload = self.bytes()?;
                    let packet =
                        Packet::decode(id, payload, packet_budget).map_err(Error::Packet)?;
                    EntryKind::Packet(packet)
                }
                KIND_LEAVE => EntryKind::Leave(self.leave_reason()?),
                KIND_CONSOLE => EntryKind::Console(self.text()?),
                KIND_JOB => EntryKind::Job(self.job()?),
                KIND_MEASURE => EntryKind::Measure(self.measure()?),
                _ => return Err(Error::Format("input kind")),
            };
            inputs.push(Entry { session, seq, kind });
        }
        let dropped_count = self.count(8)?;
        let mut dropped = Vec::with_capacity(capacity::<SessionId>(dropped_count, self.0.len()));
        for _ in 0..dropped_count {
            dropped.push(SessionId::new(self.u64()?));
        }
        // An answer takes at least 1 byte (a load failure).
        let answer_count = self.count(1)?;
        let mut responses = Vec::with_capacity(capacity::<Answer>(answer_count, self.0.len()));
        for _ in 0..answer_count {
            responses.push(self.answer()?);
        }
        Ok(Tick {
            number,
            inputs,
            dropped,
            responses,
        })
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A count of items of at least `min` bytes each, which the bytes left must be able to hold.
    fn count(&mut self, min: usize) -> Result<usize> {
        let n = usize::try_from(self.u32()?).map_err(|_| Error::Format("count"))?;
        if n.saturating_mul(min) > self.0.len() {
            return Err(Error::Format("count larger than the bytes left"));
        }
        Ok(n)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.count(1)?;
        self.take(n)
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()?.cast_signed())
    }

    fn text(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| Error::Format("text"))
    }

    fn job(&mut self) -> Result<JobOutcome> {
        match self.u8()? {
            JOB_DONE => {
                let json = self.bytes()?;
                if json.len() > crate::task::MAX_RESULT_BYTES {
                    return Err(Error::Format("job result over its limit"));
                }
                Ok(Ok(json.to_vec()))
            }
            code => crate::task::JobError::from_code(code)
                .map(Err)
                .ok_or(Error::Format("job outcome")),
        }
    }

    /// What a join tells of the client.
    fn client(&mut self) -> Result<Client> {
        let locale = self.text()?;
        let input_mode = crate::protocol::consts::InputMode::try_from(self.u32()?)
            .map_err(|_| Error::Format("input mode"))?;
        Ok(Client {
            locale,
            input_mode,
            device_model: self.text()?,
            device_id: self.text()?,
            self_signed_id: self.text()?,
            playfab_id: self.text()?,
            game_version: self.text()?,
            server_address: self.text()?,
            random_id: i64::from_le_bytes(self.array()?),
            port: u16::from_le_bytes(self.array()?),
            join_time_ms: i64::from_le_bytes(self.array()?),
        })
    }

    fn leave_reason(&mut self) -> Result<LeaveReason> {
        Ok(match self.u8()? {
            LEAVE_LEFT => LeaveReason::Left,
            LEAVE_CONNECTION_CLOSED => LeaveReason::ConnectionClosed,
            LEAVE_TIMED_OUT => LeaveReason::TimedOut,
            _ => return Err(Error::Format("leave reason")),
        })
    }

    fn measure(&mut self) -> Result<Measure> {
        Ok(match self.u8()? {
            MEASURE_TICK_TIME => Measure::TickTime(self.u32()?),
            MEASURE_PING => Measure::Ping(self.u32()?),
            MEASURE_CONNECTIONS => Measure::Connections {
                all: self.u32()?,
                logged_in: self.u32()?,
            },
            MEASURE_PROCESS => Measure::Process {
                memory: self.u64()?,
                uptime_ms: self.u64()?,
            },
            _ => return Err(Error::Format("measure kind")),
        })
    }

    fn address(&mut self) -> Result<Option<IpAddr>> {
        Ok(match self.u8()? {
            ADDRESS_NONE => None,
            ADDRESS_V4 => Some(IpAddr::from(self.array::<4>()?)),
            ADDRESS_V6 => Some(IpAddr::from(self.array::<16>()?)),
            _ => return Err(Error::Format("address kind")),
        })
    }

    fn world_index(&mut self) -> Result<WorldIndex> {
        usize::try_from(self.u32()?).map_err(|_| Error::Format("world index"))
    }

    /// An answer.
    fn answer(&mut self) -> Result<Answer> {
        Ok(match self.u8()? {
            ANSWER_CHUNK => {
                let world = self.world_index()?;
                let pos = ChunkPos::new(self.i32()?, self.i32()?);
                let origin = match self.u8()? {
                    ORIGIN_DISK => Origin::Disk,
                    ORIGIN_GENERATED => Origin::Generated,
                    ORIGIN_DAMAGED => Origin::Damaged,
                    _ => return Err(Error::Format("chunk origin")),
                };
                let checksum = self.u32()?;
                Answer::Chunk {
                    world,
                    pos,
                    origin,
                    checksum,
                }
            }
            ANSWER_PLAYER => {
                let key = self.text()?;
                let data = match self.u8()? {
                    PLAYER_FOUND => {
                        let world = self.text()?;
                        let mut position = [0.0; 3];
                        for c in &mut position {
                            *c = f64::from_bits(self.u64()?);
                        }
                        let extra = self.bytes()?;
                        read_extra(extra)?;
                        LoggedPlayer::Found {
                            world,
                            position,
                            extra: extra.to_vec(),
                        }
                    }
                    PLAYER_ABSENT => LoggedPlayer::Absent,
                    PLAYER_DAMAGED => LoggedPlayer::Damaged,
                    _ => return Err(Error::Format("player data kind")),
                };
                Answer::Player { key, data }
            }
            ANSWER_COMMITTED => Answer::Committed(self.u64()?),
            ANSWER_COMMIT_FAILED => Answer::CommitFailed(self.u64()?),
            ANSWER_CHECKED => Answer::Checked(match self.u8()? {
                CHECK_PASSED => Checked::Passed {
                    values: self.u64()?,
                },
                CHECK_DAMAGED => Checked::Damaged {
                    values: self.u64()?,
                    damaged: self.u64()?,
                    first: self.text()?,
                },
                CHECK_FAILED => Checked::Failed(self.text()?),
                _ => return Err(Error::Format("check result kind")),
            }),
            ANSWER_WORLD_OPEN_FAILED => Answer::WorldOpenFailed(self.world_index()?),
            _ => return Err(Error::Format("answer kind")),
        })
    }
}

/// Writes `log` to `<data_dir>/crash/input-<unix seconds>-<last tick>.log` and returns the path:
/// from its newest committed segment on if the whole log is over [`MAX_FILE_BYTES`], and not at
/// all if that is too. It never replaces a file. The main thread calls it after the tick thread
/// has ended.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the main thread writes the crash log after the tick thread ended"
)]
pub(crate) fn write_crash_log(data_dir: &Path, log: &InputLog) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let bytes = log
        .encode_within(MAX_FILE_BYTES)
        .map_err(std::io::Error::other)?;
    let dir = data_dir.join(CRASH_DIR);
    std::fs::create_dir_all(&dir)?;
    let tick = log
        .segments
        .iter()
        .flat_map(|s| &s.ticks)
        .last()
        .map_or(log.start(), |t| t.number);
    let path = dir.join(format!(
        "input-{}-{tick}.log",
        clock::unix_now().unwrap_or_default()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}

/// Appends a tick: its number, its inputs, the sessions it forgot and the answers it applied.
/// `body` is scratch space for packet bodies.
fn put_tick(out: &mut Vec<u8>, tick: &Tick, body: &mut Vec<u8>) -> Result<()> {
    out.extend_from_slice(&tick.number.to_le_bytes());
    put_len(out, tick.inputs.len())?;
    for entry in &tick.inputs {
        out.extend_from_slice(&entry.session.get().to_le_bytes());
        out.extend_from_slice(&entry.seq.to_le_bytes());
        match &entry.kind {
            EntryKind::Join(identity) => {
                out.push(KIND_JOIN);
                put_bytes(out, identity.name.as_bytes())?;
                out.push(u8::from(identity.xuid.is_some()));
                out.extend_from_slice(&identity.xuid.unwrap_or(0).to_le_bytes());
                out.extend_from_slice(identity.uuid.as_bytes());
                out.extend_from_slice(&identity.device_os.to_le_bytes());
                put_bytes(out, &skin_bytes(&identity.skin).map_err(Error::Packet)?)?;
                match identity.address {
                    Some(IpAddr::V4(a)) => {
                        out.push(ADDRESS_V4);
                        out.extend_from_slice(&a.octets());
                    }
                    Some(IpAddr::V6(a)) => {
                        out.push(ADDRESS_V6);
                        out.extend_from_slice(&a.octets());
                    }
                    None => out.push(ADDRESS_NONE),
                }
                put_client(out, &identity.client)?;
            }
            EntryKind::Packet(packet) => {
                out.push(KIND_PACKET);
                out.extend_from_slice(&packet.id().to_le_bytes());
                body.clear();
                packet.encode(body).map_err(Error::Packet)?;
                put_bytes(out, body)?;
            }
            EntryKind::Leave(reason) => out.extend_from_slice(&[KIND_LEAVE, leave_tag(*reason)]),
            EntryKind::Console(line) => {
                out.push(KIND_CONSOLE);
                put_bytes(out, line.as_bytes())?;
            }
            EntryKind::Job(outcome) => {
                out.push(KIND_JOB);
                match outcome {
                    Ok(json) => {
                        out.push(JOB_DONE);
                        put_bytes(out, json)?;
                    }
                    Err(e) => out.push(e.code()),
                }
            }
            EntryKind::Measure(measure) => {
                out.push(KIND_MEASURE);
                match *measure {
                    Measure::TickTime(micros) => {
                        out.push(MEASURE_TICK_TIME);
                        out.extend_from_slice(&micros.to_le_bytes());
                    }
                    Measure::Ping(millis) => {
                        out.push(MEASURE_PING);
                        out.extend_from_slice(&millis.to_le_bytes());
                    }
                    Measure::Connections { all, logged_in } => {
                        out.push(MEASURE_CONNECTIONS);
                        out.extend_from_slice(&all.to_le_bytes());
                        out.extend_from_slice(&logged_in.to_le_bytes());
                    }
                    Measure::Process { memory, uptime_ms } => {
                        out.push(MEASURE_PROCESS);
                        out.extend_from_slice(&memory.to_le_bytes());
                        out.extend_from_slice(&uptime_ms.to_le_bytes());
                    }
                }
            }
        }
    }
    put_len(out, tick.dropped.len())?;
    for session in &tick.dropped {
        out.extend_from_slice(&session.get().to_le_bytes());
    }
    put_len(out, tick.responses.len())?;
    for answer in &tick.responses {
        put_answer(out, answer)?;
    }
    Ok(())
}

/// Appends what a join tells of the client.
fn put_client(out: &mut Vec<u8>, client: &Client) -> Result<()> {
    let Client {
        locale,
        input_mode,
        device_model,
        device_id,
        self_signed_id,
        random_id,
        playfab_id,
        game_version,
        server_address,
        port,
        join_time_ms,
    } = client;
    put_bytes(out, locale.as_bytes())?;
    out.extend_from_slice(&u32::from(*input_mode).to_le_bytes());
    for text in [
        device_model,
        device_id,
        self_signed_id,
        playfab_id,
        game_version,
        server_address,
    ] {
        put_bytes(out, text.as_bytes())?;
    }
    out.extend_from_slice(&random_id.to_le_bytes());
    out.extend_from_slice(&port.to_le_bytes());
    out.extend_from_slice(&join_time_ms.to_le_bytes());
    Ok(())
}

/// Appends an answer: its kind, then its fields.
fn put_answer(out: &mut Vec<u8>, answer: &Answer) -> Result<()> {
    match answer {
        Answer::Chunk {
            world,
            pos,
            origin,
            checksum,
        } => {
            out.push(ANSWER_CHUNK);
            put_world_index(out, *world)?;
            out.extend_from_slice(&pos.x.to_le_bytes());
            out.extend_from_slice(&pos.z.to_le_bytes());
            out.push(match origin {
                Origin::Disk => ORIGIN_DISK,
                Origin::Generated => ORIGIN_GENERATED,
                Origin::Damaged => ORIGIN_DAMAGED,
            });
            out.extend_from_slice(&checksum.to_le_bytes());
        }
        Answer::Player { key, data } => {
            out.push(ANSWER_PLAYER);
            put_bytes(out, key.as_bytes())?;
            match data {
                LoggedPlayer::Found {
                    world,
                    position,
                    extra,
                } => {
                    out.push(PLAYER_FOUND);
                    put_bytes(out, world.as_bytes())?;
                    for c in position {
                        out.extend_from_slice(&c.to_bits().to_le_bytes());
                    }
                    put_bytes(out, extra)?;
                }
                LoggedPlayer::Absent => out.push(PLAYER_ABSENT),
                LoggedPlayer::Damaged => out.push(PLAYER_DAMAGED),
            }
        }
        Answer::Committed(start) => {
            out.push(ANSWER_COMMITTED);
            out.extend_from_slice(&start.to_le_bytes());
        }
        Answer::CommitFailed(start) => {
            out.push(ANSWER_COMMIT_FAILED);
            out.extend_from_slice(&start.to_le_bytes());
        }
        Answer::Checked(found) => {
            out.push(ANSWER_CHECKED);
            match found {
                Checked::Passed { values } => {
                    out.push(CHECK_PASSED);
                    out.extend_from_slice(&values.to_le_bytes());
                }
                Checked::Damaged {
                    values,
                    damaged,
                    first,
                } => {
                    out.push(CHECK_DAMAGED);
                    out.extend_from_slice(&values.to_le_bytes());
                    out.extend_from_slice(&damaged.to_le_bytes());
                    put_bytes(out, first.as_bytes())?;
                }
                Checked::Failed(why) => {
                    out.push(CHECK_FAILED);
                    put_bytes(out, why.as_bytes())?;
                }
            }
        }
        Answer::WorldOpenFailed(world) => {
            out.push(ANSWER_WORLD_OPEN_FAILED);
            put_world_index(out, *world)?;
        }
    }
    Ok(())
}

/// Appends a world index as a u32.
fn put_world_index(out: &mut Vec<u8>, world: WorldIndex) -> Result<()> {
    let world = u32::try_from(world).map_err(|_| Error::Format("world index over u32"))?;
    out.extend_from_slice(&world.to_le_bytes());
    Ok(())
}

/// Appends a count as a u32.
fn put_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len = u32::try_from(len).map_err(|_| Error::Format("count over u32"))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends bytes after their u32 length.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    put_len(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// The tag a leave reason is written and chained as.
fn leave_tag(reason: LeaveReason) -> u8 {
    match reason {
        LeaveReason::Left => LEAVE_LEFT,
        LeaveReason::ConnectionClosed => LEAVE_CONNECTION_CLOSED,
        LeaveReason::TimedOut => LEAVE_TIMED_OUT,
    }
}

/// A logged player extra: one little-endian NBT root compound and nothing after it, read within
/// the budget its players record was read with, which it came from.
fn read_extra(bytes: &[u8]) -> Result<crate::nbt::Compound> {
    let mut r = protocol::Reader::new(bytes, crate::storage::seal::DECODE_BUDGET, 0);
    let extra = r
        .nbt_within(Encoding::LittleEndian, MAX_PLAYER_EXTRA_BYTES)
        .map_err(Error::Packet)?;
    if r.remaining().is_empty() {
        Ok(extra)
    } else {
        Err(Error::Format("bytes after a player extra"))
    }
}

/// The items of room a list of the log needs for its next item: none while it has room,
/// otherwise as many as it holds (one for an empty list), so its room doubles as it grows and
/// the bound counts the room it reserves exactly ([`room_bytes`]).
fn growth<T>(list: &Vec<T>) -> usize {
    if list.len() < list.capacity() {
        0
    } else {
        list.capacity().max(1)
    }
}

/// The bytes `items` of room of a list of `T` take.
fn room_bytes<T>(items: usize) -> usize {
    items.saturating_mul(size_of::<T>())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tick::queue::Skin;

    #[test]
    fn an_empty_tick_counts_against_the_bound() {
        let mut log = InputLog::default();
        log.begin(0);
        assert_eq!(log.bytes, size_of::<Tick>());
    }

    /// The bound counts the memory the log holds: after many idle ticks of one small input each,
    /// the room of every list the log reserved, its ticks' and their inputs', is what it counted.
    #[test]
    fn the_bound_counts_the_room_the_log_holds() {
        let mut log = InputLog::default();
        for number in 0..1000 {
            log.begin(number);
            let measure = EntryKind::Measure(Measure::TickTime(7));
            log.record(SessionId::ENGINE, number, measure, 0);
        }
        let ticks = &log.last_segment().ticks;
        let held = ticks
            .iter()
            .fold(room_bytes::<Tick>(ticks.capacity()), |sum, t| {
                sum + room_bytes::<Entry>(t.inputs.capacity())
            });
        assert_eq!(log.bytes, held);
    }

    fn numbers(log: &InputLog) -> Vec<u64> {
        log.ticks().iter().map(|t| t.number).collect()
    }

    fn starts(log: &InputLog) -> Vec<u64> {
        log.segments().map(Segment::start).collect()
    }

    #[test]
    fn committed_segments_stay_until_the_bound_needs_room_and_the_newest_committed_stays() {
        let mut log = InputLog::default();
        log.begin(0);
        log.begin(1);
        log.snapshot(2);
        log.begin(2);
        log.snapshot(3);
        log.begin(3);
        assert_eq!(numbers(&log), [0, 1, 2, 3]);
        // A commit nobody snapshotted changes nothing.
        log.committed(7);
        assert_eq!(starts(&log), [0, 2, 3]);
        // A commit keeps the segments before it while there is room.
        log.committed(2);
        assert_eq!((log.start(), numbers(&log)), (0, vec![0, 1, 2, 3]));
        // Each of the four ticks took one tick of room: the first segment's two, and one each.
        let (tick, entry) = (size_of::<Tick>(), size_of::<Entry>());
        assert_eq!(log.bytes, 4 * tick);
        // An input that needs room drops the segments before the newest committed one; it leaves
        // a byte less than a tick's room free.
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave(LeaveReason::Left),
            MAX_BYTES - 3 * tick - entry + 1,
        );
        assert_eq!(starts(&log), [2, 3]);
        assert_eq!(numbers(&log), [2, 3]);
        assert!(!log.is_truncated());
        assert_eq!(
            log.bytes,
            MAX_BYTES - tick + 1,
            "dropped ticks free their bytes"
        );
        // With no committed segment to drop, the input that does not fit truncates.
        log.record(
            SessionId::new(1),
            1,
            EntryKind::Leave(LeaveReason::Left),
            tick,
        );
        assert!(log.is_truncated());
        assert_eq!(starts(&log), [2, 3]);
        // After the next commit, the segment before it goes once the bound needs room: here for
        // the next tick.
        log.committed(3);
        assert_eq!(starts(&log), [2, 3]);
        log.snapshot(4);
        log.begin(4);
        assert_eq!(starts(&log), [3, 4]);
        assert!(!log.last_segment().is_truncated());
        // The same snapshot twice starts one segment.
        log.snapshot(4);
        assert_eq!(starts(&log), [3, 4]);
    }

    #[test]
    fn recording_resumes_after_a_truncated_segment_and_room_drops_it() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave(LeaveReason::Left),
            MAX_BYTES,
        );
        assert!(log.is_truncated());
        log.snapshot(1);
        log.begin(1);
        assert!(
            numbers(&log).is_empty(),
            "a replay from the start stops in the truncated segment"
        );
        log.committed(1);
        let segments: Vec<_> = log.segments().collect();
        assert_eq!(segments[1].ticks().len(), 1, "the next segment records");
        assert!(segments[0].is_truncated() && !segments[1].is_truncated());
        log.record(
            SessionId::new(1),
            1,
            EntryKind::Leave(LeaveReason::Left),
            MAX_BYTES - size_of::<Tick>() - size_of::<Entry>(),
        );
        assert!(!log.is_truncated(), "room dropped the truncated segment");
        assert_eq!((log.start(), numbers(&log)), (1, vec![1]));
    }

    /// A replay's log from the checkpoint of a new test state, with the replayed log's snapshots
    /// at the ticks of `planned`.
    fn replay_log(planned: std::collections::BTreeSet<u64>) -> InputLog {
        let state = crate::tick::tests::new_state(
            crate::tick::tests::settings(|_| {}),
            crate::event::Listeners::default(),
        );
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let checkpoint = state.checkpoint(&pool);
        assert!(pool.shutdown());
        InputLog::replaying(checkpoint, Some(7), planned)
    }

    #[test]
    fn a_quarter_of_the_bound_since_the_last_commit_asks_for_a_commit() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave(LeaveReason::Left),
            COMMIT_BYTES / 2,
        );
        log.snapshot(1);
        log.begin(1);
        assert!(!log.wants_commit(1));
        // The bytes since the newest committed state count, across segments.
        log.record(
            SessionId::new(1),
            1,
            EntryKind::Leave(LeaveReason::Left),
            COMMIT_BYTES / 2,
        );
        assert!(log.wants_commit(2));
        log.committed(1);
        assert!(!log.wants_commit(2), "only segment 1 counts now");
        // A replay's log follows the replayed log's snapshots instead.
        let replay = replay_log([9].into());
        assert!(replay.wants_commit(9) && !replay.wants_commit(8));
    }

    #[test]
    fn a_replay_log_starts_with_its_checkpoint_and_takes_none_before_a_snapshot() {
        let mut log = replay_log(std::collections::BTreeSet::new());
        assert!(!log.needs_checkpoint());
        let first = log.last_segment();
        assert!(first.taken().is_some());
        assert_eq!(first.identity(), Some(7));
        assert_eq!(
            log.identity(),
            7,
            "the chain goes on from the file's identity"
        );
        let start = first.start();
        log.snapshot(start + 1);
        assert!(
            log.needs_checkpoint(),
            "a snapshot's segment waits for its checkpoint"
        );
    }

    #[test]
    fn a_log_over_the_limit_is_written_from_its_newest_committed_segment() {
        let mut log = InputLog::default();
        log.begin(0);
        log.segments[0].checkpoint = Some(Point::Written(vec![1; 1000]));
        log.snapshot(1);
        log.begin(1);
        log.segments[1].checkpoint = Some(Point::Written(vec![2; 1000]));
        log.committed(1);
        log.snapshot(2);
        log.begin(2);
        let full = log.encode().unwrap();
        let limit = full.len() - 1;
        let written = log.encode_within(limit).unwrap();
        assert!(written.len() <= limit, "{} bytes", written.len());
        let read = InputLog::decode(&written).unwrap();
        assert_eq!(starts(&read), [1, 2]);
        assert_eq!(log.encode_within(full.len()).unwrap(), full);
        // Not even the newest committed segment's checkpoint fits: nothing is written.
        assert!(matches!(log.encode_within(1000), Err(Error::TooLarge)));
        let mut uncommitted = InputLog::default();
        uncommitted.begin(0);
        uncommitted.segments[0].checkpoint = Some(Point::Written(vec![1; 1000]));
        assert!(matches!(
            uncommitted.encode_within(1000),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn the_commit_identity_chains_every_input_even_past_the_bound() {
        let run = |tick_time: u32, size: usize| {
            let mut log = InputLog::from_state(0, Some(3));
            log.begin(0);
            log.record(
                SessionId::new(1),
                0,
                EntryKind::Leave(LeaveReason::Left),
                size,
            );
            log.record(
                SessionId::ENGINE,
                0,
                EntryKind::Measure(Measure::TickTime(tick_time)),
                0,
            );
            log.response(&Response::CommitFailed { start: 1 });
            log.dropped(SessionId::new(2));
            log.snapshot(1);
            log
        };
        let a = run(10, 0);
        assert_ne!(a.identity(), 3);
        assert_eq!(a.identity(), run(10, 0).identity(), "the same inputs");
        assert_eq!(
            a.identity(),
            run(10, 5).identity(),
            "the size is not an input"
        );
        assert_ne!(a.identity(), run(11, 0).identity(), "another measured time");
        // Past the bound nothing is recorded, but the identity still follows the inputs.
        let full = run(10, MAX_BYTES);
        assert!(full.is_truncated());
        assert_eq!(full.identity(), a.identity());
        assert_ne!(full.identity(), run(11, MAX_BYTES).identity());
        // The snapshot's segment keeps the identity its commit writes.
        assert_eq!(a.last_segment().identity(), Some(a.identity()));
        assert_eq!(a.segments[0].identity(), Some(3));
    }

    #[test]
    fn client_data_and_measures_survive_the_written_log() {
        let client = Client {
            locale: "en_GB".to_owned(),
            input_mode: crate::protocol::consts::InputMode::GamePad,
            device_model: "Xbox Series X".to_owned(),
            device_id: "abc".to_owned(),
            self_signed_id: "8f3c1a2e-5b7d-4e6f-9a0b-1c2d3e4f5a6b".to_owned(),
            random_id: -7,
            playfab_id: "ABCDEF0123456789".to_owned(),
            game_version: "1.26.52".to_owned(),
            server_address: "[::1]:19132".to_owned(),
            port: 61_000,
            join_time_ms: -5,
        };
        let identity = |client| Identity {
            name: "Steve".to_owned(),
            xuid: None,
            uuid: protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: 12,
            address: Some(IpAddr::from([1, 2, 3, 4])),
            client,
        };
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Join(identity(client)), 0);
        for (session, seq, measure) in [
            (SessionId::new(1), 1, Measure::Ping(37)),
            (
                SessionId::NETWORK,
                0,
                Measure::Connections {
                    all: 9,
                    logged_in: 4,
                },
            ),
            (SessionId::ENGINE, 0, Measure::TickTime(u32::MAX)),
            (
                SessionId::PROCESS,
                0,
                Measure::Process {
                    memory: u64::MAX,
                    uptime_ms: 61_000,
                },
            ),
        ] {
            log.record(session, seq, EntryKind::Measure(measure), 0);
        }
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(read.ticks(), log.ticks());
    }

    #[test]
    fn leave_reasons_survive_the_written_log() {
        let mut log = InputLog::default();
        log.begin(0);
        for (session, reason) in [
            (1, LeaveReason::Left),
            (2, LeaveReason::ConnectionClosed),
            (3, LeaveReason::TimedOut),
        ] {
            log.record(SessionId::new(session), 0, EntryKind::Leave(reason), 0);
        }
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        let reasons: Vec<_> = read.ticks()[0]
            .inputs
            .iter()
            .map(|entry| entry.kind.clone())
            .collect();
        assert_eq!(
            reasons,
            [
                EntryKind::Leave(LeaveReason::Left),
                EntryKind::Leave(LeaveReason::ConnectionClosed),
                EntryKind::Leave(LeaveReason::TimedOut),
            ]
        );
    }

    /// Puts the checksum of the bytes before it at the end of a written log.
    fn reseal(bytes: &mut [u8]) {
        let Some(at) = bytes.len().checked_sub(4) else {
            return;
        };
        let mut crc = Crc32c::new();
        crc.update(&bytes[..at]);
        bytes[at..].copy_from_slice(&crc.finish().to_le_bytes());
    }

    #[test]
    fn an_unknown_leave_reason_is_damage() {
        let mut log = InputLog::default();
        log.begin(0);
        let timed_out = EntryKind::Leave(LeaveReason::TimedOut);
        log.record(SessionId::new(1), 0, timed_out, 0);
        let mut bytes = log.encode().unwrap();
        let at = bytes
            .windows(2)
            .rposition(|pair| pair == [KIND_LEAVE, LEAVE_TIMED_OUT])
            .unwrap();
        bytes[at + 1] = 9;
        reseal(&mut bytes);
        assert!(matches!(
            InputLog::decode(&bytes),
            Err(Error::Format("leave reason"))
        ));
    }

    #[test]
    fn a_log_of_another_format_or_protocol_is_refused_with_its_version() {
        let mut log = InputLog::default();
        log.begin(0);
        let ping = EntryKind::Measure(Measure::Ping(1));
        log.record(SessionId::new(1), 0, ping, 0);
        let written = log.encode().unwrap();
        assert!(InputLog::decode(&written).is_ok());
        let at = MAGIC.len();
        for format in [FORMAT - 1, FORMAT + 1] {
            let mut other = written.clone();
            other[at..at + 4].copy_from_slice(&format.to_le_bytes());
            reseal(&mut other);
            let read = InputLog::decode(&other).map(drop);
            assert!(
                matches!(read, Err(Error::OtherFormat(f)) if f == format),
                "{read:?}"
            );
        }
        for protocol in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
            let mut other = written.clone();
            other[at + 4..at + 8].copy_from_slice(&protocol.to_le_bytes());
            reseal(&mut other);
            let read = InputLog::decode(&other).map(drop);
            assert!(
                matches!(read, Err(Error::OtherProtocol(p)) if p == protocol),
                "{read:?}"
            );
        }
    }

    #[test]
    fn a_join_whose_skin_has_bytes_after_it_is_refused() {
        let identity = Identity {
            name: "Steve".to_owned(),
            xuid: None,
            uuid: protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: 7,
            address: None,
            client: Client::default(),
        };
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Join(identity), 0);
        let written = log.encode().unwrap();
        assert!(InputLog::decode(&written).is_ok());
        // The skin as the log writes it, after its length; the same bytes and one more after it.
        let skin = skin_bytes(&Skin::default()).unwrap();
        let mut logged = u32::try_from(skin.len()).unwrap().to_le_bytes().to_vec();
        logged.extend_from_slice(&skin);
        let at = written
            .windows(logged.len())
            .position(|w| w == logged)
            .expect("the log holds the skin");
        let mut longer = written[..at].to_vec();
        longer.extend_from_slice(&u32::try_from(skin.len() + 1).unwrap().to_le_bytes());
        longer.extend_from_slice(&skin);
        longer.push(0);
        longer.extend_from_slice(&written[at + logged.len()..]);
        reseal(&mut longer);
        let read = InputLog::decode(&longer).map(drop);
        assert!(matches!(read, Err(Error::Packet(_))), "{read:?}");
    }

    /// Two snapshots at one tick (the last one of a shutdown follows a boundary's) commit
    /// different identities, so a start can tell the files of the two saves apart.
    #[test]
    fn snapshots_at_one_tick_have_different_identities() {
        let mut log = InputLog::from_state(0, Some(42));
        log.begin(0);
        log.snapshot(1);
        let first = log.identity();
        log.snapshot(1);
        assert_ne!(log.identity(), first);
    }

    #[test]
    fn segments_keep_their_commit_identity() {
        let mut log = InputLog::from_state(0, Some(42));
        log.begin(0);
        log.record(
            SessionId::ENGINE,
            0,
            EntryKind::Measure(Measure::TickTime(9)),
            0,
        );
        log.snapshot(1);
        let snapshot = log.identity();
        log.begin(1);
        let identities = |log: &InputLog| log.segments().map(Segment::identity).collect::<Vec<_>>();
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(identities(&read), [Some(42), Some(snapshot)]);
        assert_eq!(read.encode().unwrap(), log.encode().unwrap());
        let unknown = InputLog::starting_at(0);
        let read = InputLog::decode(&unknown.encode().unwrap()).unwrap();
        assert_eq!(identities(&read), [None]);
    }

    #[test]
    fn console_lines_and_addresses_survive_the_written_log() {
        let identity = |address| Identity {
            name: "Steve".to_owned(),
            xuid: None,
            uuid: protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: 7,
            address,
            client: Client::default(),
        };
        let mut log = InputLog::default();
        log.begin(0);
        let v6: IpAddr = "2001:db8::7".parse().unwrap();
        log.record(SessionId::new(1), 0, EntryKind::Join(identity(Some(v6))), 0);
        log.record(
            SessionId::new(2),
            0,
            EntryKind::Join(identity(Some(IpAddr::from([1, 2, 3, 4])))),
            0,
        );
        log.record(
            SessionId::CONSOLE,
            0,
            EntryKind::Console("op steve".to_owned()),
            8,
        );
        let bytes = log.encode().unwrap();
        let read = InputLog::decode(&bytes).unwrap();
        assert_eq!(read.ticks(), log.ticks());
    }

    /// Attacks with an enchanted sword, whose item NBT costs a reader far more than its bytes,
    /// read back from the written log.
    #[test]
    fn attacks_with_an_enchanted_sword_survive_the_written_log() {
        use crate::item::{Enchantment, Extra, Kind, Stack};
        use crate::protocol::packet::InventoryTransaction;
        use crate::protocol::{InventoryTransactionData, ItemInstance};
        let sword = Stack::new(Kind::DiamondSword.default_type(), 1)
            .with_extra(Some(Extra {
                enchantments: vec![
                    (Enchantment::Sharpness, 5),
                    (Enchantment::Unbreaking, 3),
                    (Enchantment::FireAspect, 2),
                    (Enchantment::Knockback, 2),
                    (Enchantment::Mending, 1),
                ],
                ..Extra::default()
            }))
            .unwrap();
        let attack = protocol::UseItemOnEntityTransactionData {
            target_entity_runtime_id: 2,
            action_type: 1,
            held_item: ItemInstance {
                stack: sword.to_network(),
                ..ItemInstance::default()
            },
            ..protocol::UseItemOnEntityTransactionData::default()
        };
        let packet = Packet::InventoryTransaction(Box::new(InventoryTransaction {
            transaction_data: InventoryTransactionData::UseItemOnEntity(Box::new(attack)),
            ..InventoryTransaction::default()
        }));
        let mut log = InputLog::default();
        for tick in 0..20 {
            log.begin(tick);
            log.record(
                SessionId::new(1),
                tick,
                EntryKind::Packet(packet.clone()),
                64,
            );
        }
        let bytes = log.encode().unwrap();
        let read = InputLog::decode(&bytes).expect("the log reads back");
        assert_eq!(read.ticks(), log.ticks());
    }

    /// A written log whose packets together take a reader more than the live log holds is
    /// refused, though each packet alone fits a batch.
    #[test]
    fn a_log_whose_packets_decode_past_the_bound_is_refused() {
        use crate::protocol::InventoryAction;
        use crate::protocol::packet::InventoryTransaction;
        // Each default action takes a few bytes on the wire and far more decoded.
        let packet = Packet::InventoryTransaction(Box::new(InventoryTransaction {
            actions: vec![InventoryAction::default(); protocol::MAX_LIST_LEN],
            ..InventoryTransaction::default()
        }));
        let mut payload = Vec::new();
        packet.encode(&mut payload).unwrap();
        let mut budget = usize::MAX;
        Packet::decode(packet.id(), &payload, &mut budget).unwrap();
        let copies = MAX_BYTES / (usize::MAX - budget) + 1;
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Packet(packet), 0);
        let one = log.encode().unwrap();
        // The tick's one entry (session, seq, kind, id, length, payload) follows its count and
        // is followed by the dropped and answer counts and the checksum.
        let end = one.len() - 12;
        let start = end - (25 + payload.len());
        let mut crafted = one[..start - 4].to_vec();
        crafted.extend_from_slice(&u32::try_from(copies).unwrap().to_le_bytes());
        for _ in 0..copies {
            crafted.extend_from_slice(&one[start..end]);
        }
        crafted.extend_from_slice(&one[end..]);
        reseal(&mut crafted);
        let read = InputLog::decode(&crafted).map(|_| ());
        assert!(
            matches!(read, Err(Error::Packet(protocol::Error::Size))),
            "{read:?}"
        );
    }

    #[test]
    fn job_results_survive_the_written_log() {
        let mut log = InputLog::default();
        log.begin(0);
        let results = [
            Ok(br#"{"a":[1,2]}"#.to_vec()),
            Ok(Vec::new()),
            Err(crate::task::JobError::Panicked),
            Err(crate::task::JobError::TooLarge),
        ];
        for (id, outcome) in (0..).zip(results) {
            let size = outcome.as_ref().map_or(0, Vec::len);
            log.record(SessionId::JOBS, id, EntryKind::Job(outcome), size);
        }
        let bytes = log.encode().unwrap();
        let read = InputLog::decode(&bytes).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        assert_eq!(read.encode().unwrap(), bytes);
        // An unknown outcome byte is an error.
        let mut bad = InputLog::default();
        bad.begin(0);
        bad.record(
            SessionId::JOBS,
            0,
            EntryKind::Job(Err(crate::task::JobError::Encode)),
            0,
        );
        let mut bytes = bad.encode().unwrap();
        // The tick ends with the outcome byte, then no dropped sessions and no answers; the
        // checksum follows.
        let at = bytes.len() - 4 - 8 - 1;
        assert_eq!(bytes[at], crate::task::JobError::Encode.code());
        bytes[at] = 99;
        reseal(&mut bytes);
        assert!(matches!(
            InputLog::decode(&bytes),
            Err(Error::Format("job outcome"))
        ));
    }

    #[test]
    fn world_open_failures_survive_the_written_log() {
        let mut log = InputLog::default();
        log.begin(0);
        log.response(&Response::WorldOpenFailed { world: 3 });
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        assert_eq!(read.ticks()[0].responses, [Answer::WorldOpenFailed(3)]);
    }

    /// A segment dropped for room is handed over once, not freed by the log.
    #[test]
    fn segments_dropped_for_room_are_handed_over_once() {
        let mut log = InputLog::default();
        log.begin(0);
        log.snapshot(1);
        log.begin(1);
        log.committed(1);
        assert!(log.take_discarded().is_empty(), "there is room");
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave(LeaveReason::Left),
            MAX_BYTES - size_of::<Tick>() - size_of::<Entry>(),
        );
        let discarded = log.take_discarded();
        assert_eq!(
            discarded.iter().map(Segment::start).collect::<Vec<_>>(),
            [0]
        );
        assert!(log.take_discarded().is_empty());
    }

    #[test]
    fn check_results_survive_the_written_log() {
        let found = [
            Checked::Passed { values: 7 },
            Checked::Damaged {
                values: 9,
                damaged: 2,
                first: "table chunks: a stored value does not match its checksum".to_owned(),
            },
            Checked::Failed("the server file was reopened after a failure".to_owned()),
        ];
        let mut log = InputLog::default();
        log.begin(0);
        for found in &found {
            log.response(&Response::Checked(found.clone()));
        }
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        assert_eq!(read.ticks()[0].responses, found.map(Answer::Checked));
    }

    #[test]
    fn a_written_log_keeps_its_start() {
        let mut log = InputLog::starting_at(40);
        log.begin(40);
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!((read.start(), numbers(&read)), (40, vec![40]));
    }

    #[test]
    fn chunk_checksums_and_player_extras_survive_the_written_log() {
        use crate::save::Stored;
        use crate::storage::PlayerKey;
        use crate::world::{Chunk, Flat};

        let mut rng = crate::protocol::Rng(0x109_0001);
        let data = PlayerData {
            world: "world".to_owned(),
            position: [0.1, -58.38, f64::from(f32::MAX) * 4.0],
            extra: crate::storage::codec::tests::random_compound(&mut rng, 5),
        };
        let pos = ChunkPos::new(-3, 9);
        let chunk = || Box::new(Chunk::generate(&Flat, 0, pos));
        let stored = Stored {
            checksum: 0xdead_beef,
            ..Stored::default()
        };
        let responses = [
            Response::Chunk {
                world: 1,
                pos,
                loaded: Loaded::Disk(chunk(), stored.clone()),
            },
            Response::Chunk {
                world: 0,
                pos,
                loaded: Loaded::Generated(chunk(), Stored::default()),
            },
            Response::Chunk {
                world: 0,
                pos,
                loaded: Loaded::Damaged,
            },
            Response::Player {
                key: PlayerKey::offline("Steve"),
                data: PlayerLoad::Found(data.clone()),
            },
            Response::Player {
                key: PlayerKey::xuid(3),
                data: PlayerLoad::Absent,
            },
        ];
        let mut log = InputLog::default();
        log.begin(0);
        for response in &responses {
            log.response(response);
        }
        let charged = log.bytes;
        let answers = log.last_segment().ticks[0].responses.capacity();
        let room = size_of::<Tick>() + room_bytes::<Answer>(answers);
        assert!(
            charged > room + data.world.len(),
            "the player extra is charged: {charged}"
        );
        let bytes = log.encode().unwrap();
        let read = InputLog::decode(&bytes).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        assert_eq!(read.encode().unwrap(), bytes);
        let answers = &read.ticks()[0].responses;
        assert!(matches!(
            answers[0],
            Answer::Chunk {
                origin: Origin::Disk,
                checksum: 0xdead_beef,
                ..
            }
        ));
        let Answer::Player { data: logged, .. } = &answers[3] else {
            panic!("{answers:?}");
        };
        assert_eq!(logged.to_load().unwrap(), PlayerLoad::Found(data));
        // Damaged written logs are errors, never panics: a damaged log fails its checksum, and
        // the fields themselves are checked with the checksum made right again.
        let mut flipped = bytes.clone();
        flipped[40] ^= 1;
        assert!(matches!(InputLog::decode(&flipped), Err(Error::Checksum)));
        crate::protocol::fuzz(&bytes, 5000, |b| {
            let mut b = b.to_vec();
            reseal(&mut b);
            InputLog::decode(&b)
                .map(drop)
                .map_err(|_| crate::protocol::Error::Size)
        });
    }

    #[test]
    fn segments_keep_their_marks_checkpoints_and_fingerprints_in_the_written_log() {
        let fingerprints = Fingerprints {
            settings: [7; 32],
            registrations: [9; 32],
        };
        let mut log = InputLog {
            fingerprints,
            ..InputLog::default()
        };
        log.begin(0);
        log.snapshot(1);
        log.begin(1);
        log.segments[1].checkpoint = Some(Point::Written(vec![1, 2, 3]));
        log.snapshot(2);
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave(LeaveReason::Left),
            MAX_BYTES,
        );
        log.committed(1);
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert_eq!(read.fingerprints(), fingerprints);
        let segments: Vec<_> = read.segments().collect();
        let marks: Vec<_> = segments
            .iter()
            .map(|s| (s.start(), s.committed, s.is_truncated(), s.checkpoint()))
            .collect();
        assert_eq!(
            marks,
            [
                (0, true, false, None),
                (1, true, false, Some(&[1, 2, 3][..])),
                (2, false, true, None),
            ]
        );
        assert_eq!(segments[1].ticks().len(), 1);
        assert_eq!(read.encode().unwrap(), log.encode().unwrap());
    }
}
