//! `src/block/table.rs`: the block state table from [`Data`].

use std::collections::{BTreeMap, BTreeSet};

use super::body::{int_lit, str_lit};
use super::data::{Connect, Data, FACES, Prop, StateItem, StateKey, hex_lit};
use super::emit::HEADER;
use super::info::{camel, f64_lit};

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/block/table.rs";

/// Generates the file text (unformatted).
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Block states generated from the PMMP dump, the block upgrade schemas and the \
         1.26.60 reference list: {} states and {} aliases.\n\n\
         use super::{{Alias, BreakInfo, Data, Info, KindData, Property, PropertyValue, State, \
         ToolType}};\n\
         use crate::math::{{Aabb, Vec3}};\n\n",
        data.states.len(),
        data.aliases.len()
    );
    let (names, pairs) = interned(&mut out, data);

    let mut state_properties = Vec::new();
    // The name index, first property and property count of a key, its properties appended.
    let mut intern = |key: &StateKey| -> Result<(usize, String, usize), String> {
        let start = state_properties.len();
        for (k, v) in &key.properties {
            state_properties.push(*pairs.get(&(k.as_str(), v)).ok_or("pair not interned")?);
        }
        let name = *names.get(key.name.as_str()).ok_or("name not interned")?;
        let start = int_lit(&start.to_string());
        Ok((name, start, key.properties.len()))
    };
    let mut states = String::new();
    for s in &data.states {
        let (name, start, count) = intern(&s.key)?;
        put!(
            states,
            "Data::new({name}, {start}, {count}, {}, {}, {}, {}, {}),",
            hex_lit(u64::from(s.network_id), 8),
            s.light_emission,
            s.light_filter,
            s.blocks_direct_sky_light,
            s.ticks_randomly,
        );
    }
    let mut aliases = String::new();
    for a in &data.aliases {
        let (name, start, count) = intern(&a.key)?;
        put!(
            aliases,
            "Alias::new({name}, {start}, {count}, {}),",
            a.state
        );
    }
    put!(
        out,
        "/// The property pairs of each state and alias, as indices into [`PROPERTIES`].\n\
         pub(super) static STATE_PROPERTIES: [u16; {}] = [",
        state_properties.len()
    );
    for p in &state_properties {
        put!(out, "{p},");
    }
    out.push_str("];\n\n");
    put!(
        out,
        "/// Every state, sorted by name and then properties: (name, first property, property \
         count, network ID, light emission, light filter, blocks direct sky light, ticks \
         randomly).\npub(super) static STATES: [Data; {}] = [{states}];\n\n",
        data.states.len()
    );
    put!(
        out,
        "/// The reference states that are not states but that PMMP reads as one, \
         sorted by name and then properties: (name, first property, property count, the state \
         PMMP reads it as).\npub(super) static ALIASES: [Alias; {}] = [{aliases}];\n\n",
        data.aliases.len()
    );
    lookups(&mut out, data)?;
    connections(&mut out, data)?;
    named(&mut out, data)?;
    kinds(&mut out, data)?;
    info(&mut out, data)?;
    banner_patterns(&mut out, data);
    Ok(out)
}

/// Names and (name, value) property pairs, each with its index in sorted order.
type Interned<'a> = (
    BTreeMap<&'a str, usize>,
    BTreeMap<(&'a str, &'a Prop), usize>,
);

/// Interns the names and property pairs of every state and alias, which point into them, and
/// emits them as `NAMES` and `PROPERTIES`.
fn interned<'a>(out: &mut String, data: &'a Data) -> Interned<'a> {
    let keys = data
        .states
        .iter()
        .map(|s| &s.key)
        .chain(data.aliases.iter().map(|a| &a.key));
    let mut names: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pairs: BTreeMap<(&str, &Prop), usize> = BTreeMap::new();
    for key in keys {
        names.entry(&key.name).or_default();
        for (k, v) in &key.properties {
            pairs.entry((k, v)).or_default();
        }
    }
    for (i, v) in names.values_mut().enumerate() {
        *v = i;
    }
    for (i, v) in pairs.values_mut().enumerate() {
        *v = i;
    }
    put!(
        out,
        "/// Block names, sorted.\npub(super) static NAMES: [&str; {}] = [",
        names.len()
    );
    for name in names.keys() {
        put!(out, "{},", str_lit(name));
    }
    out.push_str("];\n\n");
    put!(
        out,
        "/// Property name and value pairs, sorted.\npub(super) static PROPERTIES: [Property; {}] = [",
        pairs.len()
    );
    for (name, value) in pairs.keys() {
        let value = match value {
            Prop::Byte(b) => format!("PropertyValue::Byte({b})"),
            Prop::Int(i) => format!("PropertyValue::Int({})", int_lit(&i.to_string())),
            Prop::String(s) => format!("PropertyValue::String({})", str_lit(s)),
        };
        put!(out, "Property::new({}, {value}),", str_lit(name));
    }
    out.push_str("];\n\n");
    (names, pairs)
}

