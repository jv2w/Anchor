//! Batch framing: packet lists, compression headers and batch limits.
//!
//! A batch body is a header byte and the payload, compressed or not ([`Encoder`], [`Decoder`]).
//! The payload is a list of packets, each a varuint32 length and that many bytes: a [`Header`]
//! and the packet's own bytes ([`split`], [`decode_packets`], [`encode_packets`]).

use std::fmt;
use std::mem::size_of;

use super::compression::{self, Compression, Compressor, Decompressor};
use super::packet::Packet;
use super::{Reader, Writer};
use crate::protocol;

/// The first byte of a batch body sent without compression.
const UNCOMPRESSED: u8 = 0xff;
const DEFAULT_THRESHOLD: u16 = 256;
const ZLIB_LEVELS: std::ops::RangeInclusive<u8> = 1..=9;
const DEFAULT_ZLIB_LEVEL: u8 = 6;
const MAX_BATCH_BYTES: std::ops::RangeInclusive<usize> = 1024 * 1024..=64 * 1024 * 1024;
const DEFAULT_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// The most packets a batch may hold (gophertunnel `maximumInBatch`).
const MAX_PACKETS: usize = 812;
/// The payload size [`encode_packets`] cuts batches before.
const SPLIT_BYTES: usize = 256 * 1024;
/// The largest packet ID a header can carry.
const MAX_PACKET_ID: u32 = 0x3ff;
/// The largest sub-client ID a header can carry.
const MAX_SUB_CLIENT: u8 = 3;

/// Validated batch compression settings, shared by every connection.
///
/// The default is Snappy, threshold 256, zlib level 6 and 16 MiB batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    compression: Compression,
    threshold: u16,
    zlib_level: u8,
    max_batch_bytes: usize,
}

impl Settings {
    /// Returns settings after checking every value; nothing is clamped.
    ///
    /// - `threshold`: payloads shorter than this are sent uncompressed; 0 is rejected because
    ///   implementations disagree on what it means.
    /// - `zlib_level`: 1 (fastest) to 9 (smallest); checked even when Snappy is chosen.
    /// - `max_batch_bytes`: 1 MiB to 64 MiB; the largest payload encoded or decoded.
    pub fn new(
        compression: Compression,
        threshold: u16,
        zlib_level: u8,
        max_batch_bytes: usize,
    ) -> Result<Self> {
        if threshold == 0 {
            return Err(Error::Threshold);
        }
        if !ZLIB_LEVELS.contains(&zlib_level) {
            return Err(Error::ZlibLevel(zlib_level));
        }
        if !MAX_BATCH_BYTES.contains(&max_batch_bytes) {
            return Err(Error::MaxBatchBytes(max_batch_bytes));
        }
        Ok(Self {
            compression,
            threshold,
            zlib_level,
            max_batch_bytes,
        })
    }

    /// Returns the compression algorithm.
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// Returns the smallest payload length that is compressed.
    pub fn threshold(&self) -> u16 {
        self.threshold
    }

    /// Returns the zlib compression level.
    pub fn zlib_level(&self) -> u8 {
        self.zlib_level
    }

    /// Returns the largest payload, in bytes, that a batch may carry.
    pub fn max_batch_bytes(&self) -> usize {
        self.max_batch_bytes
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            compression: Compression::default(),
            threshold: DEFAULT_THRESHOLD,
            zlib_level: DEFAULT_ZLIB_LEVEL,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
        }
    }
}

/// Compresses batch payloads into batch bodies, reusing its compression state.
///
/// Keep one per worker; it is not shared between threads while encoding.
pub struct Encoder {
    settings: Settings,
    compressor: Compressor,
}

