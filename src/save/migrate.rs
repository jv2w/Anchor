//! The move of the one server file of an older Anchor, `data/server.redb`, to the data files
//! (U16, PLAN.md 6): a start that finds it and no main data file moves it before it opens the data
//! files.
//!
//! The move never writes the old file. It copies it into `data/moving/`, opens and checks the copy
//! (a damaged file stops the start, the old file as it was), and writes every value into new data
//! files there: each world's chunks and record into its own file, the players, the operators,
//! bans and whitelist, and the server data into theirs, and the save's meta value into each. Every
//! value is written again in the current format (format 9: DEFLATE, no value for an empty
//! sub-chunk of plains); a value that does not read is copied as it is, so it stays damaged (a
//! chunk stays quarantined). Each new file is written in one transaction, compacted once and
//! closed as after a normal stop. Then each is opened and checked whole (`check_integrity()`) and
//! its values counted against the old file's. Only then are the files moved in, the main file
//! last, and the old file and the copy removed. Any failure stops the start with the old file as
//! it was; a stop during the moves leaves the old file and no main file, so the next start moves
//! it again, replacing only files this move wrote (each holds the old file's meta value,
//! [`Key::Moved`]). The old file's backups (`data/backups/server-*.redb`) are left as they are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{error, info, warn};

use super::{ChunkExtras, add_page, empty_sub_chunk, seal_pages, seal_terrain, to_chunk};
use crate::custom::Registry;
use crate::pool::Pool;
use crate::storage::codec::{
    BanRecord, ChunkRecord, Mark, Meta, PlayerRecord, SubChunkRecord, UserRecord, WorldRecord,
};
use crate::storage::seal::{seal, unseal};
use crate::storage::{
    self, AccessKey, AccessKind, Batch, CHUNK_PREFIX, ChunkId, ChunkKey, ChunkKind, ChunkPart, Key,
    Place, PlayerKey, Scan, Start, StartBackup, Storage,
};
use crate::world::SUB_CHUNKS;

/// The folder in the data folder where the move builds the data files.
const STAGING: &str = "moving";
/// Chunks converted per step on the CPU pool: bounds the chunk data held at once.
const STEP: usize = 256;

/// What the move wrote, to count the new files against.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    /// Chunks by world name.
    chunks: BTreeMap<String, u64>,
    players: u64,
    access: u64,
    user: u64,
}

/// The data files the move wrote in the staging folder.
#[derive(Debug)]
struct Built {
    /// The old file's sealed meta value (empty if it had none), the mark of every new file.
    mark: Vec<u8>,
    /// The meta value every new file holds.
    meta: Meta,
    /// The worlds' names.
    worlds: Vec<String>,
    counts: Counts,
}

/// Moves `data/server.redb` (after renaming an older `world.redb` to it) to the data files of
/// `data_dir` if there is no main data file yet, converting on a CPU pool of `cpu` threads. With a
/// main data file there, an old file this move wrote is removed (a stop came before its removal)
/// and any other is left with a warning.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the move runs once, before the server runs"
)]
pub(super) fn run(data_dir: &Path, cpu: usize) -> Result<(), crate::Error> {
    storage::rename_old_file(data_dir)?;
    let old = data_dir.join(storage::OLD_SERVER_FILE);
    if !std::fs::exists(&old).map_err(storage::Error::Io)? {
        return Ok(());
    }
    let main = Place::data(data_dir, storage::MAIN_FILE).file;
    if std::fs::exists(&main).map_err(storage::Error::Io)? {
        finish(&old, &main);
        return Ok(());
    }
    info!(file = %old.display(), "moving the server file to the data files");
    let staging = data_dir.join(STAGING);
    let moved = (|| {
        remove_folder(&staging)?;
        std::fs::create_dir_all(&staging).map_err(storage::Error::Io)?;
        let copy = copy_locked(&old, &staging)?;
        let mut source = Storage::open_checked(&copy)?;
        let pool = crate::runtime::cpu_pool(cpu)?;
        let built = build(&mut source, &staging, &pool);
        if !pool.shutdown() {
            warn!("the move's CPU pool threads did not all end");
        }
        drop(source);
        let built = built?;
        verify(&staging, &built)?;
        place(data_dir, &staging, &built)?;
        Ok::<_, crate::Error>(built)
    })();
    match moved {
        Ok(built) => {
            // The data files are whole: the old file and the copy go.
            if let Err(e) = std::fs::remove_file(&old) {
                warn!(file = %old.display(), error = %e, "could not remove the moved server file");
            }
            if let Err(e) = remove_folder(&staging) {
                warn!(folder = %staging.display(), error = %e, "could not remove the move's folder");
            }
            info!(
                worlds = built.worlds.len(),
                chunks = built.counts.chunks.values().sum::<u64>(),
                players = built.counts.players,
                "server file moved to the data files"
            );
            Ok(())
        }
        Err(e) => {
            error!(
                file = %old.display(),
                error = %e,
                "the server file could not be moved to the data files; it is left as it was"
            );
            if let Err(e) = remove_folder(&staging) {
                warn!(folder = %staging.display(), error = %e, "could not remove the move's folder");
            }
            Err(e)
        }
    }
}