/// A generated enum: doc, name, variants (Rust name, documented name) and the derives.
pub(crate) fn enumeration(out: &mut String, doc: &str, name: &str, variants: &[(String, String)]) {
    pmmp_enumeration(out, doc, "", name, variants);
}

/// [`enumeration`] with the PMMP mapping lines (`// pmmp-class:` comments and `#[doc(alias)]`
/// attributes, each ending in a newline) between the doc and the derives.
pub(crate) fn pmmp_enumeration(
    out: &mut String,
    doc: &str,
    pmmp: &str,
    name: &str,
    variants: &[(String, String)],
) {
    put!(
        out,
        "\n/// {doc}\n{pmmp}#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]\npub enum {name} {{"
    );
    for (variant, documented) in variants {
        put!(out, "/// `{documented}`.\n{variant},");
    }
    out.push_str("}\n");
}

/// Upper camel case variants for sorted names; fails if two names give one variant.
pub(crate) fn variants(names: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let variant = camel(name);
        if !seen.insert(variant.clone()) {
            return Err(format!("two names give the variant {variant}"));
        }
        out.push((variant, (*name).to_owned()));
    }
    Ok(out)
}

/// A static array from small numeric IDs to the variants of `enum_name` (`None` where no
/// variant has the ID), so a lookup by ID is one index. Fails if two variants share an ID.
pub(crate) fn by_id(
    out: &mut String,
    doc: &str,
    name: &str,
    enum_name: &str,
    ids: &[(u8, &str)],
) -> Result<(), String> {
    let len = ids
        .iter()
        .map(|(id, _)| usize::from(*id) + 1)
        .max()
        .unwrap_or(0);
    let mut slots: Vec<Option<&str>> = vec![None; len];
    for (id, variant) in ids {
        let slot = slots.get_mut(usize::from(*id)).ok_or("ID out of range")?;
        if slot.replace(variant).is_some() {
            return Err(format!("two {enum_name}s have ID {id}"));
        }
    }
    put!(
        out,
        "\n/// {doc}\npub(super) static {name}: [Option<{enum_name}>; {len}] = ["
    );
    for slot in slots {
        match slot {
            Some(v) => put!(out, "Some({enum_name}::{v}),"),
            None => out.push_str("None,"),
        }
    }
    out.push_str("];\n");
    Ok(())
}

/// A static array of strings.
pub(crate) fn strings(out: &mut String, doc: &str, name: &str, list: &[&str]) {
    put!(
        out,
        "\n/// {doc}\npub(super) static {name}: [&str; {}] = [",
        list.len()
    );
    for s in list {
        put!(out, "{},", str_lit(s));
    }
    out.push_str("];\n");
}

