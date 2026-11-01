//! LevelDB sorted tables (`.ldb`, older `.sst`) and their blocks.
//!
//! Format (google/leveldb `doc/table_format.md`, `table/format.cc`, `table/block.cc`; Mojang's
//! compression ids from its `leveldb-mcpe` fork; checked on BDS 1.26.52 output, which uses id 4
//! for data and index blocks): the last 48 bytes are the footer, two block handles
//! (metaindex, index; varint64 offset and size each), zero padding and the magic
//! `0xdb4775248b80fb57` (little-endian). Each block is followed by a compression byte and the
//! masked CRC-32C of the block and that byte. Compression: 0 none, 1 Snappy, 2 zlib with its
//! header, 4 raw DEFLATE; 3 (zstd) is refused. A block is entries (varint32 shared key bytes,
//! varint32 unshared key bytes, varint32 value length, the unshared key bytes, the value), then
//! the `u32` offsets of the restart entries (which share nothing) and their `u32` count. Index
//! block values are the handles of the data blocks, keyed by a key at or after the block's last.
//! The metaindex and filter blocks are not read: a filter only speeds up misses.
//!
//! Every key in a table is an internal key: the user key and a little-endian `u64` holding
//! `sequence << 8 | kind`. Keys must strictly increase in internal key order within a block;
//! a decoded block that breaks this is malformed. An error inside a table names its file.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use super::{Bytes, Files, MAX_KEY, TAG, compare, masked_crc};
use crate::import::{Error, Result};
use crate::protocol::compression::zlib::{Decoder, Framing};
use crate::protocol::compression::{Compression, Decompressor};

/// The footer at the end of every table.
pub(crate) const FOOTER: usize = 48;
pub(crate) const MAGIC: u64 = 0xdb47_7524_8b80_fb57;
/// The compression byte and checksum after each block.
pub(crate) const TRAILER: usize = 5;
/// The largest block, stored or decompressed, which bounds the memory one block read takes.
pub(super) const MAX_BLOCK: usize = 16 << 20;
/// Decompressed blocks kept for later reads, in bytes.
const CACHE_BYTES: usize = 64 << 20;
/// Table files kept open between reads.
pub(super) const MAX_OPEN: usize = 64;
/// The longest internal key: a user key and its tag.
const MAX_INTERNAL_KEY: usize = MAX_KEY + TAG;

/// Block compression ids.
pub(crate) const NONE: u8 = 0;
const SNAPPY: u8 = 1;
const ZLIB: u8 = 2;
pub(crate) const RAW_DEFLATE: u8 = 4;

/// A table of the current version, from the `MANIFEST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableFile {
    /// The file number: the file is `<number>.ldb` or `<number>.sst`.
    pub(super) number: u64,
    /// The file size the `MANIFEST` records.
    pub(super) size: u64,
    /// The smallest internal key in the table.
    pub(super) smallest: Vec<u8>,
    /// The largest internal key in the table.
    pub(super) largest: Vec<u8>,
}

/// A position in a table: the index block and the data block it points into.
#[derive(Debug)]
pub(super) struct Cursor {
    index: BlockCursor,
    data: Option<BlockCursor>,
}

impl Cursor {
    /// A cursor on the first entry of `file` at or after `target`, or on the first entry.
    pub(super) fn open(
        tables: &mut Tables,
        file: &TableFile,
        target: Option<&[u8]>,
    ) -> Result<Self> {
        let mut index = BlockCursor::new(tables.index(file)?);
        let sought = match target {
            Some(target) => index.seek(target),
            None => index.seek_first(),
        };
        sought.map_err(|e| tables.fault(file, e))?;
        let mut cursor = Self { index, data: None };
        cursor.load(tables, file, target)?;
        Ok(cursor)
    }

    /// The current internal key and value.
    pub(super) fn entry(&self) -> Option<(&[u8], &[u8])> {
        self.data.as_ref()?.entry()
    }

    /// The decompressed bytes this cursor keeps alive.
    pub(super) fn held(&self) -> usize {
        self.index.block().held() + self.data.as_ref().map_or(0, |d| d.block().held())
    }

