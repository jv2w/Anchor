//! Old block states and item names to the protocol version's, as PMMP upgrades
//! them when it loads a world: block states through
//! opencollab-incubator/BedrockBlockUpgradeSchema (PMMP `BlockDataUpgrader`), legacy numeric and
//! string IDs with a meta through its 1.12.0 table (`BlockIdMetaUpgrader`), and item stacks
//! through opencollab-incubator/BedrockItemUpgradeSchema (`ItemDataUpgrader`). The tables are
//! generated (`table`, `cargo xtask gen`); the rule interpreter (`apply`) is shared with the
//! generator, which checks the schemas with it.
//!
//! Where PMMP falls back to meta 0 for a legacy meta its table lacks, or drops what it cannot
//! read, this module gives `None` and the caller keeps the original as an unknown block, so
//! nothing is guessed and nothing is lost.

mod apply;
mod table;

pub(crate) use table::BLOCK_STATE_VERSION;

use crate::nbt::{Compound, List, Value};
use crate::save::palette_entry;
use apply::States;

/// The key PMMP adds to every block state it saves (`VersionInfo::TAG_WORLD_DATA_VERSION`); it
/// drops the key, of any type, when it reads a state (`BlockStateData::fromNbt`).
const PMMP_DATA_VERSION: &str = "PMMPDataVersion";

/// One item schema (PMMP `ItemIdMetaUpgradeSchema`): renamed IDs and, by ID, metas whose item
/// became an ID of its own, each sorted.
#[derive(Debug)]
struct ItemSchema {
    renamed: &'static [(&'static str, &'static str)],
    remapped: &'static [(&'static str, &'static [(u16, &'static str)])],
}

/// A disk palette entry upgraded to the protocol version (PMMP `upgradeBlockStateNbt`):
/// `{name, states, version}`. A modern entry is `{name, states}` with an optional `version` (0
/// without one), an optional `PMMPDataVersion` (dropped, as PMMP drops it) and nothing else, its
/// properties bytes, ints or strings; a legacy one is `{name, val}` with a string and a short,
/// looked up in the 1.12.0 table. `None` for anything else and for a legacy name and meta the
/// table does not have.
pub(crate) fn state(entry: &Compound) -> Option<Compound> {
    if let Some((name, meta)) = legacy_parts(entry) {
        let (name, mut states) = legacy_name(name, u8::try_from(meta).ok()?)?;
        let mut name = name.to_owned();
        // PMMP upgrades the table's state once more; it is at the newest version, so only the
        // schemas that share that version apply.
        apply::upgrade(&table::SCHEMAS, BLOCK_STATE_VERSION, &mut name, &mut states);
        return Some(to_nbt(name, states));
    }
    let (name, nbt_states) = modern_parts(entry)?;
    let version = match entry.get("version") {
        Some(Value::Int(v)) => *v,
        None => 0,
        Some(_) => return None,
    };
    let mut states = States::new();
    for (k, v) in nbt_states {
        let value = match v {
            Value::Byte(b) => apply::Value::Byte(*b),
            Value::Int(i) => apply::Value::Int(*i),
            Value::String(s) => apply::Value::String(s.clone()),
            _ => return None,
        };
        states.insert(k.clone(), value);
    }
    let mut name = name.clone();
    apply::upgrade(&table::SCHEMAS, version, &mut name, &mut states);
    Some(to_nbt(name, states))
}

/// The state of legacy terrain value `id`:`meta` (PMMP `upgradeIntIdMeta`, without its fallback
/// to meta 0), or `None` if the 1.12.0 table does not have it.
pub(crate) fn legacy(id: u16, meta: u8) -> Option<Compound> {
    let name = find_copied(&table::BLOCK_IDS, i16::try_from(id).ok()?)?;
    let (name, states) = legacy_name(name, meta)?;
    Some(to_nbt(name.to_owned(), states))
}

/// The upgraded state of a legacy string ID and meta in the 1.12.0 table.
fn legacy_name(id: &str, meta: u8) -> Option<(&'static str, States)> {
    let metas = find(&table::LEGACY, id)?;
    let index = find_copied(metas, meta)?;
    let (name, properties) = table::LEGACY_STATES.get(usize::from(*index))?;
    let states = properties
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.to_value()))
        .collect();
    Some((name, states))
}

/// A state as `{name, states, version}`.
fn to_nbt(name: String, states: States) -> Compound {
    let states = states
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                apply::Value::Byte(b) => Value::Byte(b),
                apply::Value::Int(i) => Value::Int(i),
                apply::Value::String(s) => Value::String(s),
            };
            (k, v)
        })
        .collect();
    palette_entry(name, states)
}

