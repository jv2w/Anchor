//! The input log (PLAN.md 5, 10.5): every input each tick applied, in order, so that a crash can
//! be replayed from the last saved state.
//!
//! The tick moves the inputs it has applied into the log, so recording copies nothing large.
//! Packets are encoded only when the log is written, on the main thread after the tick thread
//! ended. The log holds at most [`MAX_BYTES`] of inputs (counted by their size on the wire); past
//! that it stops and marks itself truncated, and a replay ends there.

use std::fmt;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::queue::{Identity, SessionId};
use crate::clock;
use crate::item::SHIELD_RUNTIME_ID;
use crate::protocol::{self, packet::Packet};

/// The most input bytes the log holds.
pub(crate) const MAX_BYTES: usize = 64 << 20;
/// Bytes counted per entry besides its packet, so that many empty inputs are bounded too.
const ENTRY_COST: usize = 32;
/// The first bytes of a written log.
const MAGIC: &[u8; 16] = b"ANCHOR-INPUT-LOG";
/// The written format's version.
const FORMAT: u32 = 1;
const KIND_JOIN: u8 = 0;
const KIND_PACKET: u8 = 1;
const KIND_LEAVE: u8 = 2;

/// The input log.
#[derive(Debug, Default)]
pub(crate) struct InputLog {
    ticks: Vec<Tick>,
    bytes: usize,
    truncated: bool,
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
    /// Starts the record of tick `number`; every tick counts against the bound, even an empty
    /// one. Nothing is recorded once the log is truncated.
    pub(crate) fn begin(&mut self, number: u64) {
        if self.charge(0) == Charge::Fits {
            self.ticks.push(Tick {
                number,
                inputs: Vec::new(),
                dropped: Vec::new(),
            });
        }
    }

    /// Records an applied input of `size` bytes on the wire (0 for joins and leaves).
    pub(crate) fn record(&mut self, session: SessionId, seq: u64, kind: EntryKind, size: usize) {
        if self.charge_entry(size)
            && let Some(tick) = self.ticks.last_mut()
        {
            tick.inputs.push(Entry { session, seq, kind });
        }
    }

    /// Records that the tick forgot `session` after its output.
    pub(crate) fn dropped(&mut self, session: SessionId) {
        if self.charge_entry(0)
            && let Some(tick) = self.ticks.last_mut()
        {
            tick.dropped.push(session);
        }
    }

    /// Whether the log stopped at its bound.
    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The recorded ticks, oldest first.
    #[cfg(test)]
    pub(crate) fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    /// Counts an entry of the tick being recorded; `false` if it is not recorded. When the bound
    /// is reached in the middle of a tick, that tick is incomplete, so it goes too: a replay
    /// must not apply part of a tick.
    fn charge_entry(&mut self, size: usize) -> bool {
        match self.charge(size) {
            Charge::Fits => true,
            Charge::Full => {
                self.ticks.pop();
                false
            }
            Charge::Truncated => false,
        }
    }

    /// Counts `size` bytes and an entry's cost against the bound.
    fn charge(&mut self, size: usize) -> Charge {
        if self.truncated {
            return Charge::Truncated;
        }
        let bytes = self.bytes.saturating_add(size).saturating_add(ENTRY_COST);
        if bytes > MAX_BYTES {
            warn!(
                bytes = self.bytes,
                "input log reached its bound; later ticks are not recorded"
            );
            self.truncated = true;
            return Charge::Full;
        }
        self.bytes = bytes;
        Charge::Fits
    }

    /// The written form: the header, then every tick. Integers are little-endian.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.bytes.saturating_add(64));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&protocol::packet::PROTOCOL_VERSION.to_le_bytes());
        out.push(u8::from(self.truncated));
        put_len(&mut out, self.ticks.len())?;
        let mut body = Vec::new();
        for tick in &self.ticks {
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
                        EntryKind::Join(Identity {
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
            ticks.push(Tick {
                number,
                inputs,
                dropped,
            });
        }
        if !r.0.is_empty() {
            return Err(Error::Format("bytes after the last tick"));
        }
        Ok(Self {
            ticks,
            bytes: 0,
            truncated,
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
    let tick = log.ticks.last().map_or(0, |t| t.number);
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
}
