//! `src/item/registry.rs`: the 1.26.50 item registry, every entry of the reference
//! list (`xtask/data/reference/items.json`) with its component data, in runtime ID order.

use serde::Deserialize;
use serde_json::Value as Json;

use crate::dump::{REFERENCE_DIR, REFERENCE_ITEMS};

use super::body::str_lit;
use super::data::{compound, read_json, static_compound};
use super::emit::HEADER;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/item/registry.rs";

/// The shield item, whose network stacks differ (gophertunnel `ItemStack` `BlockingTick`).
const SHIELD: &str = "minecraft:shield";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    runtime_id: i32,
    component_based: bool,
    version: i32,
    data: Json,
}

/// Generates the file text (unformatted). Fails if the list is not in strict runtime ID order or
/// an ID does not fit the network's int16.
pub(crate) fn generate(root: &std::path::Path) -> Result<String, String> {
    let entries: Vec<Entry> = read_json(root, REFERENCE_DIR, REFERENCE_ITEMS)?;
    let shield = entries
        .iter()
        .find(|e| e.name == SHIELD)
        .ok_or("the reference list has no shield")?;
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! The item registry generated from the 1.26.50 reference list: \
         {} entries.\n\n\
         use super::RegistryEntry;\n\n\
         /// The runtime ID of `{SHIELD}`: item stacks of it carry a blocking tick on the wire.\n\
         pub(crate) const SHIELD_RUNTIME_ID: i32 = {};\n\n\
         /// Every item of the reference list in runtime ID order: (name, runtime ID, component \
         based, version, data).\npub(super) static ENTRIES: [RegistryEntry; {}] = [",
        entries.len(),
        shield.runtime_id,
        entries.len()
    );
    let mut last = None;
    for e in &entries {
        let id = i16::try_from(e.runtime_id)
            .map_err(|_| format!("runtime ID of {} out of range", e.name))?;
        if last.is_some_and(|l| l >= id) {
            return Err(format!("{} is out of runtime ID order", e.name));
        }
        last = Some(id);
        let data = compound(&e.data).map_err(|err| format!("{}: {err}", e.name))?;
        put!(
            out,
            "RegistryEntry::new({}, {id}, {}, {}, {}),",
            str_lit(&e.name),
            e.component_based,
            e.version,
            static_compound(&data)
        );
    }
    out.push_str("];\n");
    Ok(out)
}