/// The PMMP block types: the `Kind` enum, its names, and each kind's default state, class chain
/// and traits; and the class, trait and type tag enums.
fn kinds(out: &mut String, data: &Data) -> Result<(), String> {
    let names: Vec<&str> = data.kinds.iter().map(|(k, _)| k.kind.as_str()).collect();
    let kind_variants = variants(&names)?;
    enumeration(
        out,
        "A PMMP block type (a `VanillaBlocks` entry), such as `OakPlanks`; [`State::kind`] gives \
         a state's.",
        "Kind",
        &kind_variants,
    );
    strings(
        out,
        "The `VanillaBlocks` name of each kind, lowercase.",
        "KIND_NAMES",
        &names,
    );
    put!(
        out,
        "\n/// Every kind, in name order.\npub(super) static KINDS: [Kind; {}] = [",
        names.len()
    );
    for (v, _) in &kind_variants {
        put!(out, "Kind::{v},");
    }
    out.push_str("];\n");

    let class_set: BTreeSet<&str> = data
        .kinds
        .iter()
        .flat_map(|(k, _)| k.classes.iter().map(String::as_str))
        .collect();
    let classes: Vec<&str> = class_set.into_iter().collect();
    let trait_set: BTreeSet<&str> = data
        .kinds
        .iter()
        .flat_map(|(k, _)| k.traits.iter().map(String::as_str))
        .collect();
    let traits: Vec<&str> = trait_set.into_iter().collect();
    let class_variants = variants(&classes)?;
    enumeration(
        out,
        "A PMMP block class or one of its parents (`Block` for every block); [`State::class`] \
         gives a state's.",
        "Class",
        &class_variants,
    );
    strings(out, "The PMMP name of each class.", "CLASS_NAMES", &classes);
    enumeration(
        out,
        "A PMMP block trait or interface (`FallableTrait` and `Fallable` are both `Fallable`); \
         [`State::has_trait`] tells a state's.",
        "Trait",
        &variants(&traits)?,
    );
    strings(out, "The PMMP name of each trait.", "TRAIT_NAMES", &traits);

    let mut chains = String::new();
    let mut chain_len = 0;
    let mut chain_at: BTreeMap<&[String], usize> = BTreeMap::new();
    let mut rows = String::new();
    for (identity, state) in &data.kinds {
        let start = *chain_at.entry(&identity.classes).or_insert_with(|| {
            for c in &identity.classes {
                put!(chains, "Class::{},", camel(c));
            }
            chain_len += identity.classes.len();
            chain_len - identity.classes.len()
        });
        put!(
            rows,
            "KindData::new(State::from_index({state}), {start}, {}, {}),",
            identity.classes.len(),
            hex_lit(bits(&identity.traits, &traits)?, 16)
        );
    }
    put!(
        out,
        "\n/// The class chains of [`KIND_DATA`] one after another, leaf first.\npub(super) static \
         CLASS_CHAINS: [Class; {chain_len}] = [{chains}];\n\n/// Each kind's default state (PMMP \
         `VanillaBlocks`), class chain and traits.\npub(super) static KIND_DATA: [KindData; {}] = \
         [{rows}];\n",
        names.len()
    );
    Ok(())
}

/// Sorted unique type tags of every state.
fn type_tags(data: &Data) -> Vec<&str> {
    let set: BTreeSet<&str> = data
        .states
        .iter()
        .flat_map(|s| s.physics.tags.iter().map(String::as_str))
        .collect();
    set.into_iter().collect()
}

/// Bits of `list` in the sorted `names`.
fn bits(list: &[String], names: &[&str]) -> Result<u64, String> {
    let mut out = 0u64;
    for item in list {
        let at = names
            .binary_search(&item.as_str())
            .map_err(|_| format!("unknown name {item}"))?;
        out |= 1u64
            .checked_shl(u32::try_from(at).map_err(|_| "bit")?)
            .ok_or("more than 64 names")?;
    }
    Ok(out)
}

/// The state item sentinels of `STATE_ITEMS` (see `src/block.rs`).
const NO_ITEM: u16 = 0xffff;
const BLOCK_ENTITY_ITEM: u16 = 0xfffe;

