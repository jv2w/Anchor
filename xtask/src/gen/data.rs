//! Block and item data: reads the committed dump (`xtask/data/dump/`), the reference lists
//! (`xtask/data/reference/`) and the block upgrade schemas that add properties after the dump's
//! data version (`upgrade::ADDING`), checks them against each other, and builds the block state
//! and item type lists, with their gameplay data, and the tables the data generators emit. A
//! reference state the dump does not have is a block state too, with the physics of
//! `block_physics.json`. The index of a state or item type in these lists is its number in the
//! generated tables, so every generator agrees on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::{int_lit, str_lit};
use super::info::{Break, EVERY_TOOL, Identity, ItemData, Physics, Tables, exact, support_codes};
use super::registry::Entry;
use super::upgrade::{ADDING, block_schema};
use crate::dump::{
    BLOCK_PHYSICS, DUMP_BLOCKS, DUMP_DESERIALIZED, DUMP_DIR, DUMP_ITEMS, DUMP_TABLES,
    REFERENCE_BLOCKS, REFERENCE_DIR, REFERENCE_ITEM_BLOCKS, REFERENCE_ITEMS,
};
use crate::fs::read;

/// The horizontal faces in the order of their bits in [`State::full_faces`] (bit 0 = north).
pub(super) const FACES: [&str; 4] = ["north", "east", "south", "west"];
/// The block type of a state the dump does not have: the dump's type for blocks without a
/// behaviour of their own, which custom blocks use too.
const INFO_UPDATE_KIND: &str = "info_update";
/// Classes whose collision box is inside out, with min above max (a defect of the dump): a wall
/// hanging sign squashes its cross bar by 3/4 from each side where 3/8 was meant. The 3/8 bar is
/// the official server's box (the reference `blocks.json` `collisionShape`, P6
/// `xtask/data/p6/facts.md`), so the box is rebuilt with 3/8 on the squashed axes.
const INSIDE_OUT_BOXES: [&str; 1] = ["WallHangingSign"];
/// The ticks the client takes to drink a consumable it is sent without item data (potion, milk
/// bucket, medicine): the client's own 1.6 s for them.
const LEGACY_USE_TICKS: u16 = 32;
/// The stack NBT `Type` of an ominous banner.
const OMINOUS: Tag = Tag::Int(1);
/// Bedrock states the dump splits by data Anchor keeps in the block entity: the Bedrock name, the
/// dump type Anchor's state takes and what it stands for. Only dump states of these names may
/// differ in type, class, block flags or item form; where their item forms differ, the Anchor
/// state's is marked as depending on the block entity. Every other value must still agree, and
/// every entry must be needed.
const SHARED: [(&str, &str, &str); 4] = [
    (
        "minecraft:bed",
        "bed",
        "the colour (bed block entity `color`)",
    ),
    (
        "minecraft:cauldron",
        "water_cauldron",
        "the fill of the cauldron, which Anchor does not keep",
    ),
    (
        "minecraft:standing_banner",
        "banner",
        "the colour and ominous type (banner block entity `Base` and `Type`)",
    ),
    (
        "minecraft:wall_banner",
        "wall_banner",
        "the colour and ominous type (banner block entity `Base` and `Type`)",
    ),
];
/// The factor from hardness to blast resistance of a state the dump does not have.
const UNIMPLEMENTED_BLAST_FACTOR: f64 = 5.0;
/// The friction of a state the dump does not have: that of air.
const UNIMPLEMENTED_FRICTION: f64 = 0.6;
/// The block-local unit cube: a shape of only this box is solid.
const UNIT_CUBE: [f64; 6] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0];

/// Where a state's item form comes from, before item types are known.
#[derive(Debug, Clone, PartialEq)]
enum ItemSource {
    /// The state has no item.
    None,
    /// The dumped Bedrock form.
    Form(Json),
    /// It depends on the block entity (see [`SHARED`]).
    BlockEntity,
}

/// A state's item form (`Block::asItem` of the dump).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateItem {
    /// The state has no item.
    None,
    /// The item type index.
    Item(u16),
    /// It depends on the block entity (see [`SHARED`]).
    BlockEntity,
}

/// A block state property value. The derived order (byte, int, string, then by value) is part of
/// the state order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Prop {
    Byte(u8),
    Int(i32),
    String(String),
}

/// A block name and its properties, sorted by property name: the identity of a Bedrock block
/// state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct StateKey {
    pub(super) name: String,
    pub(super) properties: Vec<(String, Prop)>,
}

/// A block state of the reference list, with the values of the dump, or, for a state the dump
/// does not have, of `block_physics.json`.
#[derive(Debug)]
pub(crate) struct State {
    pub(super) key: StateKey,
    /// The network ID the reference list gives.
    pub(crate) network_id: u32,
    pub(super) light_emission: u8,
    pub(super) light_filter: u8,
    pub(super) blocks_direct_sky_light: bool,
    /// The horizontal faces with full support (`getSupportType` of the dump), as [`FACES`] bits;
    /// a stair's are those of its `minecraft:corner` shape.
    pub(super) full_faces: u8,
    /// How the dump works the state out from its neighbours, if it does.
    pub(super) connect: Option<Connect>,
    /// Physical values; a fence's, pane's or stair's support and boxes are those of the variant
    /// its neighbour properties name.
    pub(super) physics: Physics,
    /// The block type, classes and traits.
    pub(super) identity: Identity,
    /// The item form.
    pub(super) item: StateItem,
}

/// The dump's `connect` entry: what it takes to work out a state from its neighbours.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Connect {
    /// `fence`, `thin`, `wall`, `fence_gate`, `stair`, `tripwire` or `tripwire_hook`.
    pub(super) kind: String,
    /// A fence's class: fences connect to fences of their own class.
    #[serde(default)]
    pub(super) group: Option<String>,
    /// The horizontal facing of fence gates, stairs and tripwire hooks.
    #[serde(default)]
    pub(super) facing: Option<String>,
    #[serde(default)]
    pub(super) upside_down: Option<bool>,
    /// The support and boxes of each variant the neighbours choose (fences, panes, stairs).
    #[serde(default)]
    variants: Option<Vec<Variant>>,
}

/// One neighbour-chosen variant of the dump: the client properties that name it, its support
/// and its collision boxes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Variant {
    properties: Json,
    support: Vec<String>,
    boxes: Vec<[f64; 6]>,
}

/// An NBT value read from the typed JSON of the dumps (`{"byte": 1}` and so on). It holds the
/// types the data uses, which `crate::nbt::Static` mirrors; another type fails generation, and
/// supporting it means adding it to both.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tag {
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
pub(super) enum List {
    End,
    Int(Vec<i32>),
    /// Bit patterns.
    Float(Vec<u32>),
    String(Vec<String>),
    Compound(Vec<BTreeMap<String, Tag>>),
}

