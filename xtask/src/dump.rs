//! `cargo xtask dump [--check]`: rebuilds the committed block and item data from
//! the pinned external checkouts in `.cache/`. It converts the reference lists of dragonfly and
//! `CloudburstMC` with `xtask/go/reference` into `xtask/data/reference/` first, as the PMMP run
//! reads every reference block state with PMMP's deserializer; then it runs PMMP's own
//! registries through `xtask/php/dump.php` with the PMMP PHP binary into `xtask/data/pmmp/`. It
//! also copies `CloudburstMC`'s biome definitions, entity types and block shapes into
//! `xtask/data/reference/`, where the Go tool also writes its jigsaw structure data and entity
//! property definitions as network NBT and this command writes the per-field decode limits from
//! Mojang's protocol schemas (`limits`). The block and item upgrade schemas and their golden files
//! go to `xtask/data/upgrade/` (see `upgrade`); then BDS's per-state block physics of every
//! reference state goes to `block_physics.json` (for the block states Anchor shows but does not
//! implement), which reads the copied 1.26.60 block schema. With `--check` it writes into
//! `target/` and fails if a committed file differs. `verify` does not run it: it needs the
//! checkouts, the PHP binary and Go, and its inputs never change (`gen --check` checks the code
//! made from the committed data instead).
//!
//! It is also xtask's file layer: [`read`], [`read_if_exists`], [`write`], [`create_dir`] and
//! [`is_file`] give every command the same path-carrying errors, and `upgrade::sha256` is the one
//! hash.

#[path = "../../src/block/hash.rs"]
pub mod hash;
pub(crate) mod upgrade;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::r#gen::biomes::{self, Biome, Colour};
use crate::r#gen::upgrade::SCHEMA_1_26_60;

pub(crate) mod limits;

/// `CloudburstMC`'s biome definitions without chunk generation data: the content BDS sends.
/// `biome_definitions.json` holds generation data BDS does not send.
const BIOMES: &str = "stripped_biome_definitions.json";
/// The block physics this command writes from [`CLOUDBURST_BLOCKS`].
pub(crate) const BLOCK_PHYSICS: &str = "block_physics.json";
/// Blocks new in 1.26.60 that `CloudburstMC`'s per-state data lacks, with the block whose
/// physics they take: the closest block of the same shape.
const BORROWED_1_26_60: [(&str, &str); 3] = [
    ("minecraft:icicle", "minecraft:pointed_dripstone"),
    ("minecraft:ice_crystal", "minecraft:amethyst_cluster"),
    ("minecraft:disc_press", "minecraft:stone"),
];
pub(crate) const CLOUDBURST: (&str, &str) = (
    ".cache/cloudburst-data",
    "293c848098cc636a2758937fb6610b8bed1f12c5",
);
/// `CloudburstMC`'s per-state block data, which BDS dumps.
const CLOUDBURST_BLOCKS: &str = "blocks.json";
const DRAGONFLY: (&str, &str) = (
    ".cache/dragonfly",
    "d86aefd9ff9b6a357d6dfadab2093af811545256",
);
/// `CloudburstMC` files Anchor sends byte for byte: `AvailableActorIdentifiers`
/// and the shapes of `VoxelShapes`.
const ENTITY_IDENTIFIERS: &str = "entity_identifiers.dat";
/// The PMMP PHP binary (pmmp/PHP-Binaries php-build-524, PHP 8.4 PM5 for Windows): plain PHP
/// lacks the extensions PMMP's registries load.
const PHP: &str = ".cache/php-pm5/bin/php/php.exe";
/// The pinned checkouts, relative to the repository root, and their commits.
const PMMP: (&str, &str) = (".cache/pmmp", "6a7cc02e9dff59b69241aa0bcffdb9903ce86beb");
pub(crate) const PMMP_BLOCKS: &str = "blocks.json";
pub(crate) const PMMP_CREATIVE: &str = "creative.json";
pub(crate) const PMMP_DESERIALIZED: &str = "deserialized.json";
/// Output directories and their files, relative to the repository root.
pub(crate) const PMMP_DIR: &str = "xtask/data/pmmp";
const PMMP_FILES: &[&str] = &[
    PMMP_BLOCKS,
    PMMP_DESERIALIZED,
    PMMP_ITEMS,
    PMMP_CREATIVE,
    PMMP_RECIPES,
    PMMP_TABLES,
];
pub(crate) const PMMP_ITEMS: &str = "items.json";
pub(crate) const PMMP_RECIPES: &str = "recipes.json";
pub(crate) const PMMP_TABLES: &str = "tables.json";
/// Mojang's bedrock-protocol-docs at the v1.26.60 release (protocol 2225).
const PROTOCOL_DOCS: (&str, &str) = (
    ".cache/bedrock-protocol-docs",
    "0879ab5c4c70be1bd520e32a8c49ea4a155616d6",
);
pub(crate) const REFERENCE_BLOCKS: &str = "blocks.json";
pub(crate) const REFERENCE_DIR: &str = "xtask/data/reference";
const REFERENCE_FILES: &[&str] = &[
    REFERENCE_BLOCKS,
    REFERENCE_ITEMS,
    REFERENCE_ITEM_BLOCKS,
    "jigsaw_structure_data.dat",
    "entity_properties.dat",
    biomes::FILE,
    ENTITY_IDENTIFIERS,
    VOXEL_SHAPES,
    BLOCK_PHYSICS,
    limits::FILE,
];
pub(crate) const REFERENCE_ITEM_BLOCKS: &str = "item_blocks.json";
pub(crate) const REFERENCE_ITEMS: &str = "items.json";
const VOXEL_SHAPES: &str = "voxel_shapes.json";