/// The per-state gameplay data: the type tag enum, the deduplicated physics rows
/// with their collision boxes, and each state's kind, row and item form.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table layout, emitted in one pass"
)]
fn info(out: &mut String, data: &Data) -> Result<(), String> {
    let tags = type_tags(data);
    if tags.len() > 16 {
        return Err("more than 16 type tags".to_owned());
    }
    let tag_names: Vec<&str> = tags
        .iter()
        .map(|t| t.strip_prefix("pocketmine:").unwrap_or(t))
        .collect();
    enumeration(
        out,
        "A PMMP block type tag (`BlockTypeTags`); [`State::has_type_tag`] tells a state's.",
        "TypeTag",
        &variants(&tag_names)?,
    );
    strings(
        out,
        "The PMMP name of each type tag.",
        "TYPE_TAG_NAMES",
        &tags,
    );

    let kind_index: BTreeMap<&str, usize> = data
        .kinds
        .iter()
        .enumerate()
        .map(|(i, (k, _))| (k.kind.as_str(), i))
        .collect();
    let mut boxes: Vec<[f64; 6]> = Vec::new();
    let mut box_at: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows: Vec<String> = Vec::new();
    let mut row_at: BTreeMap<String, usize> = BTreeMap::new();
    let mut state_kinds = Vec::with_capacity(data.states.len());
    let mut state_rows = Vec::with_capacity(data.states.len());
    let mut state_items = Vec::with_capacity(data.states.len());
    for s in &data.states {
        let kind = *kind_index
            .get(s.identity.kind.as_str())
            .ok_or_else(|| format!("unknown kind {}", s.identity.kind))?;
        state_kinds.push(kind);
        let p = &s.physics;
        let start = *box_at.entry(format!("{:?}", p.boxes)).or_insert_with(|| {
            boxes.extend(p.boxes.iter().copied());
            boxes.len() - p.boxes.len()
        });
        let support = p
            .support
            .iter()
            .enumerate()
            .fold(0u16, |acc, (i, code)| acc | u16::from(*code) << (2 * i));
        let flags = u16::from(p.flags) | u16::from(p.burns_forever) << 8;
        let b = &p.break_info;
        let row = format!(
            "Info::new(BreakInfo::new({}, {}, ToolType({}), {}, {}), {}, {}, {}, {}, {}, {}, {}, \
             {}),",
            f64_lit(b.hardness),
            f64_lit(b.blast_resistance),
            b.tool_type,
            b.harvest_level,
            b.explosion_harvestable,
            p.flammability,
            p.encouragement,
            hex_lit(u64::from(flags), 4),
            f64_lit(p.friction),
            bits(&p.tags, &tags)?,
            hex_lit(u64::from(support), 4),
            start,
            p.boxes.len(),
        );
        let index = *row_at.entry(row.clone()).or_insert_with(|| {
            rows.push(row);
            rows.len() - 1
        });
        state_rows.push(index);
        state_items.push(match s.item {
            StateItem::None => NO_ITEM,
            StateItem::BlockEntity => BLOCK_ENTITY_ITEM,
            StateItem::Item(i) if i < BLOCK_ENTITY_ITEM => i,
            StateItem::Item(_) => return Err("too many item types".to_owned()),
        });
    }
    put!(
        out,
        "\n/// The block-local collision boxes of [`INFO`] one after another.\npub(super) static \
         BOXES: [Aabb; {}] = [",
        boxes.len()
    );
    for &[x0, y0, z0, x1, y1, z1] in &boxes {
        put!(
            out,
            "Aabb::new(Vec3::new({}, {}, {}), Vec3::new({}, {}, {})),",
            f64_lit(x0),
            f64_lit(y0),
            f64_lit(z0),
            f64_lit(x1),
            f64_lit(y1),
            f64_lit(z1)
        );
    }
    put!(
        out,
        "];\n\n/// The distinct physics rows of the states (see `Info` in `src/block.rs`).\n\
         pub(super) static INFO: [Info; {}] = [{}];\n",
        rows.len(),
        rows.concat()
    );
    let list = |out: &mut String, doc: &str, name: &str, values: &[String]| {
        put!(
            out,
            "\n/// {doc}\npub(super) static {name}: [u16; {}] = [{}];\n",
            values.len(),
            values.concat()
        );
    };
    let kinds: Vec<String> = state_kinds.iter().map(|k| format!("{k},")).collect();
    list(
        out,
        "The [`KINDS`] index of each state.",
        "STATE_KIND",
        &kinds,
    );
    let rows: Vec<String> = state_rows.iter().map(|r| format!("{r},")).collect();
    list(out, "The [`INFO`] row of each state.", "STATE_INFO", &rows);
    let items: Vec<String> = state_items
        .iter()
        .map(|i| format!("{},", hex_lit(u64::from(*i), 4)))
        .collect();
    list(
        out,
        &format!(
            "The item type index of each state's item form: {NO_ITEM:#x} none, \
             {BLOCK_ENTITY_ITEM:#x} from the block entity."
        ),
        "STATE_ITEMS",
        &items,
    );
    Ok(())
}

