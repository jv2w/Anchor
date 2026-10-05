//! The import's run and report (PLAN.md 10.9): [`Server::import_world`](crate::Server) reads a
//! Bedrock world folder through the reader ([`Source`]), upgrades its blocks and items
//! ([`upgrade`]), and writes it as a new persistent world of the world file, in one write
//! transaction.
//!
//! Each chunk becomes Anchor's own records: its terrain goes through [`save::to_chunk`] (palette
//! entries Anchor knows become its states, the server's custom blocks resolve, the rest stays as
//! unknown blocks with their NBT) and the sealing the save thread uses, so the bytes are those
//! Anchor writes itself; an all-air chunk is written too, or it would be generated. Legacy
//! numeric layers become paletted ones, legacy block extra data their second layer (as PMMP
//! applies it, to sub-chunks of versions 0 to 7), and column biomes cubes (PMMP's ocean where a
//! chunk has none). Block entities and entities keep their Bedrock compounds, upgraded; pending
//! block updates become Anchor's scheduled updates, due as many ticks after the world file's
//! engine tick as they were due after the chunk's.
//!
//! Chunks are read in (x, z) order a step at a time, then decoded and converted on the CPU pool,
//! which keeps input order, so the same source gives the same bytes whatever the thread count.
//! Any chunk that fails ends the import before its commit: the world file is left as it was, and
//! the source is never written.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::{info, warn};

use super::chunk::{self, Biomes as RawBiomes, Layer, LegacyBlock, LegacyLayer, Raw};
use super::{Catalog, Error, Source, upgrade};
use crate::custom::Registry;
use crate::nbt::{Compound, List, Value};
use crate::pool::Pool;
use crate::save::{self, WorldSpec};
use crate::storage::codec::{
    Biomes, BlockLayer, ChunkRecord, MAX_EXTRA_ENTRIES, SubChunkRecord, WorldRecord,
};
use crate::storage::seal::{MAX_RECORD, seal};
use crate::storage::{Batch, ChunkKind, Import, Key, Start, Storage};
use crate::world::{Chunk, ChunkPos, Entry, Kind, MIN_Y, Paletted, SLOTS, SUB_CHUNKS};

/// Most chunks read, then converted, per step: bounds the chunk data held at once.
const STEP: usize = 256;
/// Read chunk bytes after which a step ends, if it has not reached [`STEP`] chunks: a step holds
/// less than this plus one chunk's most (`chunk::MAX_CHUNK_BYTES`) before converting.
const STEP_BYTES: usize = 256 << 20;
/// The sub-chunk Y of sub-chunk index 0.
const LOWEST_SUB_CHUNK: i32 = MIN_Y >> 4;
/// The `SpawnY` Bedrock writes before the spawn has a height: the game puts it on the ground.
const SPAWN_Y_UNSET: i32 = 32_767;
/// The biome of a chunk without biome data (PMMP `BiomeIds::OCEAN`).
const OCEAN: u32 = 0;
/// The newest sub-chunk version that takes legacy block extra data as its second layer.
const LAST_EXTRA_DATA_VERSION: u8 = 7;
/// Most unknown block names the report lists: the names come from the source's NBT. Blocks of
/// names past this or [`MAX_LISTED_UNKNOWN_BYTES`] are counted apart
/// ([`Report::unlisted_unknown_blocks`]).
const MAX_LISTED_UNKNOWN: usize = 4_096;
/// Most bytes of the unknown block names the report lists: a name may be an NBT string's 32 KiB.
const MAX_LISTED_UNKNOWN_BYTES: usize = 1 << 20;

/// What [`Server::import_world`](crate::Server::import_world) imported and what it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    chunks: u64,
    block_entities: u64,
    entities: u64,
    scheduled_updates: u64,
    upgraded_nbt: u64,
    ignored_tags: BTreeMap<u8, u64>,
    nether_chunks: u64,
    end_chunks: u64,
    other_dimension_chunks: u64,
    players: u64,
    other_keys: u64,
    unknown_blocks: BTreeMap<String, u64>,
    unlisted_unknown_blocks: u64,
}

impl Report {
    /// The overworld chunks imported.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// The block entities imported.
    pub fn block_entities(&self) -> u64 {
        self.block_entities
    }