    /// Moves to the next entry.
    pub(super) fn advance(&mut self, tables: &mut Tables, file: &TableFile) -> Result<()> {
        let Some(data) = self.data.as_mut() else {
            return Ok(());
        };
        data.advance().map_err(|e| tables.fault(file, e))?;
        if data.entry().is_none() {
            self.index.advance().map_err(|e| tables.fault(file, e))?;
            self.load(tables, file, None)?;
        }
        Ok(())
    }

    /// Loads the data block under the index cursor, moving on past empty blocks and blocks
    /// whose entries are all before `target`.
    fn load(&mut self, tables: &mut Tables, file: &TableFile, target: Option<&[u8]>) -> Result<()> {
        self.data = None;
        while let Some((_, value)) = self.index.entry() {
            let handle = Handle::decode(value).map_err(|e| tables.fault(file, e))?;
            let mut data = BlockCursor::new(tables.block(file, handle)?);
            let sought = match target {
                Some(target) => data.seek(target),
                None => data.seek_first(),
            };
            sought.map_err(|e| tables.fault(file, e))?;
            if data.entry().is_some() {
                self.data = Some(data);
                return Ok(());
            }
            self.index.advance().map_err(|e| tables.fault(file, e))?;
        }
        Ok(())
    }
}

/// Reads table blocks through a small cache of decompressed blocks.
pub(super) struct Tables {
    pub(super) files: Files,
    /// File names by number.
    names: BTreeMap<u64, String>,
    /// The index handle of each table whose footer was read.
    indexes: BTreeMap<u64, Handle>,
    cache: Cache,
    inflate: Decoder,
    raw: Vec<u8>,
}

impl Tables {
    pub(super) fn new(files: Files, names: BTreeMap<u64, String>) -> Self {
        Self {
            files,
            names,
            indexes: BTreeMap::new(),
            cache: Cache::default(),
            inflate: Decoder::new(),
            raw: Vec::new(),
        }
    }

    fn name(&self, file: &TableFile) -> Result<&str> {
        self.names
            .get(&file.number)
            .map(String::as_str)
            .ok_or(Error::Malformed("table file missing"))
    }

    /// The index block of `file`.
    fn index(&mut self, file: &TableFile) -> Result<Arc<Block>> {
        if let Some(&handle) = self.indexes.get(&file.number) {
            return self.block(file, handle);
        }
        let at = file
            .size
            .checked_sub(FOOTER as u64)
            .ok_or_else(|| self.fault(file, Error::Malformed("table shorter than its footer")))?;
        let mut tail = [0; FOOTER];
        let name = self.name(file)?.to_owned();
        self.files.read_at(&name, at, &mut tail)?;
        let handle = footer(&tail).map_err(|e| self.fault(file, e))?;
        self.indexes.insert(file.number, handle);
        self.block(file, handle)
    }

    /// The block of `file` at `handle`.
    fn block(&mut self, file: &TableFile, handle: Handle) -> Result<Arc<Block>> {
        let key = (file.number, handle.offset);
        if let Some(block) = self.cache.get(key) {
            return Ok(block);
        }
        let size = usize::try_from(handle.size)
            .ok()
            .filter(|&size| size <= MAX_BLOCK)
            .ok_or_else(|| {
                self.fault(
                    file,
                    Error::TooLarge {
                        what: "bytes in a block",
                        limit: MAX_BLOCK as u64,
                    },
                )
            })?;
        let len = size + TRAILER;
        let end = handle.offset.checked_add(len as u64);
        if end.is_none_or(|end| end > file.size) {
            return Err(self.fault(file, Error::Malformed("block outside the table")));
        }
        let name = self.name(file)?.to_owned();
        let mut raw = std::mem::take(&mut self.raw);
        raw.clear();
        raw.resize(len, 0);
        let read = self.files.read_at(&name, handle.offset, &mut raw);
        let unsealed = read.map(|()| unseal(&raw, &mut self.inflate));
        self.raw = raw;
        let block = Arc::new(unsealed?.map_err(|e| self.fault(file, e))?);
        self.cache.insert(key, Arc::clone(&block));
        Ok(block)
    }