/// An item type in its Bedrock form.
#[derive(Debug)]
pub(super) struct Item {
    pub(super) name: String,
    pub(super) meta: u16,
    /// The block state index of a block item: the state it places.
    pub(super) block: Option<u16>,
    /// The network ID of the block state a block item is sent with (0 for other items).
    pub(super) block_network_id: u32,
    /// NBT that is part of the type (`serializeType` extra data of the dump), such as an ominous
    /// banner's `Type`.
    pub(super) nbt: Option<BTreeMap<String, Tag>>,
    /// The runtime ID of the name in the reference list.
    pub(super) runtime_id: i16,
    /// Whether the dump's item registry gives this form (the default form of its kind).
    pub(super) vanilla: bool,
    /// The dump's data for the type.
    pub(super) data: ItemData,
    /// The item type left after consuming one (`ConsumableItem::getResidue` of the dump).
    pub(super) consume_residue: Option<u16>,
    /// The ticks the client takes to eat or drink one ([`use_ticks`]); 0 for items not consumed.
    pub(super) use_ticks: u16,
    /// The state index placing the item puts down, and the one against a side where that
    /// differs (`Item::getBlock` of the dump).
    pub(super) places: (Option<u16>, Option<u16>),
}

/// An item stack of the dumps (creative entries): the item type index, the count and the stack
/// NBT as the dump serializes it (type NBT merged in).
#[derive(Debug)]
pub(super) struct Stack {
    pub(super) item: u16,
    /// The stack's meta, which the dump sends; it differs from the type's meta where the dump
    /// reads meta as stack state.
    pub(super) meta: u16,
    pub(super) count: u16,
    pub(super) nbt: Option<BTreeMap<String, Tag>>,
}

/// A reference state that is not a dump state but that the dump reads as one of its states
/// (`deserialized.json`): the dump ignores some vanilla properties and writes one value for them.
#[derive(Debug)]
pub(crate) struct Alias {
    pub(super) key: StateKey,
    /// The reference list's network ID of `key`.
    pub(crate) network_id: u32,
    /// The state index of the dump state it becomes.
    pub(crate) state: u16,
}

/// The loaded and checked data.
#[derive(Debug)]
pub(crate) struct Data {
    pub(crate) states: Vec<State>,
    /// The aliases, sorted by key.
    pub(crate) aliases: Vec<Alias>,
    pub(super) items: Vec<Item>,
    /// The block types (names lowercase, sorted): their identity and the state index of their
    /// default state.
    pub(super) kinds: Vec<(Identity, u16)>,
    /// The effect, potion, enchantment and banner pattern tables.
    pub(super) tables: Tables,
    /// State index of a dump state (in its Bedrock form of the dump's data version), upgraded with
    /// the default values.
    dump_state_index: BTreeMap<StateKey, u16>,
    /// Item type indices by (name, block index).
    item_index: BTreeMap<(String, Option<u16>), Vec<u16>>,
    /// Runtime IDs by item name.
    runtime_ids: BTreeMap<String, i16>,
}

impl Data {
    /// The data of every file, loaded and checked.
    pub(crate) fn load(root: &Path) -> Result<Self, String> {
        let (network, reference_names) = read_reference(root)?;
        let added = read_upgrade(root)?;
        let reading = read_dump_reading(root, &network)?;
        let (dump_states, kind_defaults) = read_dump_states(root)?;
        let (mut states, dump_default) = upgrade(&dump_states, &added, &network, &reference_names)?;
        let (_, info_update) = kind_defaults
            .get(INFO_UPDATE_KIND)
            .ok_or("the dump has no info_update block type")?;
        let unimplemented = unimplemented_states(root, &network, &states, &reading, info_update)?;
        states.extend(unimplemented);

        let mut list = Vec::with_capacity(states.len());
        let mut sources = Vec::with_capacity(states.len());
        let mut state_index = BTreeMap::new();
        for (i, (key, values)) in states.into_iter().enumerate() {
            let index = u16::try_from(i).map_err(|_| "more than 65,536 block states")?;
            let network_id = *network.get(&key).ok_or("state lost its network ID")?;
            let physics = variant_physics(&key, &values)?;
            state_index.insert(key.clone(), index);
            sources.push(values.item);
            list.push(State {
                key,
                network_id,
                light_emission: values.light_emission,
                light_filter: values.light_filter,
                blocks_direct_sky_light: values.blocks_direct_sky_light,
                full_faces: physics.full_faces(),
                connect: values.connect,
                physics,
                identity: values.identity,
                item: StateItem::None,
            });
        }
        let mut dump_state_index = BTreeMap::new();
        for (dump, default) in dump_default {
            let index = *state_index
                .get(&default)
                .ok_or("default state not listed")?;
            dump_state_index.insert(dump, index);
        }
        let aliases = aliases(&reading, &network, &state_index, &dump_state_index)?;
        let mut kinds = Vec::with_capacity(kind_defaults.len());
        for (kind, (key, identity)) in kind_defaults {
            let index = *dump_state_index
                .get(&key)
                .ok_or_else(|| format!("default state of {kind} is not a dump state"))?;
            kinds.push((identity, index));
        }

        let mut data = Self {
            states: list,
            aliases,
            items: Vec::new(),
            kinds,
            tables: read_json(root, DUMP_DIR, DUMP_TABLES)?,
            dump_state_index,
            item_index: BTreeMap::new(),
            runtime_ids: BTreeMap::new(),
        };
        let vanilla: Vec<ItemBlock> = read_json(root, REFERENCE_DIR, REFERENCE_ITEM_BLOCKS)?;
        data.load_items(root, &network, &vanilla)?;
        for (i, source) in sources.iter().enumerate() {
            let item = match source {
                ItemSource::None => StateItem::None,
                ItemSource::BlockEntity => StateItem::BlockEntity,
                ItemSource::Form(json) => StateItem::Item(data.item_type(json)?),
            };
            if let Some(state) = data.states.get_mut(i) {
                state.item = item;
            }
        }
        data.check_vanilla_item_states(&vanilla)?;
        Ok(data)
    }

    /// The state index of a dump state in its Bedrock form of the dump's data version: its
    /// upgraded default form.
    fn dump_state(&self, key: &StateKey) -> Result<u16, String> {
        self.dump_state_index
            .get(key)
            .copied()
            .ok_or_else(|| format!("{} {:?} is not a dump state", key.name, key.properties))
    }

    /// The item type of an item in Bedrock form (`name`, `meta`, optional `block` and `nbt`), by
    /// the rules of [`Data::stack`].
    fn item_type(&self, json: &Json) -> Result<u16, String> {
        let (name, meta, block, nbt) = self.item_form(json)?;
        self.type_of(&name, meta, block, nbt.as_ref())
    }

