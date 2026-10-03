//! `src/block/table.rs`: the block state table (PLAN.md 10.4) from [`Data`].

use std::collections::BTreeMap;

use super::body::str_lit;
use super::data::{Data, Prop, hex_lit, int_lit};
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
         use super::{{Data, Property, PropertyValue}};\n\n",
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
    Ok(out)
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