impl Encoder {
    /// Returns an encoder for `settings`.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            compressor: Compressor::new(settings.compression, settings.zlib_level),
        }
    }

    /// Returns the settings this encoder uses.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the contents of `out` with the batch body for `payload`: the header byte, then
    /// the payload compressed, or as-is with header 0xff when it is shorter than the threshold.
    ///
    /// A payload longer than `max_batch_bytes` is a size error. On error the contents of `out`
    /// are unspecified.
    pub fn encode(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        if payload.len() < usize::from(self.settings.threshold) {
            out.reserve(1 + payload.len());
            out.push(UNCOMPRESSED);
            out.extend_from_slice(payload);
            return Ok(());
        }
        out.push(self.settings.compression.header());
        self.compressor
            .compress(payload, out, self.settings.max_batch_bytes)
            .map_err(Error::Compression)
    }
}

impl fmt::Debug for Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Decompresses batch bodies into batch payloads, reusing its decompression state.
///
/// Keep one per connection or worker.
pub struct Decoder {
    settings: Settings,
    decompressor: Decompressor,
}

impl Decoder {
    /// Returns a decoder for `settings`.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            decompressor: Decompressor::new(settings.compression),
        }
    }

    /// Returns the settings this decoder uses.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the contents of `out` with the payload of the batch body `batch`.
    ///
    /// The header must be 0xff or the configured algorithm's id. A payload longer than
    /// `max_batch_bytes` is a size error, found before it is fully inflated. On error the contents
    /// of `out` are unspecified.
    pub fn decode(&mut self, batch: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let (&header, body) = batch.split_first().ok_or(Error::Empty)?;
        let limit = self.settings.max_batch_bytes;
        if header == UNCOMPRESSED {
            if body.len() > limit {
                return Err(Error::Compression(compression::Error::Size { limit }));
            }
            out.clear();
            out.extend_from_slice(body);
            return Ok(());
        }
        if header != self.settings.compression.header() {
            return Err(Error::Header(header));
        }
        self.decompressor
            .decompress(body, out, limit)
            .map_err(Error::Compression)
    }
}

impl fmt::Debug for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoder")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The header before each packet in a batch payload (gophertunnel `packet.Header`): one varuint32
/// with the packet ID in bits 0 to 9, the sender sub-client in bits 10 and 11 and the target
/// sub-client in bits 12 and 13. Sub-clients are split-screen players on one connection.
///
/// Reading ignores bits above 13, like gophertunnel: only official clients show whether they set
/// them, so Anchor accepts every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// The packet ID, at most 1023.
    pub id: u32,
    /// The sub-client that sent the packet, at most 3.
    pub sender_sub_client: u8,
    /// The sub-client the packet is for, at most 3.
    pub target_sub_client: u8,
}

impl Header {
    fn read(r: &mut Reader<'_>) -> protocol::Result<Self> {
        let value = r.varuint32()?;
        let sub_client = |shift: u32| {
            u8::try_from((value >> shift) & u32::from(MAX_SUB_CLIENT))
                .map_err(|_| protocol::Error::Size)
        };
        Ok(Self {
            id: value & MAX_PACKET_ID,
            sender_sub_client: sub_client(10)?,
            target_sub_client: sub_client(12)?,
        })
    }

    fn write(self, w: &mut Writer) -> protocol::Result<()> {
        if self.id > MAX_PACKET_ID
            || self.sender_sub_client > MAX_SUB_CLIENT
            || self.target_sub_client > MAX_SUB_CLIENT
        {
            return Err(protocol::Error::InvalidValue {
                field: "packet header",
                reason: "id or sub-client out of range",
            });
        }
        w.varuint32(
            self.id
                | u32::from(self.sender_sub_client) << 10
                | u32::from(self.target_sub_client) << 12,
        );
        Ok(())
    }
}

/// The packets of a batch payload in order, from [`split`]: each header and the packet bytes
/// after it, borrowed from the payload.
#[derive(Debug, Clone)]
pub struct Packets<'a> {
    rest: &'a [u8],
    left: usize,
}

