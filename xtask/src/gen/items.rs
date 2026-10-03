//! `src/item/table.rs`: the item type table and its lookup indices (PLAN.md 10.4) from [`Data`].

use std::collections::BTreeMap;

use super::body::str_lit;
use super::data::{Data, hex_lit, int_lit, static_compound};
use super::emit::HEADER;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/item/table.rs";

/// Generates the file text (unformatted). Fails if a lookup could not tell two types apart.
pub(crate) fn generate(data: &Data) -> Result<String, String> {
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Item types generated from the PMMP dump and the 1.26.50 reference list \
         (PLAN.md 10.4): {} types.\n\n\
         use super::Data;\nuse crate::block::State;\n\n\
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
    Ok(out)
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
            int_lit(i64::from(block))
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
