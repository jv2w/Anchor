//! Sealed values: a header, then the record bytes, compressed when that makes them smaller.
//!
//! Header (12 bytes, little-endian): format version u16, method u8 (0 stored, 2 raw DEFLATE),
//! reserved u8 (0), record length u32, CRC-32C u32 over the key, the first 8 header bytes and the
//! body. The first two bytes are the format version, so a newer value is told from a damaged one.
//! Opening reads only the version before it checks the checksum, and checks the declared length
//! against [`MAX_RECORD`] before it allocates. Only the current format reads: a value of another
//! is damage.

use std::borrow::Cow;

use crate::protocol::compression::zlib::{self, Framing};
use crate::protocol::compression::{Compression, Compressor};
use crate::protocol::{Reader, Writer};

use super::codec::Record;
use super::{Error, Result};

/// The format version this Anchor writes and the only one it reads. Its layouts are in `codec`;
/// the terrain is one value per sub-chunk, none for an empty sub-chunk of plains
/// (`save::seal_terrain`), and extras lists are in pages. The main file holds the operators,
/// bans, whitelist and the user's server data; the players have a file of their own. A world
/// record holds the world's spawn after its number and time. The players file and every world
/// file keep one redb persistent savepoint, of the file as it was before its last commit, until a
/// normal stop deletes it; the main file keeps none. The main file's `meta` table also holds a
/// deletion mark (`deleted:` and the world's name, a `Mark`) for each world its save deleted
/// whose file may still be there.
pub(crate) const FORMAT_VERSION: u16 = 12;
/// The largest record, in bytes before compression. The largest sub-chunk value takes about
/// 0.92 MB with the 4096 vanilla states of the longest NBT in a layer (238 bytes at most, 222.5
/// on average) and about 9.4 MB with 4096 custom states of the longest NBT (2,293 bytes); a
/// whole column would take up to 24 times that, which is why the terrain is stored by
/// sub-chunk. Extras lists are split into pages of at most this.
pub(crate) const MAX_RECORD: usize = 16 << 20;
/// Bytes a record decoder may allocate: four times the record, for palette NBT. Extras pages
/// are cut to read back within it (`ExtraRecord::pages`), and a player record's stacks are
/// bounded so that it does (`item::MAX_NBT_BYTES`).
pub(crate) const DECODE_BUDGET: usize = 4 * MAX_RECORD;
const HEADER: usize = 12;
/// Records shorter than this are stored: compression cannot pay for itself.
const MIN_COMPRESSED: usize = 64;
const STORED: u8 = 0;
const DEFLATE: u8 = 2;
/// The libdeflate level values are compressed at: level 9 is 1.2% smaller and level 12 4.1% on
/// a whole imported world, both slower.
const DEFLATE_LEVEL: u8 = 6;

/// Encodes `record` and seals it under `key` (the key's bytes).
pub(crate) fn seal(key: &[u8], record: &impl Record) -> Result<Vec<u8>> {
    let mut w = Writer::new(Vec::new(), 0);
    record.encode(&mut w).map_err(Error::Encode)?;
    seal_encoded(key, w.as_bytes())
}

/// Seals record bytes already encoded in the current format (an extras page) under `key`,
/// compressed with raw DEFLATE when that makes them smaller.
pub(crate) fn seal_encoded(key: &[u8], record: &[u8]) -> Result<Vec<u8>> {
    if record.len() > MAX_RECORD {
        return Err(Error::Encode(crate::protocol::Error::Size));
    }
    let raw_len =
        u32::try_from(record.len()).map_err(|_| Error::Encode(crate::protocol::Error::Size))?;
    let mut out = Vec::with_capacity(HEADER + record.len() / 2);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&[STORED, 0]);
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    if record.len() >= MIN_COMPRESSED {
        Compressor::new(Compression::Zlib, DEFLATE_LEVEL)
            .compress(record, &mut out, MAX_RECORD)
            .map_err(|_| Error::Encode(crate::protocol::Error::Size))?;
    }
    let compressed = out.len() - HEADER;
    if compressed == 0 || compressed >= record.len() {
        out.truncate(HEADER);
        out.extend_from_slice(record);
    } else if let Some(method) = out.get_mut(2) {
        *method = DEFLATE;
    }
    let crc = checksum(key, &out);
    if let Some(field) = out.get_mut(8..HEADER) {
        field.copy_from_slice(&crc.to_le_bytes());
    }
    Ok(out)
}

/// Opens a value sealed under `key` and decodes its record.
pub(crate) fn unseal<R: Record>(key: &[u8], sealed: &[u8]) -> Result<R> {
    let record = unseal_bytes(key, sealed)?;
    let mut r = Reader::new(&record, DECODE_BUDGET, 0);
    R::decode(&mut r).map_err(Error::Record)
}

