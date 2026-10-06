//! Block and item data: reads the committed PMMP dump (`xtask/data/pmmp/`),
//! the reference lists (`xtask/data/reference/`) and the block upgrade schema of the protocol
//! version (`upgrade::LATEST`), checks them against each other, and builds the block state
//! and item type lists, with their gameplay data, and the tables the data generators emit. The
//! index of a state or item type in these lists is its number in the generated tables, so every
//! generator agrees on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::str_lit;
use super::info::{Break, Identity, ItemData, Physics, Tables, exact, support_codes};

type R<T> = Result<T, String>;

/// Committed data files, relative to the repository root.
pub(crate) const PMMP_DIR: &str = "xtask/data/pmmp";
pub(crate) const REFERENCE_DIR: &str = "xtask/data/reference";
const UPGRADE_PATH: &str = super::upgrade::LATEST;

/// Bedrock states PMMP splits by data Anchor keeps in the block entity: the
/// Bedrock name, the PMMP type Anchor's state takes and what the block entity holds. Only PMMP
/// states of these names may differ in PMMP type, class, item form or drops; where their item
/// forms or drops differ, the Anchor state's are marked as depending on the block entity. Every
/// other value must still agree, and every entry must be needed.
pub(crate) const SHARED: [(&str, &str, &str); 4] = [
    (
        "minecraft:bed",
        "bed",
        "the colour (bed block entity `color`)",
    ),
    (
        "minecraft:cauldron",
        "water_cauldron",
        "a potion in place of water (cauldron block entity `PotionId`)",
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

/// The drops of a state whose drops depend on its block entity (see [`SHARED`]).
pub(crate) const BLOCK_ENTITY_DROPS: &str = "block_entity";

/// Where a state's item form comes from, before item types are known.
#[derive(Debug, Clone, PartialEq)]
enum ItemSource {
    /// PMMP `asItem` is air.
    None,
    /// The dumped Bedrock form.
    Form(Json),
    /// It depends on the block entity (see [`SHARED`]).
    BlockEntity,
}

/// A state's item form (PMMP `Block::asItem`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateItem {
    /// PMMP `asItem` is air.
    None,
    /// The item type index.
    Item(u16),
    /// It depends on the block entity (see [`SHARED`]).
    BlockEntity,
}

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
    /// Physical values; a fence's, pane's or stair's support and boxes are those of the variant
    /// its neighbour properties name.
    pub(crate) physics: Physics,
    /// The PMMP type, classes and traits.
    pub(crate) identity: Identity,
    /// The item form.
    pub(crate) item: StateItem,
    /// The dumped drops (`blocks.json` `drops`), or [`BLOCK_ENTITY_DROPS`].
    pub(crate) drops: Json,
}

/// The horizontal faces in the order of their bits in [`State::full_faces`] (bit 0 = north).
pub(crate) const FACES: [&str; 4] = ["north", "east", "south", "west"];

/// The dump's `connect` entry: what PMMP needs to work out a state from its neighbours.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    /// Whether PMMP's `VanillaItems` gives this form (the default form of its kind).
    pub(crate) vanilla: bool,
    /// PMMP's data for the type.
    pub(crate) data: ItemData,
    /// The item type left after burning one as fuel (PMMP `getFuelResidue`).
    pub(crate) fuel_residue: Option<u16>,
    /// The item type left after consuming one (PMMP `ConsumableItem::getResidue`).
    pub(crate) consume_residue: Option<u16>,
    /// The state index placing the item puts down, and the one against a side where that
    /// differs (PMMP `Item::getBlock`).
    pub(crate) places: (Option<u16>, Option<u16>),
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

/// A reference state that is not an Anchor state but that PMMP reads as one of its states
/// (`deserialized.json`): PMMP ignores some vanilla properties and writes one value for them.
#[derive(Debug)]
pub(crate) struct Alias {
    pub(crate) key: StateKey,
    /// The reference list's network ID of `key`.
    pub(crate) network_id: u32,
    /// The Anchor state index of the PMMP state it becomes.
    pub(crate) state: u16,
}

