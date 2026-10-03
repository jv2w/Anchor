//! The input log (PLAN.md 5, 10.5, 10.6): every input each tick applied, in order, so that a crash
//! can be replayed from the last saved state.
//!
//! The tick moves the inputs it has applied into the log, so recording copies nothing large.
//! Packets are encoded only when the log is written, on the main thread after the tick thread
//! ended. The log is a list of segments, one per save snapshot: a segment starts at the tick its
//! snapshot was taken, and once that snapshot is committed the segments before it are dropped, so
//! the log always starts at the last committed state. It holds at most [`MAX_BYTES`] of inputs
//! (counted by their size on the wire); past that the segment being recorded stops and is marked
//! truncated, and a replay ends there. The next snapshot starts a new segment.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

use tracing::warn;

#[cfg(test)]
use super::queue::Skin;
use super::queue::{Identity, SessionId};
use crate::clock;
use crate::item::SHIELD_RUNTIME_ID;
use crate::nbt::Encoding;
use crate::protocol::{self, Writer, packet::Packet};
#[cfg(test)]
use crate::save::PlayerData;
use crate::save::{Loaded, PlayerLoad, Response, WorldIndex};
use crate::world::ChunkPos;

/// The most input bytes the log holds.
pub(crate) const MAX_BYTES: usize = 64 << 20;
/// Bytes counted per entry besides its packet, so that many empty inputs are bounded too.
const ENTRY_COST: usize = 32;
/// The first bytes of a written log.
const MAGIC: &[u8; 16] = b"ANCHOR-INPUT-LOG";
/// The written format's version: 2 adds the start tick; 3 adds chunk checksums and player
/// extras (f64 positions); 4 adds the joining player's device and skin.
const FORMAT: u32 = 4;
const KIND_JOIN: u8 = 0;
const KIND_PACKET: u8 = 1;
const KIND_LEAVE: u8 = 2;

/// The input log.
#[derive(Debug)]
pub(crate) struct InputLog {
    /// Oldest first; never empty. The first one starts at the last committed state.
    segments: VecDeque<Segment>,
    /// The bytes charged for every segment.
    bytes: usize,
}

/// The ticks recorded after one snapshot.
#[derive(Debug)]
struct Segment {
    /// The tick the snapshot was taken before: the first tick this segment can hold.
    start: u64,
    ticks: Vec<Tick>,
    /// The bytes charged for this segment.
    bytes: usize,
    /// Recording stopped at the bound; later ticks of this segment are missing.
    truncated: bool,
}

impl Segment {
    fn new(start: u64) -> Self {
        Self {
            start,
            ticks: Vec::new(),
            bytes: 0,
            truncated: false,
        }
    }
}

impl Default for InputLog {
    /// A log that starts at tick 0.
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

/// A save thread answer as the log keeps it (PLAN.md 10.6): what arrived and where from, not
/// the chunk itself, which a replay loads again from the same world file or generator, checking
/// it against the checksum.
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
                w.nbt(&data.extra, Encoding::LittleEndian)?;
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
    #[cfg(test)]
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
#[cfg(test)]
fn read_extra(bytes: &[u8]) -> Result<crate::nbt::Compound> {
    let mut r = protocol::Reader::new(bytes, bytes.len().saturating_mul(8), 0);
    let extra = r.nbt(Encoding::LittleEndian).map_err(Error::Packet)?;
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
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packet(_) => f.write_str("a logged packet does not encode or decode"),
            Self::Format(what) => write!(f, "not an input log: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Packet(e) => Some(e),
            Self::Format(_) => None,
        }
    }
}

impl InputLog {
    /// A log whose state starts at tick `start`: the engine tick of the state it replays from.
    pub(crate) fn starting_at(start: u64) -> Self {
        Self {
            segments: VecDeque::from([Segment::new(start)]),
            bytes: 0,
        }
    }

    /// Starts a segment at tick `start`, the tick a save snapshot was just taken before. The
    /// segments before it are dropped once [`committed`](Self::committed) names `start`.
    pub(crate) fn snapshot(&mut self, start: u64) {
        if let Some(last) = self.segments.back_mut()
            && last.start == start
        {
            return;
        }
        self.segments.push_back(Segment::new(start));
    }

    /// The snapshot taken before tick `start` is committed: the log now starts there. Does
    /// nothing if no segment starts at `start`.
    pub(crate) fn committed(&mut self, start: u64) {
        if !self.segments.iter().any(|s| s.start == start) {
            return;
        }
        while let Some(first) = self.segments.front()
            && first.start != start
        {
            self.bytes = self.bytes.saturating_sub(first.bytes);
            self.segments.pop_front();
        }
    }