impl<'a> Packets<'a> {
    /// Reads the next packet, or `None` at the end.
    fn read(&mut self) -> protocol::Result<Option<(Header, &'a [u8])>> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let mut r = Reader::new(self.rest, 0, 0);
        let len = r.varuint32()?;
        if len == 0 {
            return Err(protocol::Error::InvalidValue {
                field: "batch packet",
                reason: "empty",
            });
        }
        let packet = r.raw(len.into())?;
        self.rest = r.remaining();
        let mut r = Reader::new(packet, 0, 0);
        let header = Header::read(&mut r)?;
        Ok(Some((header, r.remaining())))
    }
}

impl<'a> Iterator for Packets<'a> {
    type Item = (Header, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        // `split` checked every packet, so reading does not fail here.
        let next = self.read().ok().flatten();
        self.left = if next.is_some() {
            self.left.saturating_sub(1)
        } else {
            0
        };
        next
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Packets<'_> {}

/// Splits a batch payload into its packets without copying, after checking all of it: like
/// gophertunnel `DecodeFunc`, a malformed payload gives no packets.
///
/// Errors, all [`Error::Packet`]: a length or header that does not end, an empty packet, and, as [`protocol::Error::Size`], a length over the bytes left or
/// more than 812 packets.
pub fn split(payload: &[u8]) -> Result<Packets<'_>> {
    let mut packets = Packets {
        rest: payload,
        left: 0,
    };
    let mut count = 0;
    while packets.read().map_err(Error::Packet)?.is_some() {
        count += 1;
        if count > MAX_PACKETS {
            return Err(Error::Packet(protocol::Error::Size));
        }
    }
    Ok(Packets {
        rest: payload,
        left: count,
    })
}

/// Replaces the contents of `out` with the packets of a batch payload, decoded in order.
///
/// The whole batch shares one allocation budget of 2 × `max_batch_bytes`, `out`
/// included; going over it is [`protocol::Error::Size`]. `shield_id` is the shield's item network
/// ID. Errors are those of [`split`] and of [`Packet::decode`], as [`Error::Packet`]; on error
/// `out` is empty.
pub fn decode_packets(
    payload: &[u8],
    settings: &Settings,
    shield_id: i32,
    out: &mut Vec<(Header, Packet)>,
) -> Result<()> {
    out.clear();
    let result = decode_into(payload, settings, shield_id, out);
    if result.is_err() {
        out.clear();
    }
    result
}

fn decode_into(
    payload: &[u8],
    settings: &Settings,
    shield_id: i32,
    out: &mut Vec<(Header, Packet)>,
) -> Result<()> {
    let packets = split(payload)?;
    let size = Error::Packet(protocol::Error::Size);
    let list_bytes = packets
        .len()
        .checked_mul(size_of::<(Header, Packet)>())
        .ok_or(size)?;
    let mut budget = settings
        .max_batch_bytes
        .saturating_mul(2)
        .checked_sub(list_bytes)
        .ok_or(size)?;
    out.reserve(packets.len());
    for (header, bytes) in packets {
        let packet =
            Packet::decode(header.id, bytes, &mut budget, shield_id).map_err(Error::Packet)?;
        out.push((header, packet));
    }
    Ok(())
}

/// Encodes `packets` in order into batch payloads and calls `flush` with each, in order.
///
/// A payload is cut before it would pass 256 KiB or 812 packets; a packet over
/// 256 KiB goes alone in its own payload. `payload` is a working buffer kept for reuse; its
/// contents on return are unspecified. A header whose ID is not the packet's, a header field out
/// of range or a packet that cannot be encoded is [`Error::Packet`]; payloads given to `flush`
/// before an error stay given, and the rest are dropped. An error from `flush` ends the call.
pub fn encode_packets<'p>(
    packets: impl IntoIterator<Item = (Header, &'p Packet)>,
    shield_id: i32,
    payload: &mut Vec<u8>,
    mut flush: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    payload.clear();
    let mut count = 0;
    for (header, packet) in packets {
        if count == MAX_PACKETS {
            flush(payload)?;
            payload.clear();
            count = 0;
        }
        let start = payload.len();
        append_packet(header, packet, shield_id, payload).map_err(Error::Packet)?;
        if payload.len() > SPLIT_BYTES && start > 0 {
            #[expect(
                clippy::indexing_slicing,
                reason = "invariant: start is a length the payload had before it grew"
            )]
            flush(&payload[..start])?;
            payload.drain(..start);
            count = 0;
        }
        count += 1;
    }
    if !payload.is_empty() {
        flush(payload)?;
    }
    Ok(())
}