    /// The runtime ID of an item name of the reference list.
    fn runtime_id(&self, name: &str) -> Result<i16, String> {
        self.runtime_ids
            .get(name)
            .copied()
            .ok_or_else(|| format!("item {name} is not in the reference list"))
    }

    /// Reads an item stack of the dumps (`name`, `meta`, optional `block` and `nbt`, `count`)
    /// and finds its item type the way `item::Type::from_name` does at run time: among the types
    /// of the name and block, the one with the stack's meta; if several have it, the one with the
    /// most type NBT that is part of the stack NBT; if none has it and the name and block have a
    /// single type, that type, as the dump reads such a meta as stack state (a firework star's
    /// colour). The stack keeps its own meta. A dumped block item always names the state its type
    /// places, so the fallback `from_name` has for a saved block item with another state (the
    /// block's item form) never applies here.
    pub(super) fn stack(&self, json: &Json) -> Result<Stack, String> {
        let (name, meta, block, nbt) = self.item_form(json)?;
        let count = json
            .get("count")
            .and_then(Json::as_u64)
            .and_then(|c| u16::try_from(c).ok())
            .ok_or_else(|| format!("stack of {name} has no count in range"))?;
        let item = self.type_of(&name, meta, block, nbt.as_ref())?;
        Ok(Stack {
            item,
            meta,
            count,
            nbt,
        })
    }

    /// Fails if the vanilla creative state of an item with a single dump block item type does
    /// not read, as `State::read` reads it (the state itself, else its alias), as a block whose
    /// item is that item: a stored block item is found by its block state through this reading.
    fn check_vanilla_item_states(&self, vanilla: &[ItemBlock]) -> Result<(), String> {
        let block_items = self.block_item_counts();
        for v in vanilla {
            if block_items.get(v.item.as_str()) != Some(&1) {
                continue;
            }
            let key = state_key(&v.name, &v.states)?;
            let state = self.read_state(&key).ok_or_else(|| {
                format!(
                    "the vanilla state of item {} is neither a state nor an alias",
                    v.item
                )
            })?;
            let item = match state.item {
                StateItem::Item(i) => self.items.get(usize::from(i)),
                StateItem::None | StateItem::BlockEntity => None,
            };
            if item.is_none_or(|i| i.name != v.item) {
                return Err(format!(
                    "the vanilla state of item {} reads as a block with another item",
                    v.item
                ));
            }
        }
        Ok(())
    }