/// The loaded and checked data.
#[derive(Debug)]
pub(crate) struct Data {
    pub(crate) states: Vec<State>,
    /// The aliases, sorted by key.
    pub(crate) aliases: Vec<Alias>,
    pub(crate) items: Vec<Item>,
    /// The PMMP block types (`VanillaBlocks` names, lowercase, sorted): their identity and the
    /// Anchor state of their default state.
    pub(crate) kinds: Vec<(Identity, u16)>,
    /// The effect, potion, enchantment, tag, banner pattern and furnace tables.
    pub(crate) tables: Tables,
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
    kind: String,
    bedrock: Json,
    default: bool,
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    ticks_randomly: bool,
    recipe_meta: i32,
    classes: Vec<String>,
    traits: Vec<String>,
    #[serde(rename = "break")]
    break_info: Break,
    fire: (u8, u8, bool),
    flags: Vec<String>,
    friction: f64,
    tags: Vec<String>,
    support: Vec<String>,
    boxes: Vec<[f64; 6]>,
    item: Option<Json>,
    drops: Json,
    #[serde(default)]
    connect: Option<Connect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PmmpItem {
    bedrock: Json,
    data: ItemData,
    #[serde(rename = "names")]
    _names: Vec<String>,
    vanilla: bool,
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

/// An entry of the PMMP `deserialized.json`: a reference state and the Bedrock form of the PMMP
/// state PMMP reads it as, `None` where PMMP rejects it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deserialized {
    name: String,
    states: Json,
    pmmp: Option<Json>,
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

/// PMMP's per-state values. Overlapping PMMP states must agree on all but the identity, item and
/// drops, which may differ only as [`SHARED`] allows.
#[derive(Debug, Clone, PartialEq)]
struct Values {
    light_emission: u8,
    light_filter: u8,
    blocks_direct_sky_light: bool,
    ticks_randomly: bool,
    connect: Option<Connect>,
    physics: Physics,
    identity: Identity,
    item: ItemSource,
    drops: Json,
}

impl Values {
    /// The values every overlapping PMMP state must agree on.
    fn strict(&self) -> (u8, u8, bool, bool, &Option<Connect>, &Physics) {
        (
            self.light_emission,
            self.light_filter,
            self.blocks_direct_sky_light,
            self.ticks_randomly,
            &self.connect,
            &self.physics,
        )
    }
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
        let (pmmp_states, kind_defaults) = read_pmmp_states(root)?;
        let (states, pmmp_default) = upgrade(&pmmp_states, &added, &network, &reference_names)?;

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
                ticks_randomly: values.ticks_randomly,
                full_faces: physics.full_faces(),
                connect: values.connect,
                physics,
                identity: values.identity,
                item: StateItem::None,
                drops: values.drops,
            });
        }
        let mut pmmp_state_index = BTreeMap::new();
        for (pmmp, default) in pmmp_default {
            let index = *state_index
                .get(&default)
                .ok_or("default state not listed")?;
            pmmp_state_index.insert(pmmp, index);
        }
        let aliases = read_aliases(root, &network, &state_index, &pmmp_state_index)?;
        let mut kinds = Vec::with_capacity(kind_defaults.len());
        for (kind, (key, identity)) in kind_defaults {
            let index = *pmmp_state_index
                .get(&key)
                .ok_or_else(|| format!("default state of {kind} is not a PMMP state"))?;
            kinds.push((identity, index));
        }
        let recipe_meta = pmmp_states
            .into_iter()
            .map(|(key, (_, meta))| (key, meta))
            .collect();

        let mut data = Self {
            states: list,
            aliases,
            items: Vec::new(),
            kinds,
            tables: read_json(root, PMMP_DIR, "tables.json")?,
            pmmp_state_index,
            recipe_meta,
            item_index: BTreeMap::new(),
            runtime_ids: BTreeMap::new(),
        };
        data.load_items(root, &network)?;
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
                vanilla: item.vanilla,
                data: item.data.clone(),
                fuel_residue: None,
                consume_residue: None,
                places: (
                    self.placed(item.data.places.as_ref())?,
                    self.placed(item.data.places_on_wall.as_ref())?,
                ),
            });
        }
        for i in 0..self.items.len() {
            let item = self.items.get(i).ok_or("item index")?;
            let data = &item.data;
            let fuel = data.fuel.residue.as_ref();
            let consume = data.consumable.as_ref().and_then(|c| c.residue.as_ref());
            let fuel = fuel.map(|j| self.item_type(j)).transpose()?;
            let consume = consume.map(|j| self.item_type(j)).transpose()?;
            for v in [data.tool.efficiency]
                .into_iter()
                .chain(data.food.as_ref().map(|f| f.saturation))
                .chain(data.throw_force)
            {
                exact(v)?;
            }
            if let Some(item) = self.items.get_mut(i) {
                item.fuel_residue = fuel;
                item.consume_residue = consume;
            }
        }
        self.use_vanilla_item_blocks(root, network)
    }

    /// The Anchor state of a dumped block an item places.
    fn placed(&self, json: Option<&Json>) -> R<Option<u16>> {
        json.map(|j| self.pmmp_state(&bedrock_state(j)?))
            .transpose()
    }

    /// Sets the block network ID block items are sent with: the vanilla creative state of
    /// `item_blocks.json`, which BDS sends, where the item name has a single PMMP block item type
    /// and a single vanilla state; PMMP's own default state otherwise. The vanilla
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
        let item = self.type_of(&name, meta, block, nbt.as_ref())?;
        Ok(Stack {
            item,
            meta,
            count,
            nbt,
        })
    }

    /// The item type of an item in Bedrock form (`name`, `meta`, optional `block` and `nbt`), by
    /// the rules of [`Data::stack`].
    pub(crate) fn item_type(&self, json: &Json) -> R<u16> {
        let (name, meta, block, nbt) = self.item_form(json)?;
        self.type_of(&name, meta, block, nbt.as_ref())
    }

    /// The lookup of [`Data::stack`].
    fn type_of(
        &self,
        name: &str,
        meta: u16,
        block: Option<u16>,
        nbt: Option<&BTreeMap<String, Tag>>,
    ) -> R<u16> {
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
/// (P6). Every item read from the dumps passes here, so types and stacks agree.
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
    let path = root.join(UPGRADE_PATH);
    let upgrade: Upgrade =
        serde_json::from_slice(&read(&path)?).map_err(|e| format!("{UPGRADE_PATH}: {e}"))?;
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

/// Reads how PMMP reads the reference states (`deserialized.json`, which lists the states PMMP
/// does not read as themselves) and gives the aliases: the reference states that are not Anchor
/// states but that PMMP reads as one of its states, with that state's Anchor state. Fails if a
/// listed state is not a reference state or is listed twice, if PMMP reads a state as itself
/// that is not an Anchor state, if PMMP reads an Anchor state as another state, or if PMMP reads
/// a state as one that has no Anchor state.
fn read_aliases(
    root: &Path,
    network: &BTreeMap<StateKey, u32>,
    state_index: &BTreeMap<StateKey, u16>,
    pmmp_state_index: &BTreeMap<StateKey, u16>,
) -> R<Vec<Alias>> {
    let list: Vec<Deserialized> = read_json(root, PMMP_DIR, "deserialized.json")?;
    aliases(list, network, state_index, pmmp_state_index)
}

/// The aliases of [`read_aliases`] from the read `deserialized.json`.
fn aliases(
    list: Vec<Deserialized>,
    network: &BTreeMap<StateKey, u32>,
    state_index: &BTreeMap<StateKey, u16>,
    pmmp_state_index: &BTreeMap<StateKey, u16>,
) -> R<Vec<Alias>> {
    let mut read: BTreeMap<StateKey, Option<StateKey>> = BTreeMap::new();
    for d in list {
        let key = state_key(&d.name, &d.states)?;
        if !network.contains_key(&key) {
            return Err(format!(
                "deserialized.json: {} {:?} is not a reference state",
                key.name, key.properties
            ));
        }
        let pmmp = d.pmmp.as_ref().map(bedrock_state).transpose()?;
        if read.insert(key, pmmp).is_some() {
            return Err(format!("deserialized.json: {} is listed twice", d.name));
        }
    }
    let mut aliases = Vec::new();
    for (key, &network_id) in network {
        match read.get(key) {
            None if !state_index.contains_key(key) => {
                return Err(format!(
                    "PMMP reads the reference state {} {:?} as itself, but it is not an Anchor \
                     state",
                    key.name, key.properties
                ));
            }
            // Read as itself, or rejected: its own Anchor state if it has one, else unknown.
            None | Some(None) => {}
            Some(Some(_)) if state_index.contains_key(key) => {
                return Err(format!(
                    "PMMP reads the Anchor state {} {:?} as another state",
                    key.name, key.properties
                ));
            }
            Some(Some(pmmp)) => {
                let state = *pmmp_state_index.get(pmmp).ok_or_else(|| {
                    format!(
                        "PMMP reads the reference state {} {:?} as {} {:?}, which has no Anchor \
                         state",
                        key.name, key.properties, pmmp.name, pmmp.properties
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

/// The PMMP states by their 1.26.30 Bedrock form, merged, and the default state and identity of
/// each PMMP block type by name.
type PmmpStates = (
    BTreeMap<StateKey, (Values, i32)>,
    BTreeMap<String, (StateKey, Identity)>,
);

/// Reads the PMMP states by their 1.26.30 Bedrock form and merges overlapping ones (see
/// [`merge`]); also the default state and identity of each PMMP block type, which all its
/// states must share.
fn read_pmmp_states(root: &Path) -> R<PmmpStates> {
    let pmmp: Vec<PmmpBlock> = read_json(root, PMMP_DIR, "blocks.json")?;
    let mut groups: BTreeMap<StateKey, Vec<(Values, i32)>> = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    let mut kinds: BTreeMap<String, Identity> = BTreeMap::new();
    for b in pmmp {
        let key = bedrock_state(&b.bedrock)?;
        let boxes = valid_boxes(&key, &b.classes, b.boxes)?;
        let physics = Physics::new(
            b.break_info,
            b.fire,
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
            ticks_randomly: b.ticks_randomly,
            connect: b.connect,
            physics,
            identity,
            item: b.item.map_or(ItemSource::None, ItemSource::Form),
            drops: b.drops,
        };
        groups.entry(key).or_default().push((values, b.recipe_meta));
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

/// PMMP classes whose collision box is inside out, with min above max (PMMP defect): a wall
/// hanging sign squashes its cross bar by 3/4 from each side where 3/8 was meant. The 3/8 bar is
/// BDS's box (`CloudburstMC` `blocks.json` `collisionShape`, P6 `xtask/data/p6/facts.md`), so
/// the box is rebuilt with 3/8 on the squashed axes.
const INSIDE_OUT_BOXES: [&str; 1] = ["WallHangingSign"];

/// The collision boxes of a dumped state with inside-out ones rebuilt, which only
/// [`INSIDE_OUT_BOXES`] classes may have: on an axis where min is above max, PMMP trimmed
/// `min` (= 3/4) from each side, and the box keeps half of that trim, `min / 2 ..= 1 - min / 2`.
fn valid_boxes(key: &StateKey, classes: &[String], boxes: Vec<[f64; 6]>) -> R<Vec<[f64; 6]>> {
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

/// Merges the PMMP states of one Bedrock state. They must agree on every value and the recipe
/// meta, except where [`SHARED`] names the Bedrock state. There a group of several PMMP types
/// takes the values, classes and traits of the type `SHARED` names, and the types may differ in
/// their block flags (a potion cauldron has no entity collision); an item form or drops that
/// differ become block entity data. Adds the names it needed `SHARED` for to `used`.
fn merge(
    key: &StateKey,
    group: &[(Values, i32)],
    used: &mut BTreeSet<&'static str>,
) -> R<(Values, i32)> {
    let shared = SHARED.iter().find(|(name, ..)| *name == key.name);
    let disagree = |what: &str| {
        format!(
            "PMMP states of {} {:?} disagree on their {what}{}",
            key.name,
            key.properties,
            if shared.is_some() {
                ""
            } else {
                " (if a block entity tells them apart, add the name to SHARED)"
            }
        )
    };
    let (first, meta) = group.first().ok_or("empty state group")?;
    // The state that speaks for the group: one of the type SHARED names, if the group has one.
    let chosen = shared
        .and_then(|(_, kind, _)| group.iter().find(|(v, _)| v.identity.kind == *kind))
        .map_or(first, |(v, _)| v);
    let mut merged = chosen.clone();
    let mut needed = false;
    for (values, m) in group {
        if m != meta {
            return Err(disagree("recipe meta"));
        }
        if values.identity.kind == chosen.identity.kind {
            if values.strict() != chosen.strict() || values.identity != chosen.identity {
                return Err(disagree("values"));
            }
        } else {
            let mut flagless = values.clone();
            flagless.physics.flags = chosen.physics.flags;
            if shared.is_none() || flagless.strict() != chosen.strict() {
                return Err(disagree("PMMP type or values"));
            }
            needed = true;
        }
        if values.item != chosen.item || values.drops != chosen.drops {
            if shared.is_none() {
                return Err(disagree("item form or drops"));
            }
            needed = true;
            merged.item = ItemSource::BlockEntity;
            merged.drops = Json::String(BLOCK_ENTITY_DROPS.to_owned());
        }
    }
    if let Some((name, kind, _)) = shared
        && needed
    {
        if merged.identity.kind != *kind {
            return Err(format!("{} has no PMMP state of type {kind}", key.name));
        }
        used.insert(*name);
    }
    Ok((merged, *meta))
}

/// Upgrades the PMMP states to 1.26.50: adds the new properties' defaults, then takes every value
/// the reference list has for them. Every state must end up in the reference list, and an upgrade
/// entry for a block PMMP lacks must still name a reference block (the schema covers all of
/// 1.26.50, PMMP part of it).
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
    if let Some(name) = added
        .keys()
        .find(|name| !used.contains(*name) && !reference_names.contains_key(*name))
    {
        return Err(format!("{UPGRADE_PATH}: {name} is not a 1.26.50 block"));
    }
    Ok((states, defaults))
}

/// The physical values of an upgraded state. A fence's, pane's or stair's support and collision
/// boxes come from the dumped variant whose client properties the state has: PMMP works the
/// connections and stair shape out from the neighbours, so the dumped state (unconnected,
/// straight) does not have them.
fn variant_physics(key: &StateKey, values: &Values) -> R<Physics> {
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

#[cfg(test)]
mod tests {
    use super::super::info::camel;
    use super::*;

    /// A PMMP state's values: `kind`, its light emission, flags and item name.
    fn values(kind: &str, light: u8, flags: &[&str], item: &str) -> (Values, i32) {
        let flags: Vec<String> = flags.iter().map(|f| (*f).to_owned()).collect();
        let physics = Physics::new(
            Break {
                hardness: 1.0,
                blast_resistance: 5.0,
                tool_type: 0,
                harvest_level: 0,
                explosion_harvestable: false,
            },
            (0, 0, false),
            &flags,
            0.6,
            &[],
            &["full"; 6].map(str::to_owned),
            vec![[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]],
        )
        .unwrap();
        let form = serde_json::json!({ "name": item, "meta": 0 });
        let values = Values {
            light_emission: light,
            light_filter: 15,
            blocks_direct_sky_light: true,
            ticks_randomly: false,
            connect: None,
            physics,
            identity: Identity {
                kind: kind.to_owned(),
                classes: vec![camel(kind), "Block".to_owned()],
                traits: Vec::new(),
            },
            item: ItemSource::Form(form.clone()),
            drops: serde_json::json!({ "hand": { "items": [form] } }),
        };
        (values, 0)
    }

    fn key(name: &str) -> StateKey {
        StateKey {
            name: name.to_owned(),
            properties: Vec::new(),
        }
    }

    /// The value check of overlapping PMMP states: any difference fails
    /// generation unless `SHARED` names the Bedrock state, and there only the PMMP type, block
    /// flags, item form and drops may differ.
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
            (values("stone", 0, &[], "a").0, 1),
        ] {
            let group = vec![values("stone", 0, &[], "a"), other];
            let err = merge(&stone, &group, &mut used).unwrap_err();
            assert!(err.contains("disagree"), "{err}");
        }
        assert!(used.is_empty());

        // A bed's colour: the item forms differ, so the item and drops come from the block entity.
        let bed = key("minecraft:bed");
        let group = vec![values("bed", 0, &[], "red"), values("bed", 0, &[], "blue")];
        let (merged, _) = merge(&bed, &group, &mut used).unwrap();
        assert_eq!(merged.item, ItemSource::BlockEntity);
        assert_eq!(merged.drops, Json::String(BLOCK_ENTITY_DROPS.to_owned()));
        assert!(used.contains("minecraft:bed"));
        // A cauldron takes the water cauldron's type and flags whichever state comes first; the
        // potion cauldron may differ in flags but not in other values.
        let cauldron = key("minecraft:cauldron");
        let water = values("water_cauldron", 0, &["entity_collision"], "c");
        let group = vec![values("potion_cauldron", 0, &[], "c"), water.clone()];
        let (merged, _) = merge(&cauldron, &group, &mut used).unwrap();
        assert_eq!(merged.identity.kind, "water_cauldron");
        assert_eq!(merged.physics, water.0.physics);
        assert_eq!(merged.item, water.0.item);
        let group = vec![values("potion_cauldron", 3, &[], "c"), water];
        assert!(merge(&cauldron, &group, &mut used).is_err());
        let group = vec![
            values("potion_cauldron", 0, &[], "c"),
            values("lava_cauldron", 0, &[], "c"),
        ];
        assert!(merge(&cauldron, &group, &mut used).is_err());
    }

    /// A wall hanging sign's inside-out bar becomes BDS's bar (`CloudburstMC` `blocks.json`
    /// `collisionShape`, taken from BDS: facing north or south `[0, 0.875, 0.375, 1, 1, 0.625]`,
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

    /// A reference state PMMP reads as another becomes an alias of that state's Anchor state;
    /// gen fails where PMMP reads a state as one with no Anchor state, reads an Anchor state as
    /// another, or reads a state that is not an Anchor state as itself.
    #[test]
    fn aliases_need_an_anchor_state_for_what_pmmp_reads() {
        let facing = |f: &str| StateKey {
            name: "minecraft:pumpkin".to_owned(),
            properties: vec![(
                "minecraft:cardinal_direction".to_owned(),
                Prop::String(f.to_owned()),
            )],
        };
        let entry = |f: &str, pmmp: Option<&str>| Deserialized {
            name: "minecraft:pumpkin".to_owned(),
            states: serde_json::json!({ "minecraft:cardinal_direction": { "string": f } }),
            pmmp: pmmp.map(|p| {
                serde_json::json!({
                    "name": "minecraft:pumpkin",
                    "states": { "minecraft:cardinal_direction": { "string": p } },
                })
            }),
        };
        let network = BTreeMap::from([(facing("east"), 1), (facing("south"), 2)]);
        let anchor = BTreeMap::from([(facing("south"), 7)]);
        let pmmp = BTreeMap::from([(facing("south"), 7)]);

        let aliases =
            aliases(vec![entry("east", Some("south"))], &network, &anchor, &pmmp).unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(
            (&aliases[0].key, aliases[0].network_id, aliases[0].state),
            (&facing("east"), 1, 7)
        );
        // Rejected: not an alias, and no Anchor state either (an unknown block).
        assert!(
            super::aliases(vec![entry("east", None)], &network, &anchor, &pmmp)
                .unwrap()
                .is_empty()
        );
        // PMMP's state has no Anchor state.
        let none = BTreeMap::new();
        assert!(
            super::aliases(vec![entry("east", Some("south"))], &network, &anchor, &none).is_err()
        );
        // An Anchor state read as another.
        let both = BTreeMap::from([(facing("east"), 6), (facing("south"), 7)]);
        assert!(
            super::aliases(vec![entry("east", Some("south"))], &network, &both, &pmmp).is_err()
        );
        // Read as itself (not listed) but not an Anchor state.
        assert!(super::aliases(Vec::new(), &network, &anchor, &pmmp).is_err());
        // Not a reference state.
        assert!(super::aliases(vec![entry("up", None)], &network, &anchor, &pmmp).is_err());
    }
}
