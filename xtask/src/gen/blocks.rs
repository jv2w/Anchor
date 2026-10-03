//! `src/block/table.rs`: the block state table (PLAN.md 10.4) from [`Data`].

use std::collections::BTreeMap;

use super::body::str_lit;
use super::data::{Connect, Data, FACES, Prop, StateKey, hex_lit, int_lit};
use super::emit::HEADER;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/block/table.rs";

/// Generates the file text (unformatted).
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    // Names and (name, value) property pairs are interned; a state points into them.
    let mut names: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pairs: BTreeMap<(&str, &Prop), usize> = BTreeMap::new();
    for s in &data.states {
        names.entry(&s.key.name).or_default();
        for (k, v) in &s.key.properties {
            pairs.entry((k, v)).or_default();
        }
    }
    for (i, v) in names.values_mut().enumerate() {
        *v = i;
    }
    for (i, v) in pairs.values_mut().enumerate() {
        *v = i;
    }

    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Block states generated from the PMMP dump, the block upgrade schema and the \
         1.26.50 reference list (PLAN.md 10.4): {} states.\n\n\
         use super::{{Data, Property, PropertyValue, State}};\n\n",
        data.states.len()
    );
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
            Prop::Int(i) => format!("PropertyValue::Int({})", int_lit(i64::from(*i))),
            Prop::String(s) => format!("PropertyValue::String({})", str_lit(s)),
        };
        put!(out, "Property::new({}, {value}),", str_lit(name));
    }
    out.push_str("];\n\n");

    let mut state_properties = Vec::new();
    let mut states = String::new();
    for s in &data.states {
        let start = state_properties.len();
        for (k, v) in &s.key.properties {
            state_properties.push(*pairs.get(&(k.as_str(), v)).ok_or("pair not interned")?);
        }
        let name = *names.get(s.key.name.as_str()).ok_or("name not interned")?;
        put!(
            states,
            "Data::new({name}, {}, {}, {}, {}, {}, {}, {}),",
            int_lit(i64::try_from(start).map_err(|_| "too many properties")?),
            s.key.properties.len(),
            hex_lit(u64::from(s.network_id), 8),
            s.light_emission,
            s.light_filter,
            s.blocks_direct_sky_light,
            s.ticks_randomly,
        );
    }
    put!(
        out,
        "/// The property pairs of each state, as indices into [`PROPERTIES`].\n\
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
    lookups(&mut out, data)?;
    connections(&mut out, data)?;
    named(&mut out, data)?;
    Ok(out)
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

/// The connection table (PLAN.md 10.6): a packed word per state and the variant lists.
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

/// The lookup tables: the property count bound and the states by network ID. Checks the state
/// order `State::find` searches and that network IDs are unique.
fn lookups(out: &mut String, data: &Data) -> Result<(), String> {
    if u16::try_from(data.states.len()).is_err() {
        return Err("more than 65,535 block states".to_owned());
    }
    if data
        .states
        .windows(2)
        .any(|w| matches!(w, [a, b] if a.key >= b.key))
    {
        return Err("block states are not sorted by name and properties".to_owned());
    }
    let max_properties = data
        .states
        .iter()
        .map(|s| s.key.properties.len())
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
        "/// The most properties a state has.\npub(super) const MAX_PROPERTIES: usize = {max_properties};\n\n\
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
    out.push_str("];\n");
    Ok(())
}
