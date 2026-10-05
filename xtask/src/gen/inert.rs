//! `src/block/inert/table.rs`: the block states of the reference list that are not Anchor states
//! (PLAN.md 7), with BDS's physics for them from `block_physics.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use super::blocks::strings;
use super::data::{Data, REFERENCE_DIR, hex_lit, read_json};
use super::emit::HEADER;
use super::info::f64_lit;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/block/inert/table.rs";

/// A reference list state: the fields this table reads.
#[derive(Deserialize)]
struct ReferenceBlock {
    name: String,
    network_id: u32,
}

/// `block_physics.json` (see `dump`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Physics {
    shapes: Vec<Vec<[f64; 6]>>,
    /// Network ID, hardness, light emission, light dampening, shape.
    states: Vec<(u32, f64, u8, u8, usize)>,
}

/// The block-local unit cube: a shape of only this box is solid.
const UNIT_CUBE: [f64; 6] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0];

/// Generates the file text (unformatted). Fails if a state has no physics or two states share a
/// network ID.
pub(crate) fn generate(root: &Path, data: &Data) -> Result<String, String> {
    let reference: Vec<ReferenceBlock> = read_json(root, REFERENCE_DIR, "blocks.json")?;
    let physics: Physics = read_json(root, REFERENCE_DIR, "block_physics.json")?;
    let by_id: BTreeMap<u32, (f64, u8, u8, usize)> = physics
        .states
        .iter()
        .map(|&(id, hardness, emission, dampening, shape)| {
            (id, (hardness, emission, dampening, shape))
        })
        .collect();
    let anchor: BTreeSet<u32> = data
        .states
        .iter()
        .map(|s| s.network_id)
        .chain(data.aliases.iter().map(|a| a.network_id))
        .collect();
    let mut states: Vec<(u32, &str)> = reference
        .iter()
        .filter(|b| !anchor.contains(&b.network_id))
        .map(|b| (b.network_id, b.name.as_str()))
        .collect();
    states.sort_unstable();
    if states.windows(2).any(|w| matches!(w, [a, b] if a.0 == b.0)) {
        return Err("two reference states share a network ID".to_owned());
    }
    let names: Vec<&str> = states
        .iter()
        .map(|s| s.1)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let name_index: BTreeMap<&str, usize> =
        names.iter().enumerate().map(|(i, n)| (*n, i)).collect();

    let mut boxes: Vec<[f64; 6]> = Vec::new();
    let mut shape_start: BTreeMap<usize, usize> = BTreeMap::new();
    let mut profiles: Vec<String> = Vec::new();
    let mut profile_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows = String::new();
    for &(id, name) in &states {
        let &(hardness, emission, dampening, shape) = by_id
            .get(&id)
            .ok_or_else(|| format!("{name} ({id}) has no physics"))?;
        let list = physics
            .shapes
            .get(shape)
            .ok_or_else(|| format!("{name} ({id}) names a missing shape"))?;
        let start = *shape_start.entry(shape).or_insert_with(|| {
            boxes.extend(list);
            boxes.len() - list.len()
        });
        let profile = format!(
            "Profile::new({}, {emission}, {dampening}, {start}, {}, {}),",
            f64_lit(hardness),
            list.len(),
            list.as_slice() == [UNIT_CUBE]
        );
        let index = *profile_index.entry(profile.clone()).or_insert_with(|| {
            profiles.push(profile);
            profiles.len() - 1
        });
        let name = name_index.get(name).ok_or("name not interned")?;
        put!(rows, "({}, {name}, {index}),", hex_lit(u64::from(id), 8));
    }

    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! The block states of the 1.26.50 reference list that are not Anchor states \
         (PLAN.md 7): {} states of {} blocks, with BDS's physics.\n\n\
         use super::Profile;\n\
         use crate::math::{{Aabb, Vec3}};\n",
        states.len(),
        names.len()
    );
    strings(&mut out, "Block names, sorted.", "NAMES", &names);
    put!(
        out,
        "\n/// The block-local collision boxes of [`PROFILES`] one after another.\n\
         pub(super) static BOXES: [Aabb; {}] = [",
        boxes.len()
    );
    for b in boxes {
        let [x0, y0, z0, x1, y1, z1] = b.map(f64_lit);
        put!(
            out,
            "Aabb::new(Vec3::new({x0}, {y0}, {z0}), Vec3::new({x1}, {y1}, {z1})),"
        );
    }
    put!(
        out,
        "];\n\n/// The distinct physics of the states.\n\
         pub(super) static PROFILES: [Profile; {}] = [{}];\n\n\
         /// The states by network ID: the ID, the [`NAMES`] index and the [`PROFILES`] index.\n\
         pub(super) static STATES: [(u32, u16, u16); {}] = [{rows}];\n",
        profiles.len(),
        profiles.concat(),
        states.len()
    );
    Ok(out)
}
