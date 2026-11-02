//! A LevelDB writer for the `db` folder of an exported Bedrock world, in the format the import's
//! reader reads (`import::leveldb` and its `table` and `log` modules).
//!
//! The database is written as one compacted whole. Every key is written once, with sequence
//! number 0: older than any write the game makes later, which LevelDB numbers from 1. Data
//! blocks ([`blocks`]) are made from runs of keys apart, so runs can be built and compressed on
//! the CPU pool: a run is cut into blocks of about [`DATA_BLOCK_BYTES`] with a restart every
//! [`RESTART_INTERVAL`] keys (LevelDB's default), compressed with raw DEFLATE (id 4, as the official
//! server writes its blocks). The [`Writer`] puts the blocks, in strictly increasing key
//! order, into tables of about [`TABLE_BYTES`] (LevelDB's default file size) with no filter.
//! Every table is in the last level, where tables are sorted and disjoint and none is ever picked
//! for a size compaction, so opening the database compacts nothing. The `MANIFEST` holds one
//! version edit (comparator, log number 0 for no log, next file number, last sequence 0, the
//! tables) and `CURRENT` names it. Files are numbered from 1: the tables in key order, then the
//! `MANIFEST`. Every file is created new, written whole and flushed to disk ([`write_new`]).

use std::io;
use std::path::PathBuf;

use super::{Result, write_new, written};
use crate::import::leveldb::log::{
    BLOCK_BYTES, HEADER_BYTES, RECORD_FIRST, RECORD_FULL, RECORD_LAST, RECORD_MIDDLE,
};
use crate::import::leveldb::table::{
    COMPRESSION_NONE, COMPRESSION_RAW_DEFLATE, FOOTER_BYTES, MAGIC, TRAILER_BYTES,
};
use crate::import::leveldb::{COMPARATOR, KIND_VALUE, LEVELS, TAG, masked_crc};
use crate::protocol::compression::zlib::Encoder;

/// The size a data block is closed at, before compression. The official server writes blocks of
/// over 100 KiB (a 106,535-byte one in `xtask/data/import/flat`).
pub(super) const DATA_BLOCK_BYTES: usize = 128 << 10;
/// The DEFLATE level blocks are compressed at: zlib's default.
const DEFLATE_LEVEL: u8 = 6;
/// The version edit tag of the comparator (google/leveldb `db/version_edit.cc`).
const EDIT_COMPARATOR: u8 = 1;
/// The version edit tag of the log number.
const EDIT_LOG_NUMBER: u8 = 2;
/// The version edit tag of the next file number.
const EDIT_NEXT_FILE: u8 = 3;
/// The version edit tag of the last sequence number.
const EDIT_LAST_SEQUENCE: u8 = 4;
/// The version edit tag of a new table.
const EDIT_NEW_TABLE: u8 = 7;
/// The tag of every key, little-endian `sequence << 8 | kind`: sequence 0, a value.
const KEY_TAG: [u8; TAG] = [KIND_VALUE, 0, 0, 0, 0, 0, 0, 0];
/// The level every table goes in: the last.
const LEVEL: usize = LEVELS - 1;
/// Keys between two restarts of a data block; index blocks restart at every key.
const RESTART_INTERVAL: usize = 16;
/// The size a table file is closed at.
const TABLE_BYTES: usize = 2 << 20;

/// A data block ready for a table: compressed, with its trailer.
#[derive(Debug)]
pub(super) struct Block {
    /// The first internal key.
    first: Vec<u8>,
    /// The last internal key.
    last: Vec<u8>,
    /// The stored block and its trailer.
    stored: Vec<u8>,
}

/// A block being built: entries whose keys share prefixes with the key before, and the offsets
/// of the restart entries, which share nothing.
#[derive(Debug, Default)]
struct BlockBuilder {
    data: Vec<u8>,
    restarts: Vec<u32>,
    entries: usize,
    last: Vec<u8>,
}