/// With both the old file and a main data file: removes the old file if the main file says the
/// move wrote it from that file (the stop came before the removal), else leaves it with a warning.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the move runs once, before the server runs"
)]
fn finish(old: &Path, main: &Path) {
    let mark = |path: &Path, key: Key| -> Option<Option<Vec<u8>>> {
        let mut storage = Storage::open_replica(path).ok()?;
        storage.read(&[key]).ok()?.pop()?.ok()
    };
    match (mark(old, Key::Meta), mark(main, Key::Moved)) {
        (Some(meta), Some(Some(moved))) if meta.as_deref().unwrap_or_default() == moved => {
            match std::fs::remove_file(old) {
                Ok(()) => info!(file = %old.display(), "removed the server file moved before"),
                Err(e) => {
                    warn!(file = %old.display(), error = %e, "could not remove the moved server file");
                }
            }
        }
        _ => warn!(
            file = %old.display(),
            "an old server file is left beside the data files; it is not read"
        ),
    }
}

/// Copies `old` into `dir` under an exclusive lock, which refuses a file another process has
/// open ([`storage::Error::InUse`]); returns the copy's path.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the move copies the old server file once, before the server runs"
)]
fn copy_locked(old: &Path, dir: &Path) -> Result<PathBuf, storage::Error> {
    let mut source = std::fs::File::open(old).map_err(storage::Error::Io)?;
    match source.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(storage::Error::InUse),
        Err(std::fs::TryLockError::Error(e)) => return Err(storage::Error::Io(e)),
    }
    let copy = dir.join(storage::OLD_SERVER_FILE);
    let mut target = std::fs::File::create(&copy).map_err(storage::Error::Io)?;
    std::io::copy(&mut source, &mut target)
        .and_then(|_| target.sync_all())
        .map_err(storage::Error::Io)?;
    Ok(copy)
}

/// Removes the folder `dir` and what it holds, if it exists.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the move removes its own folder, before the server runs"
)]
fn remove_folder(dir: &Path) -> Result<(), storage::Error> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(storage::Error::Io(e)),
    }
}

/// A new data file being written in one transaction.
struct Target {
    storage: Storage,
    import: storage::Import,
}

impl Target {
    /// Creates the file at `place` and starts its transaction with the meta value and `mark`.
    fn new(place: &Place, meta: &Meta, mark: &[u8]) -> Result<Self, storage::Error> {
        let start = Start {
            backup: StartBackup::Transient,
            check: crate::config::IntegrityCheck::Never,
        };
        let mut storage = Storage::open(place, start)?;
        let mut import = storage.begin_import()?;
        let mut batch = Batch::with_capacity(2);
        batch.put(Key::Meta, seal(&Key::Meta.to_bytes(), meta)?);
        batch.put(Key::Moved, mark.to_vec());
        import.write(&batch)?;
        Ok(Self { storage, import })
    }

    /// Commits the transaction, compacts the file and closes it as after a normal stop.
    fn finish(self) -> Result<(), storage::Error> {
        let Self {
            mut storage,
            import,
        } = self;
        import.commit()?;
        storage.compact()?;
        storage.close()
    }
}

/// The values of one chunk of the old file, in key order.
struct Group {
    id: ChunkId,
    values: Vec<(ChunkPart, Vec<u8>)>,
}