/// Runs `dump`; with `check`, compares instead of writing. Returns the differing files on a
/// failed check.
pub(crate) fn run(root: &Path, check: bool) -> Result<Vec<String>, String> {
    for (dir, commit) in [
        PMMP,
        DRAGONFLY,
        CLOUDBURST,
        PROTOCOL_DOCS,
        upgrade::BLOCKS,
        upgrade::ITEMS,
    ] {
        pinned(root, dir, commit)?;
    }
    if !is_dir(&root.join(PMMP.0).join("vendor")) {
        return Err(format!(
            "{}/vendor is missing; run `composer install --no-dev --ignore-platform-reqs` there",
            PMMP.0
        ));
    }
    let (pmmp_out, reference_out, upgrade_out) = if check {
        let dir = root.join("target").join("xtask-dump");
        (dir.join("pmmp"), dir.join("reference"), dir.join("upgrade"))
    } else {
        (
            root.join(PMMP_DIR),
            root.join(REFERENCE_DIR),
            root.join(upgrade::OUT),
        )
    };
    create_dir(&pmmp_out)?;
    create_dir(&reference_out)?;

    // The reference list comes first: the PMMP dump reads its block states with PMMP.
    let status = Command::new("go")
        .args(["run", "./reference", "-dragonfly"])
        .arg(root.join(DRAGONFLY.0))
        .arg("-cloudburst")
        .arg(root.join(CLOUDBURST.0))
        .arg("-out")
        .arg(&reference_out)
        .current_dir(root.join("xtask").join("go"))
        .status()
        .map_err(|e| format!("cannot run `go` ({e}); install Go and put it on PATH"))?;
    if !status.success() {
        return Err(format!("the reference conversion failed ({status})"));
    }
    let status = Command::new(root.join(PHP))
        .args(["-d", "memory_limit=4G"])
        .arg(root.join("xtask").join("php").join("dump.php"))
        .arg(&pmmp_out)
        .arg(reference_out.join(REFERENCE_BLOCKS))
        .current_dir(root.join(PMMP.0))
        .status()
        .map_err(|e| format!("cannot run {PHP} ({e}); see .claude/rules/data.md"))?;
    if !status.success() {
        return Err(format!("the PMMP dump failed ({status})"));
    }
    copy_biomes(root, &reference_out)?;
    for file in [ENTITY_IDENTIFIERS, VOXEL_SHAPES] {
        let bytes = read(&root.join(CLOUDBURST.0).join(file))?;
        write(&reference_out.join(file), &bytes)?;
    }
    let limits = limits::generate(&root.join(PROTOCOL_DOCS.0).join("json"))?;
    write(&reference_out.join(limits::FILE), limits.as_bytes())?;

    let upgrade_files = upgrade::run(root, &reference_out.join(REFERENCE_BLOCKS), &upgrade_out)?;
    upgrade::check_no_leftovers(&root.join(upgrade::OUT), &upgrade_files)?;
    copy_block_physics(root, &reference_out, &upgrade_out)?;

    let mut differ = Vec::new();
    if check {
        for file in &upgrade_files {
            let committed = root.join(upgrade::OUT).join(file);
            if read_if_exists(&committed)? != Some(read(&upgrade_out.join(file))?) {
                differ.push(format!("{}/{file}", upgrade::OUT));
            }
        }
        for (committed, made, files) in [
            (PMMP_DIR, &pmmp_out, PMMP_FILES),
            (REFERENCE_DIR, &reference_out, REFERENCE_FILES),
        ] {
            for file in files {
                let have = read_if_exists(&root.join(committed).join(file))?;
                if have != Some(read(&made.join(file))?) {
                    differ.push(format!("{committed}/{file}"));
                }
            }
        }
    }
    Ok(differ)
}