    /// The number of block item types of each item name.
    fn block_item_counts(&self) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for item in self.items.iter().filter(|i| i.block.is_some()) {
            *counts.entry(item.name.as_str()).or_default() += 1;
        }
        counts
    }

    /// The state a reference state reads as: itself, else the state its alias names.
    fn read_state(&self, key: &StateKey) -> Option<&State> {
        let by_key = |s: &State| s.key.cmp(key);
        if let Ok(at) = self.states.binary_search_by(by_key) {
            return self.states.get(at);
        }
        let at = self.aliases.binary_search_by(|a| a.key.cmp(key)).ok()?;
        self.aliases
            .get(at)
            .and_then(|a| self.states.get(usize::from(a.state)))
    }

    /// Reads an item in Bedrock form (`name`, `meta`, optional `block` and `nbt`; a `count` key is
    /// allowed).
    fn item_form(&self, json: &Json) -> Result<ItemForm, String> {
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
            Some(b) => Some(self.dump_state(&bedrock_state(b)?)?),
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

    /// The state of a dumped block an item places.
    fn placed(&self, json: Option<&Json>) -> Result<Option<u16>, String> {
        json.map(|j| self.dump_state(&bedrock_state(j)?))
            .transpose()
    }

    /// The lookup of [`Data::stack`].
    fn type_of(
        &self,
        name: &str,
        meta: u16,
        block: Option<u16>,
        nbt: Option<&BTreeMap<String, Tag>>,
    ) -> Result<u16, String> {
        let candidates = self
            .item_index
            .get(&(name.to_owned(), block))
            .ok_or_else(|| format!("no item type {name} with block {block:?}"))?;
        let empty = BTreeMap::new();
        let have = nbt.unwrap_or(&empty);
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
        Ok(match (exact.as_slice(), candidates.as_slice()) {
            ([(index, _)], _) | ([], [index]) => *index,
            ([], _) => return Err(format!("no item type {name} meta {meta}")),
            (many, _) => {
                many.iter()
                    .filter(|(_, need)| need.iter().all(|(k, v)| have.get(k) == Some(v)))
                    .max_by_key(|(_, need)| need.len())
                    .ok_or_else(|| format!("no item type of {name} matches the stack NBT"))?
                    .0
            }
        })
    }

    /// Loads the item types of the dump and gives each its runtime ID.
    fn load_items(
        &mut self,
        root: &Path,
        network: &BTreeMap<StateKey, u32>,
        vanilla: &[ItemBlock],
    ) -> Result<(), String> {
        let reference: Vec<Entry> = read_json(root, REFERENCE_DIR, REFERENCE_ITEMS)?;
        self.runtime_ids = reference
            .iter()
            .map(|i| {
                i16::try_from(i.runtime_id)
                    .map(|id| (i.name.clone(), id))
                    .map_err(|_| format!("runtime ID of {} out of range", i.name))
            })
            .collect::<Result<_, String>>()?;
        let reference_data: BTreeMap<&str, &Json> = reference
            .iter()
            .map(|i| (i.name.as_str(), &i.data))
            .collect();
        let dump: Vec<DumpItem> = read_json(root, DUMP_DIR, DUMP_ITEMS)?;
        for (i, item) in dump.iter().enumerate() {
            let index = u16::try_from(i).map_err(|_| "more than 65,536 item types")?;
            let form = self.item_form(&item.bedrock)?;
            let runtime_id = self.runtime_id(&form.0)?;
            let consumed = item.data.food.is_some() || item.data.consumable.is_some();
            let use_ticks = if consumed {
                let data = reference_data
                    .get(form.0.as_str())
                    .ok_or_else(|| format!("item {} is not in the reference list", form.0))?;
                use_ticks(&form.0, data)?
            } else {
                0
            };
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
                vanilla: item.vanilla,
                data: item.data.clone(),
                consume_residue: None,
                use_ticks,
                places: (
                    self.placed(item.data.places.as_ref())?,
                    self.placed(item.data.places_on_wall.as_ref())?,
                ),
            });
        }
        for i in 0..self.items.len() {
            let item = self.items.get(i).ok_or("item index")?;
            let data = &item.data;
            let consume = data.consumable.as_ref().and_then(|c| c.residue.as_ref());
            let consume = consume.map(|j| self.item_type(j)).transpose()?;
            for v in [data.tool.efficiency]
                .into_iter()
                .chain(data.food.as_ref().map(|f| f.saturation))
                .chain(data.throw_force)
            {
                exact(v)?;
            }
            if let Some(item) = self.items.get_mut(i) {
                item.consume_residue = consume;
            }
        }
        self.use_vanilla_item_blocks(network, vanilla)
    }

    /// Sets the block network ID block items are sent with: the vanilla creative state of
    /// `item_blocks.json`, which the official server sends, where the item name has a single dump
    /// block item type and a single vanilla state; the dump's own default state otherwise. The
    /// vanilla state may be one the dump does not have (glazed terracotta facing down); it is only
    /// the network form, and the item still places the dump's state.
    fn use_vanilla_item_blocks(
        &mut self,
        network: &BTreeMap<StateKey, u32>,
        vanilla: &[ItemBlock],
    ) -> Result<(), String> {
        let block_items = self.block_item_counts();
        let mut ids = BTreeMap::new();
        for v in vanilla {
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DumpBlock {
    #[serde(rename = "type")]
    kind: String,
    bedrock: Json,
    default: bool,
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    classes: Vec<String>,
    traits: Vec<String>,
    #[serde(rename = "break")]
    break_info: Break,
    flags: Vec<String>,
    friction: f64,
    tags: Vec<String>,
    support: Vec<String>,
    boxes: Vec<[f64; 6]>,
    item: Option<Json>,
    #[serde(default)]
    connect: Option<Connect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DumpItem {
    bedrock: Json,
    data: ItemData,
    vanilla: bool,
}

/// `block_physics.json` (see `dump`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockPhysics {
    shapes: Vec<Vec<[f64; 6]>>,
    states: Vec<PhysicsRow>,
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

/// An entry of the dump's `deserialized.json`: a reference state and the Bedrock form of the dump
/// state the dump reads it as, `None` where the dump rejects it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deserialized {
    name: String,
    states: Json,
    read_as: Option<Json>,
}

/// How the dump reads each reference state it does not read as itself: the state it becomes, or
/// `None` where it rejects the state.
type DumpReading = BTreeMap<StateKey, Option<StateKey>>;

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

/// The per-state values. Overlapping dump states must agree on all but the identity, block flags
/// and item, which may differ only as [`SHARED`] allows.
#[derive(Debug, Clone, PartialEq)]
struct Values {
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    connect: Option<Connect>,
    physics: Physics,
    identity: Identity,
    item: ItemSource,
}

impl Values {
    /// The values every overlapping dump state must agree on.
    fn strict(&self) -> (u8, u8, bool, &Option<Connect>, &Physics) {
        (
            self.light_emission,
            self.light_filter,
            self.blocks_direct_sky_light,
            &self.connect,
            &self.physics,
        )
    }
}

/// The dump states by their Bedrock form of the dump's data version, merged, and the default
/// state and identity of each block type by name.
type DumpStates = (
    BTreeMap<StateKey, Values>,
    BTreeMap<String, (StateKey, Identity)>,
);

/// An item in Bedrock form: name, meta, the block state index and the type NBT.
type ItemForm = (String, u16, Option<u16>, Option<BTreeMap<String, Tag>>);

/// A state of `block_physics.json`: network ID, hardness, light emission, light dampening and the
/// index of its collision boxes.
type PhysicsRow = (u32, f64, u8, u8, usize);

/// The reference block list: network ID by state, and the states of each name.
type Reference = (BTreeMap<StateKey, u32>, BTreeMap<String, Vec<StateKey>>);

/// The upgraded states with their values, and the upgraded default form of each dump state.
type Upgraded = (BTreeMap<StateKey, Values>, BTreeMap<StateKey, StateKey>);

/// The meta of an item form, corrected where the dump sends a different one than the official
/// server: the dump's ominous banner (`Type` 1) has the black base colour, meta 0, where the
/// official server sends it white, meta 15 (P6). Every item read from the dumps passes here, so
/// types and stacks agree.
fn corrected_meta(name: &str, meta: u16, nbt: Option<&BTreeMap<String, Tag>>) -> u16 {
    let ominous = nbt.and_then(|n| n.get("Type")) == Some(&OMINOUS);
    if name == "minecraft:banner" && ominous && meta == 0 {
        15
    } else {
        meta
    }
}

/// The ticks the client takes to eat or drink the item `name`, from its reference `data` (the
/// item data the client is sent): the `minecraft:use_duration` component, or the `use_duration`
/// of a component-based item's `item_properties`, both in ticks. An item sent without data
/// (potion, milk bucket, medicine) takes [`LEGACY_USE_TICKS`]. Fails for an item with data but
/// no such value, or a value that is not 1 to 65,535 ticks.
fn use_ticks(name: &str, data: &Json) -> Result<u16, String> {
    let data = compound(data).map_err(|e| format!("{name}: {e}"))?;
    if data.is_empty() {
        return Ok(LEGACY_USE_TICKS);
    }
    let Some(Tag::Compound(components)) = data.get("components") else {
        return Err(format!("{name}: data without components"));
    };
    let ticks = match (
        components.get("minecraft:use_duration"),
        components.get("item_properties"),
    ) {
        (Some(Tag::Int(ticks)), _) => *ticks,
        (None, Some(Tag::Compound(properties))) => match properties.get("use_duration") {
            Some(Tag::Int(ticks)) => *ticks,
            _ => return Err(format!("{name}: item_properties without use_duration")),
        },
        _ => return Err(format!("{name}: consumed but no use_duration")),
    };
    u16::try_from(ticks)
        .ok()
        .filter(|&t| t > 0)
        .ok_or_else(|| format!("{name}: use_duration {ticks} out of range"))
}

/// Reads the reference block list.
fn read_reference(root: &Path) -> Result<Reference, String> {
    let reference: Vec<ReferenceBlock> = read_json(root, REFERENCE_DIR, REFERENCE_BLOCKS)?;
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

/// Reads the added properties of the block upgrade schemas in [`ADDING`], by block name. A
/// block in two schemas gets the properties of both.
fn read_upgrade(root: &Path) -> Result<BTreeMap<String, Vec<(String, Prop)>>, String> {
    let mut added: BTreeMap<String, Vec<(String, Prop)>> = BTreeMap::new();
    for schema in ADDING {
        let upgrade: Upgrade = serde_json::from_slice(&read(&block_schema(root, schema))?)
            .map_err(|e| format!("{schema}: {e}"))?;
        for (name, props) in &upgrade.added_properties {
            let list = added.entry(name.clone()).or_default();
            for (prop, value) in props {
                let value =
                    prop_value(value).map_err(|e| format!("{schema}: {name} {prop}: {e}"))?;
                list.push((prop.clone(), value));
            }
        }
    }
    Ok(added)
}

/// Reads how the dump reads the reference states (`deserialized.json`, which lists the states it
/// does not read as themselves). Fails if a listed state is not a reference state or is listed
/// twice.
fn read_dump_reading(
    root: &Path,
    network: &BTreeMap<StateKey, u32>,
) -> Result<DumpReading, String> {
    let list: Vec<Deserialized> = read_json(root, DUMP_DIR, DUMP_DESERIALIZED)?;
    dump_reading(list, network)
}

/// The reading of [`read_dump_reading`] from the read `deserialized.json`.
fn dump_reading(
    list: Vec<Deserialized>,
    network: &BTreeMap<StateKey, u32>,
) -> Result<DumpReading, String> {
    let mut reading = DumpReading::new();
    for d in list {
        let key = state_key(&d.name, &d.states)?;
        if !network.contains_key(&key) {
            return Err(format!(
                "deserialized.json: {} {:?} is not a reference state",
                key.name, key.properties
            ));
        }
        let read_as = d.read_as.as_ref().map(bedrock_state).transpose()?;
        if reading.insert(key, read_as).is_some() {
            return Err(format!("deserialized.json: {} is listed twice", d.name));
        }
    }
    Ok(reading)
}

/// The aliases: the reference states that are not dump states but that the dump reads as one of
/// its states, with that state's index. Fails if the dump reads a state as another state, or as
/// one that has no state.
fn aliases(
    reading: &DumpReading,
    network: &BTreeMap<StateKey, u32>,
    state_index: &BTreeMap<StateKey, u16>,
    dump_state_index: &BTreeMap<StateKey, u16>,
) -> Result<Vec<Alias>, String> {
    let mut aliases = Vec::new();
    for (key, &network_id) in network {
        match reading.get(key) {
            // Read as itself, or rejected: its own state.
            None | Some(None) => {}
            Some(Some(_)) if state_index.contains_key(key) => {
                return Err(format!(
                    "the dump reads the state {} {:?} as another state",
                    key.name, key.properties
                ));
            }
            Some(Some(read_as)) => {
                let state = *dump_state_index.get(read_as).ok_or_else(|| {
                    format!(
                        "the dump reads the reference state {} {:?} as {} {:?}, which has no \
                         state",
                        key.name, key.properties, read_as.name, read_as.properties
                    )
                })?;
                aliases.push(Alias {
                    key: key.clone(),
                    network_id,
                    state,
                });
            }
        }
    }
    Ok(aliases)
}

/// The block states of the reference list that the dump does not have, with the physics of
/// `block_physics.json`: the official server's light, hardness and collision boxes. A state whose
/// only collision box is the whole cell is solid; the others can be stood in. The identity is
/// the `info_update` type's, the dump's type of a block without a behaviour of its own, and
/// the state has no item. Fails if a state has no physics.
fn unimplemented_states(
    root: &Path,
    network: &BTreeMap<StateKey, u32>,
    states: &BTreeMap<StateKey, Values>,
    reading: &DumpReading,
    info_update: &Identity,
) -> Result<Vec<(StateKey, Values)>, String> {
    let physics: BlockPhysics = read_json(root, REFERENCE_DIR, BLOCK_PHYSICS)?;
    let by_id: BTreeMap<u32, &PhysicsRow> = physics.states.iter().map(|s| (s.0, s)).collect();
    let mut found = Vec::new();
    for key in unimplemented_keys(network, states, reading)? {
        let network_id = *network.get(key).ok_or("state lost its network ID")?;
        let &&(_, hardness, light_emission, light_filter, shape) = by_id
            .get(&network_id)
            .ok_or_else(|| format!("{} ({network_id}) has no physics", key.name))?;
        let boxes = physics
            .shapes
            .get(shape)
            .ok_or_else(|| format!("{} ({network_id}) names a missing shape", key.name))?;
        let values =
            unimplemented_values((hardness, light_emission, light_filter), boxes, info_update)
                .map_err(|e| format!("{} ({network_id}): {e}", key.name))?;
        found.push((key.clone(), values));
    }
    Ok(found)
}

/// The values of a state the dump does not have, from its hardness, light emission, light
/// dampening and collision boxes. Every tool counts as the right one at harvest level 0, so the
/// break time the server allows is never longer than the client's.
fn unimplemented_values(
    (hardness, light_emission, light_filter): (f64, u8, u8),
    boxes: &[[f64; 6]],
    info_update: &Identity,
) -> Result<Values, String> {
    let (flag, support) = if boxes == [UNIT_CUBE] {
        ("solid", "full")
    } else {
        ("transparent", "none")
    };
    let break_info = Break {
        hardness,
        blast_resistance: hardness * UNIMPLEMENTED_BLAST_FACTOR,
        tool_type: EVERY_TOOL,
        harvest_level: 0,
    };
    let physics = Physics::new(
        break_info,
        &[flag.to_owned()],
        UNIMPLEMENTED_FRICTION,
        &[],
        &[support; 6].map(str::to_owned),
        boxes.to_vec(),
    )?;
    Ok(Values {
        light_emission,
        light_filter,
        blocks_direct_sky_light: light_filter > 0,
        connect: None,
        physics,
        identity: info_update.clone(),
        item: ItemSource::None,
    })
}

/// The reference states that are neither dump states nor aliases: the ones the dump rejects.
/// Fails if the dump reads a reference state as itself but has no such state.
fn unimplemented_keys<'a>(
    network: &'a BTreeMap<StateKey, u32>,
    states: &BTreeMap<StateKey, Values>,
    reading: &DumpReading,
) -> Result<Vec<&'a StateKey>, String> {
    let mut keys = Vec::new();
    for key in network.keys().filter(|key| !states.contains_key(key)) {
        match reading.get(key) {
            None => {
                return Err(format!(
                    "the dump reads the reference state {} {:?} as itself, but has no such state",
                    key.name, key.properties
                ));
            }
            Some(None) => keys.push(key),
            Some(Some(_)) => {}
        }
    }
    Ok(keys)
}

/// Reads the dump states by their Bedrock form of the dump's data version and merges overlapping
/// ones (see [`merge`]); also the default state and identity of each block type, which all its
/// states must share.
fn read_dump_states(root: &Path) -> Result<DumpStates, String> {
    let dump: Vec<DumpBlock> = read_json(root, DUMP_DIR, DUMP_BLOCKS)?;
    let mut groups: BTreeMap<StateKey, Vec<Values>> = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    let mut kinds: BTreeMap<String, Identity> = BTreeMap::new();
    for b in dump {
        let key = bedrock_state(&b.bedrock)?;
        let boxes = valid_boxes(&key, &b.classes, b.boxes)?;
        let physics = Physics::new(
            b.break_info,
            &b.flags,
            b.friction,
            &b.tags,
            &b.support,
            boxes,
        )
        .map_err(|e| format!("{} {:?}: {e}", key.name, key.properties))?;
        let mut classes = b.classes;
        classes.push("Block".to_owned());
        let mut traits = b.traits;
        traits.sort();
        let identity = Identity {
            kind: b.kind.clone(),
            classes,
            traits,
        };
        if *kinds
            .entry(b.kind.clone())
            .or_insert_with(|| identity.clone())
            != identity
        {
            return Err(format!("states of {} differ in class or traits", b.kind));
        }
        if b.default && defaults.insert(b.kind.clone(), key.clone()).is_some() {
            return Err(format!("{} has two default states", b.kind));
        }
        let values = Values {
            light_emission: b.light_emission,
            light_filter: b.light_filter,
            blocks_direct_sky_light: b.blocks_direct_sky_light,
            connect: b.connect,
            physics,
            identity,
            item: b.item.map_or(ItemSource::None, ItemSource::Form),
        };
        groups.entry(key).or_default().push(values);
    }
    let mut kind_defaults = BTreeMap::new();
    for (kind, identity) in kinds {
        let key = defaults
            .remove(&kind)
            .ok_or_else(|| format!("{kind} has no default state"))?;
        kind_defaults.insert(kind, (key, identity));
    }
    let mut used = BTreeSet::new();
    let mut states = BTreeMap::new();
    for (key, group) in groups {
        let merged = merge(&key, &group, &mut used)?;
        states.insert(key, merged);
    }
    if let Some((name, ..)) = SHARED.iter().find(|(name, ..)| !used.contains(name)) {
        return Err(format!("SHARED entry {name} is not needed"));
    }
    Ok((states, kind_defaults))
}

/// The collision boxes of a dumped state with inside-out ones rebuilt, which only
/// [`INSIDE_OUT_BOXES`] classes may have: on an axis where min is above max, the dump trimmed
/// `min` (= 3/4) from each side, and the box keeps half of that trim, `min / 2 ..= 1 - min / 2`.
fn valid_boxes(
    key: &StateKey,
    classes: &[String],
    boxes: Vec<[f64; 6]>,
) -> Result<Vec<[f64; 6]>, String> {
    let inside_out =
        |b: &[f64; 6]| matches!(b, [x0, y0, z0, x1, y1, z1] if x0 > x1 || y0 > y1 || z0 > z1);
    if boxes.iter().any(inside_out)
        && !classes
            .iter()
            .any(|c| INSIDE_OUT_BOXES.contains(&c.as_str()))
    {
        return Err(format!(
            "{} {:?} has an inside-out collision box",
            key.name, key.properties
        ));
    }
    let axis = |min: f64, max: f64| {
        if min > max {
            (min / 2.0, 1.0 - min / 2.0)
        } else {
            (min, max)
        }
    };
    Ok(boxes
        .into_iter()
        .map(|[x0, y0, z0, x1, y1, z1]| {
            let ((x0, x1), (y0, y1), (z0, z1)) = (axis(x0, x1), axis(y0, y1), axis(z0, z1));
            [x0, y0, z0, x1, y1, z1]
        })
        .collect())
}

/// Merges the dump states of one Bedrock state. They must agree on every value, except where
/// [`SHARED`] names the Bedrock state. There a group of several dump types takes the values,
/// classes and traits of the type `SHARED` names, and the types may differ in their block flags
/// (a potion cauldron has no entity collision); an item form that differs becomes block entity
/// data. Adds the names it needed `SHARED` for to `used`.
fn merge(
    key: &StateKey,
    group: &[Values],
    used: &mut BTreeSet<&'static str>,
) -> Result<Values, String> {
    let shared = SHARED.iter().find(|(name, ..)| *name == key.name);
    let disagree = |what: &str| {
        format!(
            "dump states of {} {:?} disagree on their {what}{}",
            key.name,
            key.properties,
            if shared.is_some() {
                ""
            } else {
                " (if a block entity tells them apart, add the name to SHARED)"
            }
        )
    };
    let first = group.first().ok_or("empty state group")?;
    // The state that speaks for the group: one of the type SHARED names, if the group has one.
    let chosen = shared
        .and_then(|(_, kind, _)| group.iter().find(|v| v.identity.kind == *kind))
        .unwrap_or(first);
    let mut merged = chosen.clone();
    let mut needed = false;
    for values in group {
        if values.identity.kind == chosen.identity.kind {
            if values.strict() != chosen.strict() || values.identity != chosen.identity {
                return Err(disagree("values"));
            }
        } else {
            let mut flagless = values.clone();
            flagless.physics.flags = chosen.physics.flags;
            if shared.is_none() || flagless.strict() != chosen.strict() {
                return Err(disagree("type or values"));
            }
            needed = true;
        }
        if values.item != chosen.item {
            if shared.is_none() {
                return Err(disagree("item form"));
            }
            needed = true;
            merged.item = ItemSource::BlockEntity;
        }
    }
    if let Some((name, kind, _)) = shared
        && needed
    {
        if merged.identity.kind != *kind {
            return Err(format!("{} has no dump state of type {kind}", key.name));
        }
        used.insert(*name);
    }
    Ok(merged)
}

/// Upgrades the dump states to the reference list's version: adds the new properties' defaults,
/// then takes every value the reference list has for them. Every state must end up in the
/// reference list, and an upgrade entry for a block the dump lacks must still name a reference
/// block (the schemas cover the whole reference list, the dump part of it).
fn upgrade(
    dump: &BTreeMap<StateKey, Values>,
    added: &BTreeMap<String, Vec<(String, Prop)>>,
    network: &BTreeMap<StateKey, u32>,
    reference_names: &BTreeMap<String, Vec<StateKey>>,
) -> Result<Upgraded, String> {
    let mut used = BTreeSet::new();
    let mut states = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    for (key, values) in dump {
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
    if let Some(name) = added
        .keys()
        .find(|name| !used.contains(*name) && !reference_names.contains_key(*name))
    {
        return Err(format!("{ADDING:?}: {name} is not a reference block"));
    }
    Ok((states, defaults))
}

/// The physical values of an upgraded state. A fence's, pane's or stair's support and collision
/// boxes come from the dumped variant whose client properties the state has: the dump works the
/// connections and stair shape out from the neighbours, so the dumped state (unconnected,
/// straight) does not have them.
fn variant_physics(key: &StateKey, values: &Values) -> Result<Physics, String> {
    let mut physics = values.physics.clone();
    let Some(variants) = values.connect.as_ref().and_then(|c| c.variants.as_ref()) else {
        return Ok(physics);
    };
    let mut found = None;
    for v in variants {
        let props = state_key(&key.name, &v.properties)?.properties;
        if props.iter().all(|p| key.properties.contains(p)) {
            if found.is_some() {
                return Err(format!(
                    "{} {:?} matches two variants",
                    key.name, key.properties
                ));
            }
            found = Some(v);
        }
    }
    let v = found.ok_or_else(|| format!("{} {:?} matches no variant", key.name, key.properties))?;
    for x in v.boxes.iter().flatten() {
        exact(*x)?;
    }
    physics.support = support_codes(&v.support)?;
    physics.boxes.clone_from(&v.boxes);
    Ok(physics)
}

/// Reads a committed JSON file.
pub(super) fn read_json<T: for<'de> Deserialize<'de>>(
    root: &Path,
    dir: &str,
    file: &str,
) -> Result<T, String> {
    let path = root.join(dir).join(file);
    let bytes = read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads a Bedrock block state object (`name`, `states`).
fn bedrock_state(json: &Json) -> Result<StateKey, String> {
    let name = json
        .get("name")
        .and_then(Json::as_str)
        .ok_or("block state without a name")?;
    let states = json.get("states").ok_or("block state without states")?;
    state_key(name, states)
}

fn state_key(name: &str, states: &Json) -> Result<StateKey, String> {
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
fn prop_value(json: &Json) -> Result<Prop, String> {
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
pub(super) fn compound(json: &Json) -> Result<BTreeMap<String, Tag>, String> {
    let obj = json.as_object().ok_or("NBT compound is not an object")?;
    obj.iter()
        .map(|(k, v)| Ok((k.clone(), tag(v).map_err(|e| format!("{k}: {e}"))?)))
        .collect()
}

/// Reads one typed NBT value (`{"type": value}`).
fn tag(json: &Json) -> Result<Tag, String> {
    let obj = json.as_object().ok_or("NBT value is not an object")?;
    let mut entries = obj.iter();
    let (Some((kind, value)), None) = (entries.next(), entries.next()) else {
        return Err("NBT value must have exactly one type key".to_owned());
    };
    tag_of(kind, value)
}

fn tag_of(kind: &str, value: &Json) -> Result<Tag, String> {
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
                .collect::<Result<_, String>>()?,
        ),
        "list" => Tag::List(list(value)?),
        "compound" => Tag::Compound(compound(value)?),
        other => return Err(format!("NBT type {other} is not supported (see Tag)")),
    })
}

/// A byte, signed or unsigned in the JSON, kept as its bit pattern.
fn byte(value: &Json) -> Result<u8, String> {
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

fn int(value: &Json) -> Result<i32, String> {
    value
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| format!("int value {value} out of range"))
}

fn float(value: &Json) -> Result<u32, String> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("float bits {value} out of range"))
}