/// Upgrades, in place, every block state and item stack in a block entity's or entity's NBT, at
/// any depth (inside item tags too), and returns how many compounds changed. A block state is a
/// compound of `name` and `states` with an optional `version` and `PMMPDataVersion`, or of `name`
/// and `val`, as [`state`] reads it (a falling block's, a flower pot's plant, an item's `Block`);
/// an item stack is a compound with a byte `Count` and a string `Name` or a short legacy `id`
/// ([`item`]). What cannot be upgraded stays as it was.
pub(crate) fn nbt(compound: &mut Compound) -> u64 {
    // Compounds still to visit; the walk keeps its stack on the heap (NBT nests 512 deep).
    let mut stack: Vec<&mut Compound> = vec![compound];
    let mut changed = 0u64;
    while let Some(c) = stack.pop() {
        if is_state_shape(c) {
            if let Some(upgraded) = state(c)
                && upgraded != *c
            {
                *c = upgraded;
                changed += 1;
            }
            continue;
        }
        if item(c) {
            changed += 1;
        }
        for value in c.values_mut() {
            match value {
                Value::Compound(child) => stack.push(child),
                Value::List(List::Compound(list)) => stack.extend(list.iter_mut()),
                Value::List(List::List(lists)) => {
                    let mut nested: Vec<&mut List> = lists.iter_mut().collect();
                    while let Some(list) = nested.pop() {
                        match list {
                            List::Compound(list) => stack.extend(list.iter_mut()),
                            List::List(inner) => nested.extend(inner.iter_mut()),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    changed
}

/// Whether a compound has the shape of a block state [`state`] reads.
fn is_state_shape(c: &Compound) -> bool {
    modern_parts(c).is_some() || legacy_parts(c).is_some()
}

/// The name and properties of a modern block state: a string `name`, a compound `states`, and no
/// other key but `version` and `PMMPDataVersion`.
fn modern_parts(c: &Compound) -> Option<(&String, &Compound)> {
    match (c.get("name"), c.get("states")) {
        (Some(Value::String(name)), Some(Value::Compound(states)))
            if c.keys().all(|k| is_state_key(k)) =>
        {
            Some((name, states))
        }
        _ => None,
    }
}

/// The name and meta of a legacy block state: a string `name`, a short `val`, and nothing else.
fn legacy_parts(c: &Compound) -> Option<(&str, i16)> {
    match (c.len(), c.get("name"), c.get("val")) {
        (2, Some(Value::String(name)), Some(&Value::Short(meta))) => Some((name.as_str(), meta)),
        _ => None,
    }
}

/// Whether `key` belongs in a modern block state.
fn is_state_key(key: &str) -> bool {
    matches!(key, "name" | "states" | "version" | PMMP_DATA_VERSION)
}

/// Upgrades an item stack's type in place (PMMP `upgradeItemStackNbt`), returning whether it
/// changed. The name is `Name`, or the legacy numeric `id` through the legacy item ID map (an ID
/// of 0 is air, left as it is); the meta is `Damage`. A 1.12 block item without `Block` gets the
/// state of its block ID and meta; then the ID and meta go through the item schemas, and the
/// stack keeps `Name` and `Damage` (a legacy `id` becomes `Name`). A stack whose name is unknown
/// stays as it was.
fn item(c: &mut Compound) -> bool {
    if !matches!(c.get("Count"), Some(Value::Byte(_))) {
        return false;
    }
    let name = match (c.get("Name"), c.get("id")) {
        (Some(Value::String(name)), _) => name.clone(),
        (None, Some(Value::Short(id))) if *id != 0 => {
            let Some(name) = find_copied(&table::ITEM_IDS, *id) else {
                return false;
            };
            (*name).to_owned()
        }
        _ => return false,
    };
    let meta = match c.get("Damage") {
        Some(Value::Short(m)) => *m,
        None => 0,
        Some(_) => return false,
    };
    let block = if c.contains_key("Block") {
        None
    } else {
        let lower = name.to_ascii_lowercase();
        find(&table::ITEM_BLOCKS, lower.as_str())
            .and_then(|block| legacy_name(block, u8::try_from(meta).ok()?))
    };
    let (new_name, new_meta) = item_id_meta(&name, meta);
    // Only these keys change; each insert hands back the value it replaced.
    let mut changed = c.remove("id").is_some();
    let new_name = Value::String(new_name);
    changed |= c.insert("Name".to_owned(), new_name).as_ref() != c.get("Name");
    if new_meta != 0 || c.contains_key("Damage") {
        let new_meta = Value::Short(new_meta);
        changed |= c.insert("Damage".to_owned(), new_meta).as_ref() != c.get("Damage");
    }
    if let Some((block, states)) = block {
        c.insert(
            "Block".to_owned(),
            Value::Compound(to_nbt(block.to_owned(), states)),
        );
        changed = true;
    }
    changed
}

/// An item ID and meta through every item schema in order (PMMP `ItemIdMetaUpgrader::upgrade`):
/// a schema that remaps the meta gives a new ID with meta 0, else one that renames the ID gives
/// the new ID. IDs are looked up lowercased, as PMMP does.
fn item_id_meta(id: &str, meta: i16) -> (String, i16) {
    let mut id = id.to_owned();
    let mut lower = id.to_ascii_lowercase();
    let mut meta = meta;
    for schema in &table::ITEM_SCHEMAS {
        let remapped = u16::try_from(meta)
            .ok()
            .and_then(|m| find_copied(find(schema.remapped, lower.as_str())?, m));
        let new = match remapped {
            Some(new) => {
                meta = 0;
                new
            }
            None => match find(schema.renamed, lower.as_str()) {
                Some(new) => new,
                None => continue,
            },
        };
        (*new).clone_into(&mut id);
        lower = id.to_ascii_lowercase();
    }
    (id, meta)
}

/// Finds `key` in a slice sorted by its first field.
fn find<'a, K: Ord + ?Sized, T>(list: &'a [(&K, T)], key: &K) -> Option<&'a T> {
    let i = list.binary_search_by(|(k, _)| (*k).cmp(key)).ok()?;
    list.get(i).map(|(_, v)| v)
}

/// Like [`find`] for a slice keyed by value.
fn find_copied<K: Ord + Copy, T>(list: &[(K, T)], key: K) -> Option<&T> {
    let i = list.binary_search_by(|(k, _)| k.cmp(&key)).ok()?;
    list.get(i).map(|(_, v)| v)
}

#[cfg(test)]
mod tests;