impl BlockBuilder {
    /// Adds `key` and `value`, restarting the prefix sharing every `interval` entries.
    fn add(&mut self, key: &[u8], value: &[u8], interval: usize) -> io::Result<()> {
        let shared = if self.entries.is_multiple_of(interval) {
            self.restarts.push(offset(self.data.len())?);
            0
        } else {
            self.last
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        };
        put_varint(&mut self.data, shared as u64);
        put_varint(&mut self.data, (key.len() - shared) as u64);
        put_varint(&mut self.data, value.len() as u64);
        self.data
            .extend_from_slice(key.get(shared..).unwrap_or_default());
        self.data.extend_from_slice(value);
        self.last.clear();
        self.last.extend_from_slice(key);
        self.entries += 1;
        Ok(())
    }

    /// Finishes the block and returns it: the entries, their restart offsets and the offsets'
    /// count; the builder is empty again.
    fn finish(&mut self) -> io::Result<Vec<u8>> {
        if self.restarts.is_empty() {
            self.restarts.push(0);
        }
        let mut data = std::mem::take(&mut self.data);
        for restart in &self.restarts {
            data.extend_from_slice(&restart.to_le_bytes());
        }
        data.extend_from_slice(&offset(self.restarts.len())?.to_le_bytes());
        *self = Self::default();
        Ok(data)
    }
}

/// A table written: what the `MANIFEST` lists of it.
#[derive(Debug)]
struct Table {
    number: u64,
    size: u64,
    smallest: Vec<u8>,
    largest: Vec<u8>,
}

/// A database being written into a new folder.
#[derive(Debug)]
pub(super) struct Writer {
    dir: PathBuf,
    tables: Vec<Table>,
    /// The blocks of the table being written.
    table: Vec<u8>,
    /// The index entries of its data blocks: the block's last key and its handle.
    index: Vec<(Vec<u8>, Vec<u8>)>,
    /// Its smallest key; `None` before its first block.
    smallest: Option<Vec<u8>>,
}

impl Writer {
    /// Creates a writer of the database in the folder `dir`, which must exist and be empty.
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            tables: Vec::new(),
            table: Vec::new(),
            index: Vec::new(),
            smallest: None,
        }
    }

    /// Adds `block`, whose keys come after those of every block added before.
    pub(super) fn add(&mut self, block: Block) -> Result<()> {
        let mut handle = Vec::new();
        put_varint(&mut handle, self.table.len() as u64);
        // The handle's size leaves out the trailer.
        put_varint(
            &mut handle,
            block.stored.len().saturating_sub(TRAILER_BYTES) as u64,
        );
        self.table.extend_from_slice(&block.stored);
        self.index.push((block.last, handle));
        self.smallest.get_or_insert(block.first);
        if self.table.len() >= TABLE_BYTES {
            self.finish_table()?;
        }
        Ok(())
    }

    /// Ends the database: the last table, the `MANIFEST` and `CURRENT`.
    pub(super) fn finish(mut self) -> Result<()> {
        self.finish_table()?;
        let manifest = self.tables.len() as u64 + 1;
        let mut edit = vec![EDIT_COMPARATOR];
        put_prefixed(&mut edit, COMPARATOR);
        edit.push(EDIT_LOG_NUMBER);
        put_varint(&mut edit, 0);
        edit.push(EDIT_NEXT_FILE);
        put_varint(&mut edit, manifest + 1);
        edit.push(EDIT_LAST_SEQUENCE);
        put_varint(&mut edit, 0);
        for table in &self.tables {
            edit.push(EDIT_NEW_TABLE);
            put_varint(&mut edit, LEVEL as u64);
            put_varint(&mut edit, table.number);
            put_varint(&mut edit, table.size);
            put_prefixed(&mut edit, &table.smallest);
            put_prefixed(&mut edit, &table.largest);
        }
        let name = format!("MANIFEST-{manifest:06}");
        write_new(&self.dir.join(&name), &log_record(&edit))?;
        write_new(&self.dir.join("CURRENT"), format!("{name}\n").as_bytes())
    }

    /// Ends the table being written, if it has blocks: an empty metaindex block, the index block
    /// and the footer, written to its file.
    fn finish_table(&mut self) -> Result<()> {
        let Some(smallest) = self.smallest.take() else {
            return Ok(());
        };
        let number = self.tables.len() as u64 + 1;
        let path = self.dir.join(format!("{number:06}.ldb"));
        let mut index = BlockBuilder::default();
        for (key, handle) in &self.index {
            index.add(key, handle, 1).map_err(written(&path))?;
        }
        // Taken, not copied: finishing the index block empties the builder.
        let largest = std::mem::take(&mut index.last);
        let mut footer = Vec::with_capacity(FOOTER_BYTES);
        let blocks = [
            seal(
                &BlockBuilder::default().finish().map_err(written(&path))?,
                None,
            ),
            seal(
                &index.finish().map_err(written(&path))?,
                Some(&mut Encoder::new(DEFLATE_LEVEL)),
            ),
        ];
        for block in blocks {
            put_varint(&mut footer, self.table.len() as u64);
            put_varint(
                &mut footer,
                block.len().saturating_sub(TRAILER_BYTES) as u64,
            );
            self.table.extend_from_slice(&block);
        }
        footer.resize(FOOTER_BYTES - 8, 0);
        footer.extend_from_slice(&MAGIC.to_le_bytes());
        self.table.extend_from_slice(&footer);

        write_new(&path, &self.table)?;
        self.tables.push(Table {
            number,
            size: self.table.len() as u64,
            smallest,
            largest,
        });
        // Cleared, not dropped: the next table reuses the buffer.
        self.table.clear();
        self.index.clear();
        Ok(())
    }
}

