//! `cargo xtask dump [--check]`: rebuilds the committed block and item data from
//! the pinned external checkouts in `.cache/`. It converts the reference lists of dragonfly and
//! `CloudburstMC` with `xtask/go/reference` into `xtask/data/reference/` first, as the PMMP run
//! reads every reference block state with PMMP's deserializer; then it runs PMMP's own
//! registries through `xtask/php/dump.php` with the PMMP PHP binary into `xtask/data/pmmp/`. It
//! also copies `CloudburstMC`'s biome definitions, entity types and block shapes and BDS's
//! per-state block physics (`block_physics.json`, for the block states Anchor shows but does not
//! implement) into `xtask/data/reference/`, where the Go tool also writes its jigsaw structure
//! data and entity property definitions as network NBT and this command writes the per-field
//! decode limits from Mojang's protocol schemas (`limits`). The block and item upgrade schemas
//! and their golden files go to `xtask/data/upgrade/` (see `upgrade`). With `--check` it writes into
//! `target/` and fails if a committed file differs. `verify` does not run it: it needs the
//! checkouts, the PHP binary and Go, and its inputs never change (`gen --check` checks the code
//! made from the committed data instead).

pub(crate) mod upgrade;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::r#gen::biomes::{self, Biome, Colour};

mod limits;

/// The pinned checkouts, relative to the repository root, and their commits.
const PMMP: (&str, &str) = (".cache/pmmp", "6a7cc02e9dff59b69241aa0bcffdb9903ce86beb");
const DRAGONFLY: (&str, &str) = (
    ".cache/dragonfly",
    "4c7b5074be94fa83a1cd98e9c752083ad04a6e21",
);
const CLOUDBURST: (&str, &str) = (
    ".cache/cloudburst-data",
    "659ce1e2eee3a67045693f4fd5515c6ccf571953",
);
/// Mojang's bedrock-protocol-docs at the v1.26.50 release (protocol 2193).
const PROTOCOL_DOCS: (&str, &str) = (
    ".cache/bedrock-protocol-docs",
    "475bd72ed89036af4eb18426774ef3b953de7603",
);
/// The PMMP PHP binary (pmmp/PHP-Binaries php-build-524, PHP 8.4 PM5 for Windows): plain PHP
/// lacks the extensions PMMP's registries load.
const PHP: &str = ".cache/php-pm5/bin/php/php.exe";
/// Output directories and their files, relative to the repository root.
const PMMP_OUT: &str = "xtask/data/pmmp";
const PMMP_FILES: &[&str] = &[
    "blocks.json",
    "deserialized.json",
    "items.json",
    "creative.json",
    "recipes.json",
    "tables.json",
];
const REFERENCE_OUT: &str = "xtask/data/reference";
const REFERENCE_FILES: &[&str] = &[
    "blocks.json",
    "items.json",
    "item_blocks.json",
    "jigsaw_structure_data.dat",
    "entity_properties.dat",
    biomes::FILE,
    ENTITY_IDENTIFIERS,
    VOXEL_SHAPES,
    BLOCK_PHYSICS,
    limits::FILE,
];
/// `CloudburstMC`'s biome definitions without chunk generation data: the content BDS sends.
/// `biome_definitions.json` holds generation data BDS does not send.
const BIOMES: &str = "stripped_biome_definitions.json";
/// `CloudburstMC` files Anchor sends byte for byte: `AvailableActorIdentifiers`
/// and the shapes of `VoxelShapes`.
const ENTITY_IDENTIFIERS: &str = "entity_identifiers.dat";
const VOXEL_SHAPES: &str = "voxel_shapes.json";
/// `CloudburstMC`'s per-state block data, which BDS dumps.
const CLOUDBURST_BLOCKS: &str = "blocks.json";
/// The block physics this command writes from [`CLOUDBURST_BLOCKS`].
pub(crate) const BLOCK_PHYSICS: &str = "block_physics.json";

/// Runs `dump`; with `check`, compares instead of writing. Returns the differing files on a
/// failed check.
pub(crate) fn run(root: &Path, check: bool) -> Result<Vec<String>, String> {
    for (dir, commit) in [PMMP, DRAGONFLY, CLOUDBURST, PROTOCOL_DOCS] {
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
            root.join(PMMP_OUT),
            root.join(REFERENCE_OUT),
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
        .arg(reference_out.join("blocks.json"))
        .current_dir(root.join(PMMP.0))
        .status()
        .map_err(|e| format!("cannot run {PHP} ({e}); see docs/data.md"))?;
    if !status.success() {
        return Err(format!("the PMMP dump failed ({status})"));
    }
    copy_biomes(root, &reference_out)?;
    copy_block_physics(root, &reference_out)?;
    for file in [ENTITY_IDENTIFIERS, VOXEL_SHAPES] {
        let bytes = read(&root.join(CLOUDBURST.0).join(file))?;
        write(&reference_out.join(file), &bytes)?;
    }
    let limits = limits::generate(&root.join(PROTOCOL_DOCS.0).join("json"))?;
    write(&reference_out.join(limits::FILE), limits.as_bytes())?;

    let upgrade_files = upgrade::run(root, &upgrade_out)?;

    let mut differ = Vec::new();
    if check {
        for file in &upgrade_files {
            let committed = root.join(upgrade::OUT).join(file);
            if read(&committed).ok() != Some(read(&upgrade_out.join(file))?) {
                differ.push(format!("{}/{file}", upgrade::OUT));
            }
        }
        for (committed, made, files) in [
            (PMMP_OUT, &pmmp_out, PMMP_FILES),
            (REFERENCE_OUT, &reference_out, REFERENCE_FILES),
        ] {
            for file in files {
                if read(&root.join(committed).join(file)).ok() != Some(read(&made.join(file))?) {
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
    let mut text = String::from("[\n");
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

/// Writes the physics of every block state of `CloudburstMC`'s per-state data (BDS's values) to
/// `out` as [`BLOCK_PHYSICS`]: `shapes`, the distinct collision box lists in order of first use,
/// and `states`, one `[network ID, hardness, light emission, light dampening, shape]` per line in
/// network ID order.
fn copy_block_physics(root: &Path, out: &Path) -> Result<(), String> {
    let path = root.join(CLOUDBURST.0).join(CLOUDBURST_BLOCKS);
    let all: Vec<CloudburstBlock> =
        serde_json::from_slice(&read(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut by_id = BTreeMap::new();
    for b in &all {
        let id = u32::try_from(b.block_state_hash & 0xffff_ffff)
            .map_err(|e| format!("{CLOUDBURST_BLOCKS}: {e}"))?;
        if by_id.insert(id, b).is_some() {
            return Err(format!("{CLOUDBURST_BLOCKS}: network ID {id} twice"));
        }
    }
    let mut shapes: Vec<&[[f64; 6]]> = Vec::new();
    let mut rows = Vec::with_capacity(by_id.len());
    for (id, b) in by_id {
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
    write(&out.join(BLOCK_PHYSICS), text.as_bytes())
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
fn is_dir(path: &Path) -> bool {
    path.is_dir()
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
