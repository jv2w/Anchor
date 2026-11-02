//! `src/item/table.rs`: the item type table and its lookup indices from [`Data`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use super::blocks::{api_enumeration, by_id, enumeration, strings, variants};
use super::body::{int_lit, str_lit};
use super::data::{Data, hex_lit, static_compound};
use super::emit::HEADER;
use super::info::{EffectJson, f64_lit};
use super::names::upper_camel;

/// The state index of no block in the places tables.
const NO_STATE: u16 = 0xffff;
/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/item/table.rs";

/// Generates the file text (unformatted) for the reference list of the game version
/// `game_version`. Fails if a lookup could not tell two types apart.
pub(crate) fn generate(data: &Data, game_version: &str) -> Result<String, String> {
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Item types generated from the dump and the {game_version} reference list: \
         {} types.\n\n\
         use super::{{Armor, Consumable, Cooldown, Data, Food, Info, Type}};\n\
         use crate::block::{{State, ToolType}};\nuse crate::effect;\n\n\
         /// Every item type in Bedrock form, in dump order: (name, meta, block, block network \
         ID, type NBT, runtime ID).\npub(super) static ITEMS: [Data; {}] = [",
        data.items.len(),
        data.items.len()
    );
    for item in &data.items {
        let block = item.block.map_or_else(
            || "None".to_owned(),
            |b| format!("Some(State::from_index({b}))"),
        );
        let nbt = item.nbt.as_ref().map_or_else(
            || "None".to_owned(),
            |c| format!("Some({})", static_compound(c)),
        );
        put!(
            out,
            "Data::new({}, {}, {block}, {}, {nbt}, {}),",
            str_lit(&item.name),
            item.meta,
            hex_lit(u64::from(item.block_network_id), 8),
            item.runtime_id
        );
    }
    out.push_str("];\n");
    network_index(data, &mut out)?;
    name_index(data, &mut out)?;
    kinds(data, &mut out)?;
    enchantments(data, &mut out)?;
    info(data, &mut out)?;
    Ok(out)
}

/// The item types: the `Kind` enum, its names and the item type the dump's item registry gives.
fn kinds(data: &Data, out: &mut String) -> Result<(), String> {
    let set: BTreeSet<&str> = data
        .items
        .iter()
        .filter_map(|i| i.data.kind.as_deref())
        .collect();
    let names: Vec<&str> = set.into_iter().collect();
    let variants = variants(&names)?;
    enumeration(
        out,
        "An item type, such as `IronIngot`; [`Type::kind`] gives an item type's. Block items \
         have none: their block state has a `block::Kind`.",
        "Kind",
        &variants,
    );
    strings(
        out,
        "The name of each kind, lowercase.",
        "KIND_NAMES",
        &names,
    );
    put!(
        out,
        "\n/// Every kind, in name order.\npub(super) static KINDS: [Kind; {}] = [",
        names.len()
    );
    for (v, _) in &variants {
        put!(out, "Kind::{v},");
    }
    put!(
        out,
        "];\n\n/// The item type the dump's item registry gives for each kind.\npub(super) static \
         KIND_DEFAULTS: [Type; {}] = [",
        names.len()
    );
    for name in &names {
        let mut found = data
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.data.kind.as_deref() == Some(*name) && i.vanilla);
        let (Some((index, _)), None) = (found.next(), found.next()) else {
            return Err(format!("item kind {name} has no single registry form"));
        };
        put!(out, "Type::from_index({index}),");
    }
    out.push_str("];\n");
    Ok(())
}

/// The enchantment enum, its IDs and the enchantments by ID.
fn enchantments(data: &Data, out: &mut String) -> Result<(), String> {
    let list = &data.tables.enchantments;
    let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    let variants = variants(&names)?;
    api_enumeration(
        out,
        "An enchantment.",
        "// api-class: Enchantment
#[doc(alias = \"VanillaEnchantments\")]
",
        "Enchantment",
        &variants,
    );
    put!(
        out,
        "\n/// Every enchantment, in name order.\npub(super) static ENCHANTMENTS: [Enchantment; {}] = [",
        list.len()
    );
    for (v, _) in &variants {
        put!(out, "Enchantment::{v},");
    }
    out.push_str("];\n");
    let ids: Vec<(u8, &str)> = list
        .iter()
        .zip(&variants)
        .map(|(e, (v, _))| (e.id, v.as_str()))
        .collect();
    by_id(
        out,
        "The enchantments by ID.",
        "ENCHANTMENTS_BY_ID",
        "Enchantment",
        &ids,
    )?;
    put!(
        out,
        "\n/// The ID of each enchantment, in name order.\npub(super) static ENCHANTMENT_IDS: \
         [u8; {}] = [",
        list.len()
    );
    for e in list {
        put!(out, "{},", e.id);
    }
    out.push_str("];\n");
    Ok(())
}

