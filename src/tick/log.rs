//! The input log: every input each tick applied, in order, so that a crash can be replayed from a
//! saved state.
//!
//! The tick moves each input into the log before it applies it, and applies the logged value, so
//! recording copies nothing large and the log of a tick that panics holds the input that did.
//! Packets are encoded only when the log is written, on the main thread after the tick thread
//! ended. The log is a list of segments: one from the start of the process and one per save
//! snapshot, each starting at the tick its state was taken with a checkpoint of that state
//! ([`super::checkpoint`]), which a replay starts from with the server file of the same tick.
//!
//! Committed segments are kept while there is room: the oldest go only when the bound needs
//! room ([`MAX_BYTES`], inputs counted by their size on the wire, and the checkpoints of the
//! segments that may go), never the newest committed one, which the crash log needs. Past the
//! bound the segment being recorded stops and is marked truncated, and a replay ends there; the
//! next snapshot starts a new segment. A quarter of the bound recorded since the last commit asks
//! for a snapshot ([`InputLog::wants_commit`]), so the rest covers the commit's time and a
//! segment is not truncated in normal running.
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

use super::checkpoint::{Checkpoint, capacity};
#[cfg(test)]
use super::queue::Skin;
use super::queue::{Client, Identity, JobOutcome, Measure, SessionId};
use crate::clock;
use crate::item::SHIELD_RUNTIME_ID;
use crate::nbt::Encoding;
use crate::protocol::{self, Writer, packet::Packet};
use crate::save::{Loaded, PlayerData, PlayerLoad, Response, WorldIndex};
use crate::storage::Checked;
use crate::storage::codec::MAX_PLAYER_EXTRA_BYTES;
use crate::storage::seal::Crc32c;
use crate::world::ChunkPos;

/// The most input bytes the log holds.
pub(crate) const MAX_BYTES: usize = 64 << 20;
/// The input bytes recorded since the last commit at which the log asks for a snapshot.
const COMMIT_BYTES: usize = MAX_BYTES / 4;
/// Bytes counted per entry besides its packet, so that many empty inputs are bounded too.
const ENTRY_COST: usize = 32;
/// The first bytes of a written log.
const MAGIC: &[u8; 16] = b"ANCHOR-INPUT-LOG";
/// The written format's version, the only one [`InputLog::decode`] reads: the data files of an
/// older log are of a storage format this Anchor does not read, so it could not be replayed.
const FORMAT: u32 = 14;
/// The largest written log: [`write_crash_log`] writes no more ([`InputLog::encode_within`]) and
/// [`InputLog::decode`] reads no more. Checkpoints hold every loaded chunk and have no bound of
/// their own, so a log can be larger; it is then written from its newest committed segment, or
/// not at all.
pub(crate) const MAX_FILE_BYTES: usize = 1 << 30;
const KIND_JOIN: u8 = 0;
const KIND_PACKET: u8 = 1;
const KIND_LEAVE: u8 = 2;
const KIND_CONSOLE: u8 = 3;
const KIND_JOB: u8 = 4;
const KIND_MEASURE: u8 = 5;
const MEASURE_TICK_TIME: u8 = 0;
const MEASURE_PING: u8 = 1;
const MEASURE_CONNECTIONS: u8 = 2;
const MEASURE_PROCESS: u8 = 3;
/// The outcome byte of a job result that has its JSON; other values are
/// [`Error::code`](crate::task::Error::code)s.
const JOB_DONE: u8 = 0;
const ADDRESS_NONE: u8 = 0;
const ADDRESS_V4: u8 = 4;
const ADDRESS_V6: u8 = 6;
/// What the commit identity chains before each tick, input, forgotten session and answer.
const CHAIN_TICK: u8 = b'T';
const CHAIN_INPUT: u8 = b'I';
const CHAIN_DROPPED: u8 = b'D';
const CHAIN_ANSWER: u8 = b'A';