/// Returns the data blocks of the keys and values `entries` (in strictly increasing key order).
pub(super) fn blocks(entries: &[(Vec<u8>, Vec<u8>)]) -> io::Result<Vec<Block>> {
    let mut blocks = Vec::new();
    let mut builder = BlockBuilder::default();
    let mut first = Vec::new();
    // One buffer for every internal key: each is copied into the block as it is added.
    let mut internal = Vec::new();
    let mut deflate = None;
    for (i, (key, value)) in entries.iter().enumerate() {
        internal.clear();
        internal.extend_from_slice(key);
        internal.extend_from_slice(&KEY_TAG);
        if builder.entries == 0 {
            first.clone_from(&internal);
        }
        builder.add(&internal, value, RESTART_INTERVAL)?;
        if builder.data.len() >= DATA_BLOCK_BYTES || i + 1 == entries.len() {
            // Taken, not copied: finishing the block empties the builder.
            let last = std::mem::take(&mut builder.last);
            let raw = builder.finish()?;
            let encoder = deflate.get_or_insert_with(|| Encoder::new(DEFLATE_LEVEL));
            blocks.push(Block {
                first: std::mem::take(&mut first),
                last,
                stored: seal(&raw, Some(encoder)),
            });
        }
    }
    Ok(blocks)
}

/// Returns `raw` as a table stores it: compressed with raw DEFLATE by `deflate`, or as it is
/// without, then the compression byte and the masked CRC-32C of the stored block and that byte.
fn seal(raw: &[u8], deflate: Option<&mut Encoder>) -> Vec<u8> {
    // Sized once: compressing first grows the output to the compressor's worst-case bound.
    let (mut stored, kind) = if let Some(encoder) = deflate {
        let mut stored = Vec::with_capacity(encoder.max_compressed_len(raw.len()) + TRAILER_BYTES);
        encoder.compress(raw, &mut stored);
        (stored, COMPRESSION_RAW_DEFLATE)
    } else {
        let mut stored = Vec::with_capacity(raw.len() + TRAILER_BYTES);
        stored.extend_from_slice(raw);
        (stored, COMPRESSION_NONE)
    };
    let crc = masked_crc(&[&stored, &[kind]]);
    stored.push(kind);
    stored.extend_from_slice(&crc.to_le_bytes());
    stored
}

/// Converts a block offset or count to the `u32` blocks store.
fn offset(n: usize) -> io::Result<u32> {
    u32::try_from(n).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a block over 4 GiB"))
}