/// Item classes as an enum, the deduplicated item info rows and each item type's row, then the
/// blocks the types place.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table layout, emitted in one pass"
)]
fn info(data: &Data, out: &mut String) -> Result<(), String> {
    let mut class_set = BTreeSet::from(["Item"]);
    for i in &data.items {
        class_set.extend(i.data.classes.iter().map(String::as_str));
    }
    let classes: Vec<&str> = class_set.into_iter().collect();
    let class_variants = variants(&classes)?;
    enumeration(
        out,
        "An item class or one of its parents (`Item` for every item); [`Type::class`] gives an \
         item type's.",
        "Class",
        &class_variants,
    );
    strings(out, "The name of each class.", "CLASS_NAMES", &classes);

    let mut chain_at: BTreeMap<Vec<String>, (usize, usize)> = BTreeMap::new();
    let mut chains = String::new();
    let mut chain_len = 0;
    let mut effects = String::new();
    let mut effect_len = 0;
    let mut rows: Vec<String> = Vec::new();
    let mut row_at: BTreeMap<String, usize> = BTreeMap::new();
    let mut type_rows = Vec::with_capacity(data.items.len());
    for item in &data.items {
        let d = &item.data;
        let mut chain = d.classes.clone();
        chain.push("Item".to_owned());
        let chain_len_before = chain_len;
        let class = *chain_at.entry(chain.clone()).or_insert_with(|| {
            for c in &chain {
                put!(chains, "Class::{},", upper_camel(c));
            }
            chain_len += chain.len();
            (chain_len_before, chain.len())
        });
        let opt = |v: Option<String>| v.map_or_else(|| "None".to_owned(), |v| format!("Some({v})"));
        let consumable = match &d.consumable {
            Some(c) => {
                let effects_src = match &c.effects {
                    Json::String(s) if s == "random" => "None".to_owned(),
                    list => {
                        let list: Vec<EffectJson> = serde_json::from_value(list.clone())
                            .map_err(|e| format!("item effects: {e}"))?;
                        let start = effect_len;
                        for e in &list {
                            put!(effects, "{},", effect_instance(e, "effect::"));
                        }
                        effect_len += list.len();
                        format!("Some(({start}, {}))", list.len())
                    }
                };
                format!(
                    "Some(Consumable::new({}, {effects_src}))",
                    opt(item
                        .consume_residue
                        .map(|r| format!("Type::from_index({r})")))
                )
            }
            None => "None".to_owned(),
        };
        let row = format!(
            "Info {{ kind: {}, class: ({}, {}), max_stack: {}, max_durability: {}, \
             fire_proof: {}, attack_points: {}, defense_points: {}, tool_type: ToolType::from_bits({}), \
             harvest_level: {}, efficiency: {}, armor: {}, food: {}, consumable: \
             {consumable}, use_ticks: {}, cooldown: {}, throw_force: {}, potion: {} }},",
            opt(d
                .kind
                .as_deref()
                .map(|k| format!("Kind::{}", upper_camel(k)))),
            class.0,
            class.1,
            d.max_stack,
            d.max_durability.unwrap_or(0),
            d.fire_proof,
            d.attack_points,
            d.defense_points,
            d.tool.tool_type,
            d.tool.harvest_level,
            f64_lit(d.tool.efficiency),
            opt(d
                .armor
                .as_ref()
                .map(|a| format!("Armor::new({}, {})", a.slot, a.toughness))),
            opt(d.food.as_ref().map(|f| format!(
                "Food::new({}, {}, {})",
                f.food,
                f64_lit(f.saturation),
                f.requires_hunger
            ))),
            item.use_ticks,
            opt(d.cooldown.as_ref().map(|c| format!(
                "Cooldown::new({}, {})",
                c.ticks,
                c.tag
                    .as_deref()
                    .map_or_else(|| "None".to_owned(), |t| format!("Some({})", str_lit(t)))
            ))),
            opt(d.throw_force.map(f64_lit)),
            opt(d
                .potion
                .as_deref()
                .map(|p| format!("effect::Potion::{}", upper_camel(p)))),
        );
        let index = *row_at.entry(row.clone()).or_insert_with(|| {
            rows.push(row);
            rows.len() - 1
        });
        type_rows.push(index);
    }
    put!(
        out,
        "\n/// The class chains of [`INFO`] one after another, leaf first.\npub(super) static \
         CLASS_CHAINS: [Class; {chain_len}] = [{chains}];\n\n/// The effects of consuming items, \
         one list after another.\npub(super) static CONSUME_EFFECTS: [effect::Instance; \
         {effect_len}] = [{effects}];\n\n/// The distinct data rows of the item types (see `Info` \
         in `src/item.rs`).\npub(super) static INFO: [Info; {}] = [{}];\n\n/// The [`INFO`] row of \
         each item type.\npub(super) static TYPE_INFO: [u16; {}] = [",
        rows.len(),
        rows.concat(),
        type_rows.len()
    );
    for r in &type_rows {
        put!(out, "{r},");
    }
    out.push_str("];\n");
    places(data, out)
}