/// Writes every value of `source` into new data files in `staging`.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one function moves every table of the old file in order"
)]
fn build(source: &mut Storage, staging: &Path, pool: &Pool) -> Result<Built, crate::Error> {
    let mark = source
        .read(&[Key::Meta])?
        .pop()
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let meta = if mark.is_empty() {
        Meta {
            tick: 0,
            identity: None,
        }
    } else {
        unseal::<Meta>(&Key::Meta.to_bytes(), &mark)?
    };
    // The worlds by their `chunks` key number.
    let mut worlds: BTreeMap<u32, (String, WorldRecord)> = BTreeMap::new();
    let mut names = std::collections::BTreeSet::new();
    for (name, sealed) in source.worlds()? {
        let record: WorldRecord = unseal(name.as_bytes(), &sealed)?;
        if !storage::is_world_file_name(&name) || !names.insert(storage::world_name_key(&name)) {
            return Err(crate::Error::InvalidWorldName { name });
        }
        worlds.insert(record.number, (name, record));
    }
    let mut counts = Counts::default();
    let mut damaged = 0u64;
    // Every world gets its file, chunks or not; a world's chunks come together, by number.
    let mut targets: BTreeMap<u32, Target> = BTreeMap::new();
    for (&number, (name, record)) in &worlds {
        let mut target = Target::new(&Place::world(staging, name), &meta, &mark)?;
        let mut batch = Batch::with_capacity(1);
        batch.put(Key::World, seal(&Key::World.to_bytes(), record)?);
        target.import.write(&batch)?;
        targets.insert(number, target);
        counts.chunks.insert(name.clone(), 0);
    }
    let mut orphans = 0u64;
    let mut step: Vec<Group> = Vec::with_capacity(STEP);
    let mut flush = |step: &mut Vec<Group>| -> Result<(), storage::Error> {
        let converted = pool.map(step, convert);
        for (group, (batch, bad)) in step.drain(..).zip(converted) {
            let Some(((name, _), target)) = worlds
                .get(&group.id.world)
                .zip(targets.get_mut(&group.id.world))
            else {
                orphans += 1;
                continue;
            };
            target.import.write(&batch?)?;
            damaged += u64::from(bad);
            if let Some(count) = counts.chunks.get_mut(name) {
                *count += 1;
            }
        }
        Ok(())
    };
    let mut current: Option<Group> = None;
    source.scan(Scan::Chunks, |key, value| {
        let (Some(prefix), Some(suffix)) = (key.get(..CHUNK_PREFIX), key.get(CHUNK_PREFIX..))
        else {
            return Err(storage::Error::Malformed("a chunk key no anchor writes"));
        };
        let id = chunk_of(prefix);
        let part = ChunkPart::from_suffix(suffix)?;
        if current.as_ref().is_some_and(|g| g.id != id) {
            step.extend(current.take());
            if step.len() >= STEP {
                flush(&mut step)?;
            }
        }
        current
            .get_or_insert_with(|| Group {
                id,
                values: Vec::new(),
            })
            .values
            .push((part, value.to_vec()));
        Ok(())
    })?;
    step.extend(current.take());
    flush(&mut step)?;
    if orphans > 0 {
        warn!(
            chunks = orphans,
            "chunks of no world in the server file are left out"
        );
    }
    if damaged > 0 {
        warn!(
            chunks = damaged,
            "damaged chunks are moved as they are and stay quarantined"
        );
    }
    for target in targets.into_values() {
        target.finish()?;
    }
    let mut players = Target::new(&Place::data(staging, storage::PLAYERS_FILE), &meta, &mark)?;
    counts.players = copy_table(source, Scan::Players, &mut players, |key, value| {
        let key = Key::Player(PlayerKey::from_text(std::str::from_utf8(key).ok()?));
        let record: PlayerRecord = unseal(&key.to_bytes(), value).ok()?;
        Some((key.clone(), seal(&key.to_bytes(), &record).ok()?))
    })?;
    players.finish()?;
    let mut access = Target::new(&Place::data(staging, storage::ACCESS_FILE), &meta, &mark)?;
    counts.access = copy_table(source, Scan::Access, &mut access, |key, value| {
        let key = Key::Access(AccessKey::from_bytes(key).ok()?);
        let Key::Access(AccessKey { kind, .. }) = &key else {
            return None;
        };
        let bytes = key.to_bytes();
        let sealed = match kind {
            AccessKind::Op | AccessKind::Whitelist => {
                unseal::<Mark>(&bytes, value).ok()?;
                seal(&bytes, &Mark).ok()?
            }
            AccessKind::Ban | AccessKind::IpBan => {
                seal(&bytes, &unseal::<BanRecord>(&bytes, value).ok()?).ok()?
            }
        };
        Some((key, sealed))
    })?;
    access.finish()?;
    let mut user = Target::new(&Place::data(staging, storage::USER_FILE), &meta, &mark)?;
    counts.user = copy_table(source, Scan::User, &mut user, |key, value| {
        let key = Key::User(std::str::from_utf8(key).ok()?.to_owned());
        let record: UserRecord<'_> = unseal(&key.to_bytes(), value).ok()?;
        Some((key.clone(), seal(&key.to_bytes(), &record).ok()?))
    })?;
    user.finish()?;
    // The main file holds only the meta value and the mark.
    Target::new(&Place::data(staging, storage::MAIN_FILE), &meta, &mark)?.finish()?;
    Ok(Built {
        mark,
        meta,
        worlds: worlds.into_values().map(|(name, _)| name).collect(),
        counts,
    })
}