    /// The entities imported.
    pub fn entities(&self) -> u64 {
        self.entities
    }

    /// The pending block updates imported as scheduled updates.
    pub fn scheduled_updates(&self) -> u64 {
        self.scheduled_updates
    }

    /// The block states and item stacks in block entities and entities that were upgraded.
    pub fn upgraded_nbt(&self) -> u64 {
        self.upgraded_nbt
    }

    /// The chunk tags present but not imported (finalization, metadata hashes, tags a newer one
    /// replaced, ...), with how many chunks had each.
    pub fn ignored_tags(&self) -> &BTreeMap<u8, u64> {
        &self.ignored_tags
    }

    /// The Nether chunks, left out: Anchor runs the overworld only.
    pub fn nether_chunks(&self) -> u64 {
        self.nether_chunks
    }

    /// The End chunks, left out.
    pub fn end_chunks(&self) -> u64 {
        self.end_chunks
    }

    /// The chunks of other dimension numbers, left out.
    pub fn other_dimension_chunks(&self) -> u64 {
        self.other_dimension_chunks
    }

    /// The player keys, left out: Bedrock files players by device, with no Xbox user ID to
    /// match.
    pub fn players(&self) -> u64 {
        self.players
    }

    /// The other world-level keys (scoreboard, maps, villages, ...), left out.
    pub fn other_keys(&self) -> u64 {
        self.other_keys
    }

    /// Blocks Anchor does not know, by name (`legacy:<id>:<meta>` for a legacy value without
    /// one), with how many there are. They are kept as unknown blocks with their NBT. The first
    /// 4,096 names found, up to 1 MiB of them, are listed; blocks of later names are counted in
    /// [`unlisted_unknown_blocks`](Self::unlisted_unknown_blocks).
    pub fn unknown_blocks(&self) -> &BTreeMap<String, u64> {
        &self.unknown_blocks
    }

    /// Blocks Anchor does not know whose names [`unknown_blocks`](Self::unknown_blocks) does not
    /// list, past its caps. They are kept as unknown blocks too.
    pub fn unlisted_unknown_blocks(&self) -> u64 {
        self.unlisted_unknown_blocks
    }
}

/// What a converted chunk adds to the world file and the report.
#[derive(Debug, Default)]
struct Converted {
    values: Vec<(Key, Vec<u8>)>,
    block_entities: u64,
    entities: u64,
    scheduled_updates: u64,
    upgraded_nbt: u64,
    ignored: Vec<u8>,
    unknown: BTreeMap<String, u64>,
    /// The highest block at the spawn column, if this is the spawn's chunk.
    spawn_height: Option<i32>,
}

/// The fixed inputs of every chunk's conversion.
#[derive(Debug)]
struct Target<'a> {
    /// The world's `chunks` key number.
    number: u32,
    /// The world file's engine tick, which scheduled updates count from.
    tick: u64,
    custom: &'a Registry,
    /// The chunk and column of a spawn without a height.
    spawn: Option<(ChunkPos, u8, u8)>,
}

/// Imports the world folder `dir` as the persistent world `name` of `worlds` into the world file
/// of `data_dir`, opened as `start` says, with `custom` resolving custom blocks, on a
/// CPU pool of `cpu` threads that is joined before returning.
pub(crate) fn run(
    data_dir: &Path,
    start: Start,
    worlds: &[WorldSpec],
    cpu: usize,
    custom: &Registry,
    dir: &Path,
    name: &str,
) -> crate::Result<Report> {
    let spec = worlds
        .iter()
        .find(|w| w.name == name && w.kind == Kind::Persistent)
        .ok_or_else(|| Error::UnknownWorld {
            name: name.to_owned(),
        })?;
    let mut source = Source::open(dir)?;
    let catalog = source.catalog()?;
    let world = *source.world();
    let mut storage = Storage::open(data_dir, start)?;
    let pool = crate::runtime::cpu_pool(cpu)?;
    let imported = (|| {
        let (import, report) = prepare(
            &mut storage,
            &pool,
            spec,
            custom,
            &catalog,
            &world,
            |x, z| source.read(x, z),
            chunk::decode,
            chunk::Entries::bytes,
        )?;
        // The source must still be what was read, or the world would mix two states of it.
        source.unchanged()?;
        import.commit()?;
        Ok::<_, crate::Error>(report)
    })();
    if !pool.shutdown() {
        warn!("the import's CPU pool threads did not all end");
    }
    let report = imported?;
    storage.close()?;
    info!(
        world = name,
        chunks = report.chunks,
        block_entities = report.block_entities,
        entities = report.entities,
        scheduled_updates = report.scheduled_updates,
        "world imported"
    );
    if !report.unknown_blocks.is_empty() {
        warn!(
            world = name,
            kinds = report.unknown_blocks.len(),
            blocks = report.unknown_blocks.values().sum::<u64>(),
            unlisted_blocks = report.unlisted_unknown_blocks,
            "imported blocks Anchor does not know are kept as unknown blocks"
        );
    }
    Ok(report)
}

