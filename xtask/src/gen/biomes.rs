//! `src/world/biome/table.rs`: the biome definitions of the spawn window from
//! `xtask/data/reference/biomes.json`, with the packet's string list: every biome name and tag
//! once, in order of first use, and the definitions referring to it by index.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::body::{int_lit, str_lit};
use super::data::{hex_lit, read_json};
use super::emit::HEADER;
use crate::dump::REFERENCE_DIR;

/// The committed data file in `xtask/data/reference/`, written by `cargo xtask dump`.
pub(crate) const FILE: &str = "biomes.json";
/// The generated file, relative to the repository root.
pub(super) const PATH: &str = "src/world/biome/table.rs";

/// One line of [`FILE`]: a biome definition as the official server sends it, without chunk
/// generation data. Numbers keep the decimal text of the reference list's file; [`single`] reads
/// them at the packet's precision.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Biome {
    pub(crate) name: String,
    pub(crate) temperature: f64,
    pub(crate) downfall: f64,
    pub(crate) foliage_snow: f64,
    pub(crate) depth: f64,
    pub(crate) scale: f64,
    #[serde(rename = "map_water_colour")]
    pub(crate) map_water_color: Color,
    pub(crate) rain: bool,
    pub(crate) tags: Vec<String>,
}

/// An ARGB color by component.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Color {
    pub(crate) a: u8,
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

/// Generates the file text (unformatted). Fails unless the biomes are in strict name order, every
/// value fits its packet field, and the strings fit the packet's 16-bit indices.
pub(super) fn generate(root: &Path) -> Result<String, String> {
    let biomes: Vec<Biome> = read_json(root, REFERENCE_DIR, FILE)?;
    if biomes
        .windows(2)
        .any(|w| matches!(w, [a, b] if a.name >= b.name))
    {
        return Err(format!("{FILE} is not in strict name order"));
    }
    let mut strings: Vec<&str> = Vec::new();
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for b in &biomes {
        for s in std::iter::once(&b.name).chain(&b.tags) {
            index.entry(s).or_insert_with(|| {
                strings.push(s);
                strings.len() - 1
            });
        }
    }
    // A name index is an int16 on the wire, a tag index a uint16: every index fits both.
    if i16::try_from(strings.len()).is_err() {
        return Err(format!(
            "{} strings exceed the int16 indices",
            strings.len()
        ));
    }
    let at = |s: &str| {
        index
            .get(s)
            .copied()
            .ok_or_else(|| format!("string {s} not interned"))
    };

    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! The biome definitions generated from the reference list's stripped biome \
         definitions: {} biomes, {} strings.\n\n\
         use super::Definition;\n\n\
         /// The packet's string list: every biome name and tag once, in order of first use.\n\
         pub(super) static STRINGS: [&str; {}] = [",
        biomes.len(),
        strings.len(),
        int_lit(&strings.len().to_string())
    );
    for s in &strings {
        put!(out, "{},", str_lit(s));
    }
    put!(
        out,
        "];\n\n/// Every biome in name order, its strings as indices into [`STRINGS`].\n\
         pub(super) static DEFINITIONS: [Definition; {}] = [",
        int_lit(&biomes.len().to_string())
    );
    for b in &biomes {
        let float = |v: f64| single(v).map_err(|e| format!("{}: {e}", b.name));
        let c = &b.map_water_color;
        let argb = u32::from_be_bytes([c.a, c.r, c.g, c.b]);
        let mut tags = Vec::with_capacity(b.tags.len());
        for t in &b.tags {
            tags.push(int_lit(&at(t)?.to_string()));
        }
        put!(
            out,
            "Definition {{ name: {}, temperature: {:?}, downfall: {:?}, foliage_snow: {:?}, \
             depth: {:?}, scale: {:?}, map_water_color: {}, rain: {}, tags: &[{}] }},",
            int_lit(&at(&b.name)?.to_string()),
            float(b.temperature)?,
            float(b.downfall)?,
            float(b.foliage_snow)?,
            float(b.depth)?,
            float(b.scale)?,
            hex_lit(u64::from(argb), 8),
            b.rain,
            tags.join(", ")
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// The `f32` nearest to the decimal a JSON number was written as. The shortest text of the `f64`
/// is that decimal, so the value is rounded once, as the official server and gophertunnel read it.
fn single(value: f64) -> Result<f32, String> {
    let text = value.to_string();
    match text.parse::<f32>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(format!("{text} is not a finite f32")),
    }
}