/// The input log.
#[derive(Debug)]
pub(crate) struct InputLog {
    /// Oldest first; never empty. The first committed one or one after it is the newest
    /// committed segment.
    segments: VecDeque<Segment>,
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
    /// Recording stopped at the bound; later ticks of this segment are missing.
    truncated: bool,
    /// The server file holds the state at `start`: the start of the process, or a commit the tick
    /// learnt of.
    committed: bool,
    /// The commit identity of the state at `start`; `None` if unknown (a server file that has
    /// none).
    identity: Option<u64>,
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

impl Segment {
    fn new(start: u64, committed: bool, identity: Option<u64>) -> Self {
        Self {
            start,
            checkpoint: None,
            ticks: Vec::new(),
            bytes: 0,
            truncated: false,
            committed,
            identity,
        }
    }

    /// The tick of the segment's state.
    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    /// The commit identity of the segment's state, if known.
    pub(crate) fn identity(&self) -> Option<u64> {
        self.identity
    }

    /// The written checkpoint of the segment's state, if any (logs read from a file).
    pub(crate) fn checkpoint(&self) -> Option<&[u8]> {
        match &self.checkpoint {
            Some(Point::Written(bytes)) => Some(bytes),
            Some(Point::Taken(_)) | None => None,
        }
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

    /// Whether recording stopped at the bound before the segment's last tick.
    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl Default for InputLog {
    /// A log that starts at tick 0 and takes no checkpoints.
    fn default() -> Self {
        Self::starting_at(0)
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
    LoadFailed,
    Checked(Checked),
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

    /// The bytes the log charges for it.
    fn size(&self) -> usize {
        match self {
            Self::Found { world, extra, .. } => world.len().saturating_add(extra.len()),
            Self::Absent | Self::Damaged => 0,
        }
    }
}

/// A logged player extra: one little-endian NBT root compound and nothing after it.
fn read_extra(bytes: &[u8]) -> Result<crate::nbt::Compound> {
    let mut r = protocol::Reader::new(bytes, bytes.len().saturating_mul(8), 0);
    let extra = r
        .nbt_within(Encoding::LittleEndian, MAX_PLAYER_EXTRA_BYTES)
        .map_err(Error::Packet)?;
    if r.remaining().is_empty() {
        Ok(extra)
    } else {
        Err(Error::Format("bytes after a player extra"))
    }
}

/// Where a loaded chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Disk,
    Generated,
    Damaged,
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
            Response::LoadFailed => (Self::LoadFailed, 0),
            Response::Checked(found) => {
                let size = match found {
                    Checked::Passed { .. } => 0,
                    Checked::Damaged { first, .. } => first.len(),
                    Checked::Failed(why) => why.len(),
                };
                (Self::Checked(found.clone()), size)
            }
        })
    }
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
    Leave,
    /// A console line ([`InputKind::Console`](super::queue::InputKind::Console)).
    Console(String),
    /// A blocking job's result ([`InputKind::Job`](super::queue::InputKind::Job)); the entry's
    /// sequence number is the job's id.
    Job(JobOutcome),
    /// A value measured off the tick ([`InputKind::Measure`](super::queue::InputKind::Measure)).
    Measure(Measure),
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
    /// A logged packet does not encode, or a written one does not decode.
    Packet(protocol::Error),
    /// The bytes are not a log of this format.
    Format(&'static str),
    /// The written log would be larger than a replay reads ([`MAX_FILE_BYTES`]), even from its
    /// newest committed segment on.
    TooLarge,
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packet(_) => f.write_str("a logged packet does not encode or decode"),
            Self::Format(what) => write!(f, "not an input log: {what}"),
            Self::TooLarge => f.write_str("crash log over the replay limit; replay unavailable"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Packet(e) => Some(e),
            Self::Format(_) | Self::TooLarge => None,
        }
    }
}

impl InputLog {
    /// A log whose state starts at tick `start` (which the server file holds), taking no
    /// checkpoints: tests replay it from a state they know.
    pub(crate) fn starting_at(start: u64) -> Self {
        Self::from_state(start, None)
    }

