//! `src/item/creative.rs`: the creative inventory from the PMMP dump
//! (`creative.json`), with groups built as PMMP's `CreativeInventoryCache` builds them.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use crate::dump::{PMMP_CREATIVE, PMMP_DIR};

use super::body::str_lit;
use super::data::{Data, Tag, read_json, static_compound};
use super::emit::HEADER;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/item/creative.rs";

/// A dumped stack resolved to an item type: type index, network meta, stack NBT.
type Stack = (usize, u16, Option<BTreeMap<String, Tag>>);

/// Generates the file text (unformatted).
pub(crate) fn generate(root: &std::path::Path, data: &Data) -> Result<String, String> {
    let entries: Vec<Json> = read_json(root, PMMP_DIR, PMMP_CREATIVE)?;

    // PMMP `CreativeInventoryCache::buildCacheEntry`: a group gets an index the first time it is
    // seen in a category. A PMMP group object is one group of a creative data file, whose entries
    // are consecutive, so a run of entries with the same category and group is one object.
    // Entries without a group share one anonymous group per category until an entry with a group
    // in that category starts a new anonymous group.
    let mut groups: Vec<(&str, Option<&Json>)> = Vec::new();
    // The key of a category's anonymous group (entries without a group) is `None`.
    let mut group_index: BTreeMap<(&str, Option<usize>), usize> = BTreeMap::new();
    let mut items = Vec::with_capacity(entries.len());
    let mut run = 0;
    let mut previous: Option<(&str, &Json)> = None;
    for entry in &entries {
        let item = entry.get("item").ok_or("entry without item")?;
        if is_lost_enchanted_book(item) {
            continue;
        }
        let category = category(entry.get("category"))?;
        let group = entry.get("group").filter(|g| !g.is_null());
        let key = if let Some(g) = group {
            if previous != Some((category, g)) {
                run += 1;
            }
            previous = Some((category, g));
            group_index.remove(&(category, None));
            Some(run)
        } else {
            previous = None;
            None
        };
        let next = groups.len();
        let gi = *group_index.entry((category, key)).or_insert(next);
        if gi == next {
            groups.push((category, group));
        }
        let stack = resolve(data, item)?;
        items.push((gi, stack));
    }

    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! The creative inventory generated from the PMMP dump: {} \
         groups, {} items.\n\n\
         use super::{{CreativeGroup, CreativeItem, StaticStack, Type}};\n\
         use crate::protocol::consts::CreativeCategory;\n\n\
         /// The groups in PMMP's order: (category, name, icon).\n\
         pub(super) static GROUPS: [CreativeGroup; {}] = [",
        groups.len(),
        items.len(),
        groups.len()
    );
    for (category, group) in &groups {
        let Some(g) = group else {
            put!(
                out,
                "CreativeGroup::new(CreativeCategory::{category} as u8, \"\", None),"
            );
            continue;
        };
        let name = g
            .get("name")
            .and_then(Json::as_str)
            .ok_or("group without a name")?;
        // PMMP translates the name for the client; a name without `%` comes out unchanged.
        if name.contains('%') {
            return Err(format!("group name {name} needs translation"));
        }
        let (item, meta, nbt) = resolve(data, g.get("icon").ok_or("group without icon")?)?;
        put!(
            out,
            "CreativeGroup::new(CreativeCategory::{category} as u8, {}, Some({})),",
            str_lit(name),
            stack(item, meta, nbt.as_ref())
        );
    }
    put!(
        out,
        "];\n\n/// The items in PMMP's order, so an item's index is its creative item network ID: \
         (group index, stack).\npub(super) static ITEMS: [CreativeItem; {}] = [",
        items.len()
    );
    for (group, (item, meta, nbt)) in &items {
        put!(
            out,
            "CreativeItem::new({group}, {}),",
            stack(*item, *meta, nbt.as_ref())
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// The `CreativeCategory` variant of a dumped category name; its number comes from the IR.
fn category(json: Option<&Json>) -> Result<&'static str, String> {
    match json.and_then(Json::as_str) {
        Some("construction") => Ok("Construction"),
        Some("nature") => Ok("Nature"),
        Some("equipment") => Ok("Equipment"),
        Some("items") => Ok("Items"),
        other => Err(format!("unknown creative category {other:?}")),
    }
}

/// Rust source of a `StaticStack`.
fn stack(item: usize, meta: u16, nbt: Option<&BTreeMap<String, Tag>>) -> String {
    let nbt = nbt.filter(|c| !c.is_empty()).map_or_else(
        || "None".to_owned(),
        |c| format!("Some({})", static_compound(c)),
    );
    format!("StaticStack::new(Type::from_index({item}), {meta}, {nbt})")
}

/// Whether a dumped stack is an enchanted book without enchantments. PMMP drops the enchantments
/// it does not implement when it reads the creative data, which leaves a bare book in place of
/// each book of such an enchantment (61 identical entries that BDS does not list, P6); Anchor
/// leaves them out.
fn is_lost_enchanted_book(stack: &Json) -> bool {
    stack.get("name").and_then(Json::as_str) == Some("minecraft:enchanted_book")
        && stack.get("nbt").and_then(|n| n.get("ench")).is_none()
}

/// Resolves a dumped creative stack ([`Data::stack`]); PMMP's creative stacks hold one item.
fn resolve(data: &Data, json: &Json) -> Result<Stack, String> {
    let s = data.stack(json)?;
    if s.count != 1 {
        return Err(format!("creative stack with count {}", s.count));
    }
    Ok((usize::from(s.item), s.meta, s.nbt))
}