    /// Starts the record of tick `number`; every tick counts against the bound, even an empty
    /// one. Nothing is recorded in a truncated segment.
    pub(crate) fn begin(&mut self, number: u64) {
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

    /// Records an applied input of `size` bytes on the wire (0 for joins and leaves).
    pub(crate) fn record(&mut self, session: SessionId, seq: u64, kind: EntryKind, size: usize) {
        if self.charge_entry(size)
            && let Some(tick) = self.last_tick()
        {
            tick.inputs.push(Entry { session, seq, kind });
        }
    }

    /// Records that the tick forgot `session` after its output.
    pub(crate) fn dropped(&mut self, session: SessionId) {
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

    /// The tick the log replays from.
    pub(crate) fn start(&self) -> u64 {
        self.segments.front().map_or(0, |s| s.start)
    }

    /// The ticks a replay applies, oldest first: every kept segment up to and including the first
    /// truncated one.
    fn replayable(&self) -> impl Iterator<Item = &Tick> {
        let end = self
            .segments
            .iter()
            .position(|s| s.truncated)
            .map_or(self.segments.len(), |i| i.saturating_add(1));
        self.segments.iter().take(end).flat_map(|s| &s.ticks)
    }

    /// The ticks a replay applies, oldest first.
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
    fn charge(&mut self, size: usize) -> Charge {
        let Some(segment) = self.segments.back_mut() else {
            return Charge::Truncated;
        };
        if segment.truncated {
            return Charge::Truncated;
        }
        let cost = size.saturating_add(ENTRY_COST);
        let bytes = self.bytes.saturating_add(cost);
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

    /// The written form: the header, then every tick. Integers are little-endian.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.bytes.saturating_add(64));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&protocol::packet::PROTOCOL_VERSION.to_le_bytes());
        out.push(u8::from(self.is_truncated()));
        out.extend_from_slice(&self.start().to_le_bytes());
        put_len(&mut out, self.replayable().count())?;
        let mut body = Vec::new();
        for tick in self.replayable() {
            out.extend_from_slice(&tick.number.to_le_bytes());
            put_len(&mut out, tick.inputs.len())?;
            for entry in &tick.inputs {
                out.extend_from_slice(&entry.session.get().to_le_bytes());
                out.extend_from_slice(&entry.seq.to_le_bytes());
                match &entry.kind {
                    EntryKind::Join(identity) => {
                        out.push(KIND_JOIN);
                        put_bytes(&mut out, identity.name.as_bytes())?;
                        out.push(u8::from(identity.xuid.is_some()));
                        out.extend_from_slice(&identity.xuid.unwrap_or(0).to_le_bytes());
                        out.extend_from_slice(identity.uuid.as_bytes());
                        out.extend_from_slice(&identity.device_os.to_le_bytes());
                        let mut w = Writer::new(Vec::new(), 0);
                        protocol::Codec::encode(identity.skin.get(), &mut w)
                            .map_err(Error::Packet)?;
                        put_bytes(&mut out, w.as_bytes())?;
                    }
                    EntryKind::Packet(packet) => {
                        out.push(KIND_PACKET);
                        out.extend_from_slice(&packet.id().to_le_bytes());
                        body.clear();
                        packet
                            .encode(&mut body, SHIELD_RUNTIME_ID)
                            .map_err(Error::Packet)?;
                        put_bytes(&mut out, &body)?;
                    }
                    EntryKind::Leave => out.push(KIND_LEAVE),
                }
            }
            put_len(&mut out, tick.dropped.len())?;
            for session in &tick.dropped {
                out.extend_from_slice(&session.get().to_le_bytes());
            }
            put_len(&mut out, tick.responses.len())?;
            for answer in &tick.responses {
                put_answer(&mut out, answer)?;
            }
        }
        Ok(out)
    }

    /// Reads a written log. Every count is checked against the bytes left before anything is
    /// allocated for it.
    #[cfg(test)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
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
        let truncated = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Error::Format("truncated flag")),
        };
        let start = r.u64()?;
        // A tick takes at least 16 bytes, an entry 17, a dropped session 8.
        let count = r.count(16)?;
        let mut ticks = Vec::with_capacity(count);
        let mut budget = bytes.len().saturating_mul(2);
        for _ in 0..count {
            let number = r.u64()?;
            let entries = r.count(17)?;
            let mut inputs = Vec::with_capacity(entries);
            for _ in 0..entries {
                let session = SessionId::new(r.u64()?);
                let seq = r.u64()?;
                let kind = match r.u8()? {
                    KIND_JOIN => {
                        let name = String::from_utf8(r.bytes()?.to_vec())
                            .map_err(|_| Error::Format("name"))?;
                        let has_xuid = r.u8()?;
                        let xuid = r.u64()?;
                        let uuid: [u8; 16] =
                            r.take(16)?.try_into().map_err(|_| Error::Format("uuid"))?;
                        let device_os = r.i32()?;
                        let skin = r.bytes()?;
                        let mut skin_reader =
                            protocol::Reader::new(skin, skin.len().saturating_mul(2), 0);
                        let skin = <protocol::Skin as protocol::Codec>::decode(&mut skin_reader)
                            .map_err(Error::Packet)?;
                        EntryKind::Join(Identity {
                            skin: Skin::new(skin),
                            device_os,
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
                        let id = r.u32()?;
                        let payload = r.bytes()?;
                        let packet = Packet::decode(id, payload, &mut budget, SHIELD_RUNTIME_ID)
                            .map_err(Error::Packet)?;
                        EntryKind::Packet(packet)
                    }
                    KIND_LEAVE => EntryKind::Leave,
                    _ => return Err(Error::Format("input kind")),
                };
                inputs.push(Entry { session, seq, kind });
            }
            let dropped_count = r.count(8)?;
            let mut dropped = Vec::with_capacity(dropped_count);
            for _ in 0..dropped_count {
                dropped.push(SessionId::new(r.u64()?));
            }
            // An answer takes at least 9 bytes.
            let answer_count = r.count(9)?;
            let mut responses = Vec::with_capacity(answer_count);
            for _ in 0..answer_count {
                responses.push(r.answer()?);
            }
            ticks.push(Tick {
                number,
                inputs,
                dropped,
                responses,
            });
        }
        if !r.0.is_empty() {
            return Err(Error::Format("bytes after the last tick"));
        }
        Ok(Self {
            segments: VecDeque::from([Segment {
                start,
                ticks,
                bytes: 0,
                truncated,
            }]),
            bytes: 0,
        })
    }
}

/// The folder of crash reports in the data folder (PLAN.md 1).
const CRASH_DIR: &str = "crash";

/// Writes `log` to `<data_dir>/crash/input-<unix seconds>-<last tick>.log` and returns the path.
/// It never replaces a file. The main thread calls it after the tick thread has ended.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the main thread writes the crash log after the tick thread ended"
)]
pub(crate) fn write_crash_log(data_dir: &Path, log: &InputLog) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let bytes = log.encode().map_err(std::io::Error::other)?;
    let dir = data_dir.join(CRASH_DIR);
    std::fs::create_dir_all(&dir)?;
    let tick = log.replayable().last().map_or(log.start(), |t| t.number);
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
#[cfg(test)]
struct Read<'a>(&'a [u8]);