/// The banner pattern enum and IDs (PMMP `BannerPatternType`, `BannerPatternTypeIdMap`).
fn banner_patterns(out: &mut String, data: &Data) {
    let list = &data.tables.banner_patterns;
    let names: Vec<&str> = list.iter().map(|b| b.name.as_str()).collect();
    let variants: Vec<(String, String)> =
        names.iter().map(|n| (camel(n), (*n).to_owned())).collect();
    enumeration(
        out,
        "A banner pattern (PMMP `BannerPatternType`).",
        "BannerPattern",
        &variants,
    );
    put!(
        out,
        "\n/// Every banner pattern, in name order.\npub(super) static BANNER_PATTERNS: [BannerPattern; {}] = [",
        list.len()
    );
    for (v, _) in &variants {
        put!(out, "BannerPattern::{v},");
    }
    out.push_str("];\n");
    let ids: Vec<&str> = list.iter().map(|b| b.id.as_str()).collect();
    strings(
        out,
        "The save and network ID of each banner pattern.",
        "BANNER_PATTERN_IDS",
        &ids,
    );
    let mut by_id: Vec<(&str, &str)> = ids
        .iter()
        .zip(&variants)
        .map(|(id, (v, _))| (*id, v.as_str()))
        .collect();
    by_id.sort_unstable();
    put!(
        out,
        "\n/// The banner patterns by ID, sorted by ID.\npub(super) static BANNER_PATTERNS_BY_ID: \
         [(&str, BannerPattern); {}] = [",
        by_id.len()
    );
    for (id, v) in by_id {
        put!(out, "({}, BannerPattern::{v}),", str_lit(id));
    }
    out.push_str("];\n");
}

/// A named state: constant name, block name and properties.
type Named = (&'static str, &'static str, &'static [(&'static str, Prop)]);

/// The states the engine names in code (`block::AIR` and so on).
const NAMED: &[Named] = &[
    ("AIR", "minecraft:air", &[]),
    (
        "BEDROCK",
        "minecraft:bedrock",
        &[("infiniburn_bit", Prop::Byte(0))],
    ),
    ("DIRT", "minecraft:dirt", &[]),
    ("GRASS_BLOCK", "minecraft:grass_block", &[]),
    ("INFO_UPDATE", "minecraft:info_update", &[]),
];

/// The named state constants.
fn named(out: &mut String, data: &Data) -> Result<(), String> {
    for (constant, name, properties) in NAMED {
        let index = data
            .states
            .iter()
            .position(|s| {
                s.key.name == *name
                    && s.key.properties.len() == properties.len()
                    && properties
                        .iter()
                        .all(|(p, v)| s.key.properties.iter().any(|(q, w)| q == p && w == v))
            })
            .ok_or_else(|| format!("no state {name} {properties:?}"))?;
        put!(
            out,
            "\n/// `{name}`.\npub(crate) const {constant}: State = State::from_index({index});\n"
        );
    }
    Ok(())
}

/// How a kind of connecting block lists its variants: the properties the neighbours decide and
/// the values of each, in variant order. A variant's index is the mixed-radix number of its
/// values' positions, first property most significant (`NO_VARIANTS` in `src/block.rs` documents
/// the same order).
fn variant_properties(kind: &str) -> Option<Vec<(&'static str, Vec<Prop>)>> {
    let bit = || vec![Prop::Byte(0), Prop::Byte(1)];
    let wall = || {
        ["none", "short", "tall"]
            .map(|v| Prop::String(v.to_owned()))
            .to_vec()
    };
    Some(match kind {
        // Bits west, south, east, north from the most significant: index bit 0 is north.
        "fence" | "thin" | "tripwire" => vec![
            ("minecraft:connection_west", bit()),
            ("minecraft:connection_south", bit()),
            ("minecraft:connection_east", bit()),
            ("minecraft:connection_north", bit()),
        ],
        "stair" => vec![(
            "minecraft:corner",
            [
                "none",
                "inner_left",
                "inner_right",
                "outer_left",
                "outer_right",
            ]
            .map(|v| Prop::String(v.to_owned()))
            .to_vec(),
        )],
        "wall" => vec![
            ("wall_connection_type_north", wall()),
            ("wall_connection_type_east", wall()),
            ("wall_connection_type_south", wall()),
            ("wall_connection_type_west", wall()),
            ("wall_post_bit", bit()),
        ],
        "fence_gate" => vec![("in_wall_bit", bit())],
        _ => return None,
    })
}