/// The checksum field of a sealed value, read without checking it; 0 if the value is shorter
/// than its header. After [`unseal`] succeeded it identifies the stored bytes.
pub(crate) fn stored_checksum(sealed: &[u8]) -> u32 {
    sealed
        .get(8..HEADER)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// The format version of a sealed value, read without checking anything else.
pub(crate) fn version(sealed: &[u8]) -> Result<u16> {
    let bytes = sealed
        .first_chunk::<2>()
        .ok_or(Error::Malformed("shorter than the header"))?;
    Ok(u16::from_le_bytes(*bytes))
}

/// Checks a sealed value as [`unseal`] does, without decompressing or decoding its record: its
/// header and its checksum, which covers the stored body (`/check`).
pub(crate) fn verify(key: &[u8], sealed: &[u8]) -> Result<()> {
    match checked(key, sealed)? {
        (STORED, raw_len, body) if body.len() != raw_len => {
            Err(Error::Malformed("stored length differs from the header"))
        }
        (STORED | DEFLATE, _, _) => Ok(()),
        _ => Err(Error::Malformed("unknown compression method")),
    }
}

/// Checks a sealed value's header and checksum; returns its compression method, its record
/// length (at most [`MAX_RECORD`]) and its body.
fn checked<'a>(key: &[u8], sealed: &'a [u8]) -> Result<(u8, usize, &'a [u8])> {
    let version = version(sealed)?;
    if version > FORMAT_VERSION {
        return Err(Error::NewerFormat { version });
    }
    let (header, body) = sealed
        .split_first_chunk::<HEADER>()
        .ok_or(Error::Malformed("shorter than the header"))?;
    let [_, _, method, reserved, l0, l1, l2, l3, c0, c1, c2, c3] = *header;
    if checksum(key, sealed) != u32::from_le_bytes([c0, c1, c2, c3]) {
        return Err(Error::Checksum);
    }
    if version != FORMAT_VERSION || reserved != 0 {
        return Err(Error::Malformed("unknown version or reserved bits set"));
    }
    let raw_len = usize::try_from(u32::from_le_bytes([l0, l1, l2, l3]))
        .map_err(|_| Error::Malformed("record length over the limit"))?;
    if raw_len > MAX_RECORD {
        return Err(Error::Malformed("record length over the limit"));
    }
    Ok((method, raw_len, body))
}

/// Checks a sealed value and returns its record bytes.
fn unseal_bytes<'a>(key: &[u8], sealed: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    let (method, raw_len, body) = checked(key, sealed)?;
    match method {
        STORED if body.len() == raw_len => Ok(Cow::Borrowed(body)),
        STORED => Err(Error::Malformed("stored length differs from the header")),
        DEFLATE => {
            let mut out = Vec::new();
            zlib::Decoder::new()
                .decompress_whole(body, &mut out, raw_len, Framing::Raw)
                .map_err(Error::Compression)?;
            if out.len() == raw_len {
                Ok(Cow::Owned(out))
            } else {
                Err(Error::Malformed(
                    "decompressed length differs from the header",
                ))
            }
        }
        _ => Err(Error::Malformed("unknown compression method")),
    }
}

/// The CRC-32C a sealed value carries: over `key` and the value without its checksum field.
fn checksum(key: &[u8], sealed: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(key);
    crc.update(sealed.get(..8).unwrap_or_default());
    crc.update(sealed.get(HEADER..).unwrap_or_default());
    crc.finish()
}

/// CRC-32C (Castagnoli, reflected polynomial `0x82F63B78`), eight bytes per step.
///
/// Hand-written: Rust's hardware CRC instructions need `unsafe`, and no dependency offers it.
/// A CRC detects every burst error up to 32 bits, which a non-CRC hash of the same width does not.
pub(crate) struct Crc32c(u32);

/// `TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes.
static TABLES: [[u32; 256]; 8] = crc_tables();

#[expect(
    clippy::indexing_slicing,
    reason = "invariant: the loops index arrays of 8 and 256 within their lengths"
)]
const fn crc_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0; 256]; 8];
    let mut b: u32 = 0;
    while b < 256 {
        let mut crc = b;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][b as usize] = crc;
        b += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut b = 0;
        while b < 256 {
            let prev = tables[k - 1][b];
            tables[k][b] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
            b += 1;
        }
        k += 1;
    }
    tables
}

impl Crc32c {
    /// A CRC of no bytes yet.
    pub(crate) fn new() -> Self {
        Self(!0)
    }

