//! `src/import/upgrade/table.rs`: the block and item upgrade tables of the world import from the
//! schemas `cargo xtask dump` copied into `xtask/data/upgrade/`.
//!
//! The block schemas are checked with the import's own interpreter (`apply.rs`, included by
//! path), so a defect the rules hide fails here: an ID that is both renamed and flattened in one
//! schema (the dump source throws on it at run time), a schema renaming a property to a name another rename
//! of the block reads (the result would depend on order), a byte value above 127 (the dump source reads
//! bytes signed, Anchor unsigned), and any of the reference list's states that does not upgrade
//! to itself (Anchor writes the reference version, and modern states go through the schemas that
//! share it again). The legacy 1.12.0 table is upgraded here once, as the dump source does when it loads it;
//! entries whose result is not a reference state are listed in the generated file. The item
//! tables are checked the way the dump source reads them: lowercase IDs, numeric metas, and legacy IDs that
//! map one to one.

#[path = "../../../src/import/upgrade/apply.rs"]
mod apply;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value as Json;

use crate::dump::upgrade::{
    BLOCK_LEGACY_IDS, BLOCK_SCHEMAS, ITEM_BLOCKS, ITEM_LEGACY_IDS, ITEM_SCHEMAS, LEGACY_TABLE,
    OUT as DIR,
};
use crate::dump::{REFERENCE_BLOCKS, REFERENCE_DIR, read};

use super::body::{int_lit, str_lit};
use super::emit::HEADER;
use apply::{Flatten, Kind, NewName, Remap, Rules, Schema, States, Tag, Value};

/// The block schemas that add properties after the dump source's data version, oldest first;
/// `data` reads them too ([`block_schema`]).
pub(crate) const ADDING: [&str; 2] = ["0351_1.26.40_to_1.26.50.json", SCHEMA_1_26_60];
/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/import/upgrade/table.rs";
/// The block schema of 1.26.60: it adds the connection properties of fire, redstone wire and
/// chorus plants.
pub(crate) const SCHEMA_1_26_60: &str = "0361_1.26.50_to_1.26.60.json";

/// Upgraded legacy entries: (string ID, meta) → index into the distinct states, with those states.
type Legacy = (BTreeMap<(String, u8), usize>, Vec<(String, States)>);

/// The path of the copied block schema `name`.
pub(crate) fn block_schema(root: &Path, name: &str) -> PathBuf {
    root.join(DIR).join(BLOCK_SCHEMAS).join(name)
}

/// A property value in a schema: exactly one field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TagModel {
    byte: Option<i64>,
    int: Option<i64>,
    string: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueRemapModel {
    old: TagModel,
    new: TagModel,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FlattenModel {
    prefix: String,
    flattened_property: String,
    flattened_property_type: Option<String>,
    suffix: String,
    #[serde(default)]
    flattened_value_remaps: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemapModel {
    old_state: Option<BTreeMap<String, TagModel>>,
    new_name: Option<String>,
    new_flattened_name: Option<FlattenModel>,
    new_state: Option<BTreeMap<String, TagModel>>,
    #[serde(default)]
    copied_state: Vec<String>,
}

/// A block schema file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SchemaModel {
    max_version_major: u8,
    max_version_minor: u8,
    max_version_patch: u8,
    max_version_revision: u8,
    #[serde(default)]
    renamed_ids: BTreeMap<String, String>,
    #[serde(default)]
    added_properties: BTreeMap<String, BTreeMap<String, TagModel>>,
    #[serde(default)]
    removed_properties: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    renamed_properties: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    remapped_property_values: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    remapped_property_values_index: BTreeMap<String, Vec<ValueRemapModel>>,
    #[serde(default)]
    flattened_properties: BTreeMap<String, FlattenModel>,
    #[serde(default)]
    remapped_states: BTreeMap<String, Vec<RemapModel>>,
}

/// An item schema file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ItemSchemaModel {
    #[serde(default)]
    renamed_ids: BTreeMap<String, String>,
    #[serde(default)]
    remapped_metas: BTreeMap<String, BTreeMap<String, String>>,
}

/// One entry of the legacy table, as `dump` writes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyEntry {
    id: String,
    meta: u8,
    state: Json,
}

/// An upgraded item schema.
#[derive(Debug)]
struct ItemSchema {
    renamed: BTreeMap<String, String>,
    remapped: BTreeMap<String, BTreeMap<u16, String>>,
}