/// Writes every value of `table` of `source` into `target`, each as `again` writes it again, or
/// as it is when `again` cannot read it (damaged: it stays so); returns how many. A key no Anchor
/// writes cannot be written under a key again and is left out.
fn copy_table(
    source: &mut Storage,
    table: Scan,
    target: &mut Target,
    again: impl Fn(&[u8], &[u8]) -> Option<(Key, Vec<u8>)>,
) -> Result<u64, storage::Error> {
    let mut count = 0;
    let mut left_out = 0u64;
    let mut batch = Batch::default();
    source.scan(table, |key, value| {
        let raw = || -> Option<Key> {
            Some(match table {
                Scan::Players => Key::Player(PlayerKey::from_text(std::str::from_utf8(key).ok()?)),
                Scan::Access => Key::Access(AccessKey::from_bytes(key).ok()?),
                Scan::User => Key::User(std::str::from_utf8(key).ok()?.to_owned()),
                Scan::Chunks => return None,
            })
        };
        match again(key, value).or_else(|| raw().map(|k| (k, value.to_vec()))) {
            Some((key, sealed)) => {
                batch.put(key, sealed);
                count += 1;
            }
            None => left_out += 1,
        }
        Ok(())
    })?;
    if left_out > 0 {
        warn!(
            ?table,
            values = left_out,
            "values under keys no anchor writes are left out"
        );
    }
    target.import.write(&batch)?;
    Ok(count)
}

/// The chunk a key's 12-byte prefix names.
fn chunk_of(prefix: &[u8]) -> ChunkId {
    let word = |at: usize| -> [u8; 4] {
        let mut out = [0; 4];
        if let Some(bytes) = prefix.get(at..at + 4) {
            out.copy_from_slice(bytes);
        }
        out
    };
    let flip = |bytes: [u8; 4]| (u32::from_be_bytes(bytes) ^ 0x8000_0000).cast_signed();
    ChunkId {
        world: u32::from_be_bytes(word(0)),
        x: flip(word(4)),
        z: flip(word(8)),
    }
}

/// The writes of one chunk in the current format, and whether it was damaged (then its values
/// are written as they were). A pure function of the chunk, for the CPU pool.
fn convert(group: &Group) -> (Result<Batch, storage::Error>, bool) {
    let raw = || {
        let mut batch = Batch::with_capacity(group.values.len());
        for (part, value) in &group.values {
            batch.put(group.id.key(*part), value.clone());
        }
        batch
    };
    match again(group) {
        Ok(Some(batch)) => (Ok(batch), false),
        Ok(None) => (Ok(raw()), true),
        Err(e) => (Err(e), false),
    }
}

/// The writes of one chunk in the current format, or `None` if a value does not read.
fn again(group: &Group) -> Result<Option<Batch>, storage::Error> {
    let id = group.id;
    let mut column = None;
    let mut sub_chunks: [Option<SubChunkRecord>; SUB_CHUNKS] = Default::default();
    let mut extras = ChunkExtras::default();
    let mut next_pages = [0u16; 3];
    for (part, sealed) in &group.values {
        let key = ChunkKey {
            chunk: id,
            part: *part,
        }
        .to_bytes();
        let read = match *part {
            ChunkPart::Column => unseal::<ChunkRecord>(&key, sealed).map(|r| column = Some(r)),
            ChunkPart::SubChunk(index) => unseal::<SubChunkRecord>(&key, sealed).map(|r| {
                if let Some(slot) = sub_chunks.get_mut(usize::from(index)) {
                    *slot = Some(r);
                }
            }),
            ChunkPart::Extras(kind, page) => {
                add_page(&mut extras, &mut next_pages, kind, page, &key, sealed)
            }
        };
        if read.is_err() {
            return Ok(None);
        }
    }
    let found = sub_chunks.iter().flatten().count();
    let record = match (column, found) {
        (None, 0) => None,
        (Some(record), 0) => Some(record),
        (None, _) => Some(ChunkRecord {
            sub_chunks: Box::new(sub_chunks.map(|s| s.unwrap_or_else(empty_sub_chunk))),
        }),
        (Some(_), _) => return Ok(None),
    };
    let mut batch = Batch::with_capacity(SUB_CHUNKS + 3);
    if let Some(record) = record {
        // Custom blocks stay unknown here and keep their saved form, which is written back.
        let Some(chunk) = to_chunk(record, &Registry::default()) else {
            return Ok(None);
        };
        for (key, value) in seal_terrain(id, &chunk)? {
            if let Some(value) = value {
                batch.put(key, value);
            }
        }
    }
    for kind in ChunkKind::EXTRAS {
        for (key, value) in seal_pages(id, kind, extras.get(kind))? {
            batch.put(key, value);
        }
    }
    Ok(Some(batch))
}

