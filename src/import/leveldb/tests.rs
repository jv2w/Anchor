//! The reader against a model: databases written by a test-only writer the way LevelDB lays
//! them out (tables per level, overlapping level 0, live and stale logs, stale tables, an
//! unfinished write at the end), read back and compared with a `BTreeMap` of the writes.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use super::table::tests::encode_table;
use super::*;
use crate::protocol::Rng;

pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).unwrap() | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).unwrap());
}

pub(crate) fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// The internal key of `user` at `sequence` with `kind`.
pub(crate) fn internal(user: &[u8], sequence: u64, kind: u8) -> Vec<u8> {
    let mut key = user.to_vec();
    key.extend_from_slice(&(sequence << 8 | u64::from(kind)).to_le_bytes());
    key
}

/// One write: a key and its value, or `None` for a deletion.
type Write = (Vec<u8>, Option<Vec<u8>>);
/// A write batch: its first sequence number and its writes.
type Batch = (u64, Vec<Write>);
/// What a database must read as.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// A database being written in memory.
pub(crate) struct Writer {
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    next: u64,
    /// `MANIFEST` records.
    pub(crate) edits: Vec<Vec<u8>>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        let mut comparator = vec![1];
        put_prefixed(&mut comparator, COMPARATOR);
        Self {
            files: BTreeMap::new(),
            next: 2,
            edits: vec![comparator],
        }
    }

    pub(crate) fn number(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Writes a table of internal-key `entries` and returns its file number and edit fields.
    pub(super) fn table(
        &mut self,
        entries: &[(Vec<u8>, Vec<u8>)],
        rng: &mut Rng,
        sst: bool,
    ) -> TableFile {
        let number = self.number();
        let block_size = 32 + rng.below(4096);
        let interval = 1 + rng.below(16);
        let mut kinds = || [0, 1, 2, 4][rng.below(4)];
        let bytes = encode_table(entries, block_size, interval, &mut kinds);
        let file = TableFile {
            number,
            size: bytes.len() as u64,
            smallest: entries.first().unwrap().0.clone(),
            largest: entries.last().unwrap().0.clone(),
        };
        let name = format!("{number:06}.{}", if sst { "sst" } else { "ldb" });
        self.files.insert(name, bytes);
        file
    }

    /// Writes a log of write batches; `cut` cuts the last record short, as an unfinished write.
    pub(crate) fn log(&mut self, number: u64, batches: &[Vec<u8>], cut: Option<&mut Rng>) {
        let mut bytes = log::tests::write(batches);
        if let Some(rng) = cut {
            let start = log::tests::write(&batches[..batches.len() - 1]).len();
            bytes.truncate(start + 1 + rng.below(bytes.len() - start - 1));
        }
        self.files.insert(format!("{number:06}.log"), bytes);
    }

    /// A version edit adding `file` at `level`.
    pub(super) fn add(level: usize, file: &TableFile) -> Vec<u8> {
        let mut edit = vec![7];
        put_varint(&mut edit, level as u64);
        put_varint(&mut edit, file.number);
        put_varint(&mut edit, file.size);
        put_prefixed(&mut edit, &file.smallest);
        put_prefixed(&mut edit, &file.largest);
        edit
    }

    /// A version edit deleting table `number` from `level`.
    pub(crate) fn delete(level: usize, number: u64) -> Vec<u8> {
        let mut edit = vec![6];
        put_varint(&mut edit, level as u64);
        put_varint(&mut edit, number);
        edit
    }

    /// Writes the `MANIFEST` (its records as given) and `CURRENT`.
    pub(crate) fn finish(mut self) -> Files {
        let number = self.number();
        let name = format!("MANIFEST-{number:06}");
        self.files
            .insert(name.clone(), log::tests::write(&self.edits));
        self.files
            .insert("CURRENT".to_owned(), format!("{name}\n").into_bytes());
        Files::Memory(self.files)
    }
}

/// A version edit setting the log number.
pub(crate) fn log_number(number: u64) -> Vec<u8> {
    let mut edit = vec![2];
    put_varint(&mut edit, number);
    edit
}