    /// `error` located in the table `file`.
    fn fault(&self, file: &TableFile, error: Error) -> Error {
        let name = self.names.get(&file.number).map_or("", String::as_str);
        self.files.fault(name, error)
    }
}

/// Decompressed blocks by (file number, offset), least recently used evicted first once they
/// hold more than [`CACHE_BYTES`].
#[derive(Default)]
struct Cache {
    blocks: BTreeMap<(u64, u64), (Arc<Block>, u64)>,
    ages: BTreeMap<u64, (u64, u64)>,
    bytes: usize,
    clock: u64,
}

impl Cache {
    fn get(&mut self, key: (u64, u64)) -> Option<Arc<Block>> {
        let (block, age) = self.blocks.get_mut(&key)?;
        self.ages.remove(age);
        self.clock += 1;
        *age = self.clock;
        self.ages.insert(self.clock, key);
        Some(Arc::clone(block))
    }

    fn insert(&mut self, key: (u64, u64), block: Arc<Block>) {
        self.clock += 1;
        self.bytes = self.bytes.saturating_add(block.held());
        self.ages.insert(self.clock, key);
        if let Some((old, age)) = self.blocks.insert(key, (block, self.clock)) {
            self.ages.remove(&age);
            self.bytes = self.bytes.saturating_sub(old.held());
        }
        while self.bytes > CACHE_BYTES && self.ages.len() > 1 {
            let Some((_, oldest)) = self.ages.pop_first() else {
                break;
            };
            if let Some((old, _)) = self.blocks.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(old.held());
            }
        }
    }
}

/// A position in a block.
#[derive(Debug)]
pub(super) struct BlockCursor {
    block: Arc<Block>,
    /// The offset of the entry after the current one.
    next: usize,
    key: Vec<u8>,
    value: Range<usize>,
    valid: bool,
    scratch: Vec<u8>,
}

impl BlockCursor {
    pub(super) fn new(block: Arc<Block>) -> Self {
        Self {
            block,
            next: 0,
            key: Vec::new(),
            value: 0..0,
            valid: false,
            scratch: Vec::new(),
        }
    }

    pub(super) fn block(&self) -> &Block {
        &self.block
    }

    /// The current internal key and value, if the cursor is on an entry.
    pub(super) fn entry(&self) -> Option<(&[u8], &[u8])> {
        if !self.valid {
            return None;
        }
        Some((
            &self.key,
            self.block.data.get(self.value.clone()).unwrap_or_default(),
        ))
    }

    /// Moves to the first entry.
    pub(super) fn seek_first(&mut self) -> Result<()> {
        self.start_at(0)
    }