/// Keeps `value` for the rest of the process: `gen` is one short run, and the interpreter takes
/// its tables as borrowed slices, as the generated statics are.
fn keep<T>(value: Vec<T>) -> &'static [T] {
    Box::leak(value.into_boxed_slice())
}

fn keep_str(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

fn tag(model: &TagModel) -> Result<Tag<'static>, String> {
    match model {
        TagModel {
            byte: Some(b),
            int: None,
            string: None,
        } => u8::try_from(*b)
            .ok()
            .filter(|b| *b <= 127)
            .map(Tag::Byte)
            .ok_or_else(|| format!("byte {b} is not 0 to 127")),
        TagModel {
            byte: None,
            int: Some(i),
            string: None,
        } => i32::try_from(*i)
            .map(Tag::Int)
            .map_err(|_| format!("int {i} does not fit")),
        TagModel {
            byte: None,
            int: None,
            string: Some(s),
        } => Ok(Tag::String(keep_str(s))),
        _ => Err("a value has not exactly one of byte, int and string".to_owned()),
    }
}

fn tags(
    map: Option<&BTreeMap<String, TagModel>>,
) -> Result<&'static [(&'static str, Tag<'static>)], String> {
    let mut out = Vec::new();
    for (k, v) in map.into_iter().flatten() {
        out.push((keep_str(k), tag(v)?));
    }
    Ok(keep(out))
}

fn flatten(model: &FlattenModel) -> Result<Flatten<'static>, String> {
    let kind = match model.flattened_property_type.as_deref() {
        None | Some("string") => Kind::String,
        Some("int") => Kind::Int,
        Some("byte") => Kind::Byte,
        Some(other) => return Err(format!("flattened property type {other}")),
    };
    let remaps: Vec<_> = model
        .flattened_value_remaps
        .iter()
        .map(|(k, v)| (keep_str(k), keep_str(v)))
        .collect();
    Ok(Flatten {
        prefix: keep_str(&model.prefix),
        property: keep_str(&model.flattened_property),
        suffix: keep_str(&model.suffix),
        kind,
        remaps: keep(remaps),
    })
}

/// Reads one state rule.
fn remap(model: &RemapModel) -> Result<Remap<'static>, String> {
    let new_name = match (&model.new_name, &model.new_flattened_name) {
        (Some(n), None) => NewName::Name(keep_str(n)),
        (None, Some(f)) => NewName::Flatten(flatten(f)?),
        _ => return Err("a state rule needs one new name".to_owned()),
    };
    Ok(Remap {
        old: tags(model.old_state.as_ref())?,
        new_name,
        new: tags(model.new_state.as_ref())?,
        copied: keep(model.copied_state.iter().map(|s| keep_str(s)).collect()),
    })
}

/// Reads and checks one block schema.
fn schema(model: &SchemaModel) -> Result<Schema<'static>, String> {
    let version = i32::from_be_bytes([
        model.max_version_major,
        model.max_version_minor,
        model.max_version_patch,
        model.max_version_revision,
    ]);
    if version < 0 {
        return Err(format!("version {version} is negative"));
    }
    let mut index = BTreeMap::new();
    for (key, pairs) in &model.remapped_property_values_index {
        let mut list = Vec::with_capacity(pairs.len());
        for pair in pairs {
            list.push((tag(&pair.old)?, tag(&pair.new)?));
        }
        index.insert(key.as_str(), keep(list));
    }
    let names: BTreeSet<&String> = [
        model.renamed_ids.keys().collect::<Vec<_>>(),
        model.added_properties.keys().collect(),
        model.removed_properties.keys().collect(),
        model.renamed_properties.keys().collect(),
        model.remapped_property_values.keys().collect(),
        model.flattened_properties.keys().collect(),
        model.remapped_states.keys().collect(),
    ]
    .concat()
    .into_iter()
    .collect();
    let mut blocks = Vec::with_capacity(names.len());
    for name in names {
        if model.renamed_ids.contains_key(name) && model.flattened_properties.contains_key(name) {
            return Err(format!("{name} is both renamed and flattened"));
        }
        let renames = model.renamed_properties.get(name);
        if let Some(renames) = renames
            && let Some(p) = renames.values().find(|new| renames.contains_key(*new))
        {
            return Err(format!(
                "{name} renames a property to {p}, which another rename reads"
            ));
        }
        let mut values = Vec::new();
        for (property, key) in model
            .remapped_property_values
            .get(name)
            .into_iter()
            .flatten()
        {
            let pairs = index
                .get(key.as_str())
                .ok_or_else(|| format!("{name} {property}: no value index {key}"))?;
            values.push((keep_str(property), *pairs));
        }
        let remaps = model
            .remapped_states
            .get(name)
            .into_iter()
            .flatten()
            .map(remap)
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| format!("{name}: {e}"))?;
        let rules = Rules {
            renamed: model.renamed_ids.get(name).map(|n| keep_str(n)),
            flattened: model
                .flattened_properties
                .get(name)
                .map(flatten)
                .transpose()?,
            added: tags(model.added_properties.get(name))?,
            removed: keep(
                model
                    .removed_properties
                    .get(name)
                    .into_iter()
                    .flatten()
                    .map(|s| keep_str(s))
                    .collect(),
            ),
            renamed_properties: keep(
                renames
                    .into_iter()
                    .flatten()
                    .map(|(a, b)| (keep_str(a), keep_str(b)))
                    .collect(),
            ),
            remapped_values: keep(values),
            remapped_states: keep(remaps),
        };
        blocks.push((keep_str(name), rules));
    }
    Ok(Schema {
        version,
        blocks: keep(blocks),
    })
}