/// Writes the world `spec` names into `storage` from `catalog`'s chunks, read with `read` (`size`
/// tells the bytes a read holds) and decoded with `decode`, and the world record from `world`,
/// without committing: the caller commits the returned [`Import`] once the source proved
/// unchanged. Fails with [`Error::WorldExists`] if the file holds the world already.
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: the reader's parts come in apart so tests can give chunks directly"
)]
fn prepare<E: Sync>(
    storage: &mut Storage,
    pool: &Pool,
    spec: &WorldSpec,
    custom: &Registry,
    catalog: &Catalog,
    world: &super::level_dat::WorldData,
    read: impl FnMut(i32, i32) -> super::Result<E>,
    decode: impl Fn(&E) -> super::Result<Raw> + Sync,
    size: impl Fn(&E) -> usize,
) -> crate::Result<(Import, Report)> {
    let stored = save::stored_worlds(storage)?;
    if stored.iter().any(|(n, _)| *n == spec.name) {
        return Err(Error::WorldExists {
            name: spec.name.clone(),
        }
        .into());
    }
    let [x, y, z] = world.spawn;
    let target = Target {
        number: save::next_world_number(stored.iter().map(|(_, r)| r.number)),
        tick: save::stored_tick(storage)?,
        custom,
        spawn: (y == SPAWN_Y_UNSET).then(|| {
            let column = |c: i32| u8::try_from(c & 15).unwrap_or_default();
            (ChunkPos::new(x >> 4, z >> 4), column(x), column(z))
        }),
    };
    let mut report = Report {
        nether_chunks: catalog.nether_chunks,
        end_chunks: catalog.end_chunks,
        other_dimension_chunks: catalog.other_dimension_chunks,
        players: catalog.players,
        other_keys: catalog.other_keys,
        ..Report::default()
    };
    let mut import = storage.begin_import()?;
    let height = write_chunks(
        &mut import,
        pool,
        &target,
        &catalog.chunks,
        read,
        decode,
        size,
        &mut report,
    )?;
    let spawn_y = match (target.spawn, height) {
        (None, _) => y,
        (Some(_), Some(h)) => h.saturating_add(1),
        (Some(_), None) => spec.generator.spawn(world.seed).y,
    };
    let key = Key::World(spec.name.clone());
    let record = WorldRecord {
        number: target.number,
        seed: world.seed,
        spawn: [x, spawn_y, z],
        generator: spec.generator.name().to_owned(),
        time: world.time,
        time_stopped: false,
        difficulty: None,
        weather: crate::world::Weather::Clear,
    };
    let mut batch = Batch::with_capacity(1);
    batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
    import.write(&batch)?;
    Ok((import, report))
}

