//! `src/effect/table.rs`: the status effect and potion tables from the PMMP dump
//! (`tables.json`).

use super::blocks::{api_enumeration, by_id, strings, variants};
use super::body::{int_lit, str_lit};
use super::data::Data;
use super::emit::HEADER;
use super::items::effect_instance;

/// BDS's colours (RGB) of the effects whose splash potions it shows, where they differ from
/// PMMP's older ones (P6 `potions`, `xtask/data/p6/facts.md`: the splash `LevelEvent` colours;
/// resistance from the turtle master's mixes). The client draws potions and particles in them.
const BDS_COLOURS: [(&str, [u8; 3]); 12] = [
    ("fire_resistance", [0xff, 0x99, 0x00]),
    ("instant_damage", [0xa9, 0x65, 0x6a]),
    ("invisibility", [0xf6, 0xf6, 0xf6]),
    ("jump_boost", [0xfd, 0xff, 0x84]),
    ("night_vision", [0xc2, 0xff, 0x66]),
    ("poison", [0x87, 0xa3, 0x63]),
    ("resistance", [0x91, 0x46, 0xf0]),
    ("slowness", [0x8b, 0xaf, 0xe0]),
    ("speed", [0x33, 0xeb, 0xff]),
    ("strength", [0xff, 0xc7, 0x00]),
    ("water_breathing", [0x98, 0xda, 0xc0]),
    ("wither", [0x73, 0x61, 0x56]),
];
/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/effect/table.rs";

/// An effect's colour (RGBA): BDS's where [`BDS_COLOURS`] has it, PMMP's otherwise.
fn colour(name: &str, pmmp: [u8; 4]) -> [u8; 4] {
    BDS_COLOURS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(pmmp, |(_, [r, g, b])| [*r, *g, *b, pmmp[3]])
}

/// Generates the file text (unformatted).
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    let effects = &data.tables.effects;
    let potions = &data.tables.potions;
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Status effects and potion types generated from the PMMP dump: \
         {} effects, {} potion types.\n\nuse super::{{Data, Instance, PotionData}};\n",
        effects.len(),
        potions.len()
    );
    let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
    let effect_variants = variants(&names)?;
    api_enumeration(
        &mut out,
        "A status effect type (PMMP `VanillaEffects`).",
        "#[doc(alias = \"Effect\", alias = \"VanillaEffects\")]
",
        "Type",
        &effect_variants,
    );
    put!(
        out,
        "\n/// Every effect type, in name order.\npub(super) static TYPES: [Type; {}] = [",
        names.len()
    );
    for (v, _) in &effect_variants {
        put!(out, "Type::{v},");
    }
    put!(
        out,
        "];\n\n/// The data of each effect type: (ID, name, colour as RGBA, bad, instant, default \
         duration, bubbles).\npub(super) static DATA: [Data; {}] = [",
        effects.len()
    );
    if let Some((name, _)) = BDS_COLOURS
        .iter()
        .find(|(n, _)| !effects.iter().any(|e| e.name == *n))
    {
        return Err(format!("BDS_COLOURS names {name}, which is not an effect"));
    }
    for e in effects {
        let [r, g, b, a] = colour(&e.name, [e.colour.r, e.colour.g, e.colour.b, e.colour.a]);
        put!(
            out,
            "Data::new({}, {}, [{}, {}, {}, {}], {}, {}, {}, {}),",
            e.id,
            str_lit(&e.text),
            r,
            g,
            b,
            a,
            e.bad,
            e.instant,
            int_lit(&e.default_duration.to_string()),
            e.bubbles
        );
    }
    out.push_str("];\n");
    let ids: Vec<(u8, &str)> = effects
        .iter()
        .zip(&effect_variants)
        .map(|(e, (v, _))| (e.id, v.as_str()))
        .collect();
    by_id(
        &mut out,
        "The effect types by ID.",
        "TYPES_BY_ID",
        "Type",
        &ids,
    )?;

    potion_types(&mut out, data)?;
    Ok(out)
}

/// The potion type enum and tables (PMMP `PotionType`, `PotionTypeIdMap`).
fn potion_types(out: &mut String, data: &Data) -> Result<(), String> {
    let potions = &data.tables.potions;
    let potion_names: Vec<&str> = potions.iter().map(|p| p.name.as_str()).collect();
    let potion_variants = variants(&potion_names)?;
    api_enumeration(
        out,
        "A potion type (PMMP `PotionType`): what a potion, splash potion, lingering potion or \
         tipped arrow holds.",
        "#[doc(alias = \"PotionType\")]
",
        "Potion",
        &potion_variants,
    );
    put!(
        out,
        "\n/// Every potion type, in name order.\npub(super) static POTIONS: [Potion; {}] = [",
        potions.len()
    );
    for (v, _) in &potion_variants {
        put!(out, "Potion::{v},");
    }
    out.push_str("];\n");
    let ids: Vec<(u8, &str)> = potions
        .iter()
        .zip(&potion_variants)
        .map(|(p, (v, _))| (p.id, v.as_str()))
        .collect();
    by_id(
        out,
        "The potion types by ID.",
        "POTIONS_BY_ID",
        "Potion",
        &ids,
    )?;
    strings(
        out,
        "The PMMP display name of each potion type.",
        "POTION_TEXT",
        &potions.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
    );
    let mut instances = String::new();
    let mut count = 0;
    put!(
        out,
        "\n/// The data of each potion type: (ID, first effect in [`POTION_EFFECTS`], effect \
         count).\npub(super) static POTION_DATA: [PotionData; {}] = [",
        potions.len()
    );
    for p in potions {
        put!(
            out,
            "PotionData::new({}, {count}, {}),",
            p.id,
            p.effects.len()
        );
        for e in &p.effects {
            put!(instances, "{},", effect_instance(e, ""));
        }
        count += p.effects.len();
    }
    put!(
        out,
        "];\n\n/// The effects of the potion types, one list after another.\npub(super) static \
         POTION_EFFECTS: [Instance; {count}] = [{instances}];\n"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bds_colours_replace_pmmps() {
        assert_eq!(
            colour("night_vision", [31, 31, 161, 255]),
            [0xc2, 0xff, 0x66, 255]
        );
        assert_eq!(
            colour("regeneration", [205, 92, 171, 255]),
            [205, 92, 171, 255]
        );
    }
}