    /// Moves to the first entry at or after `target` (an internal key).
    pub(super) fn seek(&mut self, target: &[u8]) -> Result<()> {
        // The first restart whose key is at or after the target; the entry sought is in the
        // run before it.
        let (mut left, mut right) = (0, self.block.count);
        while left < right {
            let mid = left + (right - left) / 2;
            let offset = self.block.restart(mid)?;
            let mut key = std::mem::take(&mut self.scratch);
            let result = self.block.entry(offset, &[], &mut key);
            self.scratch = key;
            result?;
            if compare(&self.scratch, target) == Ordering::Less {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        let start = match left.checked_sub(1) {
            Some(run) => self.block.restart(run)?,
            None => 0,
        };
        self.start_at(start)?;
        while self.valid && compare(&self.key, target) == Ordering::Less {
            self.advance()?;
        }
        Ok(())
    }

    /// Moves to the next entry; past the last one the cursor is on none.
    pub(super) fn advance(&mut self) -> Result<()> {
        if self.next >= self.block.restarts {
            self.valid = false;
            return Ok(());
        }
        let (value, next) = self.block.entry(self.next, &self.key, &mut self.scratch)?;
        if self.valid && compare(&self.scratch, &self.key) != Ordering::Greater {
            return Err(Error::Malformed("block keys out of order"));
        }
        std::mem::swap(&mut self.key, &mut self.scratch);
        self.value = value;
        self.next = next;
        self.valid = true;
        Ok(())
    }

    fn start_at(&mut self, offset: usize) -> Result<()> {
        self.next = offset;
        self.key.clear();
        self.valid = false;
        self.advance()
    }
}

/// A decompressed block: entries, then restart offsets and their count.
#[derive(Debug)]
pub(super) struct Block {
    data: Vec<u8>,
    /// Where the restart offsets start, which is where the entries end.
    restarts: usize,
    /// How many restart offsets there are.
    count: usize,
}

impl Block {
    pub(super) fn parse(data: Vec<u8>) -> Result<Self> {
        let count = data
            .last_chunk::<4>()
            .map(|c| u32::from_le_bytes(*c))
            .ok_or(Error::Malformed("block shorter than its restart count"))?;
        let count = usize::try_from(count).map_err(|_| Error::Malformed("block restart count"))?;
        let restarts = count
            .checked_mul(4)
            .and_then(|n| data.len().checked_sub(n)?.checked_sub(4))
            .ok_or(Error::Malformed("block shorter than its restart offsets"))?;
        if count == 0 && restarts != 0 {
            return Err(Error::Malformed("block entries without a restart"));
        }
        Ok(Self {
            data,
            restarts,
            count,
        })
    }

    /// The memory the block holds, which the cache and the scan count against their bounds: its
    /// buffer's capacity, which a decompressor sizes from the input and may leave above the data.
    pub(super) fn held(&self) -> usize {
        self.data.capacity()
    }

    fn restart(&self, i: usize) -> Result<usize> {
        let at = self.restarts.saturating_add(i.saturating_mul(4));
        let offset = self
            .data
            .get(at..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map(|c| u32::from_le_bytes(*c))
            .ok_or(Error::Malformed("block restart index"))?;
        let offset = usize::try_from(offset).map_err(|_| Error::Malformed("block restart"))?;
        if offset >= self.restarts {
            return Err(Error::Malformed("block restart outside the entries"));
        }
        Ok(offset)
    }

    /// Decodes the entry at `offset` whose key shares a prefix with `previous`: writes its key
    /// into `key` and returns the value range and the next entry's offset.
    fn entry(
        &self,
        offset: usize,
        previous: &[u8],
        key: &mut Vec<u8>,
    ) -> Result<(Range<usize>, usize)> {
        let entries = self.data.get(..self.restarts).unwrap_or_default();
        let mut bytes = Bytes(entries.get(offset..).unwrap_or_default());
        let shared =
            usize::try_from(bytes.varint32()?).map_err(|_| Error::Malformed("key length"))?;
        let unshared =
            usize::try_from(bytes.varint32()?).map_err(|_| Error::Malformed("key length"))?;
        let value_len =
            usize::try_from(bytes.varint32()?).map_err(|_| Error::Malformed("value length"))?;
        let prefix = previous.get(..shared).ok_or(Error::Malformed(
            "block key shares more than the previous key",
        ))?;
        if shared.saturating_add(unshared) > MAX_INTERNAL_KEY {
            return Err(Error::TooLarge {
                what: "bytes in a user key",
                limit: MAX_KEY as u64,
            });
        }
        let delta = bytes.take(unshared)?;
        key.clear();
        key.extend_from_slice(prefix);
        key.extend_from_slice(delta);
        check_internal(key)?;
        let start = self.restarts - bytes.0.len();
        let value = start..start.saturating_add(value_len);
        bytes.take(value_len)?;
        Ok((value, self.restarts - bytes.0.len()))
    }
}

/// Where a block is in its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Handle {
    offset: u64,
    size: u64,
}

impl Handle {
    fn read(bytes: &mut Bytes<'_>) -> Result<Self> {
        Ok(Self {
            offset: bytes.varint64()?,
            size: bytes.varint64()?,
        })
    }

    /// A handle that is a whole index entry value.
    fn decode(value: &[u8]) -> Result<Self> {
        let mut bytes = Bytes(value);
        let handle = Self::read(&mut bytes)?;
        if !bytes.0.is_empty() {
            return Err(Error::Malformed("block handle has trailing bytes"));
        }
        Ok(handle)
    }
}

/// The index block handle in a table's footer.
pub(super) fn footer(tail: &[u8]) -> Result<Handle> {
    let (handles, magic) = tail
        .split_last_chunk::<8>()
        .ok_or(Error::Malformed("table shorter than its footer"))?;
    if u64::from_le_bytes(*magic) != MAGIC {
        return Err(Error::Malformed("table footer magic"));
    }
    let mut bytes = Bytes(handles);
    Handle::read(&mut bytes)?;
    Handle::read(&mut bytes)
}

/// Checks a stored block's trailer and decompresses it. `raw` is the block and its trailer.
pub(super) fn unseal(raw: &[u8], inflate: &mut Decoder) -> Result<Block> {
    let (sealed, crc) = raw
        .split_last_chunk::<4>()
        .ok_or(Error::Malformed("block shorter than its trailer"))?;
    if masked_crc(&[sealed]) != u32::from_le_bytes(*crc) {
        return Err(Error::Checksum);
    }
    let (&kind, body) = sealed
        .split_last()
        .ok_or(Error::Malformed("block shorter than its trailer"))?;
    let mut out = Vec::new();
    let decoded = match kind {
        NONE => {
            out.extend_from_slice(body);
            Ok(())
        }
        SNAPPY => Decompressor::new(Compression::Snappy).decompress(body, &mut out, MAX_BLOCK),
        ZLIB => inflate.decompress_whole(body, &mut out, MAX_BLOCK, Framing::Zlib),
        RAW_DEFLATE => inflate.decompress_whole(body, &mut out, MAX_BLOCK, Framing::Raw),
        _ => return Err(Error::UnknownCompression(kind)),
    };
    decoded.map_err(Error::Compression)?;
    Block::parse(out)
}

/// Checks that `key` is an internal key: at least a tag long, with kind 0 or 1.
pub(super) fn check_internal(key: &[u8]) -> Result<()> {
    match key.len().checked_sub(TAG).and_then(|at| key.get(at)) {
        Some(&kind) if kind <= 1 => Ok(()),
        Some(_) => Err(Error::Malformed("internal key kind")),
        None => Err(Error::Malformed("internal key shorter than its tag")),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::import::leveldb::tests::{internal, put_varint};

    /// Encodes sorted entries as a block with a restart every `interval` entries.
    pub(crate) fn encode_block(entries: &[(Vec<u8>, Vec<u8>)], interval: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut restarts = Vec::new();
        let mut previous: &[u8] = &[];
        for (i, (key, value)) in entries.iter().enumerate() {
            let shared = if i % interval == 0 {
                restarts.push(u32::try_from(out.len()).unwrap());
                0
            } else {
                previous.iter().zip(key).take_while(|(a, b)| a == b).count()
            };
            put_varint(&mut out, shared as u64);
            put_varint(&mut out, (key.len() - shared) as u64);
            put_varint(&mut out, value.len() as u64);
            out.extend_from_slice(&key[shared..]);
            out.extend_from_slice(value);
            previous = key;
        }
        if restarts.is_empty() {
            restarts.push(0);
        }
        for r in &restarts {
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&u32::try_from(restarts.len()).unwrap().to_le_bytes());
        out
    }

    /// Compresses a block with compression `kind` and appends it and its trailer to `out`.
    pub(crate) fn seal_block(out: &mut Vec<u8>, block: &[u8], kind: u8) -> Vec<u8> {
        let start = out.len();
        let body = match kind {
            NONE => block.to_vec(),
            SNAPPY => {
                let mut body = Vec::new();
                crate::protocol::compression::Compressor::new(Compression::Snappy, 6)
                    .compress(block, &mut body, usize::MAX)
                    .unwrap();
                body
            }
            ZLIB => {
                let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());
                let mut body = vec![0; c.zlib_compress_bound(block.len())];
                let n = c.zlib_compress(block, &mut body).unwrap();
                body.truncate(n);
                body
            }
            _ => {
                let mut body = Vec::new();
                crate::protocol::compression::Compressor::new(Compression::Zlib, 6)
                    .compress(block, &mut body, usize::MAX)
                    .unwrap();
                body
            }
        };
        let mut handle = Vec::new();
        put_varint(&mut handle, start as u64);
        put_varint(&mut handle, body.len() as u64);
        out.extend_from_slice(&body);
        out.push(kind);
        let crc = masked_crc(&[&body, &[kind]]);
        out.extend_from_slice(&crc.to_le_bytes());
        handle
    }

    /// A table of sorted internal-key entries in data blocks of about `block_size` bytes.
    pub(crate) fn encode_table(
        entries: &[(Vec<u8>, Vec<u8>)],
        block_size: usize,
        interval: usize,
        kinds: &mut impl FnMut() -> u8,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut index = Vec::new();
        let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut pending_size = 0;
        let mut flush = |out: &mut Vec<u8>, pending: &mut Vec<(Vec<u8>, Vec<u8>)>| {
            if let Some((last, _)) = pending.last() {
                let last = last.clone();
                let handle = seal_block(out, &encode_block(pending, interval), kinds());
                index.push((last, handle));
                pending.clear();
            }
        };
        for (key, value) in entries {
            pending.push((key.clone(), value.clone()));
            pending_size += key.len() + value.len();
            if pending_size >= block_size {
                flush(&mut out, &mut pending);
                pending_size = 0;
            }
        }
        flush(&mut out, &mut pending);
        let meta = seal_block(&mut out, &encode_block(&[], 1), NONE);
        let index_handle = seal_block(&mut out, &encode_block(&index, 1), kinds());
        let mut footer = meta;
        footer.extend_from_slice(&index_handle);
        footer.resize(FOOTER - 8, 0);
        footer.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&footer);
        out
    }

    fn entries(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let key = internal(format!("key{i:05}").as_bytes(), 100 + i as u64, 1);
                (key, vec![u8::try_from(i % 251).unwrap(); i % 7])
            })
            .collect()
    }

    fn cursor(block: &[u8]) -> BlockCursor {
        BlockCursor::new(Arc::new(Block::parse(block.to_vec()).unwrap()))
    }

    #[test]
    fn block_cursors_walk_and_seek_every_restart_interval() {
        let entries = entries(100);
        for interval in [1, 3, 16, 200] {
            let mut c = cursor(&encode_block(&entries, interval));
            c.seek_first().unwrap();
            for (key, value) in &entries {
                assert_eq!(c.entry(), Some((key.as_slice(), value.as_slice())));
                c.advance().unwrap();
            }
            assert_eq!(c.entry(), None);
            for (i, (key, _)) in entries.iter().enumerate() {
                c.seek(key).unwrap();
                assert_eq!(
                    c.entry().unwrap().0,
                    key.as_slice(),
                    "interval {interval} at {i}"
                );
            }
            c.seek(&internal(b"key", u64::MAX >> 8, 1)).unwrap();
            assert_eq!(c.entry().unwrap().0, entries[0].0.as_slice());
            c.seek(&internal(b"z", 0, 1)).unwrap();
            assert_eq!(c.entry(), None);
        }
    }

    #[test]
    fn a_seek_between_versions_of_a_key_finds_the_newer_one_first() {
        let entries = vec![
            (internal(b"a", 9, 1), b"new".to_vec()),
            (internal(b"a", 3, 0), Vec::new()),
            (internal(b"b", 5, 1), b"b".to_vec()),
        ];
        let mut c = cursor(&encode_block(&entries, 2));
        c.seek(&internal(b"a", u64::MAX >> 8, 1)).unwrap();
        assert_eq!(c.entry().unwrap().1, b"new");
        c.seek(&internal(b"a", 4, 1)).unwrap();
        assert_eq!(c.entry().unwrap().0, entries[1].0.as_slice());
    }

    #[test]
    fn malformed_blocks_are_errors_not_panics() {
        let good = encode_block(&entries(10), 4);
        assert!(Block::parse(vec![1, 2]).is_err());
        assert!(Block::parse(vec![0xff, 0xff, 0xff, 0x0f]).is_err());
        assert!(Block::parse(vec![9, 0, 0, 0, 0]).is_err());
        // Keys out of order.
        let swapped = encode_block(&[entries(2)[1].clone(), entries(2)[0].clone()], 16);
        let mut c = cursor(&swapped);
        c.seek_first().unwrap();
        assert!(matches!(c.advance(), Err(Error::Malformed(_))));
        // A key kind other than 0 or 1, a key shorter than a tag.
        let mut bad_kind = entries(1);
        let at = bad_kind[0].0.len() - TAG;
        bad_kind[0].0[at] = 2;
        assert!(cursor(&encode_block(&bad_kind, 1)).seek_first().is_err());
        assert!(
            cursor(&encode_block(&[(vec![1; 7], vec![])], 1))
                .seek_first()
                .is_err()
        );
        // Over-long key.
        let long = vec![(internal(&vec![b'k'; MAX_KEY + 1], 1, 1), vec![])];
        assert!(matches!(
            cursor(&encode_block(&long, 1)).seek_first(),
            Err(Error::TooLarge { .. })
        ));
        crate::protocol::fuzz(&good, 10_000, |data| {
            let Ok(block) = Block::parse(data.to_vec()) else {
                return Ok(());
            };
            let mut c = BlockCursor::new(Arc::new(block));
            let walk = (|| {
                c.seek_first()?;
                while c.entry().is_some() {
                    c.advance()?;
                }
                c.seek(&internal(b"key00005", u64::MAX >> 8, 1))
            })();
            walk.map_err(|_| crate::protocol::Error::Size)
        });
    }

    #[test]
    fn every_compression_unseals_and_zstd_is_refused() {
        let block = encode_block(&entries(50), 16);
        let mut inflate = Decoder::new();
        for kind in [NONE, SNAPPY, ZLIB, RAW_DEFLATE] {
            let mut out = Vec::new();
            seal_block(&mut out, &block, kind);
            let unsealed = unseal(&out, &mut inflate).unwrap();
            assert_eq!(unsealed.data, block, "kind {kind}");
            out[0] ^= 1;
            assert!(matches!(unseal(&out, &mut inflate), Err(Error::Checksum)));
        }
        let mut zstd = block.clone();
        zstd.push(3);
        let crc = masked_crc(&[&zstd]);
        zstd.extend_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            unseal(&zstd, &mut inflate),
            Err(Error::UnknownCompression(3))
        ));
    }

    #[test]
    fn a_block_counts_the_memory_its_buffer_holds() {
        // An empty block: the decompressor's buffer, sized from the input, outgrows its 8 bytes.
        let block = encode_block(&[], 1);
        let mut out = Vec::new();
        seal_block(&mut out, &block, RAW_DEFLATE);
        let unsealed = unseal(&out, &mut Decoder::new()).unwrap();
        assert_eq!(unsealed.data, block);
        assert_eq!(unsealed.held(), unsealed.data.capacity());
        assert!(unsealed.held() > block.len());
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_block_first() {
        let sized = |len: usize| {
            let mut data = vec![0; len];
            data[len - 4..].copy_from_slice(&1_u32.to_le_bytes());
            Arc::new(Block::parse(data).unwrap())
        };
        let big = || sized(CACHE_BYTES / 2);
        let mut cache = Cache::default();
        cache.insert((1, 0), big());
        cache.insert((2, 0), big());
        assert!(cache.get((1, 0)).is_some());
        cache.insert((3, 0), big());
        assert!(cache.get((2, 0)).is_none());
        assert!(cache.get((1, 0)).is_some());
        assert!(cache.get((3, 0)).is_some());
        assert!(cache.bytes <= CACHE_BYTES);
        // One block over the bound stays until the next insert.
        cache.insert((4, 0), sized(CACHE_BYTES + 8));
        assert_eq!(cache.ages.len(), 1);
        assert!(cache.get((4, 0)).is_some());
    }

    #[test]
    fn footers_name_the_index_block() {
        let mut kinds = || RAW_DEFLATE;
        let table = encode_table(&entries(40), 256, 4, &mut kinds);
        let handle = footer(&table[table.len() - FOOTER..]).unwrap();
        assert!(handle.offset > 0 && handle.offset + handle.size < table.len() as u64);
        let mut bad = table[table.len() - FOOTER..].to_vec();
        bad[FOOTER - 1] ^= 1;
        assert!(matches!(footer(&bad), Err(Error::Malformed(_))));
    }
}
