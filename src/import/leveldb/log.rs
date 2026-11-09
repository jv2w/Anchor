//! LevelDB log files (`.log` write-ahead logs and the `MANIFEST`) and the write batches in logs.
//!
//! Format (google/leveldb `doc/log_format.md`, `db/write_batch.cc`; the official server 1.26.52 writes it so):
//! 32 KiB blocks of physical records, each a 7-byte header (masked CRC-32C of the type byte and
//! the payload, little-endian `u16` payload length, type) and the payload. Types: 1 a whole
//! record, 2 its first fragment, 3 a middle one, 4 the last one; a fragment never crosses a block,
//! and fewer than 7 bytes left in a block are zero padding. Type 0 with length 0 marks
//! preallocated space: the rest of the block is skipped. A header or payload cut off by the end
//! of the file, and a record whose last fragment is missing there, are a write the writer never
//! finished; LevelDB drops them silently (`log_reader.cc`), and so does this reader. Anything
//! else malformed is an error: LevelDB would skip it, but a skipped record loses writes.
//!
//! A write batch: sequence number (`u64`), count (`u32`), then `count` records: 1 = put
//! (length-prefixed key and value), 0 = delete (length-prefixed key). Record `i` has sequence
//! number `sequence + i`.

use super::{Bytes, KIND_DELETION, KIND_VALUE, MAX_KEY, MAX_SEQUENCE, masked_crc};
use crate::import::{Error, Result};

/// A write batch header: sequence number and count.
const BATCH_HEADER_BYTES: usize = 12;
/// Log files are cut into blocks of this many bytes.
pub(crate) const BLOCK_BYTES: usize = 32 * 1024;
/// The physical record header: checksum, length, type.
pub(crate) const HEADER_BYTES: usize = 7;
/// The type of preallocated space.
const RECORD_ZERO: u8 = 0;
/// The type of a whole record.
pub(crate) const RECORD_FULL: u8 = 1;
/// The type of a record's first fragment.
pub(crate) const RECORD_FIRST: u8 = 2;
/// The type of a middle fragment.
pub(crate) const RECORD_MIDDLE: u8 = 3;
/// The type of a record's last fragment.
pub(crate) const RECORD_LAST: u8 = 4;

/// Calls `f` with each whole record of the log `data`, in order.
pub(crate) fn records(data: &[u8], mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut assembled: Vec<u8> = Vec::new();
    let mut fragmented = false;
    let mut at = 0;
    while let Some(rest) = data.get(at..).filter(|rest| !rest.is_empty()) {
        let block_left = BLOCK_BYTES - at % BLOCK_BYTES;
        if block_left < HEADER_BYTES {
            at = at.saturating_add(block_left);
            continue;
        }
        let Some((&[c0, c1, c2, c3, l0, l1, kind], _)) = rest.split_first_chunk::<HEADER_BYTES>()
        else {
            // A header cut off by the end of the file: an unfinished write.
            return Ok(());
        };
        let stored = u32::from_le_bytes([c0, c1, c2, c3]);
        let len = usize::from(u16::from_le_bytes([l0, l1]));
        if kind == RECORD_ZERO && len == 0 {
            at = at.saturating_add(block_left);
            continue;
        }
        let end = HEADER_BYTES.saturating_add(len);
        let Some(payload) = rest.get(HEADER_BYTES..end).filter(|_| end <= block_left) else {
            if rest.len() < block_left {
                // The last, partial block ends inside the record: an unfinished write.
                return Ok(());
            }
            return Err(Error::Malformed("log record crosses a block"));
        };
        if masked_crc(&[&[kind], payload]) != stored {
            return Err(Error::Checksum);
        }
        at = at.saturating_add(end);
        match (kind, fragmented) {
            (RECORD_FULL, false) => f(payload)?,
            (RECORD_FIRST, false) => {
                assembled.clear();
                assembled.extend_from_slice(payload);
                fragmented = true;
            }
            (RECORD_MIDDLE, true) => assembled.extend_from_slice(payload),
            (RECORD_LAST, true) => {
                assembled.extend_from_slice(payload);
                fragmented = false;
                f(&assembled)?;
            }
            (RECORD_FULL | RECORD_FIRST, true) => {
                return Err(Error::Malformed("log record starts inside another"));
            }
            (RECORD_MIDDLE | RECORD_LAST, false) => {
                return Err(Error::Malformed("log fragment without a first fragment"));
            }
            _ => return Err(Error::Malformed("unknown log record type")),
        }
    }
    // A record whose last fragment is missing at the end of the file is dropped.
    Ok(())
}