/// Appends one packet to a batch payload: its length, header and bytes. On error `payload` is
/// left as it was.
fn append_packet(
    header: Header,
    packet: &Packet,
    shield_id: i32,
    payload: &mut Vec<u8>,
) -> protocol::Result<()> {
    if header.id != packet.id() {
        return Err(protocol::Error::InvalidValue {
            field: "packet header",
            reason: "id is not the packet's",
        });
    }
    let start = payload.len();
    let mut w = Writer::new(std::mem::take(payload), shield_id);
    let result = header.write(&mut w);
    *payload = w.into_inner();
    let result = result
        .and_then(|()| packet.encode(payload, shield_id))
        .and_then(|()| protocol::length::<u32>(payload.len() - start));
    let len = match result {
        Ok(len) => len,
        Err(error) => {
            payload.truncate(start);
            return Err(error);
        }
    };
    // The length goes after the packet, then moves in front: one pass over the packet bytes.
    let end = payload.len();
    let mut w = Writer::new(std::mem::take(payload), shield_id);
    w.varuint32(len);
    *payload = w.into_inner();
    let prefix_len = payload.len() - end;
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: start is a length the payload had before it grew"
    )]
    payload[start..].rotate_right(prefix_len);
    Ok(())
}

/// A batch settings, encoding or decoding failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The compression threshold is 0.
    Threshold,
    /// The zlib level is outside 1..=9.
    ZlibLevel(u8),
    /// The batch size limit is outside 1 MiB..=64 MiB.
    MaxBatchBytes(usize),
    /// The batch has no header byte.
    Empty,
    /// The header byte is neither 0xff nor the configured algorithm's id.
    Header(u8),
    /// Compressing or decompressing the body failed; size limits end here as
    /// [`compression::Error::Size`].
    Compression(compression::Error),
    /// The packet list or a packet in it failed to decode or encode; size limits end here as
    /// [`protocol::Error::Size`].
    Packet(protocol::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Threshold => f.write_str("compression threshold must be at least 1"),
            Self::ZlibLevel(level) => write!(f, "zlib level {level} is outside 1..=9"),
            Self::MaxBatchBytes(bytes) => {
                write!(f, "max batch bytes {bytes} is outside 1048576..=67108864")
            }
            Self::Empty => f.write_str("batch is empty"),
            Self::Header(header) => write!(f, "unexpected batch header {header:#04x}"),
            Self::Compression(_) => f.write_str("batch body compression failed"),
            Self::Packet(_) => f.write_str("batch packet list is invalid"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compression(source) => Some(source),
            Self::Packet(source) => Some(source),
            _ => None,
        }
    }
}

