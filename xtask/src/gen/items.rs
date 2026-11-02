//! `src/item/table.rs`: the item type table and its lookup indices from [`Data`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use super::blocks::{by_id, enumeration, pmmp_enumeration, strings, variants};
use super::body::{int_lit, str_lit};
use super::data::{Data, hex_lit, static_compound};
use super::emit::HEADER;
use super::info::{EffectJson, bit_set, f64_lit};
use super::names::upper_camel;

type R<T> = Result<T, String>;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/item/table.rs";

/// Generates the file text (unformatted). Fails if a lookup could not tell two types apart.
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Item types generated from the PMMP dump and the 1.26.60 reference list: \
         {} types.\n\n\
         use super::{{Armor, Consumable, Cooldown, Data, EnchantmentData, EnchantmentSet, Food, \
         Info, Type}};\nuse crate::block::{{State, ToolType}};\nuse crate::effect;\n\n\
         /// Every PMMP item type in Bedrock form, in dump order: (name, meta, block, block \
         network ID, type NBT, runtime ID).\npub(super) static ITEMS: [Data; {}] = [",
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
    let tags = tags(data, &mut out)?;
    enchantments(data, &mut out)?;
    info(data, &tags, &mut out)?;
    Ok(out)
}

/// The PMMP item types: the `Kind` enum, its names and the item type `VanillaItems` gives.
fn kinds(data: &Data, out: &mut String) -> R<()> {
    let set: BTreeSet<&str> = data
        .items
        .iter()
        .filter_map(|i| i.data.kind.as_deref())
        .collect();
    let names: Vec<&str> = set.into_iter().collect();
    let variants = variants(&names)?;
    enumeration(
        out,
        "A PMMP item type (a `VanillaItems` entry), such as `IronIngot`; [`Type::kind`] gives an \
         item type's. Block items have none: their block state has a `block::Kind`.",
        "Kind",
        &variants,
    );
    strings(
        out,
        "The `VanillaItems` name of each kind, lowercase.",
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
        "];\n\n/// The item type `VanillaItems` gives for each kind.\npub(super) static \
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
            return Err(format!("item kind {name} has no single VanillaItems form"));
        };
        put!(out, "Type::from_index({index}),");
    }
    out.push_str("];\n");
    Ok(())
}

/// The item tags (PMMP `ItemTagToIdMap`): names, and the bit of each tag in an item type's tag
/// set. Returns the tag set of each item type.
fn tags(data: &Data, out: &mut String) -> R<Vec<u128>> {
    let list = &data.tables.item_tags;
    if list.len() > 128 {
        return Err("more than 128 item tags".to_owned());
    }
    let names: Vec<&str> = list.iter().map(|t| t.tag.as_str()).collect();
    if names.windows(2).any(|w| matches!(w, [a, b] if a >= b)) {
        return Err("item tags are not sorted".to_owned());
    }
    strings(out, "The item tag names, sorted.", "TAG_NAMES", &names);
    let mut sets = vec![0u128; data.items.len()];
    for (bit, tag) in list.iter().enumerate() {
        for (set, item) in sets.iter_mut().zip(&data.items) {
            if tag.items.contains(&item.name) {
                *set |= 1 << bit;
            }
        }
    }
    Ok(sets)
}

/// The enchantment enum and table (PMMP `VanillaEnchantments`, `AvailableEnchantmentRegistry`,
/// `IncompatibleEnchantmentRegistry`).
fn enchantments(data: &Data, out: &mut String) -> R<()> {
    let list = &data.tables.enchantments;
    let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    let variants = variants(&names)?;
    pmmp_enumeration(
        out,
        "An enchantment (PMMP `VanillaEnchantments`).",
        "// pmmp-class: Enchantment
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
        "\n/// The data of each enchantment.\npub(super) static ENCHANTMENT_DATA: \
         [EnchantmentData; {}] = [",
        list.len()
    );
    for e in list {
        if e.power.len() != usize::from(e.max_level) {
            return Err(format!("enchantment {} has power for other levels", e.name));
        }
        let power: Vec<String> = e.power.iter().map(|(a, b)| format!("({a}, {b})")).collect();
        put!(
            out,
            "EnchantmentData::new({}, {}, {}, {}, {}, EnchantmentSet({}), &[{}]),",
            e.id,
            str_lit(&e.text),
            e.rarity,
            e.max_level,
            e.available,
            hex_lit(bit_set(&e.incompatible, &names, "enchantment")?, 8),
            power.join(",")
        );
    }
    out.push_str("];\n");
    Ok(())
}