#[cfg(test)]
impl<'a> Read<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(Error::Format("ends early"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
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

    #[test]
    fn a_commit_drops_the_segments_before_its_snapshot() {
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
        assert_eq!((log.start(), numbers(&log)), (0, vec![0, 1, 2, 3]));
        log.committed(2);
        assert_eq!((log.start(), numbers(&log)), (2, vec![2, 3]));
        assert_eq!(log.bytes, 2 * ENTRY_COST, "dropped ticks free their bytes");
        log.committed(3);
        assert_eq!((log.start(), numbers(&log)), (3, vec![3]));
        // The same snapshot twice starts one segment.
        log.snapshot(4);
        log.snapshot(4);
        assert_eq!(log.segments.len(), 2);
    }

    #[test]
    fn recording_resumes_after_a_truncated_segment_is_committed_away() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 0, EntryKind::Leave, MAX_BYTES);
        assert!(log.is_truncated());
        log.snapshot(1);
        log.begin(1);
        assert!(
            numbers(&log).is_empty(),
            "a replay stops in the truncated segment"
        );
        log.committed(1);
        assert!(!log.is_truncated());
        assert_eq!(numbers(&log), [1]);
        log.snapshot(2);
        log.committed(2);
        log.begin(2);
        assert_eq!((log.start(), numbers(&log)), (2, vec![2]));
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
        // Damaged written logs are errors, never panics.
        crate::protocol::fuzz(&bytes, 5_000, |b| {
            InputLog::decode(b)
                .map(drop)
                .map_err(|_| crate::protocol::Error::Size)
        });
    }
}