/// Reads the chunks at `positions` with `read`, a step at a time ([`STEP`] chunks or
/// [`STEP_BYTES`] by `size`, whichever comes first), decodes and converts each step on `pool` and
/// writes it into `import`, adding to `report`. Returns the highest block at the spawn column if
/// the target asks for it and the spawn's chunk has one.
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: the reader's parts come in apart so tests can give chunks directly"
)]
fn write_chunks<E: Sync>(
    import: &mut Import,
    pool: &Pool,
    target: &Target<'_>,
    positions: &[(i32, i32)],
    mut read: impl FnMut(i32, i32) -> super::Result<E>,
    decode: impl Fn(&E) -> super::Result<Raw> + Sync,
    size: impl Fn(&E) -> usize,
    report: &mut Report,
) -> crate::Result<Option<i32>> {
    let mut spawn_height = None;
    // Bytes of the names `report.unknown_blocks` lists.
    let mut listed_bytes = 0usize;
    let mut positions = positions.iter();
    while positions.len() > 0 {
        let mut read_step = Vec::with_capacity(positions.len().min(STEP));
        let mut bytes = 0usize;
        while read_step.len() < STEP
            && bytes < STEP_BYTES
            && let Some(&(x, z)) = positions.next()
        {
            let entries = read(x, z)?;
            bytes = bytes.saturating_add(size(&entries));
            read_step.push((ChunkPos::new(x, z), entries));
        }
        let converted = pool.map(&read_step, |(pos, entries)| {
            let raw = decode(entries)?;
            convert(*pos, &raw, target).map_err(|e| Error::Chunk {
                x: pos.x,
                z: pos.z,
                source: Box::new(e),
            })
        });
        let mut batch = Batch::with_capacity(read_step.len() * (SUB_CHUNKS + 3));
        for c in converted {
            let c = c?;
            report.chunks += 1;
            report.block_entities += c.block_entities;
            report.entities += c.entities;
            report.scheduled_updates += c.scheduled_updates;
            report.upgraded_nbt += c.upgraded_nbt;
            for tag in c.ignored {
                *report.ignored_tags.entry(tag).or_default() += 1;
            }
            for (name, count) in c.unknown {
                if let Some(listed) = report.unknown_blocks.get_mut(&name) {
                    *listed += count;
                } else if report.unknown_blocks.len() < MAX_LISTED_UNKNOWN
                    && listed_bytes + name.len() <= MAX_LISTED_UNKNOWN_BYTES
                {
                    listed_bytes += name.len();
                    report.unknown_blocks.insert(name, count);
                } else {
                    report.unlisted_unknown_blocks += count;
                }
            }
            spawn_height = spawn_height.or(c.spawn_height);
            for (key, value) in c.values {
                batch.put(key, value);
            }
        }
        import.write(&batch)?;
    }
    Ok(spawn_height)
}

/// Converts one chunk into its sealed values: a pure function of its inputs, for the CPU pool.
fn convert(pos: ChunkPos, raw: &Raw, target: &Target<'_>) -> super::Result<Converted> {
    let biomes = biomes(&raw.biomes);
    let mut sub_chunks: Vec<SubChunkRecord> = biomes
        .into_iter()
        .map(|biomes| SubChunkRecord {
            layers: Vec::new(),
            biomes,
        })
        .collect();
    for section in &raw.sections {
        let slot = usize::try_from(i32::from(section.y) - LOWEST_SUB_CHUNK)
            .ok()
            .and_then(|i| sub_chunks.get_mut(i))
            .ok_or(Error::Malformed("a sub-chunk is outside the world height"))?;
        slot.layers = section.layers.iter().map(block_layer).collect();
        if section.version <= LAST_EXTRA_DATA_VERSION
            && let Some(extra) = extra_layer(&raw.legacy_blocks, section.y)
        {
            slot.layers.truncate(1);
            slot.layers.push(extra);
        }
    }
    let sub_chunks: Box<[SubChunkRecord; SUB_CHUNKS]> = sub_chunks
        .into_boxed_slice()
        .try_into()
        .map_err(|_| Error::Malformed("a chunk has not one biome cube per sub-chunk"))?;
    let chunk = save::to_chunk(ChunkRecord { sub_chunks }, target.custom)
        .ok_or(Error::Malformed("the terrain does not make a valid chunk"))?;
    let mut converted = Converted {
        ignored: raw.ignored.clone(),
        unknown: unknown_blocks(&chunk),
        ..Converted::default()
    };
    if let Some((spawn, x, z)) = target.spawn
        && spawn == pos
    {
        converted.spawn_height = chunk.highest_block(x, z);
    }
    let too_large = |_| Error::TooLarge {
        what: "a chunk record",
        limit: MAX_RECORD as u64,
    };
    let id = save::chunk_id(target.number, pos);
    converted.values = save::seal_terrain(id, &chunk).map_err(too_large)?;
    let mut block_entities = raw.block_entities.clone();
    let mut entities = raw.entities.clone();
    for c in block_entities.iter_mut().chain(entities.iter_mut()) {
        converted.upgraded_nbt += upgrade::nbt(c);
    }
    let updates = scheduled_updates(raw.pending_ticks.as_ref(), target.tick)?;
    for (kind, list) in [
        (ChunkKind::BlockEntities, &block_entities),
        (ChunkKind::Entities, &entities),
        (ChunkKind::ScheduledUpdates, &updates),
    ] {
        if list.len() > MAX_EXTRA_ENTRIES {
            return Err(Error::TooLarge {
                what: "extras of one kind in a chunk",
                limit: MAX_EXTRA_ENTRIES as u64,
            });
        }
        converted
            .values
            .extend(save::seal_pages(id, kind, list).map_err(too_large)?);
    }
    converted.block_entities = u64::try_from(block_entities.len()).unwrap_or(u64::MAX);
    converted.entities = u64::try_from(entities.len()).unwrap_or(u64::MAX);
    converted.scheduled_updates = u64::try_from(updates.len()).unwrap_or(u64::MAX);
    Ok(converted)
}

