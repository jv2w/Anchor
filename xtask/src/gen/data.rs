//! Block and item data (PLAN.md 10.4): reads the committed PMMP dump (`xtask/data/pmmp/`), the
//! reference lists (`xtask/data/reference/`) and the block upgrade schema
//! (`xtask/data/block_upgrade.json`), checks them against each other, and builds the block state
//! and item type lists the data generators emit. The index of a state or item type in these lists
//! is its number in the generated tables, so every generator agrees on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::str_lit;

type R<T> = Result<T, String>;

/// Committed data files, relative to the repository root.
pub(crate) const PMMP_DIR: &str = "xtask/data/pmmp";
pub(crate) const REFERENCE_DIR: &str = "xtask/data/reference";
const UPGRADE_PATH: &str = "xtask/data/block_upgrade.json";

/// A block state property value. The derived order (byte, int, string, then by value) is part of
/// the state order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Prop {
    Byte(u8),
    Int(i32),
    String(String),
}

/// A block name and its properties, sorted by property name: the identity of a Bedrock block
/// state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StateKey {
    pub(crate) name: String,
    pub(crate) properties: Vec<(String, Prop)>,
}

/// An Anchor block state: a 1.26.50 Bedrock state with PMMP's values for it.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) key: StateKey,
    /// The network ID the reference list gives (`CloudburstMC` `network_id`).
    pub(crate) network_id: u32,
    pub(crate) light_emission: u8,
    pub(crate) light_filter: u8,
    pub(crate) blocks_direct_sky_light: bool,
    pub(crate) ticks_randomly: bool,
    /// The horizontal faces with full support (PMMP `getSupportType`), as [`FACES`] bits; a
    /// stair's are those of its `minecraft:corner` shape.
    pub(crate) full_faces: u8,
    /// How PMMP works the state out from its neighbours, if it does.
    pub(crate) connect: Option<Connect>,
}

/// The horizontal faces in the order of their bits in [`State::full_faces`] (bit 0 = north).
pub(crate) const FACES: [&str; 4] = ["north", "east", "south", "west"];

/// The dump's `connect` entry: what PMMP needs to work out a state from its neighbours.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connect {
    /// `fence`, `thin`, `wall`, `fence_gate`, `stair`, `tripwire` or `tripwire_hook`.
    pub(crate) kind: String,
    /// A fence's PMMP class: fences connect to fences of their own class.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// The horizontal facing of fence gates, stairs and tripwire hooks.
    #[serde(default)]
    pub(crate) facing: Option<String>,
    #[serde(default)]
    pub(crate) upside_down: Option<bool>,
    /// A stair's full faces by `minecraft:corner` value.
    #[serde(default)]
    shapes: Option<BTreeMap<String, Vec<String>>>,
}

/// An NBT value read from the typed JSON of the dumps (`{"byte": 1}` and so on). It holds the
/// types the data uses, which `crate::nbt::Static` mirrors; another type fails generation, and
/// supporting it means adding it to both.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tag {
    Byte(u8),
    Short(i16),
    Int(i32),
    /// The bit pattern.
    Float(u32),
    ByteArray(Vec<u8>),
    String(String),
    List(List),
    Compound(BTreeMap<String, Tag>),
}

/// A list of one of the element types the data uses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum List {
    End,
    Int(Vec<i32>),
    /// Bit patterns.
    Float(Vec<u32>),
    String(Vec<String>),
    Compound(Vec<BTreeMap<String, Tag>>),
}

/// A PMMP item type in its Bedrock form.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) name: String,
    pub(crate) meta: u16,
    /// The block state index of a block item: the PMMP state it places.
    pub(crate) block: Option<u16>,
    /// The network ID of the block state a block item is sent with (0 for other items).
    pub(crate) block_network_id: u32,
    /// NBT that is part of the type (PMMP `serializeType` extra data), such as an ominous
    /// banner's `Type`.
    pub(crate) nbt: Option<BTreeMap<String, Tag>>,
    /// The 1.26.50 runtime ID of the name.
    pub(crate) runtime_id: i16,
}

/// An item stack of the dumps (creative entries, recipe results): the item type index, the count
/// and the stack NBT as PMMP serializes it (type NBT merged in).
#[derive(Debug)]
pub(crate) struct Stack {
    pub(crate) item: u16,
    /// The stack's meta, which PMMP sends; it differs from the type's meta where PMMP reads
    /// meta as stack state.
    pub(crate) meta: u16,
    pub(crate) count: u16,
    pub(crate) nbt: Option<BTreeMap<String, Tag>>,
}