/// Reads the block schemas, sorted by (version, ID) as the dump source sorts them.
fn read_schemas(root: &Path) -> Result<Vec<Schema<'static>>, String> {
    let mut schemas = Vec::new();
    let mut previous = None;
    for (id, name, bytes) in schema_files(&root.join(DIR).join(BLOCK_SCHEMAS))? {
        if previous.is_some_and(|p| p >= id) {
            return Err(format!("{BLOCK_SCHEMAS}/{name}: schema ID {id} twice"));
        }
        previous = Some(id);
        let model: SchemaModel =
            serde_json::from_slice(&bytes).map_err(|e| format!("{BLOCK_SCHEMAS}/{name}: {e}"))?;
        let schema = schema(&model).map_err(|e| format!("{BLOCK_SCHEMAS}/{name}: {e}"))?;
        schemas.push(((schema.version, id), schema));
    }
    schemas.sort_by_key(|(key, _)| *key);
    Ok(schemas.into_iter().map(|(_, s)| s).collect())
}

/// The schema files of a folder (the dump source's `^(\d{4}).*\.json$`) with their number and bytes, by
/// name.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn schema_files(dir: &Path) -> Result<Vec<(u16, String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let id = name
            .get(..4)
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse().ok());
        if let Some(id) = id
            && Path::new(&name).extension() == Some("json".as_ref())
        {
            let bytes = read(&entry.path())?;
            files.push((id, name, bytes));
        }
    }
    files.sort();
    Ok(files)
}

/// A typed state object (`{"name", "states": {p: {"byte"|"int"|"string": v}}, "version"}`).
fn typed_state(json: &Json) -> Result<(i32, String, States), String> {
    let name = json
        .get("name")
        .and_then(Json::as_str)
        .ok_or("a state without a name")?;
    let version = json
        .get("version")
        .and_then(Json::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0);
    let mut states = States::new();
    for (k, v) in json
        .get("states")
        .and_then(Json::as_object)
        .ok_or_else(|| format!("{name}: no states"))?
    {
        let value = match (
            v.get("byte").and_then(Json::as_u64),
            v.get("int").and_then(Json::as_i64),
            v.get("string").and_then(Json::as_str),
        ) {
            (Some(b), None, None) => {
                Value::Byte(u8::try_from(b).map_err(|_| format!("{name} {k}: byte {b}"))?)
            }
            (None, Some(i), None) => {
                Value::Int(i32::try_from(i).map_err(|_| format!("{name} {k}: int {i}"))?)
            }
            (None, None, Some(s)) => Value::String(s.to_owned()),
            _ => return Err(format!("{name} {k}: not a byte, int or string")),
        };
        states.insert(k.clone(), value);
    }
    Ok((version, name.to_owned(), states))
}