/// One biome cube per sub-chunk: the stored cubes, column biomes stretched over every sub-chunk
/// (PMMP `extrapolate3DBiomes`), or ocean for a chunk without biomes, as PMMP fills it.
fn biomes(raw: &RawBiomes) -> Vec<Biomes> {
    let cube = |p: &Paletted<u32>| Biomes {
        bits: p.bits(),
        words: p.words().to_vec(),
        palette: p.palette().to_vec(),
    };
    match raw {
        RawBiomes::Paletted(cubes) => cubes.clone(),
        RawBiomes::Missing => vec![cube(&Paletted::uniform(OCEAN)); SUB_CHUNKS],
        RawBiomes::Columns(columns) => {
            let mut p = Paletted::uniform(OCEAN);
            for offset in 0..SLOTS {
                // Slot offset x << 8 | z << 4 | y; the columns are indexed z << 4 | x.
                let (x, z) = (usize::from(offset >> 8), usize::from((offset >> 4) & 15));
                let biome = columns.get(z << 4 | x).copied().unwrap_or_default();
                p.set(offset, u32::from(biome));
            }
            p.compact();
            vec![cube(&p); SUB_CHUNKS]
        }
    }
}

/// A stored layer as Anchor's, each palette entry upgraded ([`upgrade::state`]).
fn block_layer(layer: &Layer) -> BlockLayer {
    match layer {
        Layer::Paletted(l) => BlockLayer {
            bits: l.bits,
            words: l.words.clone(),
            palette: l
                .palette
                .iter()
                .map(|nbt| upgrade::state(nbt).unwrap_or_else(|| nbt.clone()))
                .collect(),
        },
        Layer::Legacy(l) => legacy_layer(l),
    }
}

/// A legacy layer as a paletted one.
fn legacy_layer(layer: &LegacyLayer) -> BlockLayer {
    let mut cube = Paletted::uniform(0u16);
    for offset in 0..SLOTS {
        let (x, z, y) = (
            usize::from(offset >> 8),
            usize::from((offset >> 4) & 15),
            usize::from(offset & 15),
        );
        let (id, meta) = layer.get(x, y, z);
        cube.set(offset, legacy_value(id, meta));
    }
    paletted_layer(&cube)
}

/// The legacy block extra data of sub-chunk `y` as a layer over air, if it has any (PMMP
/// `deserializeLegacyExtraData`).
fn extra_layer(blocks: &[LegacyBlock], y: i8) -> Option<BlockLayer> {
    let mut cube = Paletted::uniform(0u16);
    let mut any = false;
    for b in blocks
        .iter()
        .filter(|b| i32::from(b.y >> 4) == i32::from(y))
    {
        let offset = (u16::from(b.x & 15) << 8) | (u16::from(b.z & 15) << 4) | u16::from(b.y & 15);
        cube.set(offset, legacy_value(b.id, b.meta));
        any = true;
    }
    any.then(|| paletted_layer(&cube))
}

