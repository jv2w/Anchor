//! `src/effect/table.rs`: the status effect and potion tables (PLAN.md 10.7) from the PMMP dump
//! (`tables.json`).

use super::blocks::{by_id, enumeration, strings, variants};
use super::body::str_lit;
use super::data::{Data, int_lit};
use super::emit::HEADER;
use super::items::effect_instance;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/effect/table.rs";

/// Generates the file text (unformatted).
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    let effects = &data.tables.effects;
    let potions = &data.tables.potions;
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Status effects and potion types generated from the PMMP dump (PLAN.md 10.7): \
         {} effects, {} potion types.\n\nuse super::{{Data, Instance, PotionData}};\n",
        effects.len(),
        potions.len()
    );
    let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
    let effect_variants = variants(&names)?;
    enumeration(
        &mut out,
        "A status effect type (PMMP `VanillaEffects`).",
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
    for e in effects {
        put!(
            out,
            "Data::new({}, {}, [{}, {}, {}, {}], {}, {}, {}, {}),",
            e.id,
            str_lit(&e.text),
            e.colour.r,
            e.colour.g,
            e.colour.b,
            e.colour.a,
            e.bad,
            e.instant,
            int_lit(i64::from(e.default_duration)),
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
    enumeration(
        out,
        "A potion type (PMMP `PotionType`): what a potion, splash potion, lingering potion or \
         tipped arrow holds.",
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