/// The result of a batch operation.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::super::compression::tests::{ROUND_TRIP_SIZES, sample};
    use super::super::io::tests::Rng;
    use super::super::packet::{Login, Packet, SetTime};
    use super::{
        Compression, Decoder, Encoder, Error, Header, MAX_PACKET_ID, MAX_PACKETS, SPLIT_BYTES,
        Settings, compression, decode_packets, encode_packets, protocol, split,
    };

    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    const SHIELD_ID: i32 = 357;

    fn header(packet: &Packet) -> Header {
        Header {
            id: packet.id(),
            ..Header::default()
        }
    }

    fn login(len: usize) -> (Header, Packet) {
        let packet = Packet::Login(Login {
            client_protocol: 2193,
            connection_request: vec![7; len],
        });
        (header(&packet), packet)
    }

    fn set_time(time: i32) -> (Header, Packet) {
        let packet = Packet::SetTime(SetTime { time });
        (header(&packet), packet)
    }

    /// Encodes `packets` and returns every payload given to `flush`.
    fn payloads(packets: &[(Header, Packet)]) -> super::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut buffer = Vec::new();
        encode_packets(
            packets.iter().map(|(h, p)| (*h, p)),
            SHIELD_ID,
            &mut buffer,
            |payload| {
                out.push(payload.to_vec());
                Ok(())
            },
        )?;
        Ok(out)
    }

    fn decode(payload: &[u8], settings: &Settings) -> super::Result<Vec<(Header, Packet)>> {
        let mut out = Vec::new();
        decode_packets(payload, settings, SHIELD_ID, &mut out)?;
        Ok(out)
    }

    #[test]
    fn packet_lists_round_trip() {
        let mut packets: Vec<(Header, Packet)> = (0..=MAX_PACKET_ID)
            .filter_map(Packet::from_id)
            .filter(|p| {
                // Default packets that encode and read back alone (PlayerAuthInput does not).
                let mut bytes = Vec::new();
                let mut budget = MIB;
                p.encode(&mut bytes, SHIELD_ID).is_ok()
                    && Packet::decode(p.id(), &bytes, &mut budget, SHIELD_ID).as_ref() == Ok(p)
            })
            .enumerate()
            .map(|(i, p)| {
                let sub_client = u8::try_from(i % 4).unwrap();
                let header = Header {
                    id: p.id(),
                    sender_sub_client: sub_client,
                    target_sub_client: 3 - sub_client,
                };
                (header, p)
            })
            .collect();
        packets.push(login(1000));
        assert!(packets.len() > 200);
        let cut = payloads(&packets).unwrap();
        assert_eq!(cut.len(), 1);
        assert_eq!(decode(&cut[0], &Settings::default()), Ok(packets));
        assert!(payloads(&[]).unwrap().is_empty());
    }

    #[test]
    fn headers_carry_the_id_and_sub_clients_in_their_bits() {
        let all_bits = [0x02, 0xff, 0x7f];
        let packets: Vec<_> = split(&all_bits).unwrap().collect();
        let expected = Header {
            id: 0x3ff,
            sender_sub_client: 3,
            target_sub_client: 3,
        };
        assert_eq!(packets, [(expected, &[][..])]);
        let bit_14 = [0x03, 0x80, 0x80, 0x01];
        let packets: Vec<_> = split(&bit_14).unwrap().collect();
        assert_eq!(packets, [(Header::default(), &[][..])]);

        let (mut h, packet) = login(0);
        h.sender_sub_client = 2;
        h.target_sub_client = 1;
        let payload = payloads(&[(h, packet.clone())]).unwrap().remove(0);
        // id 1 | 2 << 10 | 1 << 12 = 6145 = varuint32 0x81 0x30.
        assert_eq!(payload[1..3], [0x81, 0x30]);
        assert_eq!(split(&payload).unwrap().next().unwrap().0, h);

        for bad in [
            Header {
                sender_sub_client: 4,
                ..h
            },
            Header {
                target_sub_client: 4,
                ..h
            },
            Header { id: 2, ..h },
        ] {
            assert!(matches!(
                payloads(&[(bad, packet.clone())]),
                Err(Error::Packet(protocol::Error::InvalidValue { .. }))
            ));
        }
    }

    #[test]
    fn a_batch_holds_at_most_812_packets() {
        let set_time_zero = [0x02, 0x0a, 0x00];
        let full = set_time_zero.repeat(MAX_PACKETS);
        assert_eq!(split(&full).unwrap().len(), 812);
        assert_eq!(decode(&full, &Settings::default()).unwrap().len(), 812);
        let over = set_time_zero.repeat(MAX_PACKETS + 1);
        let size = Err(Error::Packet(protocol::Error::Size));
        assert_eq!(split(&over).map(|p| p.len()), size);
        assert_eq!(decode(&over, &Settings::default()).map(|p| p.len()), size);
    }

    #[test]
    fn malformed_packet_lengths_are_rejected() {
        assert_eq!(split(&[]).unwrap().len(), 0);
        let cases: [(&[u8], protocol::Error); 4] = [
            (&[0x05, 0x0a], protocol::Error::Size),
            (&[0x80], protocol::Error::UnexpectedEnd),
            (&[0xff; 6], protocol::Error::VarintOverflow),
            (&[0x01, 0x80], protocol::Error::UnexpectedEnd),
        ];
        for (payload, error) in cases {
            assert_eq!(split(payload).map(|p| p.len()), Err(Error::Packet(error)));
        }
        assert!(matches!(
            split(&[0x00]),
            Err(Error::Packet(protocol::Error::InvalidValue { .. }))
        ));
        // A valid packet followed by a broken one gives no packets at all.
        let mut out = vec![set_time(1)];
        let partly_valid = [0x02, 0x0a, 0x00, 0x05];
        assert!(decode_packets(&partly_valid, &Settings::default(), 0, &mut out).is_err());
        assert!(out.is_empty());
        // Bytes after a packet inside its length are a packet error.
        assert!(matches!(
            decode(&[0x03, 0x0a, 0x00, 0x00], &Settings::default()),
            Err(Error::Packet(protocol::Error::InvalidValue { .. }))
        ));
    }

    #[test]
    fn one_allocation_budget_covers_the_whole_batch() {
        // 1 MiB batches give a 2 MiB budget: two 700 KiB packets fit, three do not.
        let settings = Settings::new(Compression::Snappy, 256, 6, MIB).unwrap();
        let one = payloads(&[login(700 * KIB)]).unwrap().remove(0);
        assert!(decode(&one, &settings).is_ok());
        assert!(decode(&one.repeat(2), &settings).is_ok());
        assert_eq!(
            decode(&one.repeat(3), &settings),
            Err(Error::Packet(protocol::Error::Size))
        );
    }

    #[test]
    fn payloads_are_cut_before_256_kib_in_order() {
        let packets: Vec<_> = (0..5).map(|_| login(100 * KIB)).collect();
        let cut = payloads(&packets).unwrap();
        assert_eq!(cut.len(), 3);
        assert!(cut.iter().all(|p| p.len() <= SPLIT_BYTES));
        let joined: Vec<_> = cut
            .iter()
            .flat_map(|p| decode(p, &Settings::default()).unwrap())
            .collect();
        assert_eq!(joined, packets);

        let big_alone = [set_time(1), login(300 * KIB), set_time(2)];
        let cut = payloads(&big_alone).unwrap();
        assert_eq!(cut.len(), 3);
        assert_eq!(
            decode(&cut[1], &Settings::default()),
            Ok(vec![big_alone[1].clone()])
        );
        assert_eq!(decode(&cut[2], &Settings::default()), Ok(vec![set_time(2)]));

        let many: Vec<_> = (0..=812).map(set_time).collect();
        let cut = payloads(&many).unwrap();
        let counts: Vec<_> = cut.iter().map(|p| split(p).unwrap().len()).collect();
        assert_eq!(counts, [812, 1]);
        assert_eq!(
            decode(&cut[1], &Settings::default()),
            Ok(vec![set_time(812)])
        );
    }

    #[test]
    fn mutated_packet_lists_never_panic() {
        let packets = [set_time(-5), login(40), set_time(i32::MAX), login(3)];
        let seed = payloads(&packets).unwrap().remove(0);
        let settings = Settings::default();
        let mut out = Vec::new();
        let decoded = protocol::fuzz(&seed, 4000, |data| {
            decode_packets(data, &settings, SHIELD_ID, &mut out)
                .map_err(|_| protocol::Error::Size)?;
            let again = payloads(&out).map_err(|_| protocol::Error::Size)?;
            for payload in &again {
                let packets = decode(payload, &settings).unwrap();
                assert_eq!(payloads(&packets).unwrap(), std::slice::from_ref(payload));
            }
            Ok(())
        });
        assert!(decoded > 0);
    }

    fn settings(compression: Compression) -> Settings {
        Settings::new(compression, 256, 6, 16 * MIB).unwrap()
    }

    #[test]
    fn default_settings_match_the_plan() {
        let default = Settings::default();
        assert_eq!(default, settings(Compression::Snappy));
        assert_eq!(default.threshold(), 256);
        assert_eq!(default.zlib_level(), 6);
        assert_eq!(default.max_batch_bytes(), 16 * MIB);
    }

    #[test]
    fn settings_reject_out_of_range_values() {
        let zlib = Compression::Zlib;
        assert_eq!(Settings::new(zlib, 0, 6, MIB), Err(Error::Threshold));
        assert_eq!(Settings::new(zlib, 1, 0, MIB), Err(Error::ZlibLevel(0)));
        assert_eq!(Settings::new(zlib, 1, 10, MIB), Err(Error::ZlibLevel(10)));
        assert_eq!(
            Settings::new(zlib, 1, 6, MIB - 1),
            Err(Error::MaxBatchBytes(MIB - 1))
        );
        assert_eq!(
            Settings::new(zlib, 1, 6, 64 * MIB + 1),
            Err(Error::MaxBatchBytes(64 * MIB + 1))
        );
        assert!(Settings::new(zlib, 1, 1, MIB).is_ok());
        assert!(Settings::new(zlib, u16::MAX, 9, 64 * MIB).is_ok());
    }

    #[test]
    fn batches_round_trip_with_both_algorithms() {
        let (mut body, mut payload) = (Vec::new(), Vec::new());
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut encoder = Encoder::new(settings(compression));
            let mut decoder = Decoder::new(settings(compression));
            for len in ROUND_TRIP_SIZES {
                for repetitive in [false, true] {
                    let input = sample(len, repetitive, 13);
                    encoder.encode(&input, &mut body).unwrap();
                    decoder.decode(&body, &mut payload).unwrap();
                    assert_eq!(payload, input, "{compression:?} len {len}");
                }
            }
        }
    }

    #[test]
    fn payload_below_the_threshold_is_sent_uncompressed() {
        let mut body = Vec::new();
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut encoder = Encoder::new(settings(compression));
            encoder.encode(&[7; 255], &mut body).unwrap();
            assert_eq!(body[0], 0xff);
            assert_eq!(&body[1..], &[7; 255]);
            encoder.encode(&[], &mut body).unwrap();
            assert_eq!(body, [0xff]);
            encoder.encode(&[7; 256], &mut body).unwrap();
            assert_eq!(body[0], compression.header());
            assert!(body.len() < 256);
        }
    }

    #[test]
    fn decode_accepts_only_the_configured_header_or_uncompressed() {
        let mut snappy_body = Vec::new();
        Encoder::new(settings(Compression::Snappy))
            .encode(&[1; 300], &mut snappy_body)
            .unwrap();
        let mut zlib = Decoder::new(settings(Compression::Zlib));
        let mut out = Vec::new();
        assert_eq!(
            zlib.decode(&snappy_body, &mut out),
            Err(Error::Header(0x01))
        );
        for header in [0x02, 0x7f, 0xfe] {
            assert_eq!(
                zlib.decode(&[header, 0], &mut out),
                Err(Error::Header(header))
            );
        }
        assert_eq!(zlib.decode(&[], &mut out), Err(Error::Empty));
        zlib.decode(&[0xff, 1, 2], &mut out).unwrap();
        assert_eq!(out, [1, 2]);
    }

    /// A packet with a string or list over its Mojang limit does not encode, whoever built it:
    /// the client would reject it.
    #[test]
    fn packets_over_a_field_limit_do_not_encode() {
        use super::super::packet::{BossEvent, ClientBoundAttributeLayerSync, PlayerList};
        let encodes = |packet: Packet| packet.encode(&mut Vec::new(), SHIELD_ID);
        let title = |len: usize| {
            Packet::BossEvent(BossEvent {
                boss_bar_title: "a".repeat(len),
                ..BossEvent::default()
            })
        };
        assert_eq!(encodes(title(256)), Ok(()));
        assert_eq!(encodes(title(257)), Err(protocol::Error::Size));
        let entries = |count: usize| {
            Packet::PlayerList(PlayerList {
                entries: vec![crate::protocol::PlayerListEntry::default(); count],
            })
        };
        assert_eq!(encodes(entries(1000)), Ok(()));
        assert_eq!(encodes(entries(1001)), Err(protocol::Error::Size));
        // A limit on each element of a list.
        let names = |len: usize| {
            Packet::ClientBoundAttributeLayerSync(ClientBoundAttributeLayerSync {
                payload_type: 3,
                remove_attribute_names: vec!["n".repeat(len)],
                ..ClientBoundAttributeLayerSync::default()
            })
        };
        assert_eq!(encodes(names(128)), Ok(()));
        assert_eq!(encodes(names(129)), Err(protocol::Error::Size));
    }

    #[test]
    fn payloads_over_the_limit_end_in_a_size_error() {
        let limit = Settings::new(Compression::Snappy, 256, 6, MIB).unwrap();
        let size = Err(Error::Compression(compression::Error::Size { limit: MIB }));
        let mut out = Vec::new();

        let mut uncompressed = vec![0xff];
        uncompressed.resize(MIB + 2, 0);
        assert_eq!(Decoder::new(limit).decode(&uncompressed, &mut out), size);
        assert_eq!(
            Encoder::new(limit).encode(&vec![0; MIB + 1], &mut out),
            size
        );

        for compression in [Compression::Zlib, Compression::Snappy] {
            let big = Settings::new(compression, 256, 6, 2 * MIB).unwrap();
            let small = Settings::new(compression, 256, 6, MIB).unwrap();
            let mut body = Vec::new();
            Encoder::new(big)
                .encode(&vec![0; MIB + 1], &mut body)
                .unwrap();
            assert_eq!(
                Decoder::new(small).decode(&body, &mut out),
                size,
                "{compression:?}"
            );
        }
    }

    #[test]
    fn errors_report_their_cause() {
        use std::error::Error as _;
        let error = Error::Compression(compression::Error::Truncated);
        assert_eq!(error.to_string(), "batch body compression failed");
        assert_eq!(
            error.source().unwrap().to_string(),
            "input ends inside an element"
        );
        assert!(Error::Header(0xfe).source().is_none());
    }

    #[test]
    fn mutated_batches_never_panic() {
        let mut rng = Rng(0xba7c);
        let mut out = Vec::new();
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut encoder = Encoder::new(settings(compression));
            let mut decoder = Decoder::new(settings(compression));
            let mut body = Vec::new();
            encoder
                .encode(&sample(20_000, true, 21), &mut body)
                .unwrap();
            for _ in 0..2000 {
                let mut input = body.clone();
                let at = rng.below(input.len());
                input[at] = input[at].wrapping_add(u8::try_from(1 + rng.below(255)).unwrap());
                if decoder.decode(&input, &mut out).is_ok() {
                    assert!(out.len() <= decoder.settings().max_batch_bytes());
                }
            }
        }
    }
}