/// One biome of `CloudburstMC`'s file, by name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CloudburstBiome {
    id: Option<Json>,
    temperature: f64,
    downfall: f64,
    red_spore_density: f64,
    blue_spore_density: f64,
    ash_density: f64,
    white_ash_density: f64,
    foliage_snow: f64,
    depth: f64,
    scale: f64,
    map_water_color: Colour,
    rain: bool,
    tags: Vec<String>,
    chunk_gen_data: Option<Json>,
}

/// Writes `CloudburstMC`'s biome definitions to `out` as [`biomes::FILE`], one biome per line in
/// name order. Fails on a field the packet has no place for that is not null or zero: BDS sends no
/// ID (-1) and no chunk generation data, and the densities have no packet field.
fn copy_biomes(root: &Path, out: &Path) -> Result<(), String> {
    let path = root.join(CLOUDBURST.0).join(BIOMES);
    let all: BTreeMap<String, CloudburstBiome> =
        serde_json::from_slice(&read(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = "[\n".to_owned();
    for (i, (name, b)) in all.into_iter().enumerate() {
        let densities = [
            b.red_spore_density,
            b.blue_spore_density,
            b.ash_density,
            b.white_ash_density,
        ];
        if b.id.is_some() || b.chunk_gen_data.is_some() || densities.iter().any(|&d| d != 0.0) {
            return Err(format!(
                "{BIOMES}: {name} has an ID, chunk generation data or a density, which the packet \
                 does not carry"
            ));
        }
        let biome = Biome {
            name,
            temperature: b.temperature,
            downfall: b.downfall,
            foliage_snow: b.foliage_snow,
            depth: b.depth,
            scale: b.scale,
            map_water_colour: b.map_water_color,
            rain: b.rain,
            tags: b.tags,
        };
        if i > 0 {
            text.push_str(",\n");
        }
        text.push_str(&serde_json::to_string(&biome).map_err(|e| format!("{BIOMES}: {e}"))?);
    }
    text.push_str("\n]\n");
    write(&out.join(biomes::FILE), text.as_bytes())
}

/// One block state of `CloudburstMC`'s per-state data: the fields Anchor reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudburstBlock {
    block_state_hash: i64,
    hardness: f64,
    light_emission: u8,
    light_dampening: u8,
    collision_shape: Vec<[f64; 6]>,
}

/// One state of the reference list: the fields the physics rule reads.
#[derive(Deserialize)]
struct ReferenceState {
    name: String,
    states: BTreeMap<String, Json>,
    network_id: u32,
}

/// The part of a block upgrade schema the physics rule reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddingSchema {
    added_properties: BTreeMap<String, BTreeMap<String, Json>>,
}

/// Writes the physics of every reference block state (BDS's values from `CloudburstMC`'s
/// per-state data, see [`reference_physics`]) to `reference_out` as [`BLOCK_PHYSICS`]: `shapes`,
/// the distinct collision box lists in order of first use, and `states`, one
/// `[network ID, hardness, light emission, light dampening, shape]` per line in network ID order.
fn copy_block_physics(root: &Path, reference_out: &Path, upgrade_out: &Path) -> Result<(), String> {
    let reference: Vec<ReferenceState> = read_json_file(&reference_out.join(REFERENCE_BLOCKS))?;
    let schema: AddingSchema = read_json_file(
        &upgrade_out
            .join(upgrade::BLOCK_SCHEMAS)
            .join(SCHEMA_1_26_60),
    )?;
    let bds: Vec<CloudburstBlock> =
        read_json_file(&root.join(CLOUDBURST.0).join(CLOUDBURST_BLOCKS))?;
    let physics = reference_physics(&reference, &schema.added_properties, &bds)?;
    let mut shapes: Vec<&[[f64; 6]]> = Vec::new();
    let mut rows = Vec::with_capacity(physics.by_id.len());
    for (id, b) in physics.by_id {
        let found = shapes
            .iter()
            .position(|s| *s == b.collision_shape.as_slice());
        let shape = if let Some(i) = found {
            i
        } else {
            shapes.push(&b.collision_shape);
            shapes.len() - 1
        };
        let row = (id, b.hardness, b.light_emission, b.light_dampening, shape);
        rows.push(serde_json::to_string(&row).map_err(|e| format!("{BLOCK_PHYSICS}: {e}"))?);
    }
    let shapes = shapes
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{BLOCK_PHYSICS}: {e}"))?;
    let text = format!(
        "{{\"shapes\": [\n{}\n],\n\"states\": [\n{}\n]}}\n",
        shapes.join(",\n"),
        rows.join(",\n")
    );
    write(&reference_out.join(BLOCK_PHYSICS), text.as_bytes())
}

