//! `src/effect/table.rs`: the status effect and potion tables from the dump
//! (`tables.json`).

use super::blocks::{api_enumeration, by_id, strings, variants};
use super::body::{int_lit, str_lit};
use super::data::Data;
use super::emit::HEADER;
use super::items::effect_instance;

/// The official server's colors (RGB) of the effects whose splash potions it shows, where they
/// differ from the dump's older ones (P6 `potions`, `xtask/data/p6/facts.md`: the splash
/// `LevelEvent` colors; resistance from the turtle master's mixes). The client draws potions and
/// particles in them.
const OFFICIAL_COLORS: [(&str, [u8; 3]); 12] = [
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
pub(super) const PATH: &str = "src/effect/table.rs";

/// Generates the file text (unformatted).
pub(super) fn generate(data: &Data) -> Result<String, String> {
    let effects = &data.tables.effects;
    let potions = &data.tables.potions;
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Status effects and potion types generated from the dump: \
         {} effects, {} potion types.\n\nuse super::{{Data, Instance, PotionData}};\n",
        effects.len(),
        potions.len()
    );
    let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
    let effect_variants = variants(&names)?;
    api_enumeration(
        "A status effect type.",
        "#[doc(alias = \"Effect\", alias = \"VanillaEffects\")]
",
        "Type",
        &effect_variants,
        &mut out,
    );
    put!(
        out,
        "\n/// Every effect type, in name order.\npub(super) static TYPES: [Type; {}] = [",
        int_lit(&names.len().to_string())
    );
    for (v, _) in &effect_variants {
        put!(out, "Type::{v},");
    }
    put!(
        out,
        "];\n\n/// The data of each effect type: (ID, translation key, color as RGBA, bad, instant, \
         default duration, bubbles).\npub(super) static DATA: [Data; {}] = [",
        int_lit(&effects.len().to_string())
    );
    if let Some((name, _)) = OFFICIAL_COLORS
        .iter()
        .find(|(n, _)| !effects.iter().any(|e| e.name == *n))
    {
        return Err(format!(
            "OFFICIAL_COLORS names {name}, which is not an effect"
        ));
    }
    for e in effects {
        let [r, g, b, a] = color(&e.name, [e.color.r, e.color.g, e.color.b, e.color.a]);
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
        "The effect types by ID.",
        "TYPES_BY_ID",
        "Type",
        &ids,
        &mut out,
    )?;

    potion_types(data, &mut out)?;
    Ok(out)
}

/// An effect's color (RGBA): the official server's where [`OFFICIAL_COLORS`] has it, the
/// dump's otherwise.
fn color(name: &str, dumped: [u8; 4]) -> [u8; 4] {
    OFFICIAL_COLORS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(dumped, |(_, [r, g, b])| [*r, *g, *b, dumped[3]])
}

/// Writes the potion type enum and tables.
fn potion_types(data: &Data, out: &mut String) -> Result<(), String> {
    let potions = &data.tables.potions;
    let potion_names: Vec<&str> = potions.iter().map(|p| p.name.as_str()).collect();
    let potion_variants = variants(&potion_names)?;
    api_enumeration(
        "A potion type: what a potion, splash potion, lingering potion or tipped arrow holds.",
        "#[doc(alias = \"PotionType\")]
",
        "Potion",
        &potion_variants,
        out,
    );
    put!(
        out,
        "\n/// Every potion type, in name order.\npub(super) static POTIONS: [Potion; {}] = [",
        int_lit(&potions.len().to_string())
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
        "The potion types by ID.",
        "POTIONS_BY_ID",
        "Potion",
        &ids,
        out,
    )?;
    strings(
        "The English display name of each potion type, not a translation key.",
        "POTION_DISPLAY_NAMES",
        &potions.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
        out,
    );
    let mut instances = String::new();
    let mut count = 0;
    put!(
        out,
        "\n/// The data of each potion type: (ID, first effect in [`POTION_EFFECTS`], effect \
         count).\npub(super) static POTION_DATA: [PotionData; {}] = [",
        int_lit(&potions.len().to_string())
    );
    for p in potions {
        put!(
            out,
            "PotionData::new({}, {}, {}),",
            p.id,
            int_lit(&count.to_string()),
            int_lit(&p.effects.len().to_string())
        );
        for e in &p.effects {
            put!(instances, "{},", effect_instance(e, ""));
        }
        count += p.effects.len();
    }
    put!(
        out,
        "];\n\n/// The effects of the potion types, one list after another.\npub(super) static \
         POTION_EFFECTS: [Instance; {}] = [{instances}];\n",
        int_lit(&count.to_string())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_colors_replace_the_dumped_ones() {
        assert_eq!(
            color("night_vision", [31, 31, 161, 255]),
            [0xc2, 0xff, 0x66, 255]
        );
        assert_eq!(
            color("regeneration", [205, 92, 171, 255]),
            [205, 92, 171, 255]
        );
    }
}