/// The loaded and checked data.
#[derive(Debug)]
pub(crate) struct Data {
    pub(crate) states: Vec<State>,
    pub(crate) items: Vec<Item>,
    /// Anchor state index of a PMMP (1.26.30) Bedrock state, upgraded with the default values.
    pmmp_state_index: BTreeMap<StateKey, u16>,
    /// PMMP's legacy recipe meta of a PMMP (1.26.30) Bedrock state.
    recipe_meta: BTreeMap<StateKey, i32>,
    /// Item type indices by (name, block index).
    item_index: BTreeMap<(String, Option<u16>), Vec<u16>>,
    /// 1.26.50 runtime IDs by item name.
    runtime_ids: BTreeMap<String, i16>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PmmpBlock {
    #[serde(rename = "type")]
    _type: String,
    bedrock: Json,
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    ticks_randomly: bool,
    recipe_meta: i32,
    full_faces: Vec<String>,
    #[serde(default)]
    connect: Option<Connect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PmmpItem {
    bedrock: Json,
    #[serde(rename = "names")]
    _names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceBlock {
    name: String,
    states: Json,
    #[serde(rename = "version")]
    _version: i32,
    network_id: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceItem {
    name: String,
    runtime_id: i32,
    #[serde(rename = "component_based")]
    _component_based: bool,
    #[serde(rename = "version")]
    _version: i32,
    #[serde(rename = "data")]
    _data: Json,
}

/// An entry of the reference `item_blocks.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemBlock {
    item: String,
    name: String,
    states: Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Upgrade {
    #[serde(rename = "maxVersionMajor")]
    _major: u32,
    #[serde(rename = "maxVersionMinor")]
    _minor: u32,
    #[serde(rename = "maxVersionPatch")]
    _patch: u32,
    #[serde(rename = "maxVersionRevision")]
    _revision: u32,
    added_properties: BTreeMap<String, BTreeMap<String, Json>>,
}

/// PMMP's per-state values, which overlapping PMMP states must agree on.
#[derive(Debug, Clone, PartialEq)]
struct Values {
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    ticks_randomly: bool,
    full_faces: Vec<String>,
    connect: Option<Connect>,
}

/// The reference block list: network ID by state, and the states of each name.
type Reference = (BTreeMap<StateKey, u32>, BTreeMap<String, Vec<StateKey>>);

/// The upgraded states with their values, and the upgraded default form of each PMMP state.
type Upgraded = (BTreeMap<StateKey, Values>, BTreeMap<StateKey, StateKey>);

impl Data {
    /// Loads and checks every file.
    pub(crate) fn load(root: &Path) -> R<Self> {
        let (network, reference_names) = read_reference(root)?;
        let added = read_upgrade(root)?;
        let pmmp_states = read_pmmp_states(root)?;
        let (states, pmmp_default) = upgrade(&pmmp_states, &added, &network, &reference_names)?;

        let mut list = Vec::with_capacity(states.len());
        let mut state_index = BTreeMap::new();
        for (i, (key, values)) in states.into_iter().enumerate() {
            let index = u16::try_from(i).map_err(|_| "more than 65,536 block states")?;
            let network_id = *network.get(&key).ok_or("state lost its network ID")?;
            let full_faces = full_faces(&key, &values)?;
            state_index.insert(key.clone(), index);
            list.push(State {
                key,
                network_id,
                light_emission: values.light_emission,
                light_filter: values.light_filter,
                blocks_direct_sky_light: values.blocks_direct_sky_light,
                ticks_randomly: values.ticks_randomly,
                full_faces,
                connect: values.connect,
            });
        }
        let mut pmmp_state_index = BTreeMap::new();
        for (pmmp, default) in pmmp_default {
            let index = *state_index
                .get(&default)
                .ok_or("default state not listed")?;
            pmmp_state_index.insert(pmmp, index);
        }
        let recipe_meta = pmmp_states
            .into_iter()
            .map(|(key, (_, meta))| (key, meta))
            .collect();

        let mut data = Self {
            states: list,
            items: Vec::new(),

            pmmp_state_index,
            recipe_meta,
            item_index: BTreeMap::new(),
            runtime_ids: BTreeMap::new(),
        };
        data.load_items(root, &network)?;
        Ok(data)
    }

    /// Loads the PMMP item types and gives each its 1.26.50 runtime ID.
    fn load_items(&mut self, root: &Path, network: &BTreeMap<StateKey, u32>) -> R<()> {
        let reference: Vec<ReferenceItem> = read_json(root, REFERENCE_DIR, "items.json")?;
        self.runtime_ids = reference
            .into_iter()
            .map(|i| {
                i16::try_from(i.runtime_id)
                    .map(|id| (i.name.clone(), id))
                    .map_err(|_| format!("runtime ID of {} out of range", i.name))
            })
            .collect::<R<_>>()?;
        let pmmp: Vec<PmmpItem> = read_json(root, PMMP_DIR, "items.json")?;
        for (i, item) in pmmp.iter().enumerate() {
            let index = u16::try_from(i).map_err(|_| "more than 65,536 item types")?;
            let form = self.item_form(&item.bedrock)?;
            let runtime_id = self.runtime_id(&form.0)?;
            self.item_index
                .entry((form.0.clone(), form.2))
                .or_default()
                .push(index);
            self.items.push(Item {
                name: form.0,
                meta: form.1,
                block: form.2,
                block_network_id: 0,
                nbt: form.3,
                runtime_id,
            });
        }
        self.use_vanilla_item_blocks(root, network)
    }

    /// Sets the block network ID block items are sent with: the vanilla creative state of
    /// `item_blocks.json`, which BDS sends, where the item name has a single PMMP block item type
    /// and a single vanilla state (PLAN.md 10.4); PMMP's own default state otherwise. The vanilla
    /// state may be one PMMP does not have (glazed terracotta facing down); it is only the network
    /// form, and the item still places PMMP's state.
    fn use_vanilla_item_blocks(&mut self, root: &Path, network: &BTreeMap<StateKey, u32>) -> R<()> {
        let vanilla: Vec<ItemBlock> = read_json(root, REFERENCE_DIR, "item_blocks.json")?;
        let mut block_items: BTreeMap<&str, usize> = BTreeMap::new();
        for item in self.items.iter().filter(|i| i.block.is_some()) {
            *block_items.entry(item.name.as_str()).or_default() += 1;
        }
        let mut ids = BTreeMap::new();
        for v in &vanilla {
            if block_items.get(v.item.as_str()) == Some(&1) {
                let key = state_key(&v.name, &v.states)?;
                let id = *network
                    .get(&key)
                    .ok_or_else(|| format!("vanilla state of item {} is not listed", v.item))?;
                ids.insert(v.item.clone(), id);
            }
        }
        for item in &mut self.items {
            let Some(block) = item.block else { continue };
            item.block_network_id = match ids.get(&item.name) {
                Some(&id) => id,
                None => {
                    self.states
                        .get(usize::from(block))
                        .ok_or("item block state out of range")?
                        .network_id
                }
            };
        }
        Ok(())
    }

    /// Reads an item in Bedrock form (`name`, `meta`, optional `block` and `nbt`, and `count`
    /// when `with_count`).
    #[expect(
        clippy::type_complexity,
        reason = "consistency: a private tuple of the four parts"
    )]
    fn item_form(
        &self,
        json: &Json,
    ) -> R<(String, u16, Option<u16>, Option<BTreeMap<String, Tag>>)> {
        let obj = json.as_object().ok_or("item is not an object")?;
        let name = obj
            .get("name")
            .and_then(Json::as_str)
            .ok_or("item without a name")?
            .to_owned();
        let meta = obj
            .get("meta")
            .and_then(Json::as_u64)
            .and_then(|m| u16::try_from(m).ok())
            .ok_or_else(|| format!("item {name} has no meta in range"))?;
        let block = match obj.get("block") {
            Some(b) => Some(self.pmmp_state(&bedrock_state(b)?)?),
            None => None,
        };
        let nbt = match obj.get("nbt") {
            Some(n) => Some(compound(n)?),
            None => None,
        };
        for k in obj.keys() {
            if !["name", "meta", "block", "nbt", "count"].contains(&k.as_str()) {
                return Err(format!("item {name} has unknown key {k}"));
            }
        }
        let meta = corrected_meta(&name, meta, nbt.as_ref());
        Ok((name, meta, block, nbt))
    }

    /// The Anchor state of a PMMP (1.26.30) Bedrock state: its upgraded default form.
    pub(crate) fn pmmp_state(&self, key: &StateKey) -> R<u16> {
        self.pmmp_state_index
            .get(key)
            .copied()
            .ok_or_else(|| format!("{} {:?} is not a PMMP state", key.name, key.properties))
    }

    /// PMMP's legacy meta of a PMMP (1.26.30) Bedrock state, which it sends for an exact block
    /// recipe ingredient.
    pub(crate) fn recipe_meta(&self, key: &StateKey) -> R<i32> {
        self.recipe_meta
            .get(key)
            .copied()
            .ok_or_else(|| format!("{} {:?} is not a PMMP state", key.name, key.properties))
    }

    /// Reads an item stack of the dumps (`name`, `meta`, optional `block` and `nbt`, `count`)
    /// and finds its item type the way `item::Type::from_name` does at run time: among the types
    /// of the name and block, the one with the stack's meta; if several have it, the one with the
    /// most type NBT that is part of the stack NBT; if none has it and the name and block have a
    /// single type, that type, as PMMP reads such a meta as stack state (a firework star's
    /// colour). The stack keeps its own meta.
    pub(crate) fn stack(&self, json: &Json) -> R<Stack> {
        let (name, meta, block, nbt) = self.item_form(json)?;
        let count = json
            .get("count")
            .and_then(Json::as_u64)
            .and_then(|c| u16::try_from(c).ok())
            .ok_or_else(|| format!("stack of {name} has no count in range"))?;
        let candidates = self
            .item_index
            .get(&(name.clone(), block))
            .ok_or_else(|| format!("no item type {name} with block {block:?}"))?;
        let empty = BTreeMap::new();
        let have = nbt.as_ref().unwrap_or(&empty);
        let mut exact = Vec::new();
        for &index in candidates {
            let item = self
                .items
                .get(usize::from(index))
                .ok_or("item index out of range")?;
            if item.meta == meta {
                exact.push((index, item.nbt.as_ref().unwrap_or(&empty)));
            }
        }
        let item = match (exact.as_slice(), candidates.as_slice()) {
            ([(index, _)], _) | ([], [index]) => *index,
            ([], _) => return Err(format!("no item type {name} meta {meta}")),
            (many, _) => {
                many.iter()
                    .filter(|(_, need)| need.iter().all(|(k, v)| have.get(k) == Some(v)))
                    .max_by_key(|(_, need)| need.len())
                    .ok_or_else(|| format!("no item type of {name} matches the stack NBT"))?
                    .0
            }
        };
        Ok(Stack {
            item,
            meta,
            count,
            nbt,
        })
    }

    /// The 1.26.50 runtime ID of an item name of the reference list. PMMP names some items it has
    /// no type for, such as `minecraft:lodestone_compass` in a potion container recipe.
    pub(crate) fn runtime_id(&self, name: &str) -> R<i16> {
        self.runtime_ids
            .get(name)
            .copied()
            .ok_or_else(|| format!("item {name} is not in the reference list"))
    }
}

/// The meta of an item form, corrected where PMMP sends a different one than BDS: PMMP's
/// ominous banner (`Type` 1) has the black base colour, meta 0, where BDS sends it white, meta 15
/// (P6, PLAN.md 10.4). Every item read from the dumps passes here, so types and stacks agree.
fn corrected_meta(name: &str, meta: u16, nbt: Option<&BTreeMap<String, Tag>>) -> u16 {
    const OMINOUS: Tag = Tag::Int(1);
    let ominous = nbt.and_then(|n| n.get("Type")) == Some(&OMINOUS);
    if name == "minecraft:banner" && ominous && meta == 0 {
        15
    } else {
        meta
    }
}

/// Reads the reference block list.
fn read_reference(root: &Path) -> R<Reference> {
    let reference: Vec<ReferenceBlock> = read_json(root, REFERENCE_DIR, "blocks.json")?;
    let mut network = BTreeMap::new();
    let mut names: BTreeMap<String, Vec<StateKey>> = BTreeMap::new();
    for b in reference {
        let key = state_key(&b.name, &b.states)?;
        names.entry(b.name.clone()).or_default().push(key.clone());
        if network.insert(key, b.network_id).is_some() {
            return Err(format!("reference block {} twice", b.name));
        }
    }
    Ok((network, names))
}

/// Reads the added properties of the block upgrade schema.
fn read_upgrade(root: &Path) -> R<BTreeMap<String, Vec<(String, Prop)>>> {
    let upgrade: Upgrade = read_json(root, "xtask/data", "block_upgrade.json")?;
    let mut added = BTreeMap::new();
    for (name, props) in &upgrade.added_properties {
        let mut list = Vec::with_capacity(props.len());
        for (prop, value) in props {
            let value =
                prop_value(value).map_err(|e| format!("{UPGRADE_PATH}: {name} {prop}: {e}"))?;
            list.push((prop.clone(), value));
        }
        added.insert(name.clone(), list);
    }
    Ok(added)
}

/// Reads the PMMP states by their 1.26.30 Bedrock form; overlapping PMMP states must agree on
/// their values.
fn read_pmmp_states(root: &Path) -> R<BTreeMap<StateKey, (Values, i32)>> {
    let pmmp: Vec<PmmpBlock> = read_json(root, PMMP_DIR, "blocks.json")?;
    let mut states: BTreeMap<StateKey, (Values, i32)> = BTreeMap::new();
    for b in &pmmp {
        let key = bedrock_state(&b.bedrock)?;
        let values = Values {
            light_emission: b.light_emission,
            light_filter: b.light_filter,
            blocks_direct_sky_light: b.blocks_direct_sky_light,
            ticks_randomly: b.ticks_randomly,
            full_faces: b.full_faces.clone(),
            connect: b.connect.clone(),
        };
        let entry = (values, b.recipe_meta);
        if let Some(old) = states.insert(key.clone(), entry.clone())
            && old != entry
        {
            return Err(format!(
                "PMMP states of {} {:?} disagree on their values",
                key.name, key.properties
            ));
        }
    }
    Ok(states)
}

/// Upgrades the PMMP states to 1.26.50: adds the new properties' defaults, then takes every value
/// the reference list has for them. Every state must end up in the reference list and every
/// upgrade entry must be used.
fn upgrade(
    pmmp: &BTreeMap<StateKey, (Values, i32)>,
    added: &BTreeMap<String, Vec<(String, Prop)>>,
    network: &BTreeMap<StateKey, u32>,
    reference_names: &BTreeMap<String, Vec<StateKey>>,
) -> R<Upgraded> {
    let mut used = BTreeSet::new();
    let mut states = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    for (key, (values, _)) in pmmp {
        let Some(new) = added.get(&key.name) else {
            if !network.contains_key(key) {
                return Err(format!(
                    "{} {:?} is not in the reference list and has no upgrade",
                    key.name, key.properties
                ));
            }
            states.insert(key.clone(), values.clone());
            defaults.insert(key.clone(), key.clone());
            continue;
        };
        used.insert(key.name.clone());
        let mut upgraded = key.clone();
        for (prop, value) in new {
            if upgraded.properties.iter().any(|(p, _)| p == prop) {
                return Err(format!("{} already has {prop}", key.name));
            }
            upgraded.properties.push((prop.clone(), value.clone()));
        }
        upgraded.properties.sort();
        if !network.contains_key(&upgraded) {
            return Err(format!(
                "upgraded {} {:?} is not in the reference list",
                key.name, upgraded.properties
            ));
        }
        defaults.insert(key.clone(), upgraded);
        for candidate in reference_names
            .get(&key.name)
            .map_or(&[][..], Vec::as_slice)
        {
            if candidate.properties.len() != key.properties.len() + new.len() {
                return Err(format!(
                    "reference {} {:?} does not have the old and added properties",
                    key.name, candidate.properties
                ));
            }
            let old: Vec<_> = candidate
                .properties
                .iter()
                .filter(|(p, _)| !new.iter().any(|(n, _)| n == p))
                .cloned()
                .collect();
            if old == key.properties {
                states.insert(candidate.clone(), values.clone());
            }
        }
    }
    if let Some(name) = added.keys().find(|name| !used.contains(*name)) {
        return Err(format!("{UPGRADE_PATH}: {name} is not used"));
    }
    Ok((states, defaults))
}

/// The full faces of an upgraded state as [`FACES`] bits. A stair's come from the shape its
/// `minecraft:corner` names: PMMP works the shape out from the neighbours, so the dumped state
/// (always straight) does not have it.
fn full_faces(key: &StateKey, values: &Values) -> R<u8> {
    let faces = match &values.connect {
        Some(Connect {
            shapes: Some(shapes),
            ..
        }) => {
            let corner = key
                .properties
                .iter()
                .find_map(|(p, v)| match (p.as_str(), v) {
                    ("minecraft:corner", Prop::String(c)) => Some(c),
                    _ => None,
                })
                .ok_or_else(|| format!("stair {} has no minecraft:corner", key.name))?;
            shapes
                .get(corner)
                .ok_or_else(|| format!("stair {} has no shape {corner}", key.name))?
        }
        _ => &values.full_faces,
    };
    let mut bits = 0;
    for face in faces {
        let i = FACES
            .iter()
            .position(|f| f == face)
            .ok_or_else(|| format!("{}: unknown face {face}", key.name))?;
        bits |= 1 << i;
    }
    Ok(bits)
}

/// Reads a committed JSON file.
pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(root: &Path, dir: &str, file: &str) -> R<T> {
    let path = root.join(dir).join(file);
    let bytes = read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads a Bedrock block state object (`name`, `states`).
pub(crate) fn bedrock_state(json: &Json) -> R<StateKey> {
    let name = json
        .get("name")
        .and_then(Json::as_str)
        .ok_or("block state without a name")?;
    let states = json.get("states").ok_or("block state without states")?;
    state_key(name, states)
}

fn state_key(name: &str, states: &Json) -> R<StateKey> {
    let obj = states
        .as_object()
        .ok_or_else(|| format!("states of {name} are not an object"))?;
    let mut properties = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        properties.push((
            k.clone(),
            prop_value(v).map_err(|e| format!("{name} {k}: {e}"))?,
        ));
    }
    properties.sort();
    Ok(StateKey {
        name: name.to_owned(),
        properties,
    })
}

/// Reads a typed block property value: byte, int or string.
fn prop_value(json: &Json) -> R<Prop> {
    match tag(json)? {
        Tag::Byte(b) => Ok(Prop::Byte(b)),
        Tag::Int(i) => Ok(Prop::Int(i)),
        Tag::String(s) => Ok(Prop::String(s)),
        other => Err(format!(
            "property type {other:?} is not byte, int or string"
        )),
    }
}

/// Reads a typed NBT compound body (`{key: typed value}`).
pub(crate) fn compound(json: &Json) -> R<BTreeMap<String, Tag>> {
    let obj = json.as_object().ok_or("NBT compound is not an object")?;
    obj.iter()
        .map(|(k, v)| Ok((k.clone(), tag(v).map_err(|e| format!("{k}: {e}"))?)))
        .collect()
}

/// Reads one typed NBT value (`{"type": value}`).
pub(crate) fn tag(json: &Json) -> R<Tag> {
    let obj = json.as_object().ok_or("NBT value is not an object")?;
    let mut entries = obj.iter();
    let (Some((kind, value)), None) = (entries.next(), entries.next()) else {
        return Err("NBT value must have exactly one type key".to_owned());
    };
    tag_of(kind, value)
}

fn tag_of(kind: &str, value: &Json) -> R<Tag> {
    Ok(match kind {
        "byte" => Tag::Byte(byte(value)?),
        "short" => Tag::Short(
            value
                .as_i64()
                .and_then(|v| i16::try_from(v).ok())
                .ok_or_else(|| format!("short value {value} out of range"))?,
        ),
        "int" => Tag::Int(int(value)?),
        "float" => Tag::Float(float(value)?),
        "string" => Tag::String(string(value)?),
        "byte_array" => Tag::ByteArray(
            value
                .as_array()
                .ok_or("byte_array is not an array")?
                .iter()
                .map(byte)
                .collect::<R<_>>()?,
        ),
        "list" => Tag::List(list(value)?),
        "compound" => Tag::Compound(compound(value)?),
        other => return Err(format!("NBT type {other} is not supported (see Tag)")),
    })
}

/// A byte, signed or unsigned in the JSON, kept as its bit pattern.
fn byte(value: &Json) -> R<u8> {
    value
        .as_i64()
        .and_then(|v| {
            i8::try_from(v)
                .map(i8::cast_unsigned)
                .or_else(|_| u8::try_from(v))
                .ok()
        })
        .ok_or_else(|| format!("byte value {value} out of range"))
}

fn int(value: &Json) -> R<i32> {
    value
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| format!("int value {value} out of range"))
}

fn float(value: &Json) -> R<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("float bits {value} out of range"))
}