/// The BDS physics of every reference state by network ID.
struct ReferencePhysics<'a> {
    by_id: BTreeMap<u32, &'a CloudburstBlock>,
    /// The states that took them by the connection rule.
    connected: usize,
    /// The states that took them by the borrowing rule.
    borrowed: usize,
}

/// The BDS physics of every reference state. `CloudburstMC`'s per-state data predates 1.26.60,
/// so two rules fill the states it lacks:
/// - Connections: a state of a block of `connections` (block name to the 1.26.60 connection
///   properties) takes the physics of the same state without those properties, the state the
///   1.26.60 block schema upgrades to it.
/// - Borrowing: a state of a block of [`BORROWED_1_26_60`] takes the physics of the one state of
///   the borrowed block that has every property the two share with the same value (the same
///   direction and shape; a borrowed block without properties has one state).
///
/// Fails on a reference state that has physics none of these ways.
fn reference_physics<'a>(
    reference: &[ReferenceState],
    connections: &BTreeMap<String, BTreeMap<String, Json>>,
    bds: &'a [CloudburstBlock],
) -> Result<ReferencePhysics<'a>, String> {
    let mut bds_by_id = BTreeMap::new();
    for b in bds {
        let id = u32::try_from(b.block_state_hash & 0xffff_ffff)
            .map_err(|e| format!("{CLOUDBURST_BLOCKS}: {e}"))?;
        if bds_by_id.insert(id, b).is_some() {
            return Err(format!("{CLOUDBURST_BLOCKS}: network ID {id} twice"));
        }
    }
    let mut by_name: BTreeMap<&str, Vec<&ReferenceState>> = BTreeMap::new();
    for state in reference {
        by_name.entry(state.name.as_str()).or_default().push(state);
    }
    let mut physics = ReferencePhysics {
        by_id: BTreeMap::new(),
        connected: 0,
        borrowed: 0,
    };
    let mut missing: BTreeMap<&str, usize> = BTreeMap::new();
    for state in reference {
        let found = match (
            bds_by_id.get(&state.network_id),
            connections.get(&state.name),
        ) {
            (Some(b), _) => Some(*b),
            (None, Some(added)) => {
                let mut old = state.states.clone();
                old.retain(|p, _| !added.contains_key(p));
                let b = bds_by_id.get(&network_hash(&state.name, &old)?).copied();
                physics.connected += usize::from(b.is_some());
                b
            }
            (None, None) => {
                let lenders = BORROWED_1_26_60
                    .iter()
                    .find(|(name, _)| *name == state.name)
                    .map(|(_, from)| {
                        by_name
                            .get(from)
                            .map_or(&[][..], Vec::as_slice)
                            .iter()
                            .filter(|lender| {
                                lender
                                    .states
                                    .iter()
                                    .all(|(p, v)| state.states.get(p).is_none_or(|own| own == v))
                            })
                            .collect::<Vec<_>>()
                    });
                match lenders.as_deref() {
                    None => None,
                    Some([lender]) => {
                        let b = bds_by_id.get(&lender.network_id).copied();
                        physics.borrowed += usize::from(b.is_some());
                        b
                    }
                    Some(lenders) => {
                        return Err(format!(
                            "{} {:?} matches {} states of the block it borrows from, not one",
                            state.name,
                            state.states,
                            lenders.len()
                        ));
                    }
                }
            }
        };
        if let Some(b) = found {
            physics.by_id.insert(state.network_id, b);
        } else {
            *missing.entry(state.name.as_str()).or_default() += 1;
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "{CLOUDBURST_BLOCKS} has no physics for these reference states (name: count): \
             {missing:?}"
        ));
    }
    Ok(physics)
}

