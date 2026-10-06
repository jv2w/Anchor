//! Reproduction tests for the fixed redb copy in `vendor/redb`: opening a file and
//! checking it must turn a damaged byte into an error. The published 4.3.0 reads the system tree
//! at open before anything verifies it, so one damaged byte there panics the open
//! (cberner/redb#1505, #1334); this test fails on it with a panic. The copy also grows the file
//! by a step instead of doubling it, which the second test checks. The copy's other fix, the page
//! cache queue that grew with every write (#1517), is internal; the copy's own tests cover it.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use redb::{Builder, ReadableDatabase, ReadableTable, StorageBackend, TableDefinition};

const TABLE: TableDefinition<'static, &[u8], &[u8]> = TableDefinition::new("chunks");
const PAGE: usize = 4096;

/// A file in memory, shared by the handles opened on it.
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: redb's backend trait takes &self from any thread"
)]
struct Memory(Arc<std::sync::Mutex<Vec<u8>>>);

impl StorageBackend for Memory {
    fn len(&self) -> io::Result<u64> {
        Ok(u64::try_from(self.0.lock().unwrap().len()).unwrap())
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let bytes = self.0.lock().unwrap();
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let source = start
            .checked_add(out.len())
            .and_then(|end| bytes.get(start..end))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        out.copy_from_slice(source);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0
            .lock()
            .unwrap()
            .resize(usize::try_from(len).unwrap(), 0);
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut bytes = self.0.lock().unwrap();
        let start = usize::try_from(offset).unwrap();
        bytes
            .get_mut(start..start + data.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?
            .copy_from_slice(data);
        Ok(())
    }
}

/// `SplitMix64`, for a fixed sequence of damaged bytes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

/// A closed database with a few quick-repair commits of chunk-sized values, as Anchor writes.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test opens and commits to a database in memory"
)]
fn closed_database() -> Vec<u8> {
    let memory = Memory::default();
    let mut rng = Rng(1);
    let db = Builder::new().create_with_backend(memory.clone()).unwrap();
    for round in 0..3_u8 {
        let mut txn = db.begin_write().unwrap();
        txn.set_quick_repair(true);
        {
            let mut table = txn.open_table(TABLE).unwrap();
            for key in 0..40_u8 {
                if (key + round) % 2 == 0 {
                    let len = 200 + rng.below(6000);
                    let value: Vec<u8> = (0..len).map(|_| (rng.next() % 7) as u8).collect();
                    table.insert(&[key][..], value.as_slice()).unwrap();
                }
            }
        }
        txn.commit().unwrap();
    }
    drop(db);
    memory.0.lock().unwrap().clone()
}

/// Opens `bytes`, checks them and reads every value, as Anchor's start does. A panic anywhere
/// is the defect; any result is fine.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test opens and checks a database in memory"
)]
fn open_and_check(bytes: Vec<u8>) -> std::thread::Result<()> {
    let memory = Memory(Arc::new(bytes.into()));
    panic::catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut db) = Builder::new().create_with_backend(memory) else {
            return;
        };
        if !matches!(db.check_integrity(), Ok(true)) {
            return;
        }
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(TABLE).unwrap();
        for (key, value) in table.iter().unwrap().flatten() {
            std::hint::black_box((key.value(), value.value()));
        }
    }))
}

#[test]
fn a_damaged_byte_is_an_error_not_a_panic() {
    let base = closed_database();
    let mut panics = Vec::new();
    // The first byte of every page is its node type: a wrong one in the system root panicked
    // the open at `Btree::get_helper`'s `unreachable!()` (#1505).
    let mut flips: Vec<(usize, u8)> = (0..base.len()).step_by(PAGE).map(|at| (at, 0xff)).collect();
    let mut rng = Rng(7);
    flips.extend((0..1_000).map(|_| (rng.below(base.len()), 1 + (rng.next() % 255) as u8)));
    for (at, mask) in flips {
        let mut bytes = base.clone();
        bytes[at] ^= mask;
        if open_and_check(bytes).is_err() {
            panics.push(at);
        }
    }
    assert!(panics.is_empty(), "damage at {panics:?} panicked");
}

/// The published 4.3.0 doubles the file whenever it needs room, so after `compact()` leaves no
/// free tail the next commit of any size doubled the file, and a large write ended up to twice
/// its data (the server file of a 270 MB world took 1.27 GB). The copy grows by an eighth.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test commits to and compacts a database in memory"
)]
fn a_small_commit_after_a_compaction_grows_the_file_by_a_step_not_double() {
    let memory = Memory::default();
    let mut db = Builder::new().create_with_backend(memory.clone()).unwrap();
    let mut rng = Rng(3);
    for round in 0..8_u32 {
        let mut txn = db.begin_write().unwrap();
        txn.set_quick_repair(true);
        {
            let mut table = txn.open_table(TABLE).unwrap();
            for key in 0..512_u32 {
                let len = 200 + rng.below(1500);
                let value: Vec<u8> = (0..len).map(|_| rng.next().to_le_bytes()[0]).collect();
                table
                    .insert(&(round * 512 + key).to_be_bytes()[..], value.as_slice())
                    .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    db.compact().unwrap();
    let compacted = memory.len().unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_quick_repair(true);
    txn.open_table(TABLE)
        .unwrap()
        .insert(&[0xff][..], &[1, 2, 3][..])
        .unwrap();
    txn.commit().unwrap();
    let after = memory.len().unwrap();
    assert!(
        after <= compacted + compacted / 4,
        "a 4-byte commit grew {compacted} bytes to {after}"
    );
}