fn string(value: &Json) -> R<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("string value {value} is not a string"))
}

/// A typed list body: `{"type": element type, "values": [typed values]}`.
fn list(value: &Json) -> R<List> {
    let element = value
        .get("type")
        .and_then(Json::as_str)
        .ok_or("list without an element type")?;
    let values = value
        .get("values")
        .and_then(Json::as_array)
        .ok_or("list without values")?;
    // Each element is itself typed; it must have the list's type.
    let body = |v: &Json| -> R<Json> {
        v.get(element)
            .cloned()
            .ok_or_else(|| format!("list of {element} holds {v}"))
    };
    Ok(match element {
        "end" if values.is_empty() => List::End,
        "int" => List::Int(values.iter().map(|v| int(&body(v)?)).collect::<R<_>>()?),
        "float" => List::Float(values.iter().map(|v| float(&body(v)?)).collect::<R<_>>()?),
        "string" => List::String(values.iter().map(|v| string(&body(v)?)).collect::<R<_>>()?),
        "compound" => List::Compound(
            values
                .iter()
                .map(|v| compound(&body(v)?))
                .collect::<R<_>>()?,
        ),
        other => return Err(format!("list of {other} is not supported (see List)")),
    })
}

/// Rust source of a `crate::nbt::StaticCompound` (`&[("key", Static::...)]`), keys in byte
/// order like `nbt::Compound`.
pub(crate) fn static_compound(c: &BTreeMap<String, Tag>) -> String {
    let mut out = String::from("&[");
    for (k, v) in c {
        put!(out, "({}, {}),", str_lit(k), static_value(v));
    }
    out.push(']');
    out
}