/// Item classes, traits and tool tiers as enums, the deduplicated item info rows and each item
/// type's row and tag set.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table layout, emitted in one pass"
)]
fn info(data: &Data, tags: &[u128], out: &mut String) -> R<()> {
    let mut class_set = BTreeSet::from(["Item"]);
    let mut trait_set = BTreeSet::new();
    for i in &data.items {
        class_set.extend(i.data.classes.iter().map(String::as_str));
        trait_set.extend(i.data.traits.iter().map(String::as_str));
    }
    let classes: Vec<&str> = class_set.into_iter().collect();
    let traits: Vec<&str> = trait_set.into_iter().collect();
    if traits.len() > 32 {
        return Err("more than 32 item traits".to_owned());
    }
    let class_variants = variants(&classes)?;
    enumeration(
        out,
        "A PMMP item class or one of its parents (`Item` for every item); [`Type::class`] gives \
         an item type's.",
        "Class",
        &class_variants,
    );
    strings(out, "The PMMP name of each class.", "CLASS_NAMES", &classes);
    enumeration(
        out,
        "A PMMP item trait or interface; [`Type::has_trait`] tells an item type's.",
        "Trait",
        &variants(&traits)?,
    );
    strings(out, "The PMMP name of each trait.", "TRAIT_NAMES", &traits);
    let mut tiers: Vec<_> = data.tables.tool_tiers.iter().collect();
    tiers.sort_by_key(|t| (t.harvest_level, t.name.as_str()));
    let tier_names: Vec<&str> = tiers.iter().map(|t| t.name.as_str()).collect();
    pmmp_enumeration(
        out,
        "A tool tier (PMMP `ToolTier`), in harvest level order.",
        "// pmmp-class: ToolTier
",
        "ToolTier",
        &variants(&tier_names)?,
    );
    put!(
        out,
        "\n/// Every tool tier with its data: (tier, harvest level, max durability, base attack \
         points, base efficiency, enchantability).\npub(super) static TOOL_TIERS: [(ToolTier, u8, \
         u16, u8, u8, u8); {}] = [",
        tiers.len()
    );
    for t in &tiers {
        put!(
            out,
            "(ToolTier::{}, {}, {}, {}, {}, {}),",
            upper_camel(&t.name),
            t.harvest_level,
            t.max_durability,
            t.base_attack_points,
            t.base_efficiency,
            t.enchantability
        );
    }
    out.push_str("];\n");
    let enchantments: Vec<&str> = data
        .tables
        .enchantments
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    let class_ids: BTreeMap<&str, String> = classes.iter().map(|c| (*c, upper_camel(c))).collect();

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
                put!(
                    chains,
                    "Class::{},",
                    class_ids.get(c.as_str()).map_or("", String::as_str)
                );
            }
            chain_len += chain.len();
            (chain_len_before, chain.len())
        });
        let opt = |v: Option<String>| v.map_or_else(|| "None".to_owned(), |v| format!("Some({v})"));
        let consumable = match &d.consumable {
            None => "None".to_owned(),
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
        };
        let tier = match &d.tool.tier {
            Some(t) => format!("Some(ToolTier::{})", upper_camel(t)),
            None => "None".to_owned(),
        };
        let row = format!(
            "Info {{ kind: {}, class: ({}, {}), traits: {}, max_stack: {}, max_durability: {}, \
             fire_proof: {}, attack_points: {}, defense_points: {}, tool_type: ToolType::from_bits({}), \
             harvest_level: {}, efficiency: {}, tier: {tier}, armor: {}, food: {}, consumable: \
             {consumable}, use_ticks: {}, fuel_time: {}, fuel_residue: {}, enchantability: {}, \
             primary_enchantments: EnchantmentSet({}), enchantments: EnchantmentSet({}), cooldown: \
             {}, throw_force: {}, potion: {} }},",
            opt(d
                .kind
                .as_deref()
                .map(|k| format!("Kind::{}", upper_camel(k)))),
            class.0,
            class.1,
            bit_set(&d.traits, &traits, "trait")?,
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
            d.fuel.time,
            opt(item.fuel_residue.map(|r| format!("Type::from_index({r})"))),
            d.enchantability,
            hex_lit(
                bit_set(&d.enchantments.primary, &enchantments, "enchantment")?,
                8
            ),
            hex_lit(
                bit_set(&d.enchantments.all, &enchantments, "enchantment")?,
                8
            ),
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
    per_type(data, tags, out)
}

/// The per-type data that rarely repeats, kept out of the deduplicated rows: the blocks an item
/// places and its tag set (an index into the distinct tag sets).
fn per_type(data: &Data, tags: &[u128], out: &mut String) -> R<()> {
    const NONE: u16 = 0xffff;
    let state = |s: Option<u16>| match s {
        Some(s) if s < NONE => Ok(s),
        Some(_) => Err("too many block states".to_owned()),
        None => Ok(NONE),
    };
    for (name, doc, pick) in [
        (
            "TYPE_PLACES",
            "The state index placing each item type puts down (PMMP `Item::getBlock`); 0xffff: \
             none.",
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
    let mut sets: Vec<u128> = Vec::new();
    let mut set_at: BTreeMap<u128, usize> = BTreeMap::new();
    let mut type_sets = Vec::with_capacity(tags.len());
    for set in tags {
        let at = *set_at.entry(*set).or_insert_with(|| {
            sets.push(*set);
            sets.len() - 1
        });
        type_sets.push(u8::try_from(at).map_err(|_| "more than 256 tag sets")?);
    }
    put!(
        out,
        "\n/// The distinct tag sets of the item types: [`TAG_NAMES`] bits.\npub(super) static \
         TAG_SETS: [u128; {}] = [",
        sets.len()
    );
    for set in &sets {
        put!(out, "{},", u128_lit(*set));
    }
    put!(
        out,
        "];\n\n/// The [`TAG_SETS`] entry of each item type.\npub(super) static TYPE_TAGS: [u8; \
         {}] = [",
        type_sets.len()
    );
    for s in &type_sets {
        put!(out, "{s},");
    }
    out.push_str("];\n");
    Ok(())
}

/// A 32-digit hex literal, grouped by four.
fn u128_lit(v: u128) -> String {
    let digits = format!("{v:032x}");
    let mut out = String::from("0x");
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && i.is_multiple_of(4) {
            out.push('_');
        }
        out.push(c);
    }
    out
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
