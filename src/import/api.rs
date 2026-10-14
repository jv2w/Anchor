//! The import's run and report: [`Server::import_world`](crate::Server) reads a
//! Bedrock world folder through the reader ([`Source`]), upgrades its blocks and items
//! ([`upgrade`]), and writes it as a new persistent world of the server file, in one write
//! transaction.
//!
//! Each chunk becomes Anchor's own records: its terrain goes through [`save::to_chunk`] (palette
//! entries Anchor knows become its states, the server's custom blocks resolve, the rest stays as
//! unknown blocks with their NBT) and the sealing the save thread uses, so the bytes are those
//! Anchor writes itself; an all-air chunk is written too, or it would be generated. Legacy
//! numeric layers become paletted ones, and legacy block extra data their second layer (as PMMP
//! applies it, to sub-chunks of versions 0 to 7). Block entities keep their Bedrock compounds
//! whole, their items upgraded. Only blocks and block entities are imported: a chunk's biomes are
//! those the world's generator gives the chunk, and entities and pending block updates are left
//! out and counted.
//!
//! Chunks are read in (x, z) order a step at a time, then decoded and converted on the CPU pool,
//! which keeps input order, so the same source gives the same bytes whatever the thread count.
//! Any chunk that fails ends the import before its commit: the server file is left as it was, and
//! the source is never written.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::{info, warn};

use super::chunk::{self, Layer, LegacyBlock, LegacyLayer, Raw};
use super::{Catalog, Error, Source, upgrade};
use crate::custom::Registry;
use crate::nbt::{Compound, Value};
use crate::pool::Pool;
use crate::save::{self, WorldSpec};
use crate::storage::codec::{
    BlockLayer, ChunkRecord, MAX_EXTRA_ENTRIES, SubChunkRecord, WorldRecord,
};
use crate::storage::seal::{MAX_RECORD, seal};
use crate::storage::{Batch, ChunkKind, Import, Key, Place, Start, Storage};
use crate::world::{BlockPos, Chunk, ChunkPos, Entry, MIN_Y, Paletted, SLOTS, SUB_CHUNKS};

/// Most chunks read, then converted, per step: bounds the chunk data held at once.
const STEP: usize = 256;
/// Read chunk bytes after which a step ends, if it has not reached [`STEP`] chunks: a step holds
/// less than this plus one chunk's most (`chunk::MAX_CHUNK_BYTES`) before converting.
const STEP_BYTES: usize = 256 << 20;
/// The sub-chunk Y of sub-chunk index 0.
const LOWEST_SUB_CHUNK: i32 = MIN_Y >> 4;
/// The `SpawnY` Bedrock writes before the spawn has a height: the game puts it on the ground.
const SPAWN_Y_UNSET: i32 = 32_767;
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
    upgraded_nbt: u64,
    entities: u64,
    ignored_tags: BTreeMap<u8, u64>,
    nether_chunks: u64,
    end_chunks: u64,
    other_dimension_chunks: u64,
    players: u64,
    other_keys: u64,
    unknown_blocks: BTreeMap<String, u64>,
    unlisted_unknown_blocks: u64,
    inert_blocks: u64,
    placeholder_blocks: u64,
    spawn: BlockPos,
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

    /// The block states and item stacks in block entities that were upgraded.
    pub fn upgraded_nbt(&self) -> u64 {
        self.upgraded_nbt
    }

    /// The entities stored one per key (`actorprefix`, since 1.18.30; the key does not say
    /// the dimension, so those of the Nether and the End count too), left out: Anchor imports
    /// blocks and block entities only. Entities of older chunks are stored in a chunk tag (50),
    /// counted in [`ignored_tags`](Self::ignored_tags).
    pub fn entities(&self) -> u64 {
        self.entities
    }

    /// The chunk tags present but not imported, with how many chunks had each: biomes (43
    /// `Data3D`, 45 `Data2D`; an imported chunk has the biomes its world's generator gives it),
    /// entities of chunks before 1.18.30 (50), pending block updates (51), finalization (54),
    /// metadata hashes (63 to 65), tags a newer one replaced, ...
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

    /// Of the blocks Anchor does not know, those clients know (states of the protocol's block
    /// list): clients see them as themselves; the server gives them no behaviour beyond
    /// their collision, light and breaking (no drops).
    pub fn inert_blocks(&self) -> u64 {
        self.inert_blocks
    }

    /// Of the blocks Anchor does not know, those clients do not know either: clients see
    /// `minecraft:info_update` ("update!") in their place.
    pub fn placeholder_blocks(&self) -> u64 {
        self.placeholder_blocks
    }

    /// The vanilla world's spawn, from `level.dat`, which the imported world keeps as its spawn; a
    /// spawn without a height stands on the highest block of its column (the generator's height
    /// if the column is empty). Anchor takes no setting of `level.dat` (seed, game mode,
    /// difficulty, game rules): the world runs with the settings of the server.
    pub fn spawn(&self) -> BlockPos {
        self.spawn
    }
}

/// What a converted chunk adds to the server file and the report.
#[derive(Debug, Default)]
struct Converted {
    values: Vec<(Key, Vec<u8>)>,
    block_entities: u64,
    upgraded_nbt: u64,
    ignored: Vec<u8>,
    unknown: BTreeMap<String, u64>,
    /// Of the unknown blocks, those clients know.
    inert: u64,
    /// The highest block at the spawn column, if this is the spawn's chunk.
    spawn_height: Option<i32>,
}

/// The fixed inputs of every chunk's conversion.
#[derive(Debug)]
struct Target<'a> {
    /// The world's `chunks` key number.
    number: u32,
    /// The world, whose generator gives each chunk its biomes with `seed`.
    spec: &'a WorldSpec,
    seed: i64,
    custom: &'a Registry,
    /// The chunk and column of a spawn without a height.
    spawn: Option<(ChunkPos, u8, u8)>,
}