fn scan_all(db: &mut Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    db.scan(&[], None, |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    out
}

fn random_value(rng: &mut Rng, big: bool) -> Vec<u8> {
    let len = if big && rng.below(8) == 0 {
        33_000 + rng.below(40_000)
    } else {
        rng.below(40)
    };
    (0..len)
        .map(|i| u8::try_from((i * 7 + len) % 251).unwrap())
        .collect()
}

/// The internal-key entries of `writes` (sequence, key, value), every version or only the
/// newest of each key, in internal key order.
fn entries(writes: &[(u64, Write)], newest_only: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut sorted: Vec<&(u64, Write)> = writes.iter().collect();
    sorted.sort_by(|a, b| a.1.0.cmp(&b.1.0).then(b.0.cmp(&a.0)));
    if newest_only {
        sorted.dedup_by(|b, a| a.1.0 == b.1.0);
    }
    sorted
        .into_iter()
        .map(|(sequence, (key, value))| {
            let kind = if value.is_some() { VALUE } else { DELETION };
            (
                internal(key, *sequence, kind),
                value.clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// Random write batches over `keys` (each with its first sequence number), the next sequence
/// number, and the map they leave.
fn history(rng: &mut Rng, keys: &[Vec<u8>]) -> (Vec<Batch>, u64, Model) {
    let big = rng.below(50) == 0;
    let mut sequence = 1 + rng.below(1000) as u64;
    let batch_count = 1 + rng.below(24);
    let mut batches: Vec<(u64, Vec<Write>)> = Vec::new();
    for _ in 0..batch_count {
        let writes: Vec<Write> = (0..=rng.below(5))
            .map(|_| {
                let key = keys[rng.below(keys.len())].clone();
                let value = (rng.below(10) >= 3).then(|| random_value(rng, big));
                (key, value)
            })
            .collect();
        batches.push((sequence, writes.clone()));
        sequence += writes.len() as u64;
    }
    let mut model = BTreeMap::new();
    for (_, writes) in &batches {
        for (key, value) in writes {
            match value {
                Some(value) => model.insert(key.clone(), value.clone()),
                None => model.remove(key),
            };
        }
    }

    (batches, sequence, model)
}

/// The layouts a random database has, to check that the generator reaches each.
#[derive(Debug, Default, Clone, Copy)]
struct Layouts {
    level0_tables: usize,
    deeper_levels: bool,
    sst: bool,
    stale_log: bool,
    stale_table: bool,
    prev_log: bool,
    cut_tail: bool,
    fragmented: bool,
}

/// Writes the `flushed` batches as tables: in level 0 one table per segment, deeper levels
/// merged and cut into disjoint tables; older segments never in shallower levels.
fn write_tables(
    w: &mut Writer,
    rng: &mut Rng,
    flushed: &[(u64, Vec<Write>)],
    layouts: &mut Layouts,
) {
    let mut level = 1 + rng.below(LEVELS - 1);
    let mut at = 0;
    let mut deeper: BTreeMap<usize, Vec<(u64, Write)>> = BTreeMap::new();
    while at < flushed.len() {
        let end = (at + 1 + rng.below(flushed.len() - at)).min(flushed.len());
        let segment: Vec<(u64, Write)> = flushed[at..end]
            .iter()
            .flat_map(|(first, writes)| {
                writes
                    .iter()
                    .enumerate()
                    .map(move |(i, write)| (first + i as u64, write.clone()))
            })
            .collect();
        level = rng.below(level + 1);
        if level == 0 {
            let table = entries(&segment, rng.below(2) == 0);
            let sst = rng.below(5) == 0;
            layouts.sst |= sst;
            layouts.level0_tables += 1;
            let file = w.table(&table, rng, sst);
            w.edits.push(Writer::add(0, &file));
        } else {
            deeper.entry(level).or_default().extend(segment);
        }
        at = end;
    }
    layouts.deeper_levels = !deeper.is_empty();
    for (&level, writes) in &deeper {
        let all = entries(writes, rng.below(2) == 0);
        // Disjoint tables: cut only between user keys.
        let mut start = 0;
        while start < all.len() {
            let mut end = (start + 1 + rng.below(all.len())).min(all.len());
            while end < all.len() && split(&all[end].0).0 == split(&all[end - 1].0).0 {
                end += 1;
            }
            let file = w.table(&all[start..end], rng, false);
            w.edits.push(Writer::add(level, &file));
            start = end;
        }
    }
}

/// One random database, the map it must read as and its layouts.
fn random_database(seed: u64) -> (Files, BTreeMap<Vec<u8>, Vec<u8>>, Layouts) {
    let mut rng = Rng(seed);
    let mut layouts = Layouts::default();
    let key_count = 1 + rng.below(40);
    let keys: Vec<Vec<u8>> = (0..key_count)
        .map(|i| {
            let mut key = format!("k{i:03}").into_bytes();
            if rng.below(10) == 0 {
                key.extend(std::iter::repeat_n(b'x', rng.below(300)));
            }
            key
        })
        .collect();
    let (batches, sequence, model) = history(&mut rng, &keys);
    let batch_count = batches.len();
    let mut w = Writer::new();
    let garbage = |rng: &mut Rng| -> Vec<Write> {
        (0..=rng.below(4))
            .map(|_| (keys[rng.below(keys.len())].clone(), Some(b"stale".to_vec())))
            .collect()
    };
    // A stale log, numbered below the log number: its writes are in tables already.
    let stale_log = (rng.below(3) == 0).then(|| {
        let number = w.number();
        let batch = log::tests::encode_batch(1 << 50, &garbage(&mut rng));
        w.log(number, &[batch], None);
        number
    });
    // The oldest batches go to tables, older ones deeper.
    let flushed = rng.below(batch_count + 1);
    write_tables(&mut w, &mut rng, &batches[..flushed], &mut layouts);
    // A stale table: listed and then deleted by a later edit, or never listed. LevelDB never
    // adds and deletes a table in one edit.
    let mut deletes = Vec::new();
    if rng.below(3) == 0 {
        let stale = entries(
            &garbage(&mut rng)
                .into_iter()
                .map(|write| (1 << 51, write))
                .collect::<Vec<_>>(),
            true,
        );
        let file = w.table(&stale, &mut rng, false);
        layouts.stale_table = true;
        if rng.below(2) == 0 {
            let level = rng.below(LEVELS);
            w.edits.push(Writer::add(level, &file));
            deletes.push(Writer::delete(level, file.number));
        }
    }
    // The newest batches stay in live logs.
    let live = &batches[flushed..];
    let mut log_numbers = Vec::new();
    let mut at = 0;
    while at < live.len() {
        let end = (at + 1 + rng.below(live.len() - at)).min(live.len());
        let number = w.number();
        let mut records: Vec<Vec<u8>> = live[at..end]
            .iter()
            .map(|(first, writes)| log::tests::encode_batch(*first, writes))
            .collect();
        let cut = end == live.len() && rng.below(3) == 0;
        layouts.cut_tail |= cut;
        layouts.fragmented |= records.iter().any(|r| r.len() > log::BLOCK);
        if cut {
            records.push(log::tests::encode_batch(1 << 52, &garbage(&mut rng)));
        }
        w.log(number, &records, cut.then_some(&mut rng));
        log_numbers.push(number);
        at = end;
    }
    let (live_from, prev) = match log_numbers.as_slice() {
        [] => (w.number(), None),
        [only] => (*only, None),
        [first, second, ..] if rng.below(2) == 0 => (*second, Some(*first)),
        [first, ..] => (*first, None),
    };
    layouts.stale_log = stale_log.is_some_and(|n| n < live_from);
    layouts.prev_log = prev.is_some();
    let mut meta = log_number(live_from);
    if let Some(prev) = prev {
        meta.push(9);
        put_varint(&mut meta, prev);
    }
    meta.push(3);
    put_varint(&mut meta, w.next + 10);
    meta.push(4);
    put_varint(&mut meta, sequence);
    if rng.below(2) == 0 {
        meta.push(5);
        put_varint(&mut meta, 1);
        put_prefixed(&mut meta, &internal(b"k", 5, 1));
    }
    w.edits.push(meta);
    if rng.below(2) == 0 {
        // Everything in one edit record.
        let joined = w.edits.concat();
        w.edits = vec![joined];
    }
    w.edits.extend(deletes);
    (w.finish(), model, layouts)
}

#[test]
fn random_databases_read_like_the_model_of_their_writes() {
    let mut reached = [0_usize; 8];
    for seed in 0..1000 {
        let (files, model, layouts) = random_database(seed);
        let flags = [
            layouts.level0_tables >= 2,
            layouts.deeper_levels,
            layouts.sst,
            layouts.stale_log,
            layouts.stale_table,
            layouts.prev_log,
            layouts.cut_tail,
            layouts.fragmented,
        ];
        for (count, flag) in reached.iter_mut().zip(flags) {
            *count += usize::from(flag);
        }
        let mut db = Db::from_files(files).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
        let expected: Vec<(Vec<u8>, Vec<u8>)> =
            model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(scan_all(&mut db), expected, "seed {seed}: full scan");
        let mut rng = Rng(seed ^ 0xdead);
        for _ in 0..5 {
            let mut bounds = [
                format!("k{:03}", rng.below(45)).into_bytes(),
                format!("k{:03}", rng.below(45)).into_bytes(),
            ];
            bounds.sort();
            let [start, end] = bounds;
            let mut got = Vec::new();
            db.scan(&start, Some(&end), |k, v| {
                got.push((k.to_vec(), v.to_vec()));
                Ok(ControlFlow::Continue(()))
            })
            .unwrap();
            let want: Vec<(Vec<u8>, Vec<u8>)> = model
                .range(start.clone()..end.clone())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(got, want, "seed {seed}: range {start:?}..{end:?}");
            let key = format!("k{:03}", rng.below(45)).into_bytes();
            assert_eq!(
                db.get(&key).unwrap(),
                model.get(&key).cloned(),
                "seed {seed}"
            );
        }
        // A scan stops where its callback breaks.
        let mut seen = 0;
        db.scan(&[], None, |_, _| {
            seen += 1;
            Ok(if seen == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })
        .unwrap();
        assert_eq!(seen, model.len().min(2), "seed {seed}");
    }
    // Overlapping level 0, deeper levels, .sst names, stale logs and tables, a previous log, an
    // unfinished write, records over a log block.
    assert!(
        reached.iter().all(|&n| n >= 5),
        "layouts reached: {reached:?}"
    );
}

/// The name of the one table in `files`.
fn table_name(files: &BTreeMap<String, Vec<u8>>) -> String {
    files
        .keys()
        .find(|n| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|e| e == "ldb")
        })
        .unwrap()
        .clone()
}

/// A database of one table, at level 1, and one log.
fn small(table: &[(Vec<u8>, Vec<u8>)], log: &[Vec<u8>]) -> Writer {
    let mut rng = Rng(9);
    let mut w = Writer::new();
    let file = w.table(table, &mut rng, false);
    w.edits.push(Writer::add(1, &file));
    let number = w.number();
    w.log(number, log, None);
    w.edits.push(log_number(number));
    w
}

fn small_entries() -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..30)
        .map(|i| {
            (
                internal(format!("key{i:02}").as_bytes(), 10 + i, VALUE),
                vec![1; usize::try_from(i).unwrap()],
            )
        })
        .collect()
}

fn open_err(files: Files) -> Error {
    match Db::from_files(files) {
        Ok(_) => panic!("opened"),
        Err(Error::File { source, .. }) => *source,
        Err(e) => e,
    }
}

#[test]
fn malformed_manifests_are_errors() {
    let unknown = {
        let mut w = small(&small_entries(), &[]);
        w.edits.push(vec![8, 1]);
        w.finish()
    };
    assert!(matches!(
        open_err(unknown),
        Error::Malformed("unknown version edit tag")
    ));
    let comparator = {
        let mut w = small(&small_entries(), &[]);
        let mut edit = vec![1];
        put_prefixed(&mut edit, b"other");
        w.edits.push(edit);
        w.finish()
    };
    assert!(matches!(open_err(comparator), Error::Malformed(_)));
    let no_log = {
        let mut w = small(&small_entries(), &[]);
        w.edits.retain(|e| e[0] != 2);
        w.finish()
    };
    assert!(matches!(
        open_err(no_log),
        Error::Malformed("MANIFEST has no log number")
    ));
    let overlap = {
        let mut w = small(&small_entries(), &[]);
        let mut rng = Rng(1);
        let file = w.table(&small_entries()[10..20], &mut rng, false);
        w.edits.push(Writer::add(1, &file));
        w.finish()
    };
    assert!(matches!(
        open_err(overlap),
        Error::Malformed("tables of a level overlap")
    ));
    // The error names the missing table, not the MANIFEST.
    let mut w = small(&small_entries(), &[]);
    let name = table_name(&w.files);
    w.files.remove(&name);
    let missing = Db::from_files(w.finish()).unwrap_err();
    assert!(
        matches!(
            &missing,
            Error::File { path, source }
                if path == std::path::Path::new(&name)
                    && matches!(**source, Error::Malformed("a listed table is missing"))
        ),
        "{missing:?}"
    );
    let resized = {
        let mut w = small(&small_entries(), &[]);
        let name = table_name(&w.files);
        w.files.get_mut(&name).unwrap().push(0);
        w.finish()
    };
    assert!(matches!(open_err(resized), Error::Malformed(_)));
    let twice = {
        let mut w = small(&small_entries(), &[]);
        let edit = w.edits[1].clone();
        let mut again = edit.clone();
        again[1] = 2;
        w.edits.push(again);
        w.finish()
    };
    assert!(matches!(
        open_err(twice),
        Error::Malformed("table in two levels")
    ));
    let level = {
        let mut w = small(&small_entries(), &[]);
        w.edits[1][1] = 7;
        w.finish()
    };
    assert!(matches!(
        open_err(level),
        Error::Malformed("version edit level")
    ));
}

/// The contents of in-memory files.
fn memory(files: Files) -> BTreeMap<String, Vec<u8>> {
    match files {
        Files::Memory(files) => files,
        Files::Disk { .. } => panic!("not in memory"),
    }
}

fn memory_mut(files: &mut Files) -> &mut BTreeMap<String, Vec<u8>> {
    match files {
        Files::Memory(files) => files,
        Files::Disk { .. } => panic!("not in memory"),
    }
}

#[test]
fn current_must_name_a_manifest_in_256_bytes() {
    for bad in [
        &b"MANIFEST-000001"[..],
        b"OTHER-1\n",
        b"MANIFEST-\n",
        b"MANIFEST-1x\n",
    ] {
        let mut files = memory(small(&small_entries(), &[]).finish());
        files.insert("CURRENT".to_owned(), bad.to_vec());
        assert!(
            matches!(open_err(Files::Memory(files)), Error::Malformed(_)),
            "{bad:?}"
        );
    }
    let mut files = memory(small(&small_entries(), &[]).finish());
    files.insert("CURRENT".to_owned(), vec![b'M'; 257]);
    assert!(matches!(
        open_err(Files::Memory(files)),
        Error::TooLarge { limit: 256, .. }
    ));
}

#[test]
fn live_logs_over_64_mib_are_refused_before_reading() {
    let mut w = small(&small_entries(), &[]);
    let number = w.number();
    w.files
        .insert(format!("{number:06}.log"), vec![0; 33 << 20]);
    let next = w.number();
    w.files.insert(format!("{next:06}.log"), vec![0; 33 << 20]);
    assert!(matches!(
        open_err(w.finish()),
        Error::TooLarge {
            what: "bytes in the live logs",
            ..
        }
    ));
}

#[test]
fn more_than_a_million_log_keys_are_refused() {
    let writes: Vec<Write> = (0..=u32::try_from(MAX_LOG_KEYS).unwrap())
        .map(|i| (i.to_le_bytes().to_vec(), None))
        .collect();
    let batch = log::tests::encode_batch(1, &writes);
    let w = small(&small_entries(), &[batch]);
    assert!(matches!(
        open_err(w.finish()),
        Error::TooLarge {
            what: "keys in the live logs",
            ..
        }
    ));
}

#[test]
fn more_than_65536_files_are_refused() {
    let mut files = memory(small(&small_entries(), &[]).finish());
    for i in 0..MAX_FILES {
        files.insert(format!("x{i}"), Vec::new());
    }
    assert!(matches!(
        open_err(Files::Memory(files)),
        Error::TooLarge { .. }
    ));
}

#[test]
fn a_block_over_16_mib_is_refused_before_reading() {
    // An index entry pointing at a 1 GiB block.
    let mut handle = Vec::new();
    put_varint(&mut handle, 0);
    put_varint(&mut handle, 1 << 30);
    let index = table::tests::encode_block(&[(internal(b"z", 1, VALUE), handle)], 1);
    let mut bytes = Vec::new();
    let meta = table::tests::seal_block(&mut bytes, &table::tests::encode_block(&[], 1), 0);
    let index_handle = table::tests::seal_block(&mut bytes, &index, 0);
    let mut footer = meta;
    footer.extend_from_slice(&index_handle);
    footer.resize(40, 0);
    footer.extend_from_slice(&0xdb47_7524_8b80_fb57_u64.to_le_bytes());
    bytes.extend_from_slice(&footer);
    let mut w = Writer::new();
    let number = w.number();
    let file = TableFile {
        number,
        size: bytes.len() as u64,
        smallest: internal(b"a", 1, VALUE),
        largest: internal(b"z", 1, VALUE),
    };
    w.files.insert(format!("{number:06}.ldb"), bytes);
    w.edits.push(Writer::add(1, &file));
    w.edits.push(log_number(w.next + 1));
    let mut db = Db::from_files(w.finish()).unwrap();
    let error = db.get(b"m").unwrap_err();
    let Error::File { source, .. } = error else {
        panic!("{error:?}")
    };
    assert!(matches!(
        *source,
        Error::TooLarge {
            what: "bytes in a block",
            ..
        }
    ));
}

#[test]
fn a_malformed_entry_in_a_table_names_the_table() {
    // One data block whose second key is before its first.
    let entries = [
        (internal(b"b", 1, VALUE), b"b".to_vec()),
        (internal(b"a", 1, VALUE), b"a".to_vec()),
    ];
    let mut kinds = || table::NONE;
    let bytes = encode_table(&entries, 4096, 16, &mut kinds);
    let mut w = Writer::new();
    let number = w.number();
    let name = format!("{number:06}.ldb");
    let file = TableFile {
        number,
        size: bytes.len() as u64,
        smallest: internal(b"a", 1, VALUE),
        largest: internal(b"b", 1, VALUE),
    };
    w.files.insert(name.clone(), bytes);
    w.edits.push(Writer::add(1, &file));
    w.edits.push(log_number(w.next + 1));
    let mut db = Db::from_files(w.finish()).unwrap();
    let error = db
        .scan(&[], None, |_, _| Ok(ControlFlow::Continue(())))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::File { path, source }
                if path == std::path::Path::new(&name)
                    && matches!(**source, Error::Malformed("block keys out of order"))
        ),
        "{error:?}"
    );
}