/// The kind, facing, upside-down and fence group bits of a connection word.
fn header(connect: &Connect, groups: &BTreeMap<String, u8>) -> Result<u32, String> {
    let kind: u32 = match connect.kind.as_str() {
        "fence" => 1,
        "thin" => 2,
        "wall" => 3,
        "fence_gate" => 4,
        "stair" => 5,
        "tripwire" => 6,
        "tripwire_hook" => 7,
        other => return Err(format!("unknown connect kind {other}")),
    };
    let facing: u32 = match connect.facing.as_deref() {
        None => 0,
        Some(f) => {
            let at = FACES
                .iter()
                .position(|g| *g == f)
                .ok_or_else(|| format!("unknown facing {f}"))?;
            u32::try_from(at).map_err(|_| "facing")? + 1
        }
    };
    let upside_down = u32::from(connect.upside_down.unwrap_or(false));
    let group = connect
        .group
        .as_ref()
        .and_then(|g| groups.get(g))
        .copied()
        .unwrap_or(0);
    Ok(kind | facing << 8 | upside_down << 11 | u32::from(group) << 12)
}

/// A state's variant list key (the state without the variant properties), its index in the
/// list and the list length; `None` if its kind has no variants or the state has none of the
/// variant properties.
fn variant_of(key: &StateKey, kind: &str) -> Result<Option<(StateKey, usize, usize)>, String> {
    let Some(props) = variant_properties(kind) else {
        return Ok(None);
    };
    let mut base = key.clone();
    base.properties
        .retain(|(p, _)| !props.iter().any(|(q, _)| p == q));
    match key.properties.len() - base.properties.len() {
        0 => return Ok(None),
        found if found != props.len() => {
            return Err(format!(
                "{} has only some of its connection properties",
                key.name
            ));
        }
        _ => {}
    }
    let mut index = 0;
    let mut count = 1;
    for (name, values) in &props {
        let value = key
            .properties
            .iter()
            .find(|(p, _)| p == name)
            .map(|(_, v)| v)
            .ok_or("variant property lost")?;
        let at = values
            .iter()
            .position(|v| v == value)
            .ok_or_else(|| format!("{} {name} has an unknown value {value:?}", key.name))?;
        index = index * values.len() + at;
        count *= values.len();
    }
    Ok(Some((base, index, count)))
}

/// The connection table: a packed word per state and the variant lists.
fn connections(out: &mut String, data: &Data) -> Result<(), String> {
    const NONE: u32 = 0xffff;
    let mut groups: BTreeMap<String, u8> = BTreeMap::new();
    for s in &data.states {
        if let Some(group) = s.connect.as_ref().and_then(|c| c.group.as_ref()) {
            groups.entry(group.clone()).or_default();
        }
    }
    for (i, v) in groups.values_mut().enumerate() {
        *v = u8::try_from(i)
            .ok()
            .filter(|&g| g < 16)
            .ok_or("more than 16 fence groups")?;
    }

    // Variant lists by the state with the variant properties taken out, in state order.
    let mut lists: Vec<Vec<Option<usize>>> = Vec::new();
    let mut list_of: BTreeMap<StateKey, usize> = BTreeMap::new();
    let mut words = Vec::with_capacity(data.states.len());
    for (i, s) in data.states.iter().enumerate() {
        let Some(connect) = &s.connect else {
            words.push(u32::from(s.full_faces) << 4 | NONE << 16);
            continue;
        };
        let mut variants = NONE;
        if let Some((base, index, count)) = variant_of(&s.key, &connect.kind)? {
            let list = *list_of.entry(base).or_insert_with(|| {
                lists.push(vec![None; count]);
                lists.len() - 1
            });
            let slot = lists
                .get_mut(list)
                .and_then(|l| l.get_mut(index))
                .ok_or("variant list")?;
            *slot = Some(i);
            variants = u32::try_from(list).map_err(|_| "variant lists")?;
        }
        words.push(header(connect, &groups)? | u32::from(s.full_faces) << 4 | variants << 16);
    }
    // Lay the lists out one after another; a state's word holds its list's offset.
    let mut offsets = Vec::with_capacity(lists.len());
    let mut flat = Vec::new();
    for (n, list) in lists.iter().enumerate() {
        offsets.push(u32::try_from(flat.len()).map_err(|_| "too many variants")?);
        for entry in list {
            flat.push(entry.ok_or_else(|| format!("variant list {n} is not complete"))?);
        }
    }
    if flat.len() >= 0xffff {
        return Err("more than 65,534 variants".to_owned());
    }
    for word in &mut words {
        let list = *word >> 16;
        if list != NONE {
            let offset = offsets
                .get(usize::try_from(list).map_err(|_| "list")?)
                .ok_or("list")?;
            *word = (*word & 0xffff) | offset << 16;
        }
    }
    put!(
        out,
        "\n/// Each state's part in neighbour connections, packed as `NO_VARIANTS` in \
         `src/block.rs` describes: kind, full faces, facing, upside down, fence group and the \
         offset of its variants in [`VARIANTS`].\npub(super) static CONNECT: [u32; {}] = [",
        words.len()
    );
    for w in &words {
        put!(out, "{},", hex_lit(u64::from(*w), 8));
    }
    put!(
        out,
        "];\n\n/// The variants of connecting states, one list after another in variant order.\n\
         pub(super) static VARIANTS: [u16; {}] = [",
        flat.len()
    );
    for v in &flat {
        put!(out, "{v},");
    }
    out.push_str("];\n");
    Ok(())
}