fn string(value: &Json) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("string value {value} is not a string"))
}

/// A typed list body: `{"type": element type, "values": [typed values]}`.
fn list(value: &Json) -> Result<List, String> {
    let element = value
        .get("type")
        .and_then(Json::as_str)
        .ok_or("list without an element type")?;
    let values = value
        .get("values")
        .and_then(Json::as_array)
        .ok_or("list without values")?;
    // Each element is itself typed; it must have the list's type.
    let body = |v: &Json| -> Result<Json, String> {
        v.get(element)
            .cloned()
            .ok_or_else(|| format!("list of {element} holds {v}"))
    };
    Ok(match element {
        "end" if values.is_empty() => List::End,
        "int" => List::Int(
            values
                .iter()
                .map(|v| int(&body(v)?))
                .collect::<Result<_, String>>()?,
        ),
        "float" => List::Float(
            values
                .iter()
                .map(|v| float(&body(v)?))
                .collect::<Result<_, String>>()?,
        ),
        "string" => List::String(
            values
                .iter()
                .map(|v| string(&body(v)?))
                .collect::<Result<_, String>>()?,
        ),
        "compound" => List::Compound(
            values
                .iter()
                .map(|v| compound(&body(v)?))
                .collect::<Result<_, String>>()?,
        ),
        other => return Err(format!("list of {other} is not supported (see List)")),
    })
}