/// The reference list's block state version, which every one of its states has (the protocol
/// version's, which Anchor writes), and its states.
fn reference(root: &Path) -> Result<(i32, BTreeSet<(String, States)>), String> {
    let path = root.join(REFERENCE_DIR).join(REFERENCE_BLOCKS);
    let list: Vec<Json> =
        serde_json::from_slice(&read(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut version = None;
    let mut out = BTreeSet::new();
    for json in &list {
        let (v, name, states) = typed_state(json)?;
        if *version.get_or_insert(v) != v {
            return Err(format!("reference {name} has version {v}"));
        }
        out.insert((name, states));
    }
    Ok((version.ok_or("the reference list is empty")?, out))
}

/// Fails unless the newest schema has the reference `version` (so imported states and the states
/// Anchor writes have one version) and every reference state upgrades to itself.
fn check_reference(
    schemas: &[Schema<'_>],
    version: i32,
    reference: &BTreeSet<(String, States)>,
) -> Result<(), String> {
    let newest = schemas.last().map(|s| s.version);
    if newest != Some(version) {
        return Err(format!(
            "the newest block schema has version {newest:?}, the reference list {version}"
        ));
    }
    let mut changed = Vec::new();
    for (name, states) in reference {
        let (mut n, mut s) = (name.clone(), states.clone());
        apply::upgrade(schemas, version, &mut n, &mut s);
        if (&n, &s) != (name, states) {
            changed.push(format!("{name} {states:?} -> {n} {s:?}"));
        }
    }
    if changed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} reference states do not upgrade to themselves:\n{}",
            changed.len(),
            changed.join("\n")
        ))
    }
}