/// Appends `bytes` with a varint length before them.
fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Appends `value` as a LevelDB varint.
fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let [low, ..] = value.to_le_bytes();
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Returns `record` as a log file: physical records of at most what is left of each 32 KiB block, a
/// block's last 6 bytes or fewer zero padding, as LevelDB's `log::Writer` cuts them.
fn log_record(record: &[u8]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(record.len() + record.len() / BLOCK_BYTES * HEADER_BYTES + HEADER_BYTES);
    let mut rest = record;
    let mut first = true;
    loop {
        let left = BLOCK_BYTES - out.len() % BLOCK_BYTES;
        if left < HEADER_BYTES {
            out.resize(out.len() + left, 0);
            continue;
        }
        let (fragment, after) = rest.split_at(rest.len().min(left - HEADER_BYTES));
        let kind = match (first, after.is_empty()) {
            (true, true) => RECORD_FULL,
            (true, false) => RECORD_FIRST,
            (false, false) => RECORD_MIDDLE,
            (false, true) => RECORD_LAST,
        };
        out.extend_from_slice(&masked_crc(&[&[kind], fragment]).to_le_bytes());
        // A fragment is at most a block less its header: its length fits the two low bytes.
        let [l0, l1, ..] = fragment.len().to_le_bytes();
        out.extend_from_slice(&[l0, l1]);
        out.push(kind);
        out.extend_from_slice(fragment);
        if after.is_empty() {
            return out;
        }
        rest = after;
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::ops::ControlFlow;

    use crate::import::leveldb::log::records;
    use crate::import::leveldb::{Db, Files};
    use crate::protocol::Rng;
    use crate::save::tests::TempDir;

    /// Returns `len` bytes of which runs repeat, so blocks compress some.
    fn value(rng: &mut Rng, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let byte = rng.next().to_le_bytes()[0];
            out.extend(std::iter::repeat_n(byte, 1 + rng.below(8)));
        }
        out.truncate(len);
        out
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn written_databases_read_back_with_the_import_reader() {
        let dir = TempDir::new("export-leveldb");
        let mut rng = Rng(0xe790_1d8b);
        let mut model = BTreeMap::new();
        while model.len() < 2000 {
            let key: Vec<u8> = (0..=rng.below(12))
                .map(|_| rng.next().to_le_bytes()[0])
                .collect();
            // Some values larger than a block.
            let len = if rng.below(40) == 0 {
                DATA_BLOCK_BYTES + rng.below(DATA_BLOCK_BYTES)
            } else {
                rng.below(6000)
            };
            model.insert(key, value(&mut rng, len));
        }
        let entries: Vec<(Vec<u8>, Vec<u8>)> = model.clone().into_iter().collect();
        let mut writer = Writer::new(dir.0.clone());
        // Runs of a few keys to many blocks' worth.
        let mut rest = entries.as_slice();
        while !rest.is_empty() {
            let (run, after) = rest.split_at(rest.len().min(1 + rng.below(100)));
            for block in blocks(run).unwrap() {
                writer.add(block).unwrap();
            }
            rest = after;
        }
        writer.finish().unwrap();
        let tables = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_string_lossy().ends_with(".ldb")
            })
            .count();
        assert!(tables > 2, "{tables} tables");

        let mut db = Db::from_files(Files::disk(dir.0.clone())).unwrap();
        let mut read = BTreeMap::new();
        db.scan(&[], None, |key, value| {
            read.insert(key.to_vec(), value.to_vec());
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(read, model);
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn an_empty_database_opens() {
        let dir = TempDir::new("export-leveldb-empty");
        assert!(blocks(&[]).unwrap().is_empty());
        Writer::new(dir.0.clone()).finish().unwrap();
        let mut db = Db::from_files(Files::disk(dir.0.clone())).unwrap();
        db.scan(&[], None, |_, _| panic!("a key in an empty database"))
            .unwrap();
        assert_eq!(
            std::fs::read(dir.0.join("CURRENT")).unwrap(),
            b"MANIFEST-000001\n"
        );
    }

    #[test]
    fn log_records_span_blocks_and_read_back_whole() {
        for len in [
            0,
            1,
            BLOCK_BYTES - HEADER_BYTES,
            BLOCK_BYTES - HEADER_BYTES + 1,
            BLOCK_BYTES - 3,
            3 * BLOCK_BYTES + 17,
        ] {
            let record: Vec<u8> = (0..len).map(|i| (i % 251).to_le_bytes()[0]).collect();
            let log = log_record(&record);
            let mut read = Vec::new();
            records(&log, |r| {
                read.push(r.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(read, [record], "{len} bytes");
        }
    }
}