/// One write of a batch.
pub(super) struct Write<'a> {
    /// The user key.
    pub(super) key: &'a [u8],
    /// The write's sequence number.
    pub(super) sequence: u64,
    /// The value; `None` deletes the key.
    pub(super) value: Option<&'a [u8]>,
}

/// Calls `f` with each write of the write batch `record`.
pub(super) fn batch(record: &[u8], mut f: impl FnMut(Write<'_>) -> Result<()>) -> Result<()> {
    let (header, body) = record
        .split_at_checked(BATCH_HEADER_BYTES)
        .ok_or(Error::Malformed("write batch shorter than its header"))?;
    let mut header = Bytes(header);
    let first = header.u64()?;
    let count = header.u32()?;
    let mut body = Bytes(body);
    let mut sequence = first;
    for _ in 0..count {
        // An internal key's tag holds the sequence in 56 bits; a larger one would lose its top.
        if sequence > MAX_SEQUENCE {
            return Err(Error::Malformed("write batch sequence above 56 bits"));
        }
        let kind = body.u8()?;
        let key = body.prefixed()?;
        if key.len() > MAX_KEY {
            return Err(Error::TooLarge {
                what: "bytes in a user key",
                limit: MAX_KEY as u64,
            });
        }
        let value = match kind {
            KIND_VALUE => Some(body.prefixed()?),
            KIND_DELETION => None,
            _ => return Err(Error::Malformed("unknown write batch record")),
        };
        f(Write {
            key,
            sequence,
            value,
        })?;
        // At most `MAX_SEQUENCE` here, so this cannot saturate.
        sequence = sequence.saturating_add(1);
    }
    if !body.0.is_empty() {
        return Err(Error::Malformed("write batch longer than its count"));
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::import::leveldb::tests::put_prefixed;

    /// Appends one physical record.
    pub(crate) fn physical(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
        out.extend_from_slice(&masked_crc(&[&[kind], payload]).to_le_bytes());
        out.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_le_bytes());
        out.push(kind);
        out.extend_from_slice(payload);
    }

    /// Writes `records` as a log, splitting them into fragments at block ends like LevelDB's
    /// `log::Writer`.
    pub(crate) fn write(records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            let mut rest = record.as_slice();
            let mut first = true;
            loop {
                let left = BLOCK_BYTES - out.len() % BLOCK_BYTES;
                if left < HEADER_BYTES {
                    out.resize(out.len() + left, 0);
                    continue;
                }
                let room = left - HEADER_BYTES;
                let take = rest.len().min(room);
                let last = take == rest.len();
                let kind = match (first, last) {
                    (true, true) => RECORD_FULL,
                    (true, false) => RECORD_FIRST,
                    (false, false) => RECORD_MIDDLE,
                    (false, true) => RECORD_LAST,
                };
                physical(&mut out, kind, &rest[..take]);
                rest = &rest[take..];
                first = false;
                if last {
                    break;
                }
            }
        }
        out
    }

    /// A write batch of `writes` starting at `sequence`.
    pub(crate) fn encode_batch(sequence: u64, writes: &[(Vec<u8>, Option<Vec<u8>>)]) -> Vec<u8> {
        let mut out = sequence.to_le_bytes().to_vec();
        out.extend_from_slice(&u32::try_from(writes.len()).unwrap().to_le_bytes());
        for (key, value) in writes {
            out.push(if value.is_some() {
                KIND_VALUE
            } else {
                KIND_DELETION
            });
            put_prefixed(&mut out, key);
            if let Some(value) = value {
                put_prefixed(&mut out, value);
            }
        }
        out
    }

    fn read_all(data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        records(data, |r| {
            out.push(r.to_vec());
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn records_spanning_blocks_read_back_whole() {
        let records = vec![
            vec![1; 10],
            vec![2; BLOCK_BYTES * 2 + 100],
            Vec::new(),
            vec![3; BLOCK_BYTES - HEADER_BYTES - 10 - HEADER_BYTES],
            vec![4; 5],
        ];
        let data = write(&records);
        assert!(data.len() > 3 * BLOCK_BYTES);
        assert_eq!(read_all(&data).unwrap(), records);
    }

    #[test]
    fn padding_at_a_block_end_is_skipped() {
        // The first record leaves 6 bytes in the block, fewer than a header.
        let records = vec![vec![7; BLOCK_BYTES - HEADER_BYTES - 6], vec![8; 3]];
        let data = write(&records);
        assert_eq!(&data[BLOCK_BYTES - 6..BLOCK_BYTES], &[0; 6]);
        assert_eq!(read_all(&data).unwrap(), records);
    }

    #[test]
    fn an_unfinished_write_at_the_end_is_dropped() {
        let records = vec![vec![1; 100], vec![2; BLOCK_BYTES + 10]];
        let data = write(&records);
        // Cut inside the second record's first fragment, its second header, and its payload.
        for cut in [200, BLOCK_BYTES + 3, data.len() - 1] {
            assert_eq!(
                read_all(&data[..cut]).unwrap(),
                vec![vec![1; 100]],
                "cut {cut}"
            );
        }
    }

    #[test]
    fn preallocated_zero_space_skips_the_rest_of_the_block() {
        let mut data = write(&[vec![5; 10]]);
        data.resize(BLOCK_BYTES, 0);
        data.extend(write(&[vec![6; 4]]));
        assert_eq!(read_all(&data).unwrap(), vec![vec![5; 10], vec![6; 4]]);
    }

    #[test]
    fn checksum_and_sequence_faults_are_errors() {
        let mut data = write(&[vec![9; 20]]);
        data[HEADER_BYTES + 3] ^= 1;
        assert!(matches!(read_all(&data), Err(Error::Checksum)));

        let mut orphan = Vec::new();
        physical(&mut orphan, RECORD_LAST, b"x");
        assert!(matches!(read_all(&orphan), Err(Error::Malformed(_))));

        let mut nested = Vec::new();
        physical(&mut nested, RECORD_FIRST, b"x");
        physical(&mut nested, RECORD_FULL, b"y");
        assert!(matches!(read_all(&nested), Err(Error::Malformed(_))));

        let mut unknown = Vec::new();
        physical(&mut unknown, 9, b"x");
        assert!(matches!(read_all(&unknown), Err(Error::Malformed(_))));

        // A length past the block end is not a truncated write.
        let mut long = Vec::new();
        physical(&mut long, RECORD_FULL, &[0; 100]);
        long[4..6].copy_from_slice(&u16::MAX.to_le_bytes());
        long.resize(2 * BLOCK_BYTES, 0);
        assert!(matches!(read_all(&long), Err(Error::Malformed(_))));
    }

    #[test]
    fn batches_give_each_write_its_sequence_number() {
        let writes = vec![
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), None),
            (b"c".to_vec(), Some(Vec::new())),
        ];
        let record = encode_batch(40, &writes);
        let mut seen = Vec::new();
        batch(&record, |w| {
            seen.push((w.key.to_vec(), w.sequence, w.value.map(<[u8]>::to_vec)));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (b"a".to_vec(), 40, Some(b"1".to_vec())),
                (b"b".to_vec(), 41, None),
                (b"c".to_vec(), 42, Some(Vec::new())),
            ]
        );
    }

    #[test]
    fn malformed_batches_are_errors() {
        let good = encode_batch(1, &[(b"k".to_vec(), Some(b"v".to_vec()))]);
        let ok = |r: &[u8]| batch(r, |_| Ok(()));
        assert!(ok(&good).is_ok());
        // Count larger, then smaller, than the records.
        let mut more = good.clone();
        more[8] = 2;
        assert!(ok(&more).is_err());
        let mut fewer = good.clone();
        fewer[8] = 0;
        assert!(matches!(ok(&fewer), Err(Error::Malformed(_))));
        let mut kind = good.clone();
        kind[12] = 7;
        assert!(matches!(ok(&kind), Err(Error::Malformed(_))));
        assert!(ok(&good[..5]).is_err());
        let long_key = encode_batch(1, &[(vec![0; MAX_KEY + 1], None)]);
        assert!(matches!(ok(&long_key), Err(Error::TooLarge { .. })));
        let overflow = encode_batch(u64::MAX, &[(b"a".to_vec(), None), (b"b".to_vec(), None)]);
        assert!(matches!(ok(&overflow), Err(Error::Malformed(_))));
    }

    /// A sequence number above 56 bits would lose its top bits in the internal key's tag, so a
    /// newer write would sort below an older table value.
    #[test]
    fn sequences_above_56_bits_are_errors() {
        let ok = |r: &[u8]| batch(r, |_| Ok(()));
        let last = encode_batch(MAX_SEQUENCE, &[(b"a".to_vec(), None)]);
        assert!(ok(&last).is_ok());
        let above = encode_batch(MAX_SEQUENCE + 1, &[(b"a".to_vec(), None)]);
        assert!(matches!(ok(&above), Err(Error::Malformed(_))));
        let crossing = encode_batch(
            MAX_SEQUENCE,
            &[(b"a".to_vec(), None), (b"b".to_vec(), None)],
        );
        assert!(matches!(ok(&crossing), Err(Error::Malformed(_))));
    }
}