    /// A log whose state starts at tick `start` with commit identity `identity`, taking no
    /// checkpoints.
    fn from_state(start: u64, identity: Option<u64>) -> Self {
        Self {
            segments: VecDeque::from([Segment::new(start, true, identity)]),
            bytes: 0,
            checkpoint_bytes: 0,
            takes_checkpoints: false,
            fingerprints: Fingerprints::default(),
            planned: None,
            chain: super::Fnv(identity.unwrap_or(super::Fnv::OFFSET)),
        }
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

    /// The commit identity of the state the inputs chained so far lead to: what the commit of a
    /// snapshot taken now writes into the server file.
    pub(crate) fn identity(&self) -> u64 {
        self.chain.0
    }

    /// What the run that wrote the log was built with.
    pub(crate) fn fingerprints(&self) -> Fingerprints {
        self.fingerprints
    }

    /// The segments, oldest first.
    pub(crate) fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter()
    }

    /// The segment being recorded.
    pub(crate) fn last_segment(&self) -> Option<&Segment> {
        self.segments.back()
    }

    /// Starts a segment at tick `start`, the tick a save snapshot was just taken before.
    pub(crate) fn snapshot(&mut self, start: u64) {
        if let Some(last) = self.segments.back_mut()
            && last.start == start
        {
            return;
        }
        let identity = Some(self.identity());
        self.segments
            .push_back(Segment::new(start, false, identity));
    }

    /// Whether the newest segment waits for the checkpoint of its state: the tick takes it at
    /// the tick boundary that started the segment, right after the snapshot, before anything
    /// changes the state.
    pub(crate) fn needs_checkpoint(&self) -> bool {
        self.takes_checkpoints && self.segments.back().is_some_and(|s| s.checkpoint.is_none())
    }

    /// Gives the newest segment the checkpoint of its state, if it starts at the checkpoint's
    /// tick and has none.
    pub(crate) fn set_checkpoint(&mut self, checkpoint: Checkpoint) {
        if let Some(last) = self.segments.back_mut()
            && last.start == checkpoint.tick()
            && last.checkpoint.is_none()
        {
            let point = Point::Taken(Box::new(checkpoint));
            self.checkpoint_bytes = self.checkpoint_bytes.saturating_add(point.size());
            last.checkpoint = Some(point);
            self.make_room(0);
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

    /// The place of the newest committed segment.
    fn newest_committed(&self) -> usize {
        self.segments.iter().rposition(|s| s.committed).unwrap_or(0)
    }

    /// Whether the inputs recorded since the state of the last commit reach a quarter of the
    /// bound: the tick then takes a snapshot even with periodic saves off (`/save-off`), so
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

    /// Drops the oldest segments before the newest committed one while the inputs, the
    /// checkpoints and `cost` more bytes exceed the bound.
    fn make_room(&mut self, cost: usize) {
        while self
            .bytes
            .saturating_add(self.checkpoint_bytes)
            .saturating_add(cost)
            > MAX_BYTES
            && self.newest_committed() > 0
        {
            if let Some(first) = self.segments.pop_front() {
                self.bytes = self.bytes.saturating_sub(first.bytes);
                let size = first.checkpoint.as_ref().map_or(0, Point::size);
                self.checkpoint_bytes = self.checkpoint_bytes.saturating_sub(size);
            }
        }
    }

    /// Starts the record of tick `number`; every tick counts against the bound, even an empty
    /// one. Nothing is recorded in a truncated segment.
    pub(crate) fn begin(&mut self, number: u64) {
        self.chain.raw(&[CHAIN_TICK]);
        self.chain.u64(number);
        if self.charge(0) == Charge::Fits
            && let Some(segment) = self.segments.back_mut()
        {
            segment.ticks.push(Tick {
                number,
                inputs: Vec::new(),
                dropped: Vec::new(),
                responses: Vec::new(),
            });
        }
    }

    /// Records an input of `size` bytes on the wire (0 for joins and leaves) before the tick
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
        if !self.charge_entry(size) {
            return Cow::Owned(kind);
        }
        match self.last_tick() {
            Some(tick) => Cow::Borrowed(&tick.inputs.push_mut(Entry { session, seq, kind }).kind),
            None => Cow::Owned(kind),
        }
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
            EntryKind::Leave => chain.raw(&[KIND_LEAVE]),
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

    /// Records that the tick forgot `session` after its output.
    pub(crate) fn dropped(&mut self, session: SessionId) {
        self.chain.raw(&[CHAIN_DROPPED]);
        self.chain.u64(session.get());
        if self.charge_entry(0)
            && let Some(tick) = self.last_tick()
        {
            tick.dropped.push(session);
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
                Response::LoadFailed => ANSWER_LOAD_FAILED,
                Response::Checked(_) => ANSWER_CHECKED,
            },
        ]);
        if self.segments.back().is_none_or(|s| s.truncated) {
            return;
        }
        let (answer, size) = match Answer::of(response) {
            Ok(logged) => logged,
            Err(e) => {
                warn!(error = %e, "an answer cannot be logged; ticks until the next save are not recorded");
                if let Some(segment) = self.segments.back_mut() {
                    segment.truncated = true;
                    segment.ticks.pop();
                }
                return;
            }
        };
        if self.charge_entry(size)
            && let Some(tick) = self.last_tick()
        {
            tick.responses.push(answer);
        }
    }