/// A legacy ID map file (string ID → number), checked one to one.
fn legacy_ids(path: &Path) -> Result<BTreeMap<i16, String>, String> {
    let map: BTreeMap<String, i64> =
        serde_json::from_slice(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for (name, id) in map {
        let id = i16::try_from(id).map_err(|_| format!("{}: ID {id}", path.display()))?;
        if out.insert(id, name).is_some() {
            return Err(format!("{}: two names for ID {id}", path.display()));
        }
    }
    Ok(out)
}

/// Upgrades every legacy entry to the newest version, as the dump source does when it loads the
/// table; returns the entries whose result is not a reference state too.
fn legacy(
    root: &Path,
    schemas: &[Schema<'_>],
    reference: &BTreeSet<(String, States)>,
) -> Result<(Legacy, Vec<String>), String> {
    let path = root.join(DIR).join(LEGACY_TABLE);
    let text = String::from_utf8(read(&path)?).map_err(|e| format!("{LEGACY_TABLE}: {e}"))?;
    let mut entries = BTreeMap::new();
    let mut states: BTreeMap<(String, States), usize> = BTreeMap::new();
    let mut outside = Vec::new();
    for line in text.lines() {
        let entry: LegacyEntry =
            serde_json::from_str(line).map_err(|e| format!("{LEGACY_TABLE}: {e}"))?;
        let (version, mut name, mut s) = typed_state(&entry.state)?;
        apply::upgrade(schemas, version, &mut name, &mut s);
        let key = (name, s);
        if !reference.contains(&key) {
            outside.push(format!(
                "{}:{} -> {} {:?}",
                entry.id, entry.meta, key.0, key.1
            ));
        }
        let next = states.len();
        let index = *states.entry(key).or_insert(next);
        if entries
            .insert((entry.id.clone(), entry.meta), index)
            .is_some()
        {
            return Err(format!("{LEGACY_TABLE}: {}:{} twice", entry.id, entry.meta));
        }
    }
    let mut list = vec![(String::new(), States::new()); states.len()];
    for (state, i) in states {
        if let Some(slot) = list.get_mut(i) {
            *slot = state;
        }
    }
    Ok(((entries, list), outside))
}

/// Reads the item schemas in ID order, checked as the dump source reads them: it looks IDs up lowercased,
/// so an ID with a capital letter could never match.
fn item_schemas(root: &Path) -> Result<Vec<ItemSchema>, String> {
    let mut out = Vec::new();
    let mut previous = None;
    for (id, name, bytes) in schema_files(&root.join(DIR).join(ITEM_SCHEMAS))? {
        if previous.is_some_and(|p| p >= id) {
            return Err(format!("{ITEM_SCHEMAS}/{name}: schema ID {id} twice"));
        }
        previous = Some(id);
        let model: ItemSchemaModel =
            serde_json::from_slice(&bytes).map_err(|e| format!("{ITEM_SCHEMAS}/{name}: {e}"))?;
        let ids = model.renamed_ids.keys().chain(model.remapped_metas.keys());
        if let Some(id) = ids.into_iter().find(|id| id.to_ascii_lowercase() != **id) {
            return Err(format!("{ITEM_SCHEMAS}/{name}: {id} is not lowercase"));
        }
        let mut remapped = BTreeMap::new();
        for (id, metas) in model.remapped_metas {
            let mut list = BTreeMap::new();
            for (meta, new) in metas {
                let meta: u16 = meta
                    .parse()
                    .map_err(|_| format!("{ITEM_SCHEMAS}/{name}: {id} meta {meta}"))?;
                list.insert(meta, new);
            }
            remapped.insert(id, list);
        }
        out.push(ItemSchema {
            renamed: model.renamed_ids,
            remapped,
        });
    }
    Ok(out)
}

/// The 1.12 item ID → block ID map with lowercased keys, as the dump source builds it.
fn item_blocks(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join(DIR).join(ITEM_SCHEMAS).join(ITEM_BLOCKS);
    let map: BTreeMap<String, String> =
        serde_json::from_slice(&read(&path)?).map_err(|e| format!("{ITEM_BLOCKS}: {e}"))?;
    let mut out = BTreeMap::new();
    for (item, block) in map {
        if out.insert(item.to_ascii_lowercase(), block).is_some() {
            return Err(format!("{ITEM_BLOCKS}: {item} twice ignoring case"));
        }
    }
    Ok(out)
}

/// Generates the file text (unformatted).
pub(crate) fn generate(root: &Path) -> Result<String, String> {
    let schemas = read_schemas(root)?;
    let (version, reference) = reference(root)?;
    check_reference(&schemas, version, &reference)?;
    let ((entries, states), outside) = legacy(root, &schemas, &reference)?;
    let block_ids = legacy_ids(&root.join(DIR).join(BLOCK_LEGACY_IDS))?;
    let item_ids = legacy_ids(&root.join(DIR).join(ITEM_SCHEMAS).join(ITEM_LEGACY_IDS))?;
    let items = item_schemas(root)?;
    let item_blocks = item_blocks(root)?;

    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Upgrade tables generated from the block and item upgrade schemas in \
         `xtask/data/upgrade/`: {} block schemas, {} legacy entries upgraded to {} states, {} item \
         schemas.\n",
        schemas.len(),
        entries.len(),
        states.len(),
        items.len()
    );
    if !outside.is_empty() {
        put!(
            out,
            "//!\n//! {} legacy entries upgrade to states outside the reference list (the dump source gives \
             the same; such a state is kept as an unknown block):\n",
            outside.len()
        );
        for line in &outside {
            put!(out, "//! - `{line}`\n");
        }
    }
    put!(
        out,
        "\nuse super::ItemSchema;\nuse super::apply::{{Flatten, Kind, NewName, Remap, Rules, \
         Schema, Tag}};\n\n/// The `version` of every block state Anchor writes and every \
         upgraded one: the reference\n/// list's, which the newest block schema has.\n\
         pub(crate) const BLOCK_STATE_VERSION: i32 = {};\n\n",
        int_lit(&version.to_string())
    );
    out.push_str(
        "/// A block with no rules of a kind: the fields a rule leaves out.\nconst NONE: Rules<'static> = \
         Rules { renamed: None, flattened: None, added: &[], removed: &[], renamed_properties: &[], \
         remapped_values: &[], remapped_states: &[] };\n\n",
    );
    put!(
        out,
        "/// The block schemas, sorted by (version, ID).\npub(super) static SCHEMAS: [Schema<'static>; {}] = [",
        schemas.len()
    );
    for s in &schemas {
        put!(
            out,
            "Schema {{ version: {}, blocks: &[",
            int_lit(&s.version.to_string())
        );
        for (name, rules) in s.blocks {
            put!(out, "({}, {}),", str_lit(name), rules_lit(rules));
        }
        out.push_str("] },");
    }
    out.push_str("];\n\n");

    put!(
        out,
        "/// The states legacy entries upgrade to: name and properties.\npub(super) static \
         LEGACY_STATES: [(&str, &[(&str, Tag<'static>)]); {}] = [",
        states.len()
    );
    for (name, s) in &states {
        put!(out, "({}, &[", str_lit(name));
        for (k, v) in s {
            put!(out, "({}, {}),", str_lit(k), value_lit(v));
        }
        out.push_str("]),");
    }
    out.push_str("];\n\n");
    let mut by_id: BTreeMap<&str, Vec<(u8, usize)>> = BTreeMap::new();
    for ((id, meta), index) in &entries {
        by_id.entry(id).or_default().push((*meta, *index));
    }
    put!(
        out,
        "/// The legacy 1.12.0 table: string ID with each known meta's index in [`LEGACY_STATES`], \
         sorted.\npub(super) static LEGACY: [(&str, &[(u8, u16)]); {}] = [",
        by_id.len()
    );
    for (id, metas) in &by_id {
        put!(out, "({}, &[", str_lit(id));
        for (meta, index) in metas {
            put!(out, "({meta}, {index}),");
        }
        out.push_str("]),");
    }
    out.push_str("];\n\n");
    id_table(
        &mut out,
        "BLOCK_IDS",
        "Legacy numeric block IDs and their string IDs",
        &block_ids,
    );
    item_tables(&mut out, &item_ids, &item_blocks, &items);
    Ok(out)
}

/// Emits the item tables.
fn item_tables(
    out: &mut String,
    item_ids: &BTreeMap<i16, String>,
    item_blocks: &BTreeMap<String, String>,
    items: &[ItemSchema],
) {
    id_table(
        out,
        "ITEM_IDS",
        "Legacy numeric item IDs and their string IDs",
        item_ids,
    );
    put!(
        out,
        "/// 1.12 item IDs, lowercase, and their block IDs, sorted.\npub(super) static ITEM_BLOCKS: [(&str, &str); {}] = [",
        item_blocks.len()
    );
    for (item, block) in item_blocks {
        put!(out, "({}, {}),", str_lit(item), str_lit(block));
    }
    out.push_str("];\n\n");
    put!(
        out,
        "/// The item schemas in ID order.\npub(super) static \
         ITEM_SCHEMAS: [ItemSchema; {}] = [",
        items.len()
    );
    for s in items {
        out.push_str("ItemSchema { renamed: &[");
        for (a, b) in &s.renamed {
            put!(out, "({}, {}),", str_lit(a), str_lit(b));
        }
        out.push_str("], remapped: &[");
        for (id, metas) in &s.remapped {
            put!(out, "({}, &[", str_lit(id));
            for (meta, new) in metas {
                put!(out, "({meta}, {}),", str_lit(new));
            }
            out.push_str("]),");
        }
        out.push_str("] },");
    }
    out.push_str("];\n");
}

fn id_table(out: &mut String, name: &str, doc: &str, ids: &BTreeMap<i16, String>) {
    put!(
        out,
        "/// {doc}, sorted.\npub(super) static {name}: [(i16, &str); {}] = [",
        ids.len()
    );
    for (id, s) in ids {
        put!(out, "({id}, {}),", str_lit(s));
    }
    out.push_str("];\n\n");
}

fn tag_lit(tag: Tag<'_>) -> String {
    match tag {
        Tag::Byte(b) => format!("Tag::Byte({b})"),
        Tag::Int(i) => format!("Tag::Int({})", int_lit(&i.to_string())),
        Tag::String(s) => format!("Tag::String({})", str_lit(s)),
    }
}

fn value_lit(value: &Value) -> String {
    match value {
        Value::Byte(b) => format!("Tag::Byte({b})"),
        Value::Int(i) => format!("Tag::Int({})", int_lit(&i.to_string())),
        Value::String(s) => format!("Tag::String({})", str_lit(s)),
    }
}

fn tags_lit(tags: &[(&str, Tag<'_>)]) -> String {
    let mut out = "&[".to_owned();
    for (k, v) in tags {
        put!(out, "({}, {}),", str_lit(k), tag_lit(*v));
    }
    out.push(']');
    out
}

fn strs_lit(list: &[&str]) -> String {
    let mut out = "&[".to_owned();
    for s in list {
        put!(out, "{},", str_lit(s));
    }
    out.push(']');
    out
}

fn flatten_lit(f: &Flatten<'_>) -> String {
    let mut remaps = "&[".to_owned();
    for (k, v) in f.remaps {
        put!(remaps, "({}, {}),", str_lit(k), str_lit(v));
    }
    remaps.push(']');
    format!(
        "Flatten {{ prefix: {}, property: {}, suffix: {}, kind: Kind::{:?}, remaps: {remaps} }}",
        str_lit(f.prefix),
        str_lit(f.property),
        str_lit(f.suffix),
        f.kind
    )
}

fn rules_lit(rules: &Rules<'_>) -> String {
    let mut out = "Rules {".to_owned();
    if let Some(n) = rules.renamed {
        put!(out, "renamed: Some({}),", str_lit(n));
    }
    if let Some(f) = &rules.flattened {
        put!(out, "flattened: Some({}),", flatten_lit(f));
    }
    if !rules.added.is_empty() {
        put!(out, "added: {},", tags_lit(rules.added));
    }
    if !rules.removed.is_empty() {
        put!(out, "removed: {},", strs_lit(rules.removed));
    }
    if !rules.renamed_properties.is_empty() {
        out.push_str("renamed_properties: &[");
        for (a, b) in rules.renamed_properties {
            put!(out, "({}, {}),", str_lit(a), str_lit(b));
        }
        out.push_str("],");
    }
    if !rules.remapped_values.is_empty() {
        out.push_str("remapped_values: &[");
        for (p, pairs) in rules.remapped_values {
            put!(out, "({}, &[", str_lit(p));
            for (a, b) in *pairs {
                put!(out, "({}, {}),", tag_lit(*a), tag_lit(*b));
            }
            out.push_str("]),");
        }
        out.push_str("],");
    }
    if !rules.remapped_states.is_empty() {
        out.push_str("remapped_states: &[");
        for r in rules.remapped_states {
            let name = match &r.new_name {
                NewName::Name(n) => format!("NewName::Name({})", str_lit(n)),
                NewName::Flatten(f) => format!("NewName::Flatten({})", flatten_lit(f)),
            };
            put!(
                out,
                "Remap {{ old: {}, new_name: {name}, new: {}, copied: {} }},",
                tags_lit(r.old),
                tags_lit(r.new),
                strs_lit(r.copied)
            );
        }
        out.push_str("],");
    }
    out.push_str("..NONE }");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version of a schema [`model`] makes.
    const VERSION: i32 = 18_168_865;

    fn model(json: &str) -> SchemaModel {
        let text = format!(
            r#"{{"maxVersionMajor":1,"maxVersionMinor":21,"maxVersionPatch":60,"maxVersionRevision":33,{json}}}"#
        );
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn a_schema_reads_with_its_version_and_sorted_blocks() {
        let s = schema(&model(
            r#""renamedIds":{"b":"c","a":"d"},"addedProperties":{"a":{"p":{"byte":1}}}"#,
        ))
        .unwrap();
        assert_eq!(s.version, VERSION);
        let names: Vec<&str> = s.blocks.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(s.blocks[0].1.added, [("p", Tag::Byte(1))]);
    }

    #[test]
    fn schema_defects_fail_the_generator() {
        for json in [
            // The dump source throws on an ID both renamed and flattened.
            r#""renamedIds":{"a":"b"},"flattenedProperties":{"a":{"prefix":"","flattenedProperty":"p","suffix":""}}"#,
            // The result of these renames would depend on their order.
            r#""renamedProperties":{"a":{"p":"q","q":"r"}}"#,
            // The dump source reads bytes signed.
            r#""addedProperties":{"a":{"p":{"byte":200}}}"#,
            r#""addedProperties":{"a":{"p":{"byte":1,"int":1}}}"#,
            r#""remappedPropertyValues":{"a":{"p":"missing"}}"#,
            r#""flattenedProperties":{"a":{"prefix":"","flattenedProperty":"p","suffix":"","flattenedPropertyType":"long"}}"#,
            r#""remappedStates":{"a":[{"oldState":null,"newState":null,"copiedState":[]}]}"#,
        ] {
            assert!(schema(&model(json)).is_err(), "{json}");
        }
    }

    #[test]
    fn a_reference_state_that_does_not_upgrade_to_itself_fails_the_generator() {
        let reference = BTreeSet::from([("a".to_owned(), States::new())]);
        let rename = schema(&model(r#""renamedIds":{"a":"b"}"#)).unwrap();
        let other = schema(&model(r#""renamedIds":{"x":"y"}"#)).unwrap();
        // The first schema of the reference version introduced it, so it does not apply; the
        // second does.
        assert!(check_reference(&[other, rename], VERSION, &reference).is_err());
        assert!(check_reference(&[rename, other], VERSION, &reference).is_ok());
    }

    #[test]
    fn a_newest_schema_of_another_version_than_the_reference_fails_the_generator() {
        let reference = BTreeSet::from([("a".to_owned(), States::new())]);
        let mut newer = schema(&model(r#""renamedIds":{"x":"y"}"#)).unwrap();
        newer.version += 1;
        assert!(check_reference(&[newer], VERSION, &reference).is_err());
    }
}