/// Imports the world folder `dir` as the persistent world `name` of `worlds` into the server file
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
    let spec = save::file_world(worlds, name).ok_or_else(|| Error::UnknownWorld {
        name: name.to_owned(),
    })?;
    if !crate::storage::is_world_file_name(name) {
        return Err(crate::Error::InvalidWorldName {
            name: name.to_owned(),
        });
    }
    if save::world_file_exists(data_dir, name)? {
        return Err(Error::WorldExists {
            name: name.to_owned(),
        }
        .into());
    }
    let mut source = Source::open(dir)?;
    let catalog = source.catalog()?;
    let world = *source.world();
    let place = Place::world(data_dir, name);
    let mut storage = Storage::open(&place, start)?;
    let pool = crate::runtime::cpu_pool(cpu)?;
    let imported = (|| {
        let (import, report) = prepare(
            &mut storage,
            &pool,
            &spec,
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
    let report = match imported {
        Ok(report) => report,
        Err(e) => {
            // The world file is the import's own: it goes whole.
            drop(storage);
            remove_file(&place.file);
            return Err(e);
        }
    };
    storage.close()?;
    info!(
        world = name,
        chunks = report.chunks,
        block_entities = report.block_entities,
        left_out_entities = report.entities,
        spawn = ?report.spawn,
        "world imported"
    );
    if !report.unknown_blocks.is_empty() {
        warn!(
            world = name,
            kinds = report.unknown_blocks.len(),
            shown_as_themselves = report.inert_blocks,
            shown_as_update_blocks = report.placeholder_blocks,
            unlisted_blocks = report.unlisted_unknown_blocks,
            "imported blocks Anchor does not implement are kept: clients see the ones they know \
             as themselves, with no server behaviour, and the rest as update! blocks"
        );
    }
    Ok(report)
}

/// Removes the world file of a failed import.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the import removes its own world file before the server runs"
)]
fn remove_file(file: &Path) {
    if let Err(e) = std::fs::remove_file(file) {
        warn!(file = %file.display(), error = %e, "could not remove the failed import's world file");
    }
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
    if storage
        .read(&[Key::World])?
        .pop()
        .transpose()?
        .flatten()
        .is_some()
    {
        return Err(Error::WorldExists {
            name: spec.name.clone(),
        }
        .into());
    }
    let [x, y, z] = world.spawn;
    // The seed the world runs with (its settings', not level.dat's), so the generated biomes are
    // those of the chunks the world generates itself.
    let seed = spec.seed.unwrap_or_else(|| save::default_seed(&spec.name));
    let target = Target {
        // The world's file holds it alone.
        number: 0,
        spec,
        seed,
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
        entities: catalog.entities,
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
        (Some(_), None) => spec.generator.spawn(seed).y,
    };
    report.spawn = BlockPos::new(x, spawn_y, z);
    // The world's state alone: its time and spawn. Its settings come from the server's.
    let key = Key::World;
    let record = WorldRecord {
        number: target.number,
        time: world.time,
        spawn: report.spawn,
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
            report.upgraded_nbt += c.upgraded_nbt;
            for tag in c.ignored {
                *report.ignored_tags.entry(tag).or_default() += 1;
            }
            let unknown: u64 = c.unknown.values().sum();
            report.inert_blocks += c.inert;
            report.placeholder_blocks += unknown - c.inert;
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
    let generated = Chunk::generate(target.spec.generator.as_ref(), target.seed, pos);
    let mut sub_chunks: Vec<SubChunkRecord> = (0..SUB_CHUNKS)
        .map(|i| SubChunkRecord {
            layers: Vec::new(),
            biomes: save::biome_record(&generated, i),
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
    let (unknown, inert) = unknown_blocks(&chunk);
    let mut converted = Converted {
        ignored: raw.ignored.clone(),
        unknown,
        inert,
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
    // A new world: the empty sub-chunks a value leaves out are not there to delete.
    converted.values = save::seal_terrain(id, &chunk)
        .map_err(too_large)?
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect();
    let mut block_entities = raw.block_entities.clone();
    for c in &mut block_entities {
        converted.upgraded_nbt += upgrade::nbt(c);
    }
    if block_entities.len() > MAX_EXTRA_ENTRIES {
        return Err(Error::TooLarge {
            what: "block entities in a chunk",
            limit: MAX_EXTRA_ENTRIES as u64,
        });
    }
    converted.values.extend(
        save::seal_pages(id, ChunkKind::BlockEntities, &block_entities).map_err(too_large)?,
    );
    converted.block_entities = u64::try_from(block_entities.len()).unwrap_or(u64::MAX);
    Ok(converted)
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

/// The chunk's unknown blocks by name, with how many there are, and how many of them are inert.
fn unknown_blocks(chunk: &Chunk) -> (BTreeMap<String, u64>, u64) {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut inert = 0u64;
    let unknown = chunk.unknown();
    if unknown.is_empty() {
        return (counts, inert);
    }
    let mut per_entry = vec![0u64; unknown.len()];
    for sub in (0..SUB_CHUNKS).filter_map(|i| chunk.sub_chunk(i)) {
        for layer in sub.layers() {
            if layer.palette().iter().all(|e| e.unknown_index().is_none()) {
                continue;
            }
            for offset in 0..SLOTS {
                let entry = layer.get(offset);
                if let Some(i) = entry.unknown_index()
                    && let Some(n) = usize::try_from(i).ok().and_then(|i| per_entry.get_mut(i))
                {
                    *n += 1;
                    inert += u64::from(matches!(entry, Entry::Inert(..)));
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
    (counts, inert)
}

#[cfg(test)]
mod tests;