    /// The tick being recorded.
    fn last_tick(&mut self) -> Option<&mut Tick> {
        self.segments.back_mut()?.ticks.last_mut()
    }

    /// Whether a replay of this log stops before its last tick: a kept segment stopped at the
    /// bound.
    pub(crate) fn is_truncated(&self) -> bool {
        self.segments.iter().any(|s| s.truncated)
    }

    /// The tick of the oldest kept state.
    pub(crate) fn start(&self) -> u64 {
        self.segments.front().map_or(0, |s| s.start)
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

    /// The ticks a replay from the oldest segment applies, oldest first.
    #[cfg(test)]
    pub(crate) fn ticks(&self) -> Vec<Tick> {
        self.replayable().cloned().collect()
    }

    /// Counts an entry of the tick being recorded; `false` if it is not recorded. When the bound
    /// is reached in the middle of a tick, that tick is incomplete, so it goes too: a replay
    /// must not apply part of a tick.
    fn charge_entry(&mut self, size: usize) -> bool {
        match self.charge(size) {
            Charge::Fits => true,
            Charge::Full => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.ticks.pop();
                }
                false
            }
            Charge::Truncated => false,
        }
    }

    /// Counts `size` bytes and an entry's cost against the bound, for the segment being recorded.
    /// Old committed segments go first if the bound needs room.
    fn charge(&mut self, size: usize) -> Charge {
        if self.segments.back().is_none_or(|s| s.truncated) {
            return Charge::Truncated;
        }
        let cost = size.saturating_add(ENTRY_COST);
        self.make_room(cost);
        let bytes = self.bytes.saturating_add(cost);
        let Some(segment) = self.segments.back_mut() else {
            return Charge::Truncated;
        };
        if bytes > MAX_BYTES {
            warn!(
                bytes = self.bytes,
                "input log reached its bound; ticks until the next save are not recorded"
            );
            segment.truncated = true;
            return Charge::Full;
        }
        self.bytes = bytes;
        segment.bytes = segment.bytes.saturating_add(cost);
        Charge::Fits
    }

    /// The written form: the header with the fingerprints, then every segment (its start, marks,
    /// commit identity, checkpoint and ticks), then the CRC-32C of everything before it. Integers
    /// are little-endian; checkpoints are encoded here, on the main thread.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.encode_from(0)
    }

    /// The written form if it takes at most `limit` bytes; otherwise the written form from the
    /// newest committed segment on, which a replay starts from with the server file of the last
    /// commit, if that fits; otherwise [`Error::TooLarge`].
    fn encode_within(&self, limit: usize) -> Result<Vec<u8>> {
        let all = self.encode()?;
        if all.len() <= limit {
            return Ok(all);
        }
        drop(all);
        let from = self.newest_committed();
        if from > 0 {
            let newest = self.encode_from(from)?;
            if newest.len() <= limit {
                return Ok(newest);
            }
        }
        Err(Error::TooLarge)
    }

    /// The written form of the segments from the one at place `from` on.
    fn encode_from(&self, from: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.bytes.saturating_add(128));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&protocol::packet::PROTOCOL_VERSION.to_le_bytes());
        out.extend_from_slice(&self.fingerprints.settings);
        out.extend_from_slice(&self.fingerprints.registrations);
        put_len(&mut out, self.segments.len().saturating_sub(from))?;
        let mut body = Vec::new();
        for segment in self.segments.iter().skip(from) {
            out.extend_from_slice(&segment.start.to_le_bytes());
            out.push(u8::from(segment.committed));
            out.push(u8::from(segment.truncated));
            match segment.identity {
                None => out.push(0),
                Some(identity) => {
                    out.push(1);
                    out.extend_from_slice(&identity.to_le_bytes());
                }
            }
            match &segment.checkpoint {
                None => out.push(0),
                Some(Point::Taken(c)) => {
                    out.push(1);
                    put_bytes(&mut out, &c.to_bytes())?;
                }
                Some(Point::Written(bytes)) => {
                    out.push(1);
                    put_bytes(&mut out, bytes)?;
                }
            }
            put_len(&mut out, segment.ticks.len())?;
            for tick in &segment.ticks {
                put_tick(&mut out, tick, &mut body)?;
            }
        }
        let mut crc = Crc32c::new();
        crc.update(&out);
        out.extend_from_slice(&crc.finish().to_le_bytes());
        Ok(out)
    }

    /// Reads a written log of at most [`MAX_FILE_BYTES`], of [`FORMAT`]. It is checked against
    /// its checksum first, and every count against the bytes left before anything is allocated
    /// for it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Format("larger than a log can be"));
        }
        let mut r = Read(bytes);
        if r.take(MAGIC.len())? != MAGIC {
            return Err(Error::Format("magic"));
        }
        if r.u32()? != FORMAT {
            return Err(Error::Format("format version"));
        }
        if r.u32()?.cast_signed() != protocol::packet::PROTOCOL_VERSION {
            return Err(Error::Format("protocol version"));
        }
        let mut budget = bytes.len().saturating_mul(2);
        let Some((body, crc)) = r.0.split_last_chunk::<4>() else {
            return Err(Error::Format("ends early"));
        };
        let mut check = Crc32c::new();
        check.update(bytes.get(..bytes.len() - 4).unwrap_or_default());
        if check.finish() != u32::from_le_bytes(*crc) {
            return Err(Error::Format("checksum"));
        }
        let mut r = Read(body);
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
        for _ in 0..count {
            let start = r.u64()?;
            let committed = r.flag("committed flag")?;
            let truncated = r.flag("truncated flag")?;
            let identity = if r.flag("identity flag")? {
                Some(r.u64()?)
            } else {
                None
            };
            let mut segment = Segment::new(start, committed, identity);
            segment.truncated = truncated;
            segment.checkpoint = match r.u8()? {
                0 => None,
                1 => Some(Point::Written(r.bytes()?.to_vec())),
                _ => return Err(Error::Format("checkpoint flag")),
            };
            let ticks = r.count(16)?;
            segment.ticks = Vec::with_capacity(capacity::<Tick>(ticks, r.0.len()));
            for _ in 0..ticks {
                segment.ticks.push(r.tick(&mut budget)?);
            }
            segments.push_back(segment);
        }
        if !r.0.is_empty() {
            return Err(Error::Format("bytes after the last segment"));
        }
        Ok(Self {
            segments,
            bytes: 0,
            checkpoint_bytes: 0,
            takes_checkpoints: false,
            fingerprints,
            planned: None,
            chain: super::Fnv(super::Fnv::OFFSET),
        })
    }
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
                let mut w = Writer::new(Vec::new(), 0);
                protocol::Codec::encode(identity.skin.get(), &mut w).map_err(Error::Packet)?;
                put_bytes(out, w.as_bytes())?;
                match identity.address {
                    None => out.push(ADDRESS_NONE),
                    Some(IpAddr::V4(a)) => {
                        out.push(ADDRESS_V4);
                        out.extend_from_slice(&a.octets());
                    }
                    Some(IpAddr::V6(a)) => {
                        out.push(ADDRESS_V6);
                        out.extend_from_slice(&a.octets());
                    }
                }
                put_client(out, &identity.client)?;
            }
            EntryKind::Packet(packet) => {
                out.push(KIND_PACKET);
                out.extend_from_slice(&packet.id().to_le_bytes());
                body.clear();
                packet
                    .encode(body, SHIELD_RUNTIME_ID)
                    .map_err(Error::Packet)?;
                put_bytes(out, body)?;
            }
            EntryKind::Leave => out.push(KIND_LEAVE),
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