/// The blocks the item types place, kept out of the deduplicated rows because they rarely repeat.
fn places(data: &Data, out: &mut String) -> Result<(), String> {
    let state = |s: Option<u16>| match s {
        Some(s) if s < NO_STATE => Ok(s),
        Some(_) => Err("too many block states".to_owned()),
        None => Ok(NO_STATE),
    };
    for (name, doc, pick) in [
        (
            "TYPE_PLACES",
            "The state index placing each item type puts down; 0xffff: none.",
            0,
        ),
        (
            "TYPE_PLACES_ON_WALL",
            "The state index each item type puts down against a side of a block, where it \
             differs from [`TYPE_PLACES`]; 0xffff: the same.",
            1,
        ),
    ] {
        put!(
            out,
            "\n/// {doc}\npub(super) static {name}: [u16; {}] = [",
            data.items.len()
        );
        for item in &data.items {
            let s = if pick == 0 {
                item.places.0
            } else {
                item.places.1
            };
            put!(out, "{},", hex_lit(u64::from(state(s)?), 4));
        }
        out.push_str("];\n");
    }
    Ok(())
}

/// Rust source of a const `effect::Instance`; `path` is the path to the `effect` module with
/// its `::` (empty inside it).
pub(crate) fn effect_instance(e: &EffectJson, path: &str) -> String {
    format!(
        "{path}Instance::new({path}Type::{}, {}, {}, {}, {})",
        upper_camel(&e.effect),
        int_lit(&e.duration.to_string()),
        e.amplifier,
        e.visible,
        e.ambient
    )
}

/// The types sorted by network form, so a lookup finds the types of a runtime ID and block by
/// binary search. Checks that the network form tells types apart where the lookup needs it: a
/// block item's network ID is not 0 (the "no block" value), and types sharing runtime ID, block
/// and meta differ in the size of their type NBT, so the most specific match is unique.
fn network_index(data: &Data, out: &mut String) -> Result<(), String> {
    let mut index = Vec::with_capacity(data.items.len());
    for (i, item) in data.items.iter().enumerate() {
        let block = item.block_network_id;
        if item.block.is_some() && block == 0 {
            return Err(format!("block item {} has network ID 0", item.name));
        }
        let size = item.nbt.as_ref().map_or(0, BTreeMap::len);
        index.push((item.runtime_id, block, item.meta, size, i));
    }
    index.sort_unstable();
    for pair in index.windows(2) {
        if let [a, b] = pair
            && (a.0, a.1, a.2, a.3) == (b.0, b.1, b.2, b.3)
        {
            return Err(format!(
                "item types {} and {} share network form and type NBT size",
                a.4, b.4
            ));
        }
    }
    put!(
        out,
        "\n/// The types by network form, sorted: (runtime ID, network ID of the block or 0, meta, \
         type index).\npub(super) static BY_NETWORK: [(i16, u32, u16, u16); {}] = [",
        index.len()
    );
    for (runtime_id, block, meta, _, i) in index {
        put!(
            out,
            "({runtime_id}, {}, {meta}, {i}),",
            int_lit(&block.to_string())
        );
    }
    out.push_str("];\n");
    Ok(())
}

/// The item names with their runtime IDs, sorted by name. Every name has one runtime ID and no
/// two names share one.
fn name_index(data: &Data, out: &mut String) -> Result<(), String> {
    let mut names = BTreeMap::new();
    for item in &data.items {
        if *names.entry(item.name.as_str()).or_insert(item.runtime_id) != item.runtime_id {
            return Err(format!("item {} has two runtime IDs", item.name));
        }
    }
    let mut ids: Vec<i16> = names.values().copied().collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w.first() == w.get(1)) {
        return Err("two item names share a runtime ID".to_owned());
    }
    put!(
        out,
        "\n/// The item names with their runtime IDs, sorted by name.\n\
         pub(super) static NAMES: [(&str, i16); {}] = [",
        names.len()
    );
    for (name, runtime_id) in names {
        put!(out, "({}, {runtime_id}),", str_lit(name));
    }
    out.push_str("];\n");
    Ok(())
}