/// Rust source of a `crate::nbt::StaticCompound` (`&[("key", Static::...)]`), keys in byte
/// order like `nbt::Compound`.
pub(super) fn static_compound(c: &BTreeMap<String, Tag>) -> String {
    let mut out = "&[".to_owned();
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
        Tag::Short(x) => format!("crate::nbt::Static::Short({})", int_lit(&x.to_string())),
        Tag::Int(x) => format!("crate::nbt::Static::Int({})", int_lit(&x.to_string())),
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
            join(x.iter().map(|v| int_lit(&v.to_string())).collect())
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

/// A hex literal of `width` digits, grouped by four.
pub(super) fn hex_lit(v: u64, width: usize) -> String {
    let digits = format!("{v:0width$x}");
    let mut out = "0x".to_owned();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && i.is_multiple_of(4) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::r#gen::names::upper_camel;

    /// A dump state's values: `kind`, its light emission, flags and item name.
    fn values(kind: &str, light: u8, flags: &[&str], item: &str) -> Values {
        let flags: Vec<String> = flags.iter().map(|f| (*f).to_owned()).collect();
        let physics = Physics::new(
            Break {
                hardness: 1.0,
                blast_resistance: 5.0,
                tool_type: 0,
                harvest_level: 0,
            },
            &flags,
            0.6,
            &[],
            &["full"; 6].map(str::to_owned),
            vec![UNIT_CUBE],
        )
        .unwrap();
        Values {
            light_emission: light,
            light_filter: 15,
            blocks_direct_sky_light: true,
            connect: None,
            physics,
            identity: Identity {
                kind: kind.to_owned(),
                classes: vec![upper_camel(kind), "Block".to_owned()],
                traits: Vec::new(),
            },
            item: ItemSource::Form(serde_json::json!({ "name": item, "meta": 0 })),
        }
    }

    fn key(name: &str) -> StateKey {
        StateKey {
            name: name.to_owned(),
            properties: Vec::new(),
        }
    }

    /// The value check of overlapping dump states: any difference fails generation unless
    /// `SHARED` names the Bedrock state, and there only the type, block flags and item form may
    /// differ.
    #[test]
    fn overlapping_states_must_agree() {
        let mut used = BTreeSet::new();
        let stone = key("minecraft:stone");
        let same = vec![values("stone", 0, &[], "a"), values("stone", 0, &[], "a")];
        assert!(merge(&stone, &same, &mut used).is_ok());
        for other in [
            values("stone", 1, &[], "a"),
            values("stone", 0, &["solid"], "a"),
            values("stone", 0, &[], "b"),
            values("granite", 0, &[], "a"),
        ] {
            let group = vec![values("stone", 0, &[], "a"), other];
            let err = merge(&stone, &group, &mut used).unwrap_err();
            assert!(err.contains("disagree"), "{err}");
        }
        assert!(used.is_empty());

        // A bed's colour: the item forms differ, so the item comes from the block entity.
        let bed = key("minecraft:bed");
        let group = vec![values("bed", 0, &[], "red"), values("bed", 0, &[], "blue")];
        let merged = merge(&bed, &group, &mut used).unwrap();
        assert_eq!(merged.item, ItemSource::BlockEntity);
        assert!(used.contains("minecraft:bed"));
        // A cauldron takes the water cauldron's type and flags whichever state comes first; the
        // potion cauldron may differ in flags but not in other values.
        let cauldron = key("minecraft:cauldron");
        let water = values("water_cauldron", 0, &["entity_collision"], "c");
        let group = vec![values("potion_cauldron", 0, &[], "c"), water.clone()];
        let merged = merge(&cauldron, &group, &mut used).unwrap();
        assert_eq!(merged.identity.kind, "water_cauldron");
        assert_eq!(merged.physics, water.physics);
        assert_eq!(merged.item, water.item);
        let group = vec![values("potion_cauldron", 3, &[], "c"), water];
        assert!(merge(&cauldron, &group, &mut used).is_err());
        let group = vec![
            values("potion_cauldron", 0, &[], "c"),
            values("lava_cauldron", 0, &[], "c"),
        ];
        assert!(merge(&cauldron, &group, &mut used).is_err());
    }

    /// A wall hanging sign's inside-out bar becomes the official server's bar (the reference
    /// `blocks.json` `collisionShape`: facing north or south `[0, 0.875, 0.375, 1, 1, 0.625]`,
    /// east or west `[0.375, 0.875, 0, 0.625, 1, 1]`); another class may not have one.
    #[test]
    fn only_known_classes_may_have_inside_out_boxes() {
        let north = vec![[0.0, 0.875, 0.75, 1.0, 1.0, 0.25]];
        let east = vec![[0.75, 0.875, 0.0, 0.25, 1.0, 1.0]];
        let sign = ["WallHangingSign".to_owned()];
        let hanging = key("minecraft:oak_wall_hanging_sign");
        assert_eq!(
            valid_boxes(&hanging, &sign, north.clone()).unwrap(),
            [[0.0, 0.875, 0.375, 1.0, 1.0, 0.625]]
        );
        assert_eq!(
            valid_boxes(&hanging, &sign, east).unwrap(),
            [[0.375, 0.875, 0.0, 0.625, 1.0, 1.0]]
        );
        let stone = ["Opaque".to_owned()];
        assert!(valid_boxes(&key("minecraft:stone"), &stone, north).is_err());
    }

    /// A reference state the dump reads as another becomes an alias of that state; gen fails
    /// where the dump reads a state as one with no state, or reads a state as another.
    #[test]
    fn aliases_need_a_state_for_what_the_dump_reads() {
        let facing = |f: &str| StateKey {
            name: "minecraft:pumpkin".to_owned(),
            properties: vec![(
                "minecraft:cardinal_direction".to_owned(),
                Prop::String(f.to_owned()),
            )],
        };
        let entry = |f: &str, read_as: Option<&str>| Deserialized {
            name: "minecraft:pumpkin".to_owned(),
            states: serde_json::json!({ "minecraft:cardinal_direction": { "string": f } }),
            read_as: read_as.map(|p| {
                serde_json::json!({
                    "name": "minecraft:pumpkin",
                    "states": { "minecraft:cardinal_direction": { "string": p } },
                })
            }),
        };
        let network = BTreeMap::from([(facing("east"), 1), (facing("south"), 2)]);
        let states = BTreeMap::from([(facing("south"), 7)]);
        let dump = BTreeMap::from([(facing("south"), 7)]);
        let reading = |list: Vec<Deserialized>| dump_reading(list, &network).unwrap();

        let reading_east = reading(vec![entry("east", Some("south"))]);
        let found = aliases(&reading_east, &network, &states, &dump).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (&found[0].key, found[0].network_id, found[0].state),
            (&facing("east"), 1, 7)
        );
        // Rejected: not an alias.
        let rejected = reading(vec![entry("east", None)]);
        assert!(
            aliases(&rejected, &network, &states, &dump)
                .unwrap()
                .is_empty()
        );
        // The dump's state has no state.
        assert!(aliases(&reading_east, &network, &states, &BTreeMap::new()).is_err());
        // A state read as another.
        let both = BTreeMap::from([(facing("east"), 6), (facing("south"), 7)]);
        assert!(aliases(&reading_east, &network, &both, &dump).is_err());
        // Not a reference state.
        assert!(dump_reading(vec![entry("up", None)], &network).is_err());
    }

    /// The reference states the dump rejects are the unimplemented ones; gen fails where the dump
    /// reads a state as itself but has no such state.
    #[test]
    fn unimplemented_keys_are_the_states_the_dump_rejects() {
        let state = |name: &str| StateKey {
            name: name.to_owned(),
            properties: Vec::new(),
        };
        let network = BTreeMap::from([
            (state("minecraft:a"), 1),
            (state("minecraft:b"), 2),
            (state("minecraft:c"), 3),
        ]);
        let states = BTreeMap::from([(state("minecraft:a"), values("a", 0, &[], "a"))]);
        let reading = DumpReading::from([
            (state("minecraft:b"), None),
            (state("minecraft:c"), Some(state("minecraft:a"))),
        ]);
        assert_eq!(
            unimplemented_keys(&network, &states, &reading).unwrap(),
            [&state("minecraft:b")]
        );
        assert!(unimplemented_keys(&network, &states, &DumpReading::new()).is_err());
    }

    /// A state the dump does not have is solid when its only box is the whole cell, and is broken
    /// by any tool in its hardness times the blast factor.
    #[test]
    fn unimplemented_values_follow_the_collision_boxes() {
        let info_update = values("info_update", 0, &[], "a").identity;
        let solid = unimplemented_values((2.0, 3, 15), &[UNIT_CUBE], &info_update).unwrap();
        assert_eq!(solid.physics.flags, 0b1);
        assert_eq!(solid.physics.full_faces(), 0b1111);
        assert_eq!(solid.physics.break_info.blast_resistance, 10.0);
        assert_eq!(solid.physics.break_info.tool_type, EVERY_TOOL);
        assert_eq!(
            (
                solid.light_emission,
                solid.light_filter,
                solid.blocks_direct_sky_light
            ),
            (3, 15, true)
        );
        assert_eq!(solid.item, ItemSource::None);
        let slab = [[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]];
        let open = unimplemented_values((0.5, 0, 0), &slab, &info_update).unwrap();
        assert_eq!(open.physics.flags, 0b10);
        assert_eq!(open.physics.full_faces(), 0);
        assert!(!open.blocks_direct_sky_light);
    }
}