#[test]
fn held_blocks_are_bounded() {
    assert!(check_held(MAX_HELD).is_ok());
    assert!(matches!(
        check_held(MAX_HELD + 1),
        Err(Error::TooLarge { .. })
    ));
}

#[test]
fn held_blocks_are_bounded_while_level_0_tables_open() {
    // Each table holds one data block of almost 16 MiB that compresses to a few KiB, so the
    // first 17 overlapping level 0 tables hold more than 256 MiB together.
    let entry = (internal(b"k", 1, VALUE), vec![0; table::MAX_BLOCK - 64]);
    let mut kinds = || table::RAW_DEFLATE;
    let bytes = encode_table(&[entry], table::MAX_BLOCK, 1, &mut kinds);
    let over = MAX_HELD / table::MAX_BLOCK + 1;
    let mut w = Writer::new();
    for i in 0..=over {
        let number = w.number();
        let mut table = bytes.clone();
        if i == over {
            // Opened last: a scan that checks what it holds only after opening every table
            // fails here instead.
            table[0] ^= 1;
        }
        let file = TableFile {
            number,
            size: table.len() as u64,
            smallest: internal(b"k", 1, VALUE),
            largest: internal(b"k", 1, VALUE),
        };
        w.files.insert(format!("{number:06}.ldb"), table);
        w.edits.push(Writer::add(0, &file));
    }
    w.edits.push(log_number(w.next + 1));
    let mut db = Db::from_files(w.finish()).unwrap();
    let error = db
        .scan(&[], None, |_, _| Ok(ControlFlow::Continue(())))
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::TooLarge {
                what: "bytes of blocks held by one scan",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn varints_are_bounded() {
    assert_eq!(
        Bytes(&[0xff, 0xff, 0xff, 0xff, 0x0f]).varint32().unwrap(),
        u32::MAX
    );
    assert!(Bytes(&[0xff, 0xff, 0xff, 0xff, 0x1f]).varint32().is_err());
    assert!(
        Bytes(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01])
            .varint32()
            .is_err()
    );
    let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
    assert_eq!(Bytes(&max).varint64().unwrap(), u64::MAX);
    let mut over = max;
    over[9] = 0x02;
    assert!(Bytes(&over).varint64().is_err());
    assert!(Bytes(&[0x80]).varint64().is_err());
}

#[test]
fn the_masked_crc_matches_leveldb() {
    // google/leveldb util/crc32c_test.cc: Mask(Value("foo")) differs from Value("foo"), and
    // Value of 32 zero bytes is 0x8a9136aa.
    let mut crc = Crc32c::new();
    crc.update(&[0; 32]);
    assert_eq!(crc.finish(), 0x8a91_36aa);
    assert_eq!(
        masked_crc(&[&[0; 32]]),
        0x8a91_36aa_u32.rotate_right(15).wrapping_add(MASK_DELTA)
    );
}

#[test]
fn source_changes_are_found() {
    let w = small(&small_entries(), &[]);
    let db = Db::from_files(w.finish()).unwrap();
    assert!(db.unchanged().is_ok());
    let mut changed = Db::from_files(small(&small_entries(), &[]).finish()).unwrap();
    memory_mut(&mut changed.tables.files).insert("new.log".to_owned(), Vec::new());
    assert!(matches!(
        changed.unchanged(),
        Err(Error::SourceChanged { .. })
    ));
    let mut grown = Db::from_files(small(&small_entries(), &[]).finish()).unwrap();
    memory_mut(&mut grown.tables.files)
        .get_mut("CURRENT")
        .unwrap()
        .push(b'x');
    assert!(matches!(
        grown.unchanged(),
        Err(Error::SourceChanged { .. })
    ));
}

/// Opens and reads everything; any error is fine, a panic is not.
fn open_and_read(files: Files) -> crate::protocol::Result<()> {
    let fail = |_| crate::protocol::Error::Size;
    let mut db = Db::from_files(files).map_err(fail)?;
    db.scan(&[], None, |_, _| Ok(ControlFlow::Continue(())))
        .map_err(fail)?;
    db.get(b"key15").map_err(fail)?;
    Ok(())
}

fn mutate_file(
    name_ends: &str,
    files: &BTreeMap<String, Vec<u8>>,
    f: impl Fn(&[u8]) -> BTreeMap<String, Vec<u8>>,
) {
    let name = files.keys().find(|n| n.ends_with(name_ends)).unwrap();
    let decoded = crate::protocol::fuzz(&files[name], 10_000, |data| {
        open_and_read(Files::Memory(f(data)))
    });
    assert!(decoded > 0, "{name_ends}");
}

#[test]
fn mutated_tables_logs_manifests_and_current_never_panic() {
    let batch = log::tests::encode_batch(
        500,
        &[
            (b"key03".to_vec(), None),
            (b"key99".to_vec(), Some(vec![5; 40_000])),
        ],
    );
    let files = memory(small(&small_entries(), &[batch]).finish());
    // A table: the MANIFEST is rewritten with the mutated size, so the mutation is read.
    let entries = small_entries();
    mutate_file(".ldb", &files, |data| {
        let mut w = Writer::new();
        let number = w.number();
        w.files.insert(format!("{number:06}.ldb"), data.to_vec());
        let file = TableFile {
            number,
            size: data.len() as u64,
            smallest: entries[0].0.clone(),
            largest: entries[entries.len() - 1].0.clone(),
        };
        w.edits.push(Writer::add(1, &file));
        w.edits.push(log_number(w.next + 1));
        memory(w.finish())
    });
    for name in [".log", "CURRENT"] {
        mutate_file(name, &files, |data| {
            let mut mutated = files.clone();
            let key = mutated.keys().find(|n| n.ends_with(name)).unwrap().clone();
            mutated.insert(key, data.to_vec());
            mutated
        });
    }
    let manifest = files
        .keys()
        .find(|n| n.starts_with("MANIFEST"))
        .unwrap()
        .clone();
    let decoded = crate::protocol::fuzz(&files[&manifest], 10_000, |data| {
        let mut mutated = files.clone();
        mutated.insert(manifest.clone(), data.to_vec());
        open_and_read(Files::Memory(mutated))
    });
    assert!(decoded > 0);
}

/// `hex(key) hex(sha256(value))` for every live key, one line each: the form of the Go dump.
pub(crate) fn dump(db: &mut Db) -> String {
    use std::fmt::Write as _;
    let hex = |out: &mut String, bytes: &[u8]| {
        for b in bytes {
            write!(out, "{b:02x}").unwrap();
        }
    };
    let mut out = String::new();
    db.scan(&[], None, |key, value| {
        hex(&mut out, key);
        out.push(' ');
        hex(&mut out, &crate::crypto::sha256(&[value]));
        out.push('\n');
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    out
}

#[test]
fn the_bds_fixture_reads_like_goleveldb() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("xtask/data/import");
    let mut db = Db::open(root.join("flat")).unwrap();
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    let want = std::fs::read_to_string(root.join("flat.dump")).unwrap();
    assert!(
        dump(&mut db) == want,
        "differs from xtask/data/import/flat.dump"
    );
    assert!(db.unchanged().is_ok());
}

/// The files of a database whose one live log holds `writes`, starting at sequence 1.
pub(crate) fn database_files(writes: &[(Vec<u8>, Option<Vec<u8>>)]) -> BTreeMap<String, Vec<u8>> {
    let mut w = Writer::new();
    let number = w.number();
    w.log(number, &[log::tests::encode_batch(1, writes)], None);
    w.edits.push(log_number(number));
    memory(w.finish())
}

#[test]
fn an_ldb_table_wins_over_an_sst_of_the_same_number() {
    let mut w = small(&small_entries(), &[]);
    let ldb = table_name(&w.files);
    let sst = ldb.replace(".ldb", ".sst");
    w.files.insert(sst, b"not a table".to_vec());
    let mut db = Db::from_files(w.finish()).unwrap();
    assert_eq!(db.get(b"key05").unwrap(), Some(vec![1; 5]));
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn tables_on_disk_are_read_through_at_most_64_open_files() {
    let dir = crate::save::tests::TempDir::new("leveldb-open-files");
    let mut rng = Rng(3);
    let mut w = Writer::new();
    let mut model = BTreeMap::new();
    for i in 0..100_u64 {
        let key = format!("key{i:03}").into_bytes();
        let entry = vec![(internal(&key, i + 1, VALUE), vec![1; 3])];
        let file = w.table(&entry, &mut rng, false);
        w.edits.push(Writer::add(0, &file));
        model.insert(key, vec![1; 3]);
    }
    w.edits.push(log_number(w.next + 1));
    for (name, bytes) in memory(w.finish()) {
        std::fs::write(dir.0.join(name), bytes).unwrap();
    }
    std::fs::write(dir.0.join("abc.ldb"), b"not a table").unwrap();
    let mut db = Db::from_files(Files::disk(dir.0.clone())).unwrap();
    let expected: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(scan_all(&mut db), expected);
    let Files::Disk { open, .. } = &db.tables.files else {
        panic!("not on disk")
    };
    assert_eq!(open.len(), table::MAX_OPEN);
    db.unchanged().unwrap();
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn the_file_just_read_stays_open_when_the_open_files_are_full() {
    let dir = crate::save::tests::TempDir::new("leveldb-open-file-kept");
    let name = |i: usize| format!("{i:06}.ldb");
    for i in 0..=table::MAX_OPEN {
        std::fs::write(dir.0.join(name(i)), b"data").unwrap();
    }
    let mut files = Files::disk(dir.0.clone());
    let mut buf = [0; 4];
    for i in 1..=table::MAX_OPEN {
        files.read_at(&name(i), 0, &mut buf).unwrap();
    }
    // The smallest name, read once every slot is taken.
    files.read_at(&name(0), 0, &mut buf).unwrap();
    let Files::Disk { open, .. } = &files else {
        panic!("not on disk")
    };
    assert_eq!(open.len(), table::MAX_OPEN);
    assert!(open.contains_key(&name(0)));
}