    /// Adds `bytes`.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a byte indexes a table of 256"
    )]
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        let [t0, t1, t2, t3, t4, t5, t6, t7] = &TABLES;
        let mut crc = self.0;
        let (eights, rest) = bytes.as_chunks::<8>();
        for &[b0, b1, b2, b3, b4, b5, b6, b7] in eights {
            let [c0, c1, c2, c3] = crc.to_le_bytes();
            crc = t7[usize::from(b0 ^ c0)]
                ^ t6[usize::from(b1 ^ c1)]
                ^ t5[usize::from(b2 ^ c2)]
                ^ t4[usize::from(b3 ^ c3)]
                ^ t3[usize::from(b4)]
                ^ t2[usize::from(b5)]
                ^ t1[usize::from(b6)]
                ^ t0[usize::from(b7)];
        }
        for &b in rest {
            crc = (crc >> 8) ^ t0[usize::from(crc.to_le_bytes()[0] ^ b)];
        }
        self.0 = crc;
    }

    /// The CRC of the bytes added.
    pub(crate) fn finish(&self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbt::Compound;
    use crate::protocol::Rng;
    use crate::protocol::compression::Decompressor;
    use crate::storage::codec::tests::{
        empty_chunk, encode, random_chunk, random_compound, terrain_chunk,
    };
    use crate::storage::codec::{BlockLayer, ChunkRecord, ExtraRecord, Meta, PlayerRecord};

    const KEY: &[u8] = b"\x00\x00\x00\x00\x80\x00\x00\x01\x7f\xff\xff\xff\x00";

    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = Crc32c::new();
        crc.update(bytes);
        crc.finish()
    }

    /// The CRC-32C definition, one bit at a time.
    fn crc_bitwise(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0x82f6_3b78
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Replaces the checksum of a sealed value so that changed bytes reach the later checks.
    fn reseal(key: &[u8], sealed: &mut [u8]) {
        let crc = checksum(key, sealed);
        if sealed.len() >= HEADER {
            sealed[8..HEADER].copy_from_slice(&crc.to_le_bytes());
        }
    }

    #[test]
    fn crc32c_matches_its_definition() {
        assert_eq!(crc(b"123456789"), 0xe306_9283);
        assert_eq!(crc(b""), 0);
        let mut rng = Rng(9);
        let data: Vec<u8> = (0..300).map(|_| rng.next().to_le_bytes()[0]).collect();
        for len in 0..data.len() {
            assert_eq!(crc(&data[..len]), crc_bitwise(&data[..len]), "{len}");
        }
        // Updates in pieces equal one update.
        let mut pieces = Crc32c::new();
        for piece in data.chunks(7) {
            pieces.update(piece);
        }
        assert_eq!(pieces.finish(), crc(&data));
    }

    #[test]
    fn records_round_trip_and_large_ones_are_compressed() {
        let mut rng = Rng(21);
        let chunk = terrain_chunk(&mut rng);
        let sealed = seal(KEY, &chunk).unwrap();
        assert_eq!(sealed[2], DEFLATE);
        assert!(sealed.len() < encode(&chunk).len() / 4);
        assert_eq!(unseal::<ChunkRecord>(KEY, &sealed).unwrap(), chunk);
        let chunk = random_chunk(&mut rng);
        assert_eq!(
            unseal::<ChunkRecord>(KEY, &seal(KEY, &chunk).unwrap()).unwrap(),
            chunk
        );
        let meta = Meta::at(77);
        let sealed = seal(b"meta", &meta).unwrap();
        assert_eq!((sealed[2], sealed.len()), (STORED, HEADER + 9));
        assert_eq!(unseal::<Meta>(b"meta", &sealed).unwrap(), meta);
        assert_eq!(version(&sealed).unwrap(), FORMAT_VERSION);
        // The checksum field is what the seal wrote.
        let sealed = seal(KEY, &chunk).unwrap();
        assert_eq!(stored_checksum(&sealed), checksum(KEY, &sealed));
        assert_eq!(stored_checksum(&sealed[..5]), 0);
    }

    #[test]
    fn a_value_opens_only_under_its_own_key() {
        let sealed = seal(KEY, &empty_chunk()).unwrap();
        let mut other = KEY.to_vec();
        other[12] = 1;
        assert!(matches!(
            unseal::<ChunkRecord>(&other, &sealed),
            Err(Error::Checksum)
        ));
    }

    #[test]
    fn versions_are_checked() {
        let mut sealed = seal(b"meta", &Meta::at(1)).unwrap();
        sealed[..2].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            unseal::<Meta>(b"meta", &sealed),
            Err(Error::NewerFormat { version }) if version == FORMAT_VERSION + 1
        ));
        // An older format is damage, as is version 0.
        for older in [FORMAT_VERSION - 1, 1, 0] {
            sealed[..2].copy_from_slice(&older.to_le_bytes());
            reseal(b"meta", &mut sealed);
            assert!(
                matches!(unseal::<Meta>(b"meta", &sealed), Err(Error::Malformed(_))),
                "{older}"
            );
            assert!(matches!(verify(b"meta", &sealed), Err(Error::Malformed(_))));
        }
    }

    #[test]
    fn over_limit_lengths_are_refused_before_allocating() {
        // A declared record length over the limit.
        let mut sealed = seal(KEY, &terrain_chunk(&mut Rng(4))).unwrap();
        let over = u32::try_from(MAX_RECORD + 1).unwrap();
        sealed[4..8].copy_from_slice(&over.to_le_bytes());
        reseal(KEY, &mut sealed);
        assert!(matches!(
            unseal::<ChunkRecord>(KEY, &sealed),
            Err(Error::Malformed(_))
        ));
        // A DEFLATE body that inflates past the header's length: refused by the decoder's limit.
        let mut sealed = seal(KEY, &terrain_chunk(&mut Rng(4))).unwrap();
        let short = u32::from_le_bytes(sealed[4..8].try_into().unwrap()) - 1;
        sealed[4..8].copy_from_slice(&short.to_le_bytes());
        reseal(KEY, &mut sealed);
        assert!(matches!(
            unseal::<ChunkRecord>(KEY, &sealed),
            Err(Error::Compression(_))
        ));
        // Encoding refuses a record over the limit.
        let big = vec![0u8; MAX_RECORD + 1];
        assert!(matches!(seal_encoded(KEY, &big), Err(Error::Encode(_))));
        // A player extra or an extras list whose NBT declares more than is there.
        let mut record = encode(&PlayerRecord {
            world: "w".to_owned(),
            position: [0.0; 3],
            extra: Compound::from([("a".to_owned(), crate::nbt::Value::ByteArray(vec![1; 4]))]),
        });
        let at = record.len() - 5;
        record[at - 4..at].copy_from_slice(&u32::MAX.to_le_bytes());
        let sealed = seal_encoded(KEY, &record).unwrap();
        assert!(matches!(
            unseal::<PlayerRecord>(KEY, &sealed),
            Err(Error::Record(_))
        ));
        let mut record = encode(&ExtraRecord(vec![Compound::new(); 3].into()));
        record[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let sealed = seal_encoded(KEY, &record).unwrap();
        assert!(matches!(
            unseal::<ExtraRecord<'_>>(KEY, &sealed),
            Err(Error::Record(crate::protocol::Error::Size))
        ));
    }

    /// 10,000 mutated values: every one is an error, never a panic. Half get a valid checksum
    /// again, so the header, decompression and record checks see the damage too.
    #[test]
    fn mutated_values_are_errors_and_never_panic() {
        let mut rng = Rng(0x0dd_ba11);
        let originals = [
            seal(KEY, &terrain_chunk(&mut rng)).unwrap(),
            seal(KEY, &random_chunk(&mut rng)).unwrap(),
            seal(KEY, &empty_chunk()).unwrap(),
            seal(
                KEY,
                &PlayerRecord {
                    world: "w".repeat(80),
                    position: [1.0, 2.0, 3.0],
                    extra: random_compound(&mut rng, 4),
                },
            )
            .unwrap(),
            seal(
                KEY,
                &ExtraRecord(
                    (0..12)
                        .map(|_| random_compound(&mut rng, 3))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            )
            .unwrap(),
        ];
        let mut deep = 0;
        for round in 0..10_000 {
            let original = &originals[round % originals.len()];
            let mut sealed = original.clone();
            for _ in 0..=rng.below(3) {
                let at = rng.below(sealed.len());
                match rng.below(4) {
                    _ if sealed.is_empty() => sealed.push(rng.next().to_le_bytes()[0]),
                    0 => sealed[at] ^= 1 << rng.below(8),
                    1 => sealed.truncate(at),
                    2 => sealed.insert(at, rng.next().to_le_bytes()[0]),
                    _ => sealed[at] = rng.next().to_le_bytes()[0],
                }
            }
            if sealed == *original {
                continue;
            }
            let fixed = round % 2 == 0;
            if fixed {
                reseal(KEY, &mut sealed);
            }
            let result = match round % originals.len() {
                3 => unseal::<PlayerRecord>(KEY, &sealed).map(drop),
                4 => unseal::<ExtraRecord<'_>>(KEY, &sealed).map(drop),
                _ => unseal::<ChunkRecord>(KEY, &sealed).map(drop),
            };
            match result {
                Err(Error::Checksum) => assert!(!fixed, "round {round}"),
                // A flip inside packed indices or a float can still decode: the checksum is
                // what catches it, and it was replaced.
                Ok(()) => assert!(fixed, "round {round}"),
                Err(_) => deep += 1,
            }
        }
        assert!(
            deep > 1_000,
            "only {deep} mutations reached the later checks"
        );
    }

    /// Measures the compression of chunk records: `cargo test --release --lib
    /// storage::seal::tests::measure_compression -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        reason = "consistency: a measurement prints its results"
    )]
    fn measure_compression() {
        let mut rng = Rng(1);
        let samples: [(&str, Vec<Vec<u8>>); 3] = [
            (
                "terrain",
                (0..200).map(|_| encode(&terrain_chunk(&mut rng))).collect(),
            ),
            ("flat", vec![encode(&flat_chunk()); 200]),
            (
                "random",
                (0..50).map(|_| encode(&random_chunk(&mut rng))).collect(),
            ),
        ];
        for (name, records) in &samples {
            let raw: usize = records.iter().map(Vec::len).sum();
            let n = records.len();
            println!("{name}: {} bytes per record", raw / n);
            for (method, compression, level) in [
                ("snappy", Compression::Snappy, 1),
                ("deflate 1", Compression::Zlib, 1),
                ("deflate 6", Compression::Zlib, 6),
            ] {
                let mut compressor = Compressor::new(compression, level);
                let mut packed = Vec::new();
                let start = crate::clock::now();
                for record in records {
                    let mut out = Vec::new();
                    compressor.compress(record, &mut out, MAX_RECORD).unwrap();
                    packed.push(out);
                }
                let compress = crate::clock::now().duration_since(start);
                let mut decompressor = Decompressor::new(compression);
                let start = crate::clock::now();
                for (record, packed) in records.iter().zip(&packed) {
                    let mut out = Vec::new();
                    decompressor
                        .decompress(packed, &mut out, record.len())
                        .unwrap();
                }
                let decompress = crate::clock::now().duration_since(start);
                let size: usize = packed.iter().map(Vec::len).sum();
                println!(
                    "  {method}: {:.1}% ({} B), compress {:.1} µs, decompress {:.1} µs per record",
                    float(size) * 100.0 / float(raw),
                    size / n,
                    compress.as_secs_f64() * 1e6 / float(n),
                    decompress.as_secs_f64() * 1e6 / float(n),
                );
            }
            let chunks: Vec<ChunkRecord> = records
                .iter()
                .map(|r| ChunkRecord::decode(&mut Reader::new(r, DECODE_BUDGET, 0)).unwrap())
                .collect();
            let start = crate::clock::now();
            let sealed: Vec<Vec<u8>> = chunks.iter().map(|c| seal(KEY, c).unwrap()).collect();
            let sealing = crate::clock::now().duration_since(start);
            let start = crate::clock::now();
            for value in &sealed {
                std::hint::black_box(unseal::<ChunkRecord>(KEY, value).unwrap());
            }
            let unsealing = crate::clock::now().duration_since(start);
            println!(
                "  seal (encode, deflate, crc) {:.1} µs, unseal (crc, inflate, decode) {:.1} µs",
                sealing.as_secs_f64() * 1e6 / float(n),
                unsealing.as_secs_f64() * 1e6 / float(n),
            );
            let start = crate::clock::now();
            for record in records {
                std::hint::black_box(crc(record));
            }
            let elapsed = crate::clock::now().duration_since(start);
            println!(
                "  crc32c: {:.0} MB/s",
                float(raw) / elapsed.as_secs_f64() / 1e6
            );
        }
    }

    fn float(n: usize) -> f64 {
        f64::from(u32::try_from(n).unwrap())
    }

    /// A BDS default flat chunk: bedrock, two dirt and grass at the bottom, plains.
    fn flat_chunk() -> ChunkRecord {
        use crate::storage::codec::tests::{block, pack};
        let mut chunk = empty_chunk();
        let indices: Vec<u16> = (0..4096)
            .map(|offset| match offset & 15 {
                0 => 1,
                1 | 2 => 2,
                3 => 3,
                _ => 0,
            })
            .collect();
        chunk.sub_chunks[0].layers.push(BlockLayer {
            bits: 2,
            words: pack(2, &indices),
            palette: ["air", "bedrock", "dirt", "grass_block"]
                .iter()
                .map(|n| block(&format!("minecraft:{n}"), &[]))
                .collect(),
        });
        chunk
    }
}