/// Opens every file the move wrote and checks it whole, then counts its values against what was
/// written.
fn verify(staging: &Path, built: &Built) -> Result<(), crate::Error> {
    let differ = |what: &'static str| {
        crate::Error::Storage(format!("the moved {what} differ from the server file's").into())
    };
    let mut found = Counts::default();
    let open = |place: Place| -> Result<Storage, storage::Error> {
        let mut storage = Storage::open_checked(&place.file)?;
        let meta = super::stored_meta(&mut storage)?;
        if meta != built.meta {
            return Err(storage::Error::Malformed("a moved file holds another save"));
        }
        Ok(storage)
    };
    for name in &built.worlds {
        let mut storage = open(Place::world(staging, name))?;
        let mut chunks = 0u64;
        let mut last = None;
        storage.scan(Scan::Chunks, |key, _| {
            let prefix = key.get(..CHUNK_PREFIX).map(<[u8]>::to_vec);
            if prefix != last {
                chunks += 1;
                last = prefix;
            }
            Ok(())
        })?;
        found.chunks.insert(name.clone(), chunks);
        // Opening it took away the mark of a normal stop; the move closes it as one.
        storage.close()?;
    }
    for (name, count) in [
        (storage::PLAYERS_FILE, &mut found.players),
        (storage::ACCESS_FILE, &mut found.access),
        (storage::USER_FILE, &mut found.user),
    ] {
        let table = match name {
            storage::PLAYERS_FILE => Scan::Players,
            storage::ACCESS_FILE => Scan::Access,
            _ => Scan::User,
        };
        let mut storage = open(Place::data(staging, name))?;
        storage.scan(table, |_, _| {
            *count += 1;
            Ok(())
        })?;
        storage.close()?;
    }
    open(Place::data(staging, storage::MAIN_FILE))?.close()?;
    if found.chunks != built.counts.chunks {
        return Err(differ("chunks"));
    }
    if found != built.counts {
        return Err(differ("players, operators, bans or server data"));
    }
    Ok(())
}

/// Moves the files the move wrote in `staging` into `data_dir`, the main file last. A file
/// already there is replaced only if this move wrote it (a stop during an earlier move of the
/// same old file); any other stops the move.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the move runs once, before the server runs"
)]
fn place(data_dir: &Path, staging: &Path, built: &Built) -> Result<(), crate::Error> {
    let mut moves: Vec<(Place, Place)> = built
        .worlds
        .iter()
        .map(|name| (Place::world(staging, name), Place::world(data_dir, name)))
        .collect();
    for name in [
        storage::PLAYERS_FILE,
        storage::ACCESS_FILE,
        storage::USER_FILE,
        storage::MAIN_FILE,
    ] {
        moves.push((Place::data(staging, name), Place::data(data_dir, name)));
    }
    for (_, to) in &moves {
        if !std::fs::exists(&to.file).map_err(storage::Error::Io)? {
            continue;
        }
        let ours = Storage::open_replica(&to.file)
            .and_then(|mut s| s.read(&[Key::Moved]))
            .ok()
            .and_then(|mut v| v.pop())
            .and_then(Result::ok)
            .flatten()
            .is_some_and(|mark| mark == built.mark);
        if !ours {
            return Err(crate::Error::Storage(
                format!(
                    "{} is already there; the server file is not moved over it",
                    to.file.display()
                )
                .into(),
            ));
        }
    }
    std::fs::create_dir_all(data_dir.join(storage::WORLD_DIR)).map_err(storage::Error::Io)?;
    for (from, to) in moves {
        std::fs::rename(&from.file, &to.file).map_err(storage::Error::Io)?;
    }
    Ok(())
}