/// A legacy ID and meta (0 to 15) as one number, `id << 4 | meta`; air is 0.
fn legacy_value(id: u8, meta: u8) -> u16 {
    u16::from(id) << 4 | u16::from(meta & 15)
}

/// A layer from a cube of [`legacy_value`]s, each upgraded: its state, or for a value the legacy
/// table lacks, `{id, val}` as it was (an unknown block).
fn paletted_layer(cube: &Paletted<u16>) -> BlockLayer {
    BlockLayer {
        bits: cube.bits(),
        words: cube.words().to_vec(),
        palette: cube
            .palette()
            .iter()
            .map(|&value| {
                let (id, meta) = (value >> 4, u8::try_from(value & 15).unwrap_or_default());
                upgrade::legacy(id, meta).unwrap_or_else(|| {
                    Compound::from([
                        ("id".to_owned(), Value::Short(id.cast_signed())),
                        ("val".to_owned(), Value::Short(i16::from(meta))),
                    ])
                })
            })
            .collect(),
    }
}

/// The chunk's unknown blocks by name, with how many there are.
fn unknown_blocks(chunk: &Chunk) -> BTreeMap<String, u64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let unknown = chunk.unknown();
    if unknown.is_empty() {
        return counts;
    }
    let mut per_entry = vec![0u64; unknown.len()];
    for sub in (0..SUB_CHUNKS).filter_map(|i| chunk.sub_chunk(i)) {
        for layer in sub.layers() {
            if !layer
                .palette()
                .iter()
                .any(|e| matches!(e, Entry::Unknown(_)))
            {
                continue;
            }
            for offset in 0..SLOTS {
                if let Entry::Unknown(i) = layer.get(offset)
                    && let Some(n) = usize::try_from(i).ok().and_then(|i| per_entry.get_mut(i))
                {
                    *n += 1;
                }
            }
        }
    }
    for (nbt, count) in unknown.iter().zip(per_entry) {
        let name = match (nbt.get("name"), nbt.get("id"), nbt.get("val")) {
            (Some(Value::String(name)), _, _) => name.clone(),
            (None, Some(Value::Short(id)), Some(Value::Short(meta))) => {
                format!("legacy:{id}:{meta}")
            }
            _ => "?".to_owned(),
        };
        *counts.entry(name).or_default() += count;
    }
    counts
}

/// Bedrock's pending block updates (`PendingTicks`: `currentTick`, `tickList` of `x`, `y`, `z`,
/// `time`) as Anchor's scheduled updates (`x`, `y`, `z` and the engine tick `time` they are due
/// at): due `time - currentTick` ticks (at least 0) after the world file's engine tick `tick`.
fn scheduled_updates(pending: Option<&Compound>, tick: u64) -> super::Result<Vec<Compound>> {
    let Some(pending) = pending else {
        return Ok(Vec::new());
    };
    let long = |v: Option<&Value>| match v {
        Some(Value::Int(t)) => Some(i64::from(*t)),
        Some(Value::Long(t)) => Some(*t),
        _ => None,
    };
    let current = long(pending.get("currentTick"))
        .ok_or(Error::Malformed("pending ticks without their tick"))?;
    let list = match pending.get("tickList") {
        Some(Value::List(List::Compound(list))) => list.as_slice(),
        Some(Value::List(List::End)) | None => &[],
        Some(_) => {
            return Err(Error::Malformed(
                "pending ticks are not a list of compounds",
            ));
        }
    };
    list.iter()
        .map(|entry| {
            let int = |k: &str| match entry.get(k) {
                Some(Value::Int(v)) => Ok(Value::Int(*v)),
                _ => Err(Error::Malformed("a pending tick without its position")),
            };
            let time = long(entry.get("time"))
                .ok_or(Error::Malformed("a pending tick without its time"))?;
            let delay = u64::try_from(time.saturating_sub(current)).unwrap_or(0);
            let due = i64::try_from(tick.saturating_add(delay)).unwrap_or(i64::MAX);
            Ok(Compound::from([
                ("x".to_owned(), int("x")?),
                ("y".to_owned(), int("y")?),
                ("z".to_owned(), int("z")?),
                ("time".to_owned(), Value::Long(due)),
            ]))
        })
        .collect()
}

#[cfg(test)]
mod tests;