/// Rust source of a `crate::nbt::Static`.
fn static_value(t: &Tag) -> String {
    match t {
        Tag::Byte(x) => format!("crate::nbt::Static::Byte({x})"),
        Tag::Short(x) => format!("crate::nbt::Static::Short({})", int_lit(i64::from(*x))),
        Tag::Int(x) => format!("crate::nbt::Static::Int({})", int_lit(i64::from(*x))),
        Tag::Float(x) => format!("crate::nbt::Static::Float({})", float_lit(*x)),
        Tag::ByteArray(x) => format!("crate::nbt::Static::ByteArray(&{x:?})"),
        Tag::String(x) => format!("crate::nbt::Static::String({})", str_lit(x)),
        Tag::List(l) => format!("crate::nbt::Static::List({})", static_list(l)),
        Tag::Compound(c) => format!("crate::nbt::Static::Compound({})", static_compound(c)),
    }
}

/// Rust source of a `crate::nbt::StaticList`.
fn static_list(l: &List) -> String {
    let join = |items: Vec<String>| items.join(",");
    match l {
        List::End => "crate::nbt::StaticList::End".to_owned(),
        List::Int(x) => format!(
            "crate::nbt::StaticList::Int(&[{}])",
            join(x.iter().map(|v| int_lit(i64::from(*v))).collect())
        ),
        List::Float(x) => format!(
            "crate::nbt::StaticList::Float(&[{}])",
            join(x.iter().map(|v| float_lit(*v)).collect())
        ),
        List::String(x) => format!(
            "crate::nbt::StaticList::String(&[{}])",
            join(x.iter().map(|v| str_lit(v)).collect())
        ),
        List::Compound(x) => format!(
            "crate::nbt::StaticList::Compound(&[{}])",
            join(x.iter().map(static_compound).collect())
        ),
    }
}

/// A float from its bit pattern, so the value is exact.
fn float_lit(bits: u32) -> String {
    format!("f32::from_bits({})", hex_lit(u64::from(bits), 8))
}

/// A decimal literal, digits grouped by three when clippy calls it long (more than five digits).
pub(crate) fn int_lit(v: i64) -> String {
    let digits = v.unsigned_abs().to_string();
    if digits.len() <= 5 {
        return v.to_string();
    }
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    if v < 0 { format!("-{out}") } else { out }
}

/// A hex literal of `width` digits, grouped by four.
pub(crate) fn hex_lit(v: u64, width: usize) -> String {
    let digits = format!("{v:0width$x}");
    let mut out = String::from("0x");
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && i.is_multiple_of(4) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read(path: &Path) -> R<Vec<u8>> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}