/// The folder of crash reports in the data folder.
const CRASH_DIR: &str = "crash";

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

const ANSWER_CHUNK: u8 = 0;
const ANSWER_PLAYER: u8 = 1;
const ANSWER_COMMITTED: u8 = 2;
const ANSWER_COMMIT_FAILED: u8 = 3;
const ANSWER_LOAD_FAILED: u8 = 4;
const ANSWER_CHECKED: u8 = 5;
const CHECK_PASSED: u8 = 0;
const CHECK_DAMAGED: u8 = 1;
const CHECK_FAILED: u8 = 2;
const PLAYER_FOUND: u8 = 0;
const PLAYER_ABSENT: u8 = 1;
const PLAYER_DAMAGED: u8 = 2;

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
            out.extend_from_slice(
                &u32::try_from(*world)
                    .map_err(|_| Error::Format("world index over u32"))?
                    .to_le_bytes(),
            );
            out.extend_from_slice(&pos.x.to_le_bytes());
            out.extend_from_slice(&pos.z.to_le_bytes());
            out.push(match origin {
                Origin::Disk => 0,
                Origin::Generated => 1,
                Origin::Damaged => 2,
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
        Answer::LoadFailed => out.push(ANSWER_LOAD_FAILED),
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
    }
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

/// A reader over a written log.
struct Read<'a>(&'a [u8]);