/// The network block hash of a state with typed JSON property values (`src/block/hash.rs`).
fn network_hash(name: &str, states: &BTreeMap<String, Json>) -> Result<u32, String> {
    let mut properties = Vec::with_capacity(states.len());
    for (key, value) in states {
        let typed = value.as_object().and_then(|o| o.iter().next());
        let value = match typed {
            Some((tag, Json::Number(n))) if tag == "byte" => n
                .as_u64()
                .and_then(|b| u8::try_from(b).ok())
                .map(hash::PropertyValue::Byte)
                .ok_or_else(|| format!("{name} {key}: byte {n}"))?,
            Some((tag, Json::Number(n))) if tag == "int" => n
                .as_i64()
                .and_then(|i| i32::try_from(i).ok())
                .map(hash::PropertyValue::Int)
                .ok_or_else(|| format!("{name} {key}: int {n}"))?,
            Some((tag, Json::String(s))) if tag == "string" => hash::PropertyValue::String(s),
            _ => return Err(format!("{name} {key}: unknown property value {value}")),
        };
        properties.push((key.as_str(), value));
    }
    Ok(hash::network_hash(name, properties))
}

/// The value in the JSON file at `path`.
fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// Fails unless the git checkout `dir` is at `commit`.
fn pinned(root: &Path, dir: &str, commit: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root.join(dir))
        .output()
        .map_err(|e| format!("cannot run git in {dir}: {e}"))?;
    let head = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || head.trim() != commit {
        return Err(format!(
            "{dir} must be a checkout of commit {commit}, found {}",
            head.trim()
        ));
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn is_dir(path: &Path) -> bool {
    path.is_dir()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn is_file(path: &Path) -> bool {
    path.is_file()
}

/// The names of the entries directly in `dir`, sorted.
pub(crate) fn file_names(dir: &Path) -> Result<Vec<String>, String> {
    Ok(read_dir(dir)?
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect())
}

/// The files in a directory, sorted.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read_dir(path: &Path) -> Result<Vec<PathBuf>, String> {
    let error = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(path).map_err(error)? {
        paths.push(entry.map_err(error)?.path());
    }
    paths.sort();
    Ok(paths)
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The bytes of the file at `path`, or none when there is no such file; any other failure is an
/// error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn create_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))
}

/// Removes the file at `path`; a file that is already gone is not an error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference states of icicles, ice crystals and disc presses that borrow physics (20, 6
    /// and 4).
    const BORROWED_STATES_1_26_60: usize = 30;
    /// The reference states of fire, redstone wire and chorus plants that take the physics of
    /// their state without the 1.26.60 connection properties (512, 1,296 and 64).
    const CONNECTED_1_26_60: usize = 1872;

    #[test]
    fn network_hash_equals_the_reference_ids() {
        let states = BTreeMap::from([
            (
                "button_pressed_bit".to_owned(),
                serde_json::json!({ "byte": 0 }),
            ),
            (
                "facing_direction".to_owned(),
                serde_json::json!({ "int": 0 }),
            ),
        ]);
        assert_eq!(
            network_hash("minecraft:acacia_button", &states).unwrap(),
            152_215_376
        );
    }

    /// Runs on the committed reference list and 1.26.60 block schema with the pinned
    /// `CloudburstMC` checkout in `.cache` (`cargo xtask dump` needs it too).
    #[test]
    fn the_1_26_60_states_without_bds_physics_take_them_by_the_rules() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let bds_path = root.join(CLOUDBURST.0).join(CLOUDBURST_BLOCKS);
        if !is_file(&bds_path) {
            return;
        }
        let reference: Vec<ReferenceState> =
            read_json_file(&root.join(REFERENCE_DIR).join(REFERENCE_BLOCKS)).unwrap();
        let schema: AddingSchema = read_json_file(
            &root
                .join(upgrade::OUT)
                .join(upgrade::BLOCK_SCHEMAS)
                .join(SCHEMA_1_26_60),
        )
        .unwrap();
        let bds: Vec<CloudburstBlock> = read_json_file(&bds_path).unwrap();
        let physics = reference_physics(&reference, &schema.added_properties, &bds).unwrap();
        assert_eq!(physics.connected, CONNECTED_1_26_60);
        assert_eq!(physics.borrowed, BORROWED_STATES_1_26_60);
        assert_eq!(physics.by_id.len(), reference.len());
    }
}