/// The lookup tables: the property count bound, the states by network ID and the aliases'
/// network IDs. Checks the state
/// and alias order `State::find` and `State::read` search and that network IDs are unique.
fn lookups(out: &mut String, data: &Data) -> Result<(), String> {
    if u16::try_from(data.states.len()).is_err() {
        return Err("more than 65,535 block states".to_owned());
    }
    let sorted = |keys: &[&StateKey]| keys.windows(2).all(|w| matches!(w, [a, b] if a < b));
    if !sorted(&data.states.iter().map(|s| &s.key).collect::<Vec<_>>()) {
        return Err("block states are not sorted by name and properties".to_owned());
    }
    if !sorted(&data.aliases.iter().map(|a| &a.key).collect::<Vec<_>>()) {
        return Err("block state aliases are not sorted by name and properties".to_owned());
    }
    let max_properties = data
        .states
        .iter()
        .map(|s| &s.key)
        .chain(data.aliases.iter().map(|a| &a.key))
        .map(|key| key.properties.len())
        .max()
        .unwrap_or(0);
    let mut by_network: Vec<(u32, usize)> = data
        .states
        .iter()
        .enumerate()
        .map(|(i, s)| (s.network_id, i))
        .collect();
    by_network.sort_unstable();
    if by_network
        .windows(2)
        .any(|w| matches!(w, [a, b] if a.0 == b.0))
    {
        return Err("two block states have the same network ID".to_owned());
    }
    put!(
        out,
        "/// The most properties a state has.\npub(crate) const MAX_PROPERTIES: usize = {max_properties};\n\n\
         /// The network IDs of the states, sorted; [`NETWORK_ID_STATES`] holds the state of each.\n\
         pub(super) static NETWORK_IDS: [u32; {}] = [",
        by_network.len()
    );
    for (id, _) in &by_network {
        put!(out, "{},", hex_lit(u64::from(*id), 8));
    }
    put!(
        out,
        "];\n\n/// The state index of each entry of [`NETWORK_IDS`].\n\
         pub(super) static NETWORK_ID_STATES: [u16; {}] = [",
        by_network.len()
    );
    for (_, index) in &by_network {
        put!(out, "{index},");
    }
    let mut alias_ids: Vec<u32> = data.aliases.iter().map(|a| a.network_id).collect();
    alias_ids.sort_unstable();
    put!(
        out,
        "];\n\n/// The network IDs of the aliases, sorted.\n\
         pub(super) static ALIAS_NETWORK_IDS: [u32; {}] = [",
        alias_ids.len()
    );
    for id in &alias_ids {
        put!(out, "{},", hex_lit(u64::from(*id), 8));
    }
    out.push_str("];\n");
    Ok(())
}