impl<'a> Read<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(Error::Format("ends early"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Format("fixed width"))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
    }

    /// A byte that is 0 or 1.
    fn flag(&mut self, what: &'static str) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Format(what)),
        }
    }

    /// A tick of a written log; `budget` bounds what its packets may allocate.
    fn tick(&mut self, budget: &mut usize) -> Result<Tick> {
        let number = self.u64()?;
        // An entry takes at least 17 bytes, a dropped session 8.
        let entries = self.count(17)?;
        let mut inputs = Vec::with_capacity(capacity::<Entry>(entries, self.0.len()));
        for _ in 0..entries {
            let session = SessionId::new(self.u64()?);
            let seq = self.u64()?;
            let kind = match self.u8()? {
                KIND_JOIN => {
                    let name = String::from_utf8(self.bytes()?.to_vec())
                        .map_err(|_| Error::Format("name"))?;
                    let has_xuid = self.u8()?;
                    let xuid = self.u64()?;
                    let uuid: [u8; 16] = self.array()?;
                    let device_os = self.i32()?;
                    let skin = self.bytes()?;
                    let mut skin_reader =
                        protocol::Reader::new(skin, skin.len().saturating_mul(2), 0);
                    let skin = <protocol::Skin as protocol::Codec>::decode(&mut skin_reader)
                        .map_err(Error::Packet)?;
                    let address = self.address()?;
                    let client = self.client()?;
                    EntryKind::Join(Identity {
                        skin: super::queue::Skin::new(skin),
                        device_os,
                        address,
                        client,
                        name,
                        xuid: match has_xuid {
                            0 => None,
                            1 => Some(xuid),
                            _ => return Err(Error::Format("xuid flag")),
                        },
                        uuid: protocol::Uuid::from_bytes(uuid),
                    })
                }
                KIND_PACKET => {
                    let id = self.u32()?;
                    let payload = self.bytes()?;
                    let packet = Packet::decode(id, payload, budget, SHIELD_RUNTIME_ID)
                        .map_err(Error::Packet)?;
                    EntryKind::Packet(packet)
                }
                KIND_LEAVE => EntryKind::Leave,
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
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(
            b.try_into().map_err(|_| Error::Format("u32"))?,
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(
            b.try_into().map_err(|_| Error::Format("u64"))?,
        ))
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
            code => crate::task::Error::from_code(code)
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

    fn measure(&mut self) -> Result<Measure> {
        Ok(match self.u8()? {
            MEASURE_TICK_TIME => Measure::TickTime(self.u32()?),
            MEASURE_PING => Measure::Ping(self.u32()?),
            MEASURE_CONNECTIONS => Measure::Connections {
                all: self.u32()?,
                logged_in: self.u32()?,
            },
            MEASURE_PROCESS => Measure::Process {
                memory: u64::from_le_bytes(self.array()?),
                uptime_ms: u64::from_le_bytes(self.array()?),
            },
            _ => return Err(Error::Format("measure kind")),
        })
    }

    fn address(&mut self) -> Result<Option<IpAddr>> {
        Ok(match self.u8()? {
            ADDRESS_NONE => None,
            ADDRESS_V4 => {
                let b: [u8; 4] = self
                    .take(4)?
                    .try_into()
                    .map_err(|_| Error::Format("ipv4"))?;
                Some(IpAddr::from(b))
            }
            ADDRESS_V6 => {
                let b: [u8; 16] = self
                    .take(16)?
                    .try_into()
                    .map_err(|_| Error::Format("ipv6"))?;
                Some(IpAddr::from(b))
            }
            _ => return Err(Error::Format("address kind")),
        })
    }

    /// An answer.
    fn answer(&mut self) -> Result<Answer> {
        Ok(match self.u8()? {
            ANSWER_CHUNK => {
                let world =
                    usize::try_from(self.u32()?).map_err(|_| Error::Format("world index"))?;
                let pos = ChunkPos::new(self.i32()?, self.i32()?);
                let origin = match self.u8()? {
                    0 => Origin::Disk,
                    1 => Origin::Generated,
                    2 => Origin::Damaged,
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
            ANSWER_LOAD_FAILED => Answer::LoadFailed,
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
            _ => return Err(Error::Format("answer kind")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_tick_counts_against_the_bound() {
        let mut log = InputLog::default();
        log.begin(0);
        assert_eq!(log.bytes, ENTRY_COST);
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
        // An input that needs room drops the segments before the newest committed one.
        log.record(
            SessionId::new(1),
            0,
            EntryKind::Leave,
            MAX_BYTES - 4 * ENTRY_COST,
        );
        assert_eq!(starts(&log), [2, 3]);
        assert_eq!(numbers(&log), [2, 3]);
        assert!(!log.is_truncated());
        assert_eq!(
            log.bytes,
            MAX_BYTES - ENTRY_COST,
            "dropped ticks free their bytes"
        );
        // With no committed segment to drop, the input that does not fit truncates.
        log.record(SessionId::new(1), 1, EntryKind::Leave, 8 * ENTRY_COST);
        assert!(log.is_truncated());
        assert_eq!(starts(&log), [2, 3]);
        // After the next commit, the segment before it goes once the bound needs room.
        log.committed(3);
        assert_eq!(starts(&log), [2, 3]);
        log.snapshot(4);
        log.begin(4);
        log.record(SessionId::new(1), 2, EntryKind::Leave, 0);
        assert_eq!(starts(&log), [3, 4]);
        assert!(!log.last_segment().is_some_and(Segment::is_truncated));
        // The same snapshot twice starts one segment.
        log.snapshot(4);
        assert_eq!(starts(&log), [3, 4]);
    }

    #[test]
    fn recording_resumes_after_a_truncated_segment_and_room_drops_it() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Leave, MAX_BYTES);
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
            EntryKind::Leave,
            MAX_BYTES - 2 * ENTRY_COST,
        );
        assert!(!log.is_truncated(), "room dropped the truncated segment");
        assert_eq!((log.start(), numbers(&log)), (1, vec![1]));
    }

    #[test]
    fn a_quarter_of_the_bound_since_the_last_commit_asks_for_a_commit() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Leave, COMMIT_BYTES / 2);
        log.snapshot(1);
        log.begin(1);
        assert!(!log.wants_commit(1));
        // The bytes since the newest committed state count, across segments.
        log.record(SessionId::new(1), 1, EntryKind::Leave, COMMIT_BYTES / 2);
        assert!(log.wants_commit(2));
        log.committed(1);
        assert!(!log.wants_commit(2), "only segment 1 counts now");
        // A replay's log follows the replayed log's snapshots instead.
        let replay = replay_log([9].into());
        assert!(replay.wants_commit(9) && !replay.wants_commit(8));
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
    fn a_replay_log_starts_with_its_checkpoint_and_takes_none_before_a_snapshot() {
        let mut log = replay_log(std::collections::BTreeSet::new());
        assert!(!log.needs_checkpoint());
        let first = log.last_segment().unwrap();
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
    fn logs_before_segments_are_refused() {
        // Format 7: not truncated, start 0, no ticks.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&7_u32.to_le_bytes());
        bytes.extend_from_slice(&protocol::packet::PROTOCOL_VERSION.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            InputLog::decode(&bytes),
            Err(Error::Format("format version"))
        ));
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
            log.record(SessionId::new(1), 0, EntryKind::Leave, size);
            log.record(
                SessionId::ENGINE,
                0,
                EntryKind::Measure(Measure::TickTime(tick_time)),
                0,
            );
            log.response(&Response::LoadFailed);
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
        assert_eq!(a.last_segment().unwrap().identity(), Some(a.identity()));
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
    fn a_log_of_another_format_is_refused() {
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
            assert!(matches!(
                InputLog::decode(&other),
                Err(Error::Format("format version"))
            ));
        }
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
    fn console_lines_and_addresses_survive_the_written_log() {
        let identity = |address| Identity {
            name: "Steve".to_owned(),
            xuid: None,
            uuid: protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: 7,
            address,
            client: crate::tick::queue::Client::default(),
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

    #[test]
    fn job_results_survive_the_written_log() {
        let mut log = InputLog::default();
        log.begin(0);
        let results = [
            Ok(br#"{"a":[1,2]}"#.to_vec()),
            Ok(Vec::new()),
            Err(crate::task::Error::Panicked),
            Err(crate::task::Error::TooLarge),
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
            EntryKind::Job(Err(crate::task::Error::Encode)),
            0,
        );
        let mut bytes = bad.encode().unwrap();
        // The tick ends with the outcome byte, then no dropped sessions and no answers; the
        // checksum follows.
        let at = bytes.len() - 4 - 8 - 1;
        assert_eq!(bytes[at], crate::task::Error::Encode.code());
        bytes[at] = 99;
        reseal(&mut bytes);
        assert!(matches!(
            InputLog::decode(&bytes),
            Err(Error::Format("job outcome"))
        ));
    }

    #[test]
    fn load_failures_survive_the_written_log() {
        let mut log = InputLog::default();
        log.begin(0);
        log.response(&Response::LoadFailed);
        let bytes = log.encode().unwrap();
        let read = InputLog::decode(&bytes).unwrap();
        assert_eq!(read.ticks(), log.ticks());
        assert_eq!(read.ticks()[0].responses, [Answer::LoadFailed]);
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
        assert!(
            charged > 5 * ENTRY_COST + 100,
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
        assert!(matches!(
            InputLog::decode(&flipped),
            Err(Error::Format("checksum"))
        ));
        crate::protocol::fuzz(&bytes, 5_000, |b| {
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
        log.record(SessionId::new(1), 0, EntryKind::Leave, MAX_BYTES);
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
