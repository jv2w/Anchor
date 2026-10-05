//! Block entities: containers, furnaces, brewing stands, campfires, signs, beds, banners and the
//! other blocks with data beyond their state (PLAN.md 10.7, PMMP `tile`), by position, saved with
//! their chunk (chunk extras kind 1, in Bedrock form: `id`, `x`, `y`, `z`, `Items` with `Slot`,
//! `CustomName`, `pairx`/`pairz`…).
//!
//! The store keeps a typed [`BlockEntity`] per position of every loaded chunk; a saved entry of an
//! id Anchor does not type is kept as it came ([`Kind::Other`]). A kind with slots keeps them in
//! [`BlockEntity::items`], which the inventory windows read and write; the keys of a kind that
//! Anchor does not read stay in the entity and are written back unchanged.
//!
//! Furnaces, brewing stands, campfires and beacons have work in the tick: the store keeps the
//! positions that may have some ([`BlockEntities::active`]), and [`tick`] runs only those. A
//! furnace or brewing stand leaves the set when it has nothing to do (PMMP `onUpdate` returning
//! false) and comes back when its slots change (PMMP schedules an update on every inventory
//! change); campfires stay while they hold items, beacons while they are loaded.
//!
//! Items that cannot be dropped when a container breaks (the world's entity limit) wait in the
//! chunk as spills ([`Spill`], saved with the chunk) and drop on later ticks: nothing is lost.
//!
//! Events (PLAN.md 10.8) sit between the steps of a tick: `FurnaceBurn` before a fuel burns,
//! `FurnaceSmelt` before an item smelts, `BrewingFuelUse` and `BrewItem` (one per bottle),
//! `CampfireCook` before a cooked item drops, `ChestPair` before two chests pair. After a
//! listener the step is checked again on the block as the listener left it.

mod api;
mod beacon;
mod brewing;
mod campfire;
mod decor;
mod furnace;

use std::collections::{BTreeMap, BTreeSet, btree_map};

use tracing::{error, warn};

use super::view::Key;
use super::{SessionId, State, dispatch, entity, interact, inventory, survival};
use crate::block::{self, Class, Face, Facing, PropertyValue};
use crate::crafting::FurnaceType;
use crate::custom::Registry;
use crate::event::{BrewItem, BrewingFuelUse, CampfireCook, ChestPair, FurnaceBurn, FurnaceSmelt};
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::packet::{BlockActorData, BlockEvent, Packet};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{BlockPos, ChunkPos, World};

pub(super) use beacon::{allowed as beacon_effect_allowed, is_payment as beacon_payment};
pub(super) use campfire::{cooks as campfire_cooks, kind_of as campfire_kind};
pub(super) use decor::{BLACK_TEXT, FRAME_ROTATIONS, book_pages};
pub(super) use furnace::OUTPUT as FURNACE_OUTPUT;

/// The most spills one chunk keeps; beyond it new spills are lost with an error (a chunk would
/// need this many containers broken while its world is at the entity limit).
const MAX_SPILLS: usize = 1024;
/// The most spilled stacks dropped per tick, over every world.
const SPILL_DROPS_PER_TICK: usize = 64;
/// The id of a spill entry in the saved list (Anchor's own; Bedrock has no such entity).
const SPILL_ID: &str = "AnchorSpill";
/// `BlockEvent` type of a chest lid (gophertunnel `BlockEventChangeChestState`).
const BLOCK_EVENT_CHEST: i32 = 1;
/// Slots of a chiseled bookshelf (PMMP `ChiseledBookshelfSlot`).
pub(super) const BOOKSHELF_SLOTS: usize = 6;
/// The random stream purpose of block entity ticks (smelting experience).
const TICK: u64 = random::tag("block-entity-tick");

/// The `ContainerSetData` values a furnace or brewing stand tick changed (key and value), kept
/// without allocating: each has three values a tick can change.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct DataChanges {
    values: [(i32, i32); 3],
    len: u8,
}

impl DataChanges {
    /// Adds a changed value; each key is added at most once a tick.
    fn push(&mut self, key: i32, value: i32) {
        if let Some(slot) = self.values.get_mut(usize::from(self.len)) {
            *slot = (key, value);
            self.len += 1;
        }
    }

    /// The changed values in the order they were added.
    pub(super) fn as_slice(&self) -> &[(i32, i32)] {
        self.values.get(..usize::from(self.len)).unwrap_or_default()
    }
}

/// The block entities of the loaded chunks.
#[derive(Debug, Default)]
pub(super) struct BlockEntities {
    /// The block entities of each loaded chunk.
    chunks: BTreeMap<Key, Chunk>,
    /// Chunks whose block entities changed since [`Self::take_changed`].
    changed: BTreeSet<Key>,
    /// The sessions with a window open on each block (both halves of a double chest).
    viewers: BTreeMap<(WorldIndex, BlockPos), Vec<SessionId>>,
    /// The loaded chunks with spills waiting, so a tick looks only at them.
    spilling: BTreeSet<Key>,
    /// The block entities that may have work in the tick (at most one per loaded block entity).
    active: BTreeSet<(WorldIndex, BlockPos)>,
}

/// One chunk's block entities.
#[derive(Debug, Default, Clone, PartialEq)]
struct Chunk {
    entities: BTreeMap<BlockPos, BlockEntity>,
    /// Container contents waiting for room in the world to drop.
    spills: Vec<Spill>,
    /// Saved entries without a valid position in this chunk, kept as they came.
    raw: Vec<Compound>,
}

/// Items of a broken container that could not be dropped yet.
#[derive(Debug, Clone, PartialEq)]
struct Spill {
    pos: BlockPos,
    items: Vec<Stack>,
}

/// A block entity.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BlockEntity {
    pub(super) kind: Kind,
    /// The slots (27 for a chest, 3 for a furnace…), empty for kinds without slots.
    pub(super) items: Vec<Stack>,
    /// `CustomName`.
    pub(super) name: Option<String>,
    /// The other saved keys (`Lock`, `isMovable`…), written back unchanged.
    rest: Compound,
}

/// What a block entity is, with the data of its kind.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    /// A chest or trapped chest, and the chest it pairs with into a double chest.
    Chest {
        /// The other half of a double chest.
        pair: Option<BlockPos>,
    },
    /// A barrel.
    Barrel,
    /// A shulker box and the face it opens toward (Bedrock `facing`, 0 down … 5 east).
    ShulkerBox {
        /// The `facing` byte.
        facing: u8,
    },
    /// A hopper (PMMP moves no items with it).
    Hopper,
    /// An ender chest: its contents are each player's own.
    EnderChest,
    /// A furnace, blast furnace or smoker.
    Furnace(furnace::Furnace),
    /// A brewing stand.
    BrewingStand(brewing::Brewing),
    /// A campfire or soul campfire.
    Campfire(campfire::Campfire),
    /// An enchanting table (only its name).
    EnchantTable,
    /// A beacon and its chosen effects (effect IDs, 0 for none).
    Beacon {
        /// The primary effect.
        primary: i32,
        /// The secondary effect.
        secondary: i32,
    },
    /// A sign or hanging sign.
    Sign(Box<decor::Sign>),
    /// A bed and its colour (PMMP `DyeColorIdMap` ID, the bed item's meta).
    Bed {
        /// The colour.
        colour: u8,
    },
    /// A banner.
    Banner(decor::Banner),
    /// A flower pot and its plant.
    FlowerPot {
        /// The plant, if any.
        plant: Option<block::State>,
    },
    /// An item frame or glow item frame.
    ItemFrame(Box<decor::Frame>),
    /// A jukebox and its record (state only: the music is the client's).
    Jukebox {
        /// The record, or empty.
        record: Stack,
    },
    /// A lectern, its book and the page open.
    Lectern {
        /// The book, or empty.
        book: Stack,
        /// The page shown.
        page: i32,
    },
    /// A chiseled bookshelf (its books are the slots).
    ChiseledBookshelf {
        /// The slot last used, plus one; 0 for none (PMMP `LastInteractedSlot`).
        last: i32,
    },
    /// A mob head: its PMMP `MobHeadTypeIdMap` type and its rotation on the floor (sixteenths).
    Skull {
        /// The `SkullType` byte.
        skull: u8,
        /// The rotation, 0 to 15.
        rotation: u8,
    },
    /// A cauldron's potion and dyed water colour.
    Cauldron(decor::Cauldron),
    /// A kind no module types: the saved compound without `x`, `y`, `z`, kept as it came.
    Other(Compound),
}

/// The furnace type of a furnace-like block, by its name.
fn furnace_type(state: block::State) -> FurnaceType {
    let name = state.name();
    if name.ends_with("blast_furnace") {
        FurnaceType::BlastFurnace
    } else if name.ends_with("smoker") {
        FurnaceType::Smoker
    } else {
        FurnaceType::Furnace
    }
}

/// The PMMP `MobHeadTypeIdMap` ID of a mob head block, by its name.
fn skull_type(state: block::State) -> u8 {
    match state.name() {
        "minecraft:wither_skeleton_skull" => 1,
        "minecraft:zombie_head" => 2,
        "minecraft:player_head" => 3,
        "minecraft:creeper_head" => 4,
        "minecraft:dragon_head" => 5,
        "minecraft:piglin_head" => 6,
        _ => 0,
    }
}

/// Whether a sign block hangs (PMMP `HangingSign` tile).
fn is_hanging_sign(state: block::State) -> bool {
    state.is(Class::CeilingCenterHangingSign)
        || state.is(Class::CeilingEdgesHangingSign)
        || state.is(Class::WallHangingSign)
}

impl Kind {
    /// The kind a block state gets a block entity of, with its slot count; `None` for a block
    /// without one (PMMP: the tile class each block type registers).
    fn of(state: block::State) -> Option<(Self, usize)> {
        let classes = state.kind().classes();
        let is = |c: Class| classes.contains(&c);
        Some(if is(Class::Chest) {
            (Self::Chest { pair: None }, 27)
        } else if is(Class::Barrel) {
            (Self::Barrel, 27)
        } else if is(Class::ShulkerBox) {
            // Bedrock's state has no facing; `placed` takes the clicked face.
            (Self::ShulkerBox { facing: 1 }, 27)
        } else if is(Class::Hopper) {
            (Self::Hopper, 5)
        } else if is(Class::EnderChest) {
            (Self::EnderChest, 0)
        } else if is(Class::Furnace) {
            let f = furnace::Furnace::new(furnace_type(state));
            (Self::Furnace(f), furnace::SLOTS)
        } else if is(Class::BrewingStand) {
            (
                Self::BrewingStand(brewing::Brewing::default()),
                brewing::SLOTS,
            )
        } else if is(Class::Campfire) {
            (
                Self::Campfire(campfire::Campfire::default()),
                campfire::SLOTS,
            )
        } else if is(Class::EnchantingTable) {
            (Self::EnchantTable, 0)
        } else if is(Class::Beacon) {
            (
                Self::Beacon {
                    primary: 0,
                    secondary: 0,
                },
                0,
            )
        } else if is(Class::BaseSign) {
            let sign = decor::Sign {
                hanging: is_hanging_sign(state),
                ..decor::Sign::default()
            };
            (Self::Sign(Box::new(sign)), 0)
        } else if is(Class::Bed) {
            (Self::Bed { colour: 14 }, 0)
        } else if is(Class::BaseBanner) {
            (Self::Banner(decor::Banner::default()), 0)
        } else if is(Class::FlowerPot) {
            (Self::FlowerPot { plant: None }, 0)
        } else if is(Class::ItemFrame) {
            let frame = decor::Frame {
                item: Stack::EMPTY,
                rotation: 0,
                drop_chance: 1.0,
                glow: state.name() == "minecraft:glow_frame",
            };
            (Self::ItemFrame(Box::new(frame)), 0)
        } else if is(Class::Jukebox) {
            (
                Self::Jukebox {
                    record: Stack::EMPTY,
                },
                0,
            )
        } else if is(Class::Lectern) {
            (
                Self::Lectern {
                    book: Stack::EMPTY,
                    page: 0,
                },
                0,
            )
        } else if is(Class::ChiseledBookshelf) {
            (Self::ChiseledBookshelf { last: 0 }, BOOKSHELF_SLOTS)
        } else if is(Class::MobHead) {
            (
                Self::Skull {
                    skull: skull_type(state),
                    rotation: 0,
                },
                0,
            )
        } else if is(Class::Cauldron) || is(Class::FillableCauldron) {
            (Self::Cauldron(decor::Cauldron::default()), 0)
        } else {
            return None;
        })
    }

    /// The Bedrock block entity id.
    fn id(&self) -> &str {
        match self {
            Self::Chest { .. } => "Chest",
            Self::Barrel => "Barrel",
            Self::ShulkerBox { .. } => "ShulkerBox",
            Self::Hopper => "Hopper",
            Self::EnderChest => "EnderChest",
            Self::Furnace(f) => match f.kind {
                FurnaceType::BlastFurnace => "BlastFurnace",
                FurnaceType::Smoker => "Smoker",
                _ => "Furnace",
            },
            Self::BrewingStand(_) => "BrewingStand",
            Self::Campfire(_) => "Campfire",
            Self::EnchantTable => "EnchantTable",
            Self::Beacon { .. } => "Beacon",
            Self::Sign(s) if s.hanging => "HangingSign",
            Self::Sign(_) => "Sign",
            Self::Bed { .. } => "Bed",
            Self::Banner(_) => "Banner",
            Self::FlowerPot { .. } => "FlowerPot",
            Self::ItemFrame(f) if f.glow => "GlowItemFrame",
            Self::ItemFrame(_) => "ItemFrame",
            Self::Jukebox { .. } => "Jukebox",
            Self::Lectern { .. } => "Lectern",
            Self::ChiseledBookshelf { .. } => "ChiseledBookshelf",
            Self::Skull { .. } => "Skull",
            Self::Cauldron(_) => "Cauldron",
            Self::Other(nbt) => match nbt.get("id") {
                Some(Value::String(id)) => id,
                _ => "",
            },
        }
    }

    /// Whether the kind's slots are saved as a `Slot`-numbered `Items` list.
    fn has_item_list(&self) -> bool {
        matches!(
            self,
            Self::Chest { .. }
                | Self::Barrel
                | Self::ShulkerBox { .. }
                | Self::Hopper
                | Self::Furnace(_)
                | Self::BrewingStand(_)
        )
    }

    /// Whether the tick may have work for it.
    fn ticks(&self, items: &[Stack]) -> bool {
        match self {
            Self::Furnace(_) | Self::BrewingStand(_) | Self::Beacon { .. } => true,
            Self::Campfire(_) => items.iter().any(|s| !s.is_empty()),
            _ => false,
        }
    }
}

impl BlockEntity {
    /// A new block entity of `kind` with `slots` empty slots.
    fn new(kind: Kind, slots: usize) -> Self {
        Self {
            kind,
            items: vec![Stack::EMPTY; slots],
            name: None,
            rest: Compound::new(),
        }
    }

    /// The other half of a double chest.
    pub(super) fn pair(&self) -> Option<BlockPos> {
        match self.kind {
            Kind::Chest { pair } => pair,
            _ => None,
        }
    }

    /// The `Lock` key a held item's custom name must match to open the container.
    pub(super) fn lock(&self) -> Option<&str> {
        match self.rest.get("Lock") {
            Some(Value::String(lock)) => Some(lock),
            _ => None,
        }
    }

    /// Reads a saved compound at `pos` (its `x`, `y`, `z`, already checked).
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one arm per Bedrock block entity id, side by side with PMMP's tiles"
    )]
    fn read(mut nbt: Compound, pos: BlockPos, custom: &Registry) -> Self {
        for key in ["x", "y", "z"] {
            nbt.remove(key);
        }
        let Some(Value::String(id)) = nbt.get("id").cloned() else {
            return Self::new(Kind::Other(nbt), 0);
        };
        let mut items = Vec::new();
        let kind = match id.as_str() {
            "Chest" => Kind::Chest { pair: None },
            "Barrel" => Kind::Barrel,
            "ShulkerBox" => Kind::ShulkerBox { facing: 1 },
            "Hopper" => Kind::Hopper,
            "EnderChest" => Kind::EnderChest,
            "Furnace" | "BlastFurnace" | "Smoker" => {
                let kind = match id.as_str() {
                    "BlastFurnace" => FurnaceType::BlastFurnace,
                    "Smoker" => FurnaceType::Smoker,
                    _ => FurnaceType::Furnace,
                };
                Kind::Furnace(furnace::Furnace::read(&mut nbt, kind))
            }
            "BrewingStand" => Kind::BrewingStand(brewing::Brewing::read(&mut nbt)),
            "Campfire" => {
                items = vec![Stack::EMPTY; campfire::SLOTS];
                Kind::Campfire(campfire::Campfire::read(&mut nbt, &mut items, custom))
            }
            "EnchantTable" => Kind::EnchantTable,
            "Beacon" => {
                let (primary, secondary) = decor::read_beacon(&mut nbt);
                Kind::Beacon { primary, secondary }
            }
            "Sign" | "HangingSign" => {
                Kind::Sign(Box::new(decor::Sign::read(&mut nbt, id == "HangingSign")))
            }
            "Bed" => Kind::Bed {
                colour: decor::read_bed(&mut nbt),
            },
            "Banner" => Kind::Banner(decor::Banner::read(&mut nbt)),
            "FlowerPot" => Kind::FlowerPot {
                plant: decor::read_plant(&mut nbt),
            },
            "ItemFrame" | "GlowItemFrame" => Kind::ItemFrame(Box::new(decor::Frame::read(
                &mut nbt,
                id == "GlowItemFrame",
                custom,
            ))),
            "Jukebox" | "RecordPlayer" => Kind::Jukebox {
                record: decor::read_item(&mut nbt, "RecordItem", custom),
            },
            "Lectern" => {
                let (book, page) = decor::read_lectern(&mut nbt, custom);
                Kind::Lectern { book, page }
            }
            "ChiseledBookshelf" => {
                items = vec![Stack::EMPTY; BOOKSHELF_SLOTS];
                if let Some(Value::List(List::Compound(list))) = nbt.remove("Items") {
                    for (slot, entry) in items.iter_mut().zip(&list) {
                        *slot = decor::read_stack(entry, "bookshelf", custom).with_count(1);
                    }
                }
                let last = match nbt.remove("LastInteractedSlot") {
                    Some(Value::Int(l)) if (0..=6).contains(&l) => l,
                    _ => 0,
                };
                Kind::ChiseledBookshelf { last }
            }
            "Skull" => {
                let (skull, rotation) = decor::read_skull(&mut nbt);
                Kind::Skull { skull, rotation }
            }
            "Cauldron" => Kind::Cauldron(decor::Cauldron::read(&mut nbt)),
            _ => return Self::new(Kind::Other(nbt), 0),
        };
        nbt.remove("id");
        if kind.has_item_list() {
            let slots = match &kind {
                Kind::Hopper => 5,
                Kind::Furnace(_) => furnace::SLOTS,
                Kind::BrewingStand(_) => brewing::SLOTS,
                _ => 27,
            };
            items = vec![Stack::EMPTY; slots];
            if let Some(Value::List(List::Compound(list))) = nbt.remove("Items") {
                read_items(&mut items, &list, pos, custom);
            }
        }
        let mut entity = Self::new(kind, 0);
        entity.items = items;
        // A name over the limit (only an Anchor before the limit saved one) stays as saved and is
        // not shown, so every name is one a stack may carry ([`with_block_data`]).
        if let Some(Value::String(name)) = nbt.get("CustomName")
            && !name.is_empty()
            && name.len() <= crate::tile::MAX_NAME_BYTES
        {
            entity.name = Some(name.clone());
            nbt.remove("CustomName");
        }
        match &mut entity.kind {
            Kind::Chest { pair } => {
                // PMMP `Chest::readSaveData`: a pair must be a horizontal neighbour.
                if let (Some(Value::Int(x)), Some(Value::Int(z))) =
                    (nbt.get("pairx"), nbt.get("pairz"))
                {
                    let (x, z) = (*x, *z);
                    let next = (x == pos.x && z.abs_diff(pos.z) == 1)
                        || (z == pos.z && x.abs_diff(pos.x) == 1);
                    if next {
                        *pair = Some(BlockPos::new(x, pos.y, z));
                    }
                }
                nbt.remove("pairx");
                nbt.remove("pairz");
            }
            Kind::ShulkerBox { facing } => {
                if let Some(Value::Byte(f)) = nbt.remove("facing") {
                    *facing = f.min(5);
                }
            }
            _ => {}
        }
        entity.rest = nbt;
        entity
    }

    /// Takes the stacks a shulker box's slots do not take ([`Stack::fits_shulker_box`]) out of
    /// it: what a box item made by a plugin held, or an Anchor before the limit saved. None for
    /// other kinds.
    fn take_unfit(&mut self) -> Vec<Stack> {
        if !matches!(self.kind, Kind::ShulkerBox { .. }) {
            return Vec::new();
        }
        self.items
            .iter_mut()
            .filter(|s| !s.fits_shulker_box())
            .map(std::mem::take)
            .collect()
    }

    /// The keys of the kind's own data, which saves and clients share.
    fn write_kind(&self, nbt: &mut Compound, client: bool) {
        match &self.kind {
            Kind::Chest { pair: Some(p) } => {
                nbt.insert("pairx".to_owned(), Value::Int(p.x));
                nbt.insert("pairz".to_owned(), Value::Int(p.z));
            }
            Kind::ShulkerBox { facing } => {
                nbt.insert("facing".to_owned(), Value::Byte(*facing));
            }
            Kind::Furnace(f) if !client => f.write(nbt),
            Kind::BrewingStand(b) => b.write(nbt),
            Kind::Campfire(c) if client => campfire::Campfire::spawn(&self.items, nbt),
            Kind::Campfire(c) => c.write(&self.items, nbt),
            Kind::Beacon { primary, secondary } => decor::write_beacon(*primary, *secondary, nbt),
            Kind::Sign(s) if client => s.spawn(nbt),
            Kind::Sign(s) => s.write(nbt),
            Kind::Bed { colour } => {
                nbt.insert("color".to_owned(), Value::Byte(*colour));
            }
            Kind::Banner(b) => b.write(nbt),
            Kind::FlowerPot { plant } => decor::write_plant(*plant, nbt),
            Kind::ItemFrame(f) => f.write(nbt),
            Kind::Jukebox { record } => decor::write_item(record, "RecordItem", nbt),
            Kind::Lectern { book, page } => decor::write_lectern(book, *page, nbt),
            Kind::ChiseledBookshelf { last } if !client => {
                // PMMP `ChiseledBookshelf::saveItems`: every slot, an empty one as an air item.
                let list = self
                    .items
                    .iter()
                    .map(|s| {
                        let mut nbt = s.to_nbt();
                        if s.is_empty() {
                            nbt.insert("WasPickedUp".to_owned(), Value::Byte(0));
                        }
                        nbt
                    })
                    .collect();
                nbt.insert("Items".to_owned(), Value::List(List::Compound(list)));
                nbt.insert("LastInteractedSlot".to_owned(), Value::Int(*last));
            }
            Kind::Skull { skull, rotation } => decor::write_skull(*skull, *rotation, nbt),
            Kind::Cauldron(c) => c.write(nbt),
            _ => {}
        }
    }

    /// The saved compound at `pos`.
    fn write(&self, pos: BlockPos) -> Compound {
        let mut nbt = match &self.kind {
            Kind::Other(nbt) => nbt.clone(),
            kind => {
                let mut nbt = self.rest.clone();
                nbt.insert("id".to_owned(), Value::String(kind.id().to_owned()));
                if kind.has_item_list() {
                    nbt.insert("Items".to_owned(), write_items(&self.items));
                }
                self.write_kind(&mut nbt, false);
                nbt
            }
        };
        nbt.insert("x".to_owned(), Value::Int(pos.x));
        nbt.insert("y".to_owned(), Value::Int(pos.y));
        nbt.insert("z".to_owned(), Value::Int(pos.z));
        if let Some(name) = &self.name {
            nbt.insert("CustomName".to_owned(), Value::String(name.clone()));
        }
        nbt
    }

    /// What clients get of it (PMMP `Spawnable::getSpawnCompound`): id, position, the custom
    /// name and the kind's client keys (a chest's pair, a sign's text, a bed's colour…).
    pub(super) fn spawn_nbt(&self, pos: BlockPos) -> Compound {
        let mut nbt = Compound::from([
            ("id".to_owned(), Value::String(self.kind.id().to_owned())),
            ("x".to_owned(), Value::Int(pos.x)),
            ("y".to_owned(), Value::Int(pos.y)),
            ("z".to_owned(), Value::Int(pos.z)),
        ]);
        self.write_kind(&mut nbt, true);
        if let Some(name) = &self.name {
            nbt.insert("CustomName".to_owned(), Value::String(name.clone()));
        }
        nbt
    }

    /// Whether clients get it (PMMP `Spawnable` tiles; a chiseled bookshelf and unknown kinds are
    /// not).
    pub(super) fn is_spawnable(&self) -> bool {
        !matches!(
            self.kind,
            Kind::Other(_) | Kind::ChiseledBookshelf { .. } | Kind::EnderChest
        )
    }

    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv, pos: BlockPos) {
        let Self {
            kind: _,
            items: _,
            name: _,
            rest: _,
        } = self;
        // The saved form holds every field (the kinds' caches are derived).
        super::extra::hash_nbt(h, &self.write(pos));
    }
}

/// Reads an `Items` list into `slots` by `Slot` (custom items through `custom`); entries of
/// unknown items or slots are dropped with a warning (PMMP `Item::safeNbtDeserialize`).
fn read_items(slots: &mut [Stack], items: &[Compound], pos: BlockPos, custom: &Registry) {
    for entry in items {
        let slot = match entry.get("Slot") {
            Some(Value::Byte(s)) => usize::from(*s),
            _ => usize::MAX,
        };
        if let (Some(target), Some(stack)) =
            (slots.get_mut(slot), Stack::from_nbt_with(entry, custom))
        {
            *target = stack;
        } else {
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                slot,
                "a container item is unknown or in no slot; it is left out"
            );
        }
    }
}

/// An `Items` list of the non-empty slots, with `Slot`.
fn write_items(slots: &[Stack]) -> Value {
    let list = (0u8..)
        .zip(slots)
        .filter(|(_, s)| !s.is_empty())
        .map(|(slot, s)| {
            let mut nbt = s.to_nbt();
            nbt.insert("Slot".to_owned(), Value::Byte(slot));
            nbt
        })
        .collect();
    Value::List(List::Compound(list))
}

/// The position a saved entry names, if it is in the chunk of `key`.
fn position(nbt: &Compound, key: Key) -> Option<BlockPos> {
    let coordinate = |k: &str| match nbt.get(k) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let pos = BlockPos::new(coordinate("x")?, coordinate("y")?, coordinate("z")?);
    (pos.chunk() == key.1 && (crate::world::MIN_Y..=crate::world::MAX_Y).contains(&pos.y))
        .then_some(pos)
}

impl Chunk {
    fn read(key: Key, list: &[Compound], custom: &Registry) -> Self {
        let mut chunk = Self::default();
        for nbt in list {
            let Some(pos) = position(nbt, key) else {
                chunk.raw.push(nbt.clone());
                continue;
            };
            if nbt.get("id") == Some(&Value::String(SPILL_ID.to_owned())) {
                let mut items = vec![Stack::EMPTY; 54];
                if let Some(Value::List(List::Compound(list))) = nbt.get("Items") {
                    read_items(&mut items, list, pos, custom);
                }
                items.retain(|s| !s.is_empty());
                if !items.is_empty() {
                    chunk.spills.push(Spill { pos, items });
                }
            } else if let btree_map::Entry::Vacant(free) = chunk.entities.entry(pos) {
                free.insert(BlockEntity::read(nbt.clone(), pos, custom));
            } else {
                chunk.raw.push(nbt.clone());
            }
        }
        chunk
    }

    fn write(&self) -> Vec<Compound> {
        let mut list = Vec::with_capacity(self.entities.len() + self.spills.len() + self.raw.len());
        list.extend(self.entities.iter().map(|(pos, e)| e.write(*pos)));
        for spill in &self.spills {
            list.push(Compound::from([
                ("id".to_owned(), Value::String(SPILL_ID.to_owned())),
                ("x".to_owned(), Value::Int(spill.pos.x)),
                ("y".to_owned(), Value::Int(spill.pos.y)),
                ("z".to_owned(), Value::Int(spill.pos.z)),
                ("Items".to_owned(), write_items(&spill.items)),
            ]));
        }
        list.extend(self.raw.iter().cloned());
        list
    }
}

impl BlockEntities {
    /// A chunk's saved list arrived ([`mod@super::rules`]); `world` holds the chunk's blocks and
    /// `custom` is the server's custom content (saved custom items read back through it).
    ///
    /// A block entity whose block does not take it (an older Anchor, a world edited outside) is
    /// removed with a warning, as PMMP drops a tile without its block on load; what it held
    /// waits as a spill and drops in the next ticks (PMMP drops nothing: fixed, no item is
    /// lost). A furnace's stored experience goes with it. With the chunk's spills full it is kept
    /// as it is. A shulker box's stacks its slots do not take (an Anchor before the limit saved
    /// them) wait as a spill too, so the box with its items is always one stack.
    pub(super) fn load(&mut self, key: Key, list: &[Compound], world: &World, custom: &Registry) {
        let mut chunk = Chunk::read(key, list, custom);
        let stray: Vec<BlockPos> = chunk
            .entities
            .iter()
            .filter(|(pos, e)| world.block(**pos).is_some_and(|b| !fits(&e.kind, b)))
            .map(|(pos, _)| *pos)
            .collect();
        let mut removed = !stray.is_empty();
        for pos in stray {
            if chunk.spills.len() >= MAX_SPILLS {
                break;
            }
            let Some(mut entity) = chunk.entities.remove(&pos) else {
                continue;
            };
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                id = entity.kind.id(),
                "a saved block entity does not belong to its block; removed, its items dropped"
            );
            let mut items = Vec::new();
            take_contents(&mut entity, &mut items);
            items.extend(entity.items.into_iter().filter(|s| !s.is_empty()));
            if !items.is_empty() {
                chunk.spills.push(Spill { pos, items });
            }
        }
        for (pos, entity) in &mut chunk.entities {
            let unfit = entity.take_unfit();
            if !unfit.is_empty() {
                chunk.spills.push(Spill {
                    pos: *pos,
                    items: unfit,
                });
                removed = true;
            }
        }
        if chunk.spills.is_empty() {
            self.spilling.remove(&key);
        } else {
            self.spilling.insert(key);
        }
        for (pos, e) in &chunk.entities {
            if e.kind.ticks(&e.items) {
                self.active.insert((key.0, *pos));
            }
        }
        self.chunks.insert(key, chunk);
        if removed {
            self.changed.insert(key);
        } else {
            self.changed.remove(&key);
        }
    }

    /// A chunk leaves: drops its state; its list if it changed since [`Self::take_changed`].
    pub(super) fn unload(&mut self, key: Key) -> Option<Vec<Compound>> {
        self.spilling.remove(&key);
        let chunk = self.chunks.remove(&key)?;
        for pos in chunk.entities.keys() {
            self.active.remove(&(key.0, *pos));
        }
        self.changed.remove(&key).then(|| chunk.write())
    }

    /// The lists of the chunks that changed since the last call, for a save snapshot.
    pub(super) fn take_changed(&mut self) -> Vec<(Key, Vec<Compound>)> {
        let changed = std::mem::take(&mut self.changed);
        changed
            .into_iter()
            .filter_map(|key| Some((key, self.chunks.get(&key)?.write())))
            .collect()
    }

    /// The block entity at `pos`.
    pub(super) fn get(&self, world: WorldIndex, pos: BlockPos) -> Option<&BlockEntity> {
        self.chunks.get(&(world, pos.chunk()))?.entities.get(&pos)
    }

    /// The block entity at `pos` to change; its chunk is marked changed.
    pub(super) fn get_mut(&mut self, world: WorldIndex, pos: BlockPos) -> Option<&mut BlockEntity> {
        let key = (world, pos.chunk());
        let entity = self.chunks.get_mut(&key)?.entities.get_mut(&pos)?;
        self.changed.insert(key);
        Some(entity)
    }

    /// Changes the block entity at `pos` with `f`; its chunk is marked changed when `f` gives
    /// `Some` (`None`: `f` changed nothing).
    fn update<T>(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        f: impl FnOnce(&mut BlockEntity) -> Option<T>,
    ) -> Option<T> {
        let key = (world, pos.chunk());
        let out = f(self.chunks.get_mut(&key)?.entities.get_mut(&pos)?)?;
        self.changed.insert(key);
        Some(out)
    }

    /// The block entities of the loaded chunks of `world`.
    pub(super) fn count(&self, world: WorldIndex) -> usize {
        let chunks =
            (world, ChunkPos::new(i32::MIN, i32::MIN))..=(world, ChunkPos::new(i32::MAX, i32::MAX));
        self.chunks
            .range(chunks)
            .map(|(_, c)| c.entities.len())
            .sum()
    }

    /// The positions of the block entities of a loaded chunk, in order.
    fn positions(&self, world: WorldIndex, pos: ChunkPos) -> impl Iterator<Item = BlockPos> + '_ {
        self.chunks
            .get(&(world, pos))
            .into_iter()
            .flat_map(|c| c.entities.keys().copied())
    }

    /// The slots of the block entity at `pos` changed outside its tick (a window, an interaction):
    /// it may have work again (PMMP schedules an update on every inventory change).
    pub(super) fn wake(&mut self, world: WorldIndex, pos: BlockPos) {
        if self.get(world, pos).is_some_and(|e| e.kind.ticks(&e.items)) {
            self.active.insert((world, pos));
        }
    }

    /// Puts a block entity at `pos` of a loaded chunk; the one there before, if any.
    fn insert(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        entity: BlockEntity,
    ) -> Option<BlockEntity> {
        let key = (world, pos.chunk());
        let chunk = self.chunks.get_mut(&key)?;
        self.changed.insert(key);
        if entity.kind.ticks(&entity.items) {
            self.active.insert((world, pos));
        }
        chunk.entities.insert(pos, entity)
    }

    /// Removes the block entity at `pos`.
    fn remove(&mut self, world: WorldIndex, pos: BlockPos) -> Option<BlockEntity> {
        let key = (world, pos.chunk());
        let entity = self.chunks.get_mut(&key)?.entities.remove(&pos)?;
        self.changed.insert(key);
        self.active.remove(&(world, pos));
        Some(entity)
    }

    /// What clients get of the block entities of a loaded chunk.
    #[cfg(test)]
    pub(super) fn client_nbt(&self, key: Key) -> Vec<Compound> {
        self.chunks.get(&key).map_or_else(Vec::new, |c| {
            c.entities
                .iter()
                .filter(|(_, e)| e.is_spawnable())
                .map(|(pos, e)| e.spawn_nbt(*pos))
                .collect()
        })
    }

    /// The client form of the block entities in sub-chunk `index` (0 is the lowest) of a loaded
    /// chunk, as network little-endian NBT roots one after another: what a sub-chunk answer
    /// carries after the blocks (dragonfly `subChunkEntry`). Empty when there are none.
    pub(super) fn client_bytes(&self, world: WorldIndex, pos: ChunkPos, index: usize) -> Vec<u8> {
        let Some(chunk) = self.chunks.get(&(world, pos)) else {
            return Vec::new();
        };
        let mut w = crate::protocol::Writer::new(Vec::new(), 0);
        for (p, e) in &chunk.entities {
            let sub = usize::try_from((p.y - crate::world::MIN_Y) >> 4).unwrap_or(usize::MAX);
            if sub != index || !e.is_spawnable() {
                continue;
            }
            if w.nbt(&e.spawn_nbt(*p), crate::nbt::Encoding::NetworkLittleEndian)
                .is_err()
            {
                warn!(
                    x = p.x,
                    y = p.y,
                    z = p.z,
                    "a block entity is too large to send"
                );
            }
        }
        w.into_inner()
    }

    /// The `ContainerSetData` values a window on the block at `pos` opens with (BDS sends a
    /// furnace's keys 0 to 3 on open, P6 facts 3); empty for other blocks.
    pub(super) fn window_data(&self, world: WorldIndex, pos: BlockPos) -> Vec<(i32, i32)> {
        match self.get(world, pos).map(|e| &e.kind) {
            Some(Kind::Furnace(f)) => f.data().to_vec(),
            Some(Kind::BrewingStand(b)) => b.data().to_vec(),
            _ => Vec::new(),
        }
    }

    /// The sessions with a window open on the block at `pos`.
    pub(super) fn viewers(&self, world: WorldIndex, pos: BlockPos) -> &[SessionId] {
        self.viewers
            .get(&(world, pos))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// `session` opened a window on the block at `pos`; returns the number of viewers.
    pub(super) fn add_viewer(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        session: SessionId,
    ) -> usize {
        let list = self.viewers.entry((world, pos)).or_default();
        if !list.contains(&session) {
            list.push(session);
        }
        list.len()
    }

    /// `session` closed its window on the block at `pos`; returns the number left.
    pub(super) fn remove_viewer(
        &mut self,
        world: WorldIndex,
        pos: BlockPos,
        session: SessionId,
    ) -> usize {
        let Some(list) = self.viewers.get_mut(&(world, pos)) else {
            return 0;
        };
        list.retain(|s| *s != session);
        let left = list.len();
        if left == 0 {
            self.viewers.remove(&(world, pos));
        }
        left
    }

    /// Keeps items of a broken block to drop later, in the chunk of `pos`.
    pub(super) fn spill(&mut self, world: WorldIndex, pos: BlockPos, items: Vec<Stack>) {
        let key = (world, pos.chunk());
        let Some(chunk) = self.chunks.get_mut(&key) else {
            error!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "container items lost: the chunk is gone"
            );
            return;
        };
        if chunk.spills.len() >= MAX_SPILLS {
            error!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "container items lost: too many spills waiting"
            );
            return;
        }
        chunk.spills.push(Spill { pos, items });
        self.changed.insert(key);
        self.spilling.insert(key);
    }

    /// Every item the block entities and spills of the loaded chunks hold (for item counts).
    #[cfg(test)]
    pub(super) fn all_items(&self) -> impl Iterator<Item = &Stack> {
        self.chunks.values().flat_map(|c| {
            c.entities
                .values()
                .flat_map(|e| e.items.iter().chain(own_stacks(&e.kind)))
                .chain(c.spills.iter().flat_map(|s| s.items.iter()))
        })
    }

    /// The number of loaded chunks, viewer entries, waiting spills and active block entities
    /// (for leak tests).
    #[cfg(test)]
    pub(super) fn sizes(&self) -> (usize, usize, usize, usize) {
        let spills = self.chunks.values().map(|c| c.spills.len()).sum();
        (
            self.chunks.len(),
            self.viewers.len(),
            spills,
            self.active.len(),
        )
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            chunks,
            changed,
            viewers,
            // The chunks with spills, derived from `chunks`.
            spilling: _,
            active,
        } = self;
        h.usize(chunks.len());
        for (&(world, pos), chunk) in chunks {
            let Chunk {
                entities,
                spills,
                raw,
            } = chunk;
            h.usize(world);
            h.u64(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
            h.usize(entities.len());
            for (p, e) in entities {
                hash_pos(h, *p);
                e.hash_into(h, *p);
            }
            h.usize(spills.len());
            for s in spills {
                hash_pos(h, s.pos);
                h.usize(s.items.len());
                for i in &s.items {
                    inventory::hash_stack(h, i);
                }
            }
            h.usize(raw.len());
            for r in raw {
                super::extra::hash_nbt(h, r);
            }
        }
        h.usize(changed.len());
        for (world, pos) in changed {
            h.usize(*world);
            h.u64(u64::from(pos.x.cast_unsigned()) << 32 | u64::from(pos.z.cast_unsigned()));
        }
        h.usize(viewers.len());
        for ((world, pos), list) in viewers {
            h.usize(*world);
            hash_pos(h, *pos);
            h.usize(list.len());
            for s in list {
                h.u64(s.get());
            }
        }
        h.usize(active.len());
        for (world, pos) in active {
            h.usize(*world);
            hash_pos(h, *pos);
        }
    }
}

super::checkpoint::fields!(Spill { pos, items });

/// Each block entity by its position and saved form, which holds every field (the kinds' caches
/// are derived), then the spills and the raw entries.
impl super::checkpoint::Codec for Chunk {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            entities,
            spills,
            raw,
        } = self;
        w.len(entities.len());
        for (&pos, e) in entities {
            pos.encode(w);
            e.write(pos).encode(w);
        }
        spills.encode(w);
        raw.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::{Codec, Error};
        let n = r.len()?;
        let mut entities = BTreeMap::new();
        for _ in 0..n {
            let pos = BlockPos::decode(r)?;
            let nbt = Compound::decode(r)?;
            let e = BlockEntity::read(nbt, pos, r.custom);
            if entities.insert(pos, e).is_some() {
                return Err(Error::Format("repeated block entity"));
            }
        }
        Ok(Self {
            entities,
            spills: Codec::decode(r)?,
            raw: Codec::decode(r)?,
        })
    }
}

/// Every field but the chunks with spills, which are `chunks` again.
impl super::checkpoint::Codec for BlockEntities {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            chunks,
            changed,
            viewers,
            spilling: _,
            active,
        } = self;
        chunks.encode(w);
        changed.encode(w);
        viewers.encode(w);
        active.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        let chunks: BTreeMap<Key, Chunk> = Codec::decode(r)?;
        let spilling = chunks
            .iter()
            .filter(|(_, c)| !c.spills.is_empty())
            .map(|(&k, _)| k)
            .collect();
        Ok(Self {
            chunks,
            changed: Codec::decode(r)?,
            viewers: Codec::decode(r)?,
            spilling,
            active: Codec::decode(r)?,
        })
    }
}

/// The stacks a kind holds outside its slots (a framed item, a record, a book), for item counts.
#[cfg(test)]
fn own_stacks(kind: &Kind) -> impl Iterator<Item = &Stack> {
    let one = match kind {
        Kind::ItemFrame(f) => Some(&f.item),
        Kind::Jukebox { record } => Some(record),
        Kind::Lectern { book, .. } => Some(book),
        _ => None,
    };
    one.into_iter().filter(|s| !s.is_empty())
}

#[cfg(test)]
fn hash_pos(h: &mut super::Fnv, pos: BlockPos) {
    for c in [pos.x, pos.y, pos.z] {
        h.u64(u64::from(c.cast_unsigned()));
    }
}

/// The horizontal facing of a chest-like block state (`minecraft:cardinal_direction`).
fn cardinal(state: block::State) -> Option<Facing> {
    match state.value("minecraft:cardinal_direction")? {
        PropertyValue::String("north") => Some(Facing::North),
        PropertyValue::String("east") => Some(Facing::East),
        PropertyValue::String("south") => Some(Facing::South),
        PropertyValue::String("west") => Some(Facing::West),
        _ => None,
    }
}

/// The block at `pos` of `world`, if loaded.
fn block_at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<block::State> {
    state.worlds.get(world)?.block(pos)
}

/// The block one step toward `facing`.
fn beside(pos: BlockPos, facing: Facing) -> Option<BlockPos> {
    let (dx, dz) = facing.offset();
    Some(BlockPos::new(
        pos.x.checked_add(dx)?,
        pos.y,
        pos.z.checked_add(dz)?,
    ))
}

/// The centre of a block.
fn center(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

/// Sends the client form of the block entity at `pos` to every player that sees its chunk (PMMP
/// sends a tile's spawn compound when its block is set).
pub(super) fn broadcast_spawn(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let Some(entity) = state
        .rules
        .block_entities
        .get(world, pos)
        .filter(|e| e.is_spawnable())
    else {
        return;
    };
    let packet = Packet::BlockActorData(BlockActorData {
        position: [pos.x, pos.y, pos.z],
        nbt_data: entity.spawn_nbt(pos),
    });
    state.broadcast(world, pos.chunk(), &packet, None);
}

/// The block at `pos` was placed by `session` (or by the world when `None`) from `item` against
/// `face` (the clicked face): creates its block entity if the block has one.
///
/// The new entity takes the item's custom name and the data its item carries (PMMP
/// `Tile::copyDataFromItem` and each block's `place`): a shulker box's items and the face it opens
/// toward, a bed's colour, a banner's colour and patterns, a floor mob head's rotation from the
/// player's yaw. A chest pairs with a chest of the same kind and facing beside it that is not
/// paired yet, trying the counter-clockwise side first (PMMP `Chest::onPostPlace`). A block entity
/// already at `pos` (one nothing removed) is replaced, everything it held dropped first
/// ([`discard`]). The players who see the chunk get its client data.
pub(super) fn placed(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    session: Option<SessionId>,
    item: &Stack,
    face: Face,
) {
    if create(state, world, pos, session, item, face) {
        broadcast_spawn(state, world, pos);
    }
}

/// A window opens on a container block without its block entity (a block a task or a console
/// set): creates an empty one. Clients are not sent it: the client makes a block's block entity
/// with the block, and BDS sends no `BlockActorData` when a window opens (P6 F2); a chest that
/// pairs sends both halves as on placing.
pub(super) fn missing(state: &mut State, world: WorldIndex, pos: BlockPos) {
    create(state, world, pos, None, &Stack::EMPTY, Face::Up);
}

/// Creates the block entity of the block at `pos` as [`placed`] describes; false if the block
/// has none.
fn create(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    session: Option<SessionId>,
    item: &Stack,
    face: Face,
) -> bool {
    let Some(block) = block_at(state, world, pos) else {
        return false;
    };
    let Some((kind, slots)) = Kind::of(block) else {
        return false;
    };
    if state.rules.block_entities.get(world, pos).is_some() {
        discard(state, world, pos);
    }
    let yaw = session
        .and_then(|s| state.players.get(&s))
        .map(|p| survival::rotation(p).0);
    let mut entity = BlockEntity::new(kind, slots);
    entity.name = Some(item.custom_name().to_owned()).filter(|n| !n.is_empty());
    let tag = item.tag();
    let mut unfit = Vec::new();
    match &mut entity.kind {
        Kind::ShulkerBox { facing } => {
            *facing = face.index();
            if let Some(Value::List(List::Compound(items))) =
                tag.as_ref().and_then(|t| t.get("Items"))
            {
                read_items(&mut entity.items, items, pos, &state.custom);
            }
            unfit = entity.take_unfit();
        }
        Kind::Bed { colour } => {
            if let Ok(c) = u8::try_from(item.meta())
                && c < 16
            {
                *colour = c;
            }
        }
        Kind::Banner(banner) => {
            banner.base = i32::from(item.meta()).min(15);
            if let Some(tag) = &tag {
                if let Some(Value::List(List::Compound(list))) = tag.get("Patterns") {
                    banner.patterns = decor::read_patterns(list);
                }
                if let Some(Value::Int(ty)) = tag.get("Type") {
                    banner.ty = *ty;
                }
            }
        }
        Kind::Skull { rotation, .. } => {
            if face == Face::Up
                && let Some(yaw) = yaw
            {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "consistency: the yaw is a finite angle from the client's f32"
                )]
                let yaw = yaw as f32;
                *rotation = decor::skull_rotation(yaw);
            }
        }
        _ => {}
    }
    let is_chest = matches!(entity.kind, Kind::Chest { .. });
    if state
        .rules
        .block_entities
        .insert(world, pos, entity)
        .is_some()
    {
        warn!(
            x = pos.x,
            y = pos.y,
            z = pos.z,
            "a block entity was replaced"
        );
    }
    if is_chest {
        pair_chest(state, world, pos, block, session);
    }
    drop_all(state, world, pos, unfit);
    true
}

/// Pairs the new chest at `pos` with a free chest of the same kind and facing beside it. After a
/// [`ChestPair`] listener the chests must be as they were, and the hand of `session` (the player
/// placing, whose interaction goes on with it) the same stack.
fn pair_chest(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    session: Option<SessionId>,
) {
    let Some(facing) = cardinal(block) else {
        return;
    };
    let held = session.map(|s| inventory::held_of(state, s));
    // PMMP tries `rotateY(facing, false)` (counter-clockwise) first; that side's chest is the
    // right half.
    for (side, clockwise) in [
        (facing.clockwise().opposite(), false),
        (facing.clockwise(), true),
    ] {
        let Some(other) = beside(pos, side) else {
            continue;
        };
        let Some(other_block) = block_at(state, world, other) else {
            continue;
        };
        let pairable = |state: &State| {
            session.map(|s| inventory::held_of(state, s)) == held
                && block_at(state, world, pos) == Some(block)
                && block_at(state, world, other) == Some(other_block)
                && matches!(
                    state.rules.block_entities.get(world, pos).map(|e| &e.kind),
                    Some(Kind::Chest { pair: None })
                )
                && matches!(
                    state
                        .rules
                        .block_entities
                        .get(world, other)
                        .map(|e| &e.kind),
                    Some(Kind::Chest { pair: None })
                )
        };
        if other_block.kind() != block.kind()
            || cardinal(other_block) != Some(facing)
            || !pairable(state)
        {
            continue;
        }
        let mut event = dispatch::heard::<ChestPair>(state).then(|| {
            let (left, right) = if clockwise {
                (other, pos)
            } else {
                (pos, other)
            };
            ChestPair {
                world: inventory::world_id(world),
                left,
                right,
                cancelled: false,
            }
        });
        if let Some(e) = &mut event {
            dispatch::call(state, e);
            if e.cancelled || (dispatch::listened::<ChestPair>(state) && !pairable(state)) {
                continue;
            }
        }
        let entities = &mut state.rules.block_entities;
        for (at, with) in [(pos, other), (other, pos)] {
            if let Some(BlockEntity {
                kind: Kind::Chest { pair },
                ..
            }) = entities.get_mut(world, at)
            {
                *pair = Some(with);
            }
        }
        broadcast_spawn(state, world, pos);
        broadcast_spawn(state, world, other);
        if let Some(e) = event {
            dispatch::watch(state, &e);
        }
        return;
    }
}

/// The block at `pos` is gone (broken, replaced, burnt, exploded): removes its block entity,
/// closes the windows on it and drops what it held as items (PMMP `Container::onBlockDestroyed`);
/// a furnace's stored experience drops as orbs.
///
/// A shulker box keeps its contents: they leave in the dropped box's NBT, which the breaking
/// code takes from [`with_block_data`] before calling this. A paired chest's other half becomes
/// a single chest. Items that cannot be dropped now (the world's entity limit) wait as a spill in
/// the chunk and drop on later ticks. The contents of decorations (a framed item, a record, a
/// book, a potted plant) go with the block's drops ([`drops`]).
pub(super) fn removed(state: &mut State, world: WorldIndex, pos: BlockPos) {
    if state.rules.block_entities.get(world, pos).is_none() {
        return;
    }
    let pair = state
        .rules
        .block_entities
        .get(world, pos)
        .and_then(BlockEntity::pair);
    for session in window_viewers(state, world, pos) {
        inventory::close_window(state, session);
    }
    let Some(entity) = state.rules.block_entities.remove(world, pos) else {
        return;
    };
    if let Some(other) = pair {
        if let Some(BlockEntity {
            kind: Kind::Chest { pair },
            ..
        }) = state.rules.block_entities.get_mut(world, other)
        {
            *pair = None;
        }
        broadcast_spawn(state, world, other);
    }
    if let Kind::Furnace(f) = &entity.kind
        && f.xp > 0
    {
        entity::drop_experience(state, world, center(pos), u32::from(f.xp));
    }
    if matches!(entity.kind, Kind::ShulkerBox { .. }) {
        return;
    }
    drop_all(state, world, pos, entity.items);
}

/// The sessions with a window open on the block at `pos`; a double chest window covers both
/// halves.
pub(super) fn window_viewers(state: &State, world: WorldIndex, pos: BlockPos) -> Vec<SessionId> {
    let entities = &state.rules.block_entities;
    let mut out = entities.viewers(world, pos).to_vec();
    if let Some(other) = entities.get(world, pos).and_then(BlockEntity::pair) {
        for s in entities.viewers(world, other) {
            if !out.contains(s) {
                out.push(*s);
            }
        }
    }
    out
}

/// Drops `stacks` at the centre of `pos`; what cannot drop now (the world's entity limit) waits
/// as a spill in the chunk.
fn drop_all(state: &mut State, world: WorldIndex, pos: BlockPos, stacks: Vec<Stack>) {
    let at = center(pos);
    let mut left = Vec::new();
    for stack in stacks.into_iter().filter(|s| !s.is_empty()) {
        if !inventory::drop_stack(state, world, at, stack.clone()) {
            left.push(stack);
        }
    }
    if !left.is_empty() {
        state.rules.block_entities.spill(world, pos, left);
    }
}

/// The block at `pos` changed (the world's change feed, before its neighbour updates): a block
/// entity the new block does not take is removed (PMMP `Block::writeStateToWorld` closes a tile
/// whose class the new block's type does not register) and everything it held drops: its slots,
/// a shulker box's items, a framed item, a record, a lectern's book, a potted plant. PMMP's
/// `Tile::close` drops nothing, so a chest burnt or replaced loses its items there: fixed, no
/// item is lost.
///
/// Breaking ([`interact::destroy`]) removes block entities itself; this catches every other
/// change: fire, liquids, falling blocks, a task's `World::set_block`. A block entity of a kind
/// Anchor does not type ([`Kind::Other`]) belongs to blocks Anchor cannot tell, so it goes only
/// when its block becomes air.
pub(super) fn block_changed(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        return;
    };
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    if fits(&entity.kind, block) {
        return;
    }
    discard(state, world, pos);
}

/// Removes the block entity at `pos` whose block is gone, dropping everything it held (its
/// slots and what [`take_contents`] takes, which [`removed`] leaves to the breaking code).
fn discard(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let mut contents = Vec::new();
    if let Some(e) = state.rules.block_entities.get_mut(world, pos) {
        take_contents(e, &mut contents);
    }
    removed(state, world, pos);
    drop_all(state, world, pos, contents);
}

/// Whether a block entity of `kind` belongs to `block`: the kind the block's type takes, by its
/// Bedrock id (a furnace and its lit form share one, a blast furnace has its own).
fn fits(kind: &Kind, block: block::State) -> bool {
    match (kind, Kind::of(block)) {
        (Kind::Other(_), None) => block != block::AIR,
        (_, None) => false,
        (kind, Some((new, _))) => kind.id() == new.id(),
    }
}

/// Takes out what a block entity holds that [`removed`] does not drop: a shulker box's slots
/// (they leave with a broken box's item), a decoration's contents (they come with a broken
/// block's drops).
fn take_contents(entity: &mut BlockEntity, out: &mut Vec<Stack>) {
    match &mut entity.kind {
        Kind::ShulkerBox { .. } => out.extend(entity.items.iter_mut().map(std::mem::take)),
        Kind::ItemFrame(f) => out.push(std::mem::take(&mut f.item)),
        Kind::Jukebox { record } => out.push(std::mem::take(record)),
        Kind::Lectern { book, .. } => out.push(std::mem::take(book)),
        Kind::FlowerPot { plant } => {
            if let Some(block::ItemForm::Item(t)) = plant.take().map(block::State::item) {
                out.push(Stack::new(t, 1));
            }
        }
        _ => {}
    }
    out.retain(|s| !s.is_empty());
}

/// The item a block whose item form is its block entity's turns into (PMMP `asItem` of a bed and
/// a banner): the bed of its colour, the banner of its colour with its patterns. `None` for other
/// blocks.
pub(super) fn own_item(state: &State, world: WorldIndex, pos: BlockPos) -> Option<Stack> {
    let empty = Compound::new();
    match &state.rules.block_entities.get(world, pos)?.kind {
        Kind::Bed { colour } => {
            let t = item::Type::from_name("minecraft:bed", u16::from(*colour), None, &empty)?;
            Some(Stack::new(t, 1))
        }
        Kind::Banner(b) => {
            let mut tag = Compound::new();
            if !b.patterns.is_empty() {
                tag.insert("Patterns".to_owned(), decor::patterns_value(&b.patterns));
            }
            if b.ty != 0 {
                tag.insert("Type".to_owned(), Value::Int(b.ty));
            }
            let meta = u16::try_from(b.base).unwrap_or(0);
            let t = item::Type::from_name("minecraft:banner", meta, None, &tag)?;
            Stack::from_parts(t, 1, meta, (!tag.is_empty()).then_some(tag)).ok()
        }
        _ => None,
    }
}

/// What a decoration adds to its block's drops (PMMP `getDropsForCompatibleTool` and `getDrops`
/// overrides): a potted plant, a framed item (with its drop chance), a record, a lectern's book.
pub(super) fn drops(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    rng: &mut Random,
) -> Vec<Stack> {
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        return Vec::new();
    };
    let one = match &entity.kind {
        Kind::FlowerPot { plant } => plant.and_then(|p| match p.item() {
            block::ItemForm::Item(t) => Some(Stack::new(t, 1)),
            _ => None,
        }),
        Kind::ItemFrame(f) => {
            (!f.item.is_empty() && rng.unit() <= f64::from(f.drop_chance)).then(|| f.item.clone())
        }
        Kind::Jukebox { record } => Some(record.clone()),
        Kind::Lectern { book, .. } => Some(book.clone()),
        _ => None,
    };
    one.into_iter().filter(|s| !s.is_empty()).collect()
}

/// `stack` (the item form of the block at `pos`, without extra data) with the block entity's
/// data an item keeps: a shulker box's items and custom name (PMMP `ShulkerBox::addDataFromTile`),
/// a container's custom name. The stack unchanged for other blocks.
pub(super) fn with_block_data(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    stack: Stack,
) -> Stack {
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        return stack;
    };
    let Some(item) = stack.item() else {
        return stack;
    };
    let mut tag = stack.tag().unwrap_or_default();
    if matches!(entity.kind, Kind::ShulkerBox { .. }) && entity.items.iter().any(|s| !s.is_empty())
    {
        tag.insert("Items".to_owned(), write_items(&entity.items));
    }
    if let Some(name) = &entity.name {
        let display = Compound::from([("Name".to_owned(), Value::String(name.clone()))]);
        tag.insert("display".to_owned(), Value::Compound(display));
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: a block entity's name is at most tile::MAX_NAME_BYTES and a shulker \
                  box holds only stacks that fit it (every write, placing and loading check), so \
                  27 of them with the name are one stack (item::MAX_NBT_BYTES)"
    )]
    Stack::from_parts(item, stack.count(), stack.meta(), Some(tag))
        .expect("a block's item with its block entity's data is within the stack limits")
}

/// The pyramid levels under the beacon at `pos`, 0 to 4.
pub(super) fn beacon_level(state: &State, world: WorldIndex, pos: BlockPos) -> u8 {
    state.worlds.get(world).map_or(0, |w| beacon::level(w, pos))
}

/// Sets the effects of the beacon at `pos` (a payment) and tells the players who see it.
pub(super) fn set_beacon(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    primary: i32,
    secondary: i32,
) {
    if let Some(BlockEntity {
        kind: Kind::Beacon {
            primary: p,
            secondary: s,
        },
        ..
    }) = state.rules.block_entities.get_mut(world, pos)
    {
        (*p, *s) = (primary, secondary);
        broadcast_spawn(state, world, pos);
    }
}

/// Puts `stack` into slot `index` of the block entity at `pos` outside a window (the inventory
/// API): it may have work again, a campfire slot starts cooking from 0, and the players who see a
/// campfire's or brewing stand's items get its client data. Ok(false) if the slot held `stack`
/// already; `NoSlot` without such a block entity or slot. The caller tells the window viewers.
pub(super) fn set_slot(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    index: usize,
    stack: Stack,
) -> crate::inventory::Result<bool> {
    let entities = &mut state.rules.block_entities;
    let entity = entities
        .get_mut(world, pos)
        .ok_or(crate::inventory::Error::NoSlot)?;
    if matches!(entity.kind, Kind::ShulkerBox { .. }) && !stack.fits_shulker_box() {
        return Err(crate::inventory::Error::NotAccepted);
    }
    let slot = entity
        .items
        .get_mut(index)
        .ok_or(crate::inventory::Error::NoSlot)?;
    if *slot == stack {
        return Ok(false);
    }
    *slot = stack;
    let shown = match &mut entity.kind {
        Kind::Campfire(c) => {
            if let Some(t) = c.times.get_mut(index) {
                *t = 0;
            }
            true
        }
        Kind::BrewingStand(_) => true,
        _ => false,
    };
    if entity.kind.ticks(&entity.items) {
        entities.active.insert((world, pos));
    } else {
        entities.active.remove(&(world, pos));
    }
    if shown {
        broadcast_spawn(state, world, pos);
    }
    Ok(true)
}

/// Takes a furnace's stored experience (when its output is taken); 0 for other blocks.
pub(super) fn take_experience(state: &mut State, world: WorldIndex, pos: BlockPos) -> u32 {
    match state.rules.block_entities.get_mut(world, pos) {
        Some(BlockEntity {
            kind: Kind::Furnace(f),
            ..
        }) if f.xp > 0 => u32::from(std::mem::take(&mut f.xp)),
        _ => 0,
    }
}

/// One tick of block entities: furnaces, brewing stands, campfires and beacons with work, then
/// spilled items of broken containers, which drop when the world has room, at most
/// [`SPILL_DROPS_PER_TICK`] stacks a tick, in chunk order.
pub(super) fn tick(state: &mut State) {
    // In order, without a copy of the set: each step finds the next position after the last.
    let mut at = state.rules.block_entities.active.first().copied();
    while let Some((world, pos)) = at {
        if state.is_awake(world) {
            tick_one(state, world, pos);
        }
        at = next(state, (world, pos));
    }
    drop_spills(state);
}

/// The active block entity the tick visits after `at`: past every one of `at`'s world in one
/// step when that world is asleep (CLAUDE.md 2.2, no walk over what does not run).
fn next(state: &State, (world, pos): (WorldIndex, BlockPos)) -> Option<(WorldIndex, BlockPos)> {
    let after = if state.is_awake(world) {
        pos
    } else {
        BlockPos::new(i32::MAX, i32::MAX, i32::MAX)
    };
    state
        .rules
        .block_entities
        .active
        .range((
            std::ops::Bound::Excluded((world, after)),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .copied()
}

/// A tick's stagger for periodic work at `pos`, so blocks of one period do not all run on the
/// same tick (CLAUDE.md 2.2).
fn stagger(pos: BlockPos, period: u64) -> u64 {
    let key = random::key(
        TICK,
        &[
            u64::from(pos.x.cast_unsigned()),
            u64::from(pos.y.cast_unsigned()),
            u64::from(pos.z.cast_unsigned()),
        ],
    );
    key % period
}

/// Whether the periodic work of the block entity at `pos` (once in `interval` ticks: campfires,
/// beacons) runs this tick: on its staggered tick, or on every run of a world that
/// `slow-empty-worlds` holds back ([`State::is_slowed`]). Such a world runs on few ticks, which
/// the staggered one rarely or never meets; running on each of them keeps the work going there
/// (a campfire cooks one interval a run) with no count kept per world.
fn is_due(state: &State, world: WorldIndex, pos: BlockPos, interval: u64) -> bool {
    state.is_slowed(world) || (state.tick + stagger(pos, interval)).is_multiple_of(interval)
}

/// The tick of one active block entity.
fn tick_one(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
    let Some(entity) = state.rules.block_entities.get(world, pos) else {
        state.rules.block_entities.active.remove(&(world, pos));
        return;
    };
    match &entity.kind {
        Kind::Furnace(_) => tick_furnace(state, world, pos, block, seed),
        Kind::BrewingStand(_) => tick_brewing(state, world, pos, block),
        Kind::Campfire(_) => {
            if is_due(state, world, pos, campfire::INTERVAL) {
                tick_campfire(state, world, pos, block);
            }
        }
        Kind::Beacon { primary, secondary } => {
            let (primary, secondary) = (*primary, *secondary);
            if is_due(state, world, pos, beacon::INTERVAL) {
                beacon::update(state, world, pos, primary, secondary);
            }
        }
        _ => {
            state.rules.block_entities.active.remove(&(world, pos));
        }
    }
}

/// A furnace tick: the step, then the lit block, the slot and data updates of its viewers and the
/// last fuel (BDS `LastFuel`).
fn tick_furnace(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    seed: i64,
) {
    let lit = block.name().contains(":lit_");
    let mut rng = Random::new(
        seed,
        state.tick,
        random::key(
            TICK,
            &[
                u64::from(pos.x.cast_unsigned()),
                u64::from(pos.y.cast_unsigned()),
                u64::from(pos.z.cast_unsigned()),
            ],
        ),
    );
    let key = (world, pos.chunk());
    let Some((f, _, _)) = furnace_at(state, world, pos) else {
        return;
    };
    let before = (f.burn, f.cook, f.max_burn, f.xp);
    let mut t = f.begin(lit);
    let (mut burnt, mut smelted) = (None, None);
    if furnace_fuel(state, world, pos, block, &mut t, &mut burnt).is_none()
        || furnace_smelt(state, world, pos, block, &mut rng, &mut t, &mut smelted).is_none()
    {
        return;
    }
    let Some((f, _, _)) = furnace_at(state, world, pos) else {
        return;
    };
    let step = f.finish(t);
    let after = (f.burn, f.cook, f.max_burn, f.xp);
    let entities = &mut state.rules.block_entities;
    if after != before || step.slots != 0 {
        entities.changed.insert(key);
    }
    if !step.active {
        entities.active.remove(&(world, pos));
    }
    if let Some(lit) = step.lit {
        let name = lit_name(block, lit);
        if let Some(next) = name.and_then(|n| interact::props::renamed(block, &n))
            && let Some(w) = state.worlds.get_mut(world)
            && w.set_block(pos, next).is_err()
        {
            warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "could not light the furnace"
            );
        }
    }
    if let Some(fuel) = step.fuel {
        let packet = Packet::BlockActorData(BlockActorData {
            position: [pos.x, pos.y, pos.z],
            nbt_data: Compound::from([(
                "LastFuel".to_owned(),
                Value::Compound(fuel.to_sent_nbt()),
            )]),
        });
        state.broadcast(world, pos.chunk(), &packet, None);
    }
    inventory::block_changed(
        state,
        world,
        pos,
        step.slots,
        step.kept,
        step.data.as_slice(),
    );
    if let Some(e) = burnt {
        dispatch::watch(state, &e);
    }
    if let Some(e) = smelted {
        dispatch::watch(state, &e);
    }
}

/// The fuel step of a furnace tick: the fuel that is due burns, after [`FurnaceBurn`] (a burn
/// time of 0 burns nothing, as cancelled); `burnt` gets the event if it applied. `None` if the
/// furnace is gone.
fn furnace_fuel(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    t: &mut furnace::Tick,
    burnt: &mut Option<FurnaceBurn>,
) -> Option<()> {
    let (f, items, custom) = furnace_at(state, world, pos)?;
    let Some((fuel, time)) = f.fuel_due(items, custom) else {
        return Some(());
    };
    let mut burn = Some((time, true));
    if dispatch::heard::<FurnaceBurn>(state) {
        let mut e = FurnaceBurn {
            world: inventory::world_id(world),
            pos,
            block,
            fuel: fuel.clone(),
            burn_time_ticks: u32::from(time),
            burning: true,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let changed = dispatch::listened::<FurnaceBurn>(state)
            && furnace_at(state, world, pos)
                .and_then(|(f, items, custom)| f.fuel_due(items, custom))
                .is_none_or(|due| due != (fuel, time));
        burn = (!e.cancelled && !changed && e.burn_time_ticks > 0).then(|| {
            let time = u16::try_from(e.burn_time_ticks).unwrap_or(u16::MAX);
            (time, e.burning)
        });
        *burnt = burn.is_some().then_some(e);
    }
    let (f, items, _) = furnace_at(state, world, pos)?;
    if let Some((time, consume)) = burn {
        f.burn(items, time, consume, t);
    }
    Some(())
}

/// The cooking step of a furnace tick: a smelt that is due puts its result (after
/// [`FurnaceSmelt`], which may change it; one that does not fit the output cancels the smelt) on
/// the output; `smelted` gets the event if it applied. `None` if the furnace is gone.
fn furnace_smelt(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    rng: &mut Random,
    t: &mut furnace::Tick,
    smelted: &mut Option<FurnaceSmelt>,
) -> Option<()> {
    let (f, items, custom) = furnace_at(state, world, pos)?;
    let Some((source, result)) = f.cook(items, custom, t) else {
        return Some(());
    };
    let mut made = Some(result.clone());
    if dispatch::heard::<FurnaceSmelt>(state) {
        let mut e = FurnaceSmelt {
            world: inventory::world_id(world),
            pos,
            block,
            source: source.clone(),
            result: result.clone(),
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let changed = dispatch::listened::<FurnaceSmelt>(state)
            && furnace_at(state, world, pos)
                .and_then(|(f, items, custom)| f.ready(items, custom))
                .is_none_or(|ready| ready != (source, result));
        made = (!e.cancelled && !changed).then(|| e.result.clone());
        *smelted = made.is_some().then_some(e);
    }
    let experience = state.settings.has_orbs().then_some(rng);
    let (f, items, _) = furnace_at(state, world, pos)?;
    match made {
        Some(made) if furnace::Furnace::fits(items, &made) => f.smelt(items, &made, experience, t),
        _ => *smelted = None,
    }
    Some(())
}

/// The furnace at `pos`, its slots and the server's custom content (for its recipes).
fn furnace_at(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
) -> Option<(&mut furnace::Furnace, &mut Vec<Stack>, &Registry)> {
    let custom = &*state.custom;
    let entities = &mut state.rules.block_entities;
    match entities
        .chunks
        .get_mut(&(world, pos.chunk()))?
        .entities
        .get_mut(&pos)?
    {
        BlockEntity {
            kind: Kind::Furnace(f),
            items,
            ..
        } => Some((f, items, custom)),
        _ => None,
    }
}

/// The name of a furnace-like block lit or not (`minecraft:furnace` ↔ `minecraft:lit_furnace`).
fn lit_name(block: block::State, lit: bool) -> Option<String> {
    let rest = block.name().strip_prefix("minecraft:")?;
    let base = rest.strip_prefix("lit_").unwrap_or(rest);
    Some(if lit {
        format!("minecraft:lit_{base}")
    } else {
        format!("minecraft:{base}")
    })
}

/// A brewing stand tick.
fn tick_brewing(state: &mut State, world: WorldIndex, pos: BlockPos, block: block::State) {
    let key = (world, pos.chunk());
    let Some((b, _, _)) = brewing_at(state, world, pos) else {
        return;
    };
    let before = (b.brew, b.fuel, b.fuel_total);
    let mut t = b.begin();
    let mut watched = Vec::new();
    if brewing_fuel(state, world, pos, block, &mut t, &mut watched).is_none()
        || brewing_brew(state, world, pos, block, &mut t, &mut watched).is_none()
    {
        return;
    }
    let Some((b, _, _)) = brewing_at(state, world, pos) else {
        return;
    };
    let step = b.finish(t);
    let after = (b.brew, b.fuel, b.fuel_total);
    let entities = &mut state.rules.block_entities;
    if after != before || step.slots != 0 {
        entities.changed.insert(key);
    }
    if !step.active {
        entities.active.remove(&(world, pos));
    }
    if step.brewed {
        interact::sound(
            state,
            world,
            center(pos),
            crate::world::Sound::PotionFinishBrewing,
        );
    }
    inventory::block_changed(
        state,
        world,
        pos,
        step.slots,
        step.kept,
        step.data.as_slice(),
    );
    if step.slots != 0 || !step.data.as_slice().is_empty() {
        broadcast_spawn(state, world, pos);
    }
    for e in watched {
        match e {
            Watched::Fuel(e) => dispatch::watch(state, &e),
            Watched::Brew(e) => dispatch::watch(state, &e),
        }
    }
}

/// The fuel step of a brewing tick: a blaze powder that is due is used up, after
/// [`BrewingFuelUse`] (which may change how many brews it fuels). `None` if the stand is gone.
fn brewing_fuel(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    t: &mut brewing::Tick,
    watched: &mut Vec<Watched>,
) -> Option<()> {
    let (b, items, custom) = brewing_at(state, world, pos)?;
    if !b.fuel_due(items, custom) {
        return Some(());
    }
    let mut brews = Some(brewing::FUEL_BREWS);
    if dispatch::heard::<BrewingFuelUse>(state) {
        let mut e = BrewingFuelUse {
            world: inventory::world_id(world),
            pos,
            block,
            fuel_time: u32::from(brewing::FUEL_BREWS),
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let changed = dispatch::listened::<BrewingFuelUse>(state)
            && brewing_at(state, world, pos)
                .is_none_or(|(b, items, custom)| !b.fuel_due(items, custom));
        brews = (!e.cancelled && !changed).then(|| u16::try_from(e.fuel_time).unwrap_or(u16::MAX));
        if brews.is_some() {
            watched.push(Watched::Fuel(e));
        }
    }
    let (b, items, _) = brewing_at(state, world, pos)?;
    if let Some(brews) = brews {
        b.refuel(items, brews, t);
    }
    Some(())
}

/// The brewing step of a brewing tick: when the brew finishes, every bottle with a result
/// becomes it, after [`BrewItem`] (which may change it; an empty result keeps the bottle), and
/// one ingredient is used up. `None` if the stand is gone.
fn brewing_brew(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    t: &mut brewing::Tick,
    watched: &mut Vec<Watched>,
) -> Option<()> {
    let (b, items, custom) = brewing_at(state, world, pos)?;
    let Some(results) = b.brew_tick(items, custom, t) else {
        return Some(());
    };
    for (k, (slot, result)) in brewing::BOTTLES.into_iter().zip(results).enumerate() {
        let Some(result) = result else {
            continue;
        };
        let (_, items, _) = brewing_at(state, world, pos)?;
        let input = items.get(slot).cloned().unwrap_or_default();
        let mut made = Some(result.clone());
        if dispatch::heard::<BrewItem>(state) {
            let mut e = BrewItem {
                world: inventory::world_id(world),
                pos,
                block,
                slot: u8::try_from(slot).unwrap_or(u8::MAX),
                input: input.clone(),
                result: result.clone(),
                cancelled: false,
            };
            dispatch::call(state, &mut e);
            let changed = dispatch::listened::<BrewItem>(state)
                && brewing_at(state, world, pos).is_none_or(|(b, items, custom)| {
                    items.get(slot) != Some(&input)
                        || b.bottle_results(items, custom).get(k).cloned().flatten()
                            != Some(result.clone())
                });
            made = (!e.cancelled && !changed)
                .then(|| e.result.clone())
                .filter(|r| !r.is_empty());
            if made.is_some() {
                watched.push(Watched::Brew(e));
            }
        }
        let (_, items, _) = brewing_at(state, world, pos)?;
        if let Some(made) = made {
            brewing::bottle(items, slot, made, t);
        }
    }
    let (_, items, _) = brewing_at(state, world, pos)?;
    brewing::use_ingredient(items, t);
    Some(())
}

/// A brewing event that applied, for its monitors.
enum Watched {
    Fuel(BrewingFuelUse),
    Brew(BrewItem),
}

/// The brewing stand at `pos`, its slots and the recipes it brews with.
fn brewing_at(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
) -> Option<(&mut brewing::Brewing, &mut Vec<Stack>, &Registry)> {
    let custom = &*state.custom;
    match state
        .rules
        .block_entities
        .chunks
        .get_mut(&(world, pos.chunk()))?
        .entities
        .get_mut(&pos)?
    {
        BlockEntity {
            kind: Kind::BrewingStand(b),
            items,
            ..
        } => Some((b, items, custom)),
        _ => None,
    }
}

/// A campfire cooking update: lit campfires cook; done items drop above the fire (an item whose
/// result cannot drop at the entity limit waits in its slot).
fn tick_campfire(state: &mut State, world: WorldIndex, pos: BlockPos, block: block::State) {
    if block.value("extinguished") == Some(PropertyValue::Byte(1)) {
        return;
    }
    let kind = campfire::kind_of(block.is(Class::SoulCampfire));
    let key = (world, pos.chunk());
    let heard = dispatch::heard::<CampfireCook>(state);
    let custom = &*state.custom;
    let entities = &mut state.rules.block_entities;
    let Some(BlockEntity {
        kind: Kind::Campfire(c),
        items,
        ..
    }) = entities
        .chunks
        .get_mut(&key)
        .and_then(|c| c.entities.get_mut(&pos))
    else {
        return;
    };
    let done = c.step(kind, items, custom);
    let inputs: Vec<Stack> = if heard {
        done.iter()
            .map(|(slot, _)| items.get(*slot).cloned().unwrap_or_default())
            .collect()
    } else {
        Vec::new()
    };
    entities.changed.insert(key);
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 1.0,
        f64::from(pos.z) + 0.5,
    );
    let mut changed = false;
    for (k, (slot, mut result)) in done.into_iter().enumerate() {
        let mut event = inputs.get(k).map(|input| CampfireCook {
            world: inventory::world_id(world),
            pos,
            block,
            slot: u8::try_from(slot).unwrap_or(u8::MAX),
            input: input.clone(),
            result: result.clone(),
            cancelled: false,
        });
        if let Some(e) = &mut event {
            dispatch::call(state, e);
            let moved = dispatch::listened::<CampfireCook>(state)
                && state
                    .rules
                    .block_entities
                    .get(world, pos)
                    .and_then(|b| b.items.get(slot))
                    != Some(&e.input);
            if e.cancelled || moved {
                continue;
            }
            result = e.result.clone();
        }
        if !result.is_empty() && !inventory::drop_stack(state, world, at, result) {
            continue;
        }
        if let Some(BlockEntity {
            kind: Kind::Campfire(c),
            items,
            ..
        }) = state.rules.block_entities.get_mut(world, pos)
        {
            if let Some(s) = items.get_mut(slot) {
                *s = Stack::EMPTY;
            }
            if let Some(t) = c.times.get_mut(slot) {
                *t = 0;
            }
            changed = true;
        }
        if let Some(e) = event {
            dispatch::watch(state, &e);
        }
    }
    if changed {
        broadcast_spawn(state, world, pos);
        if state
            .rules
            .block_entities
            .get(world, pos)
            .is_some_and(|e| e.items.iter().all(Stack::is_empty))
        {
            state.rules.block_entities.active.remove(&(world, pos));
        }
    }
}

/// Drops spilled items of broken containers while the world has room.
fn drop_spills(state: &mut State) {
    let mut budget = SPILL_DROPS_PER_TICK;
    let keys: Vec<Key> = state
        .rules
        .block_entities
        .spilling
        .iter()
        .copied()
        .collect();
    for key in keys {
        let entities = &mut state.rules.block_entities;
        if entities
            .chunks
            .get(&key)
            .is_none_or(|c| c.spills.is_empty())
        {
            entities.spilling.remove(&key);
            continue;
        }
        while budget > 0 {
            let Some((pos, stack)) = state
                .rules
                .block_entities
                .chunks
                .get(&key)
                .and_then(|c| c.spills.first())
                .and_then(|s| Some((s.pos, s.items.last()?.clone())))
            else {
                break;
            };
            budget -= 1;
            if !inventory::drop_stack(state, key.0, center(pos), stack) {
                return;
            }
            let entities = &mut state.rules.block_entities;
            if let Some(chunk) = entities.chunks.get_mut(&key) {
                if let Some(first) = chunk.spills.first_mut() {
                    first.items.pop();
                    if first.items.is_empty() {
                        chunk.spills.remove(0);
                    }
                }
                entities.changed.insert(key);
            }
        }
    }
}

/// Sends a chest-like lid event for `viewers` people looking in (PMMP
/// `AnimatedBlockInventoryTrait`): chests, ender chests and shulker boxes animate their lid; a
/// barrel shows `open_bit`.
pub(super) fn animate(state: &mut State, world: WorldIndex, pos: BlockPos, viewers: usize) {
    let Some(block) = block_at(state, world, pos) else {
        return;
    };
    if block.is(Class::Barrel) {
        let open = PropertyValue::Byte(u8::from(viewers > 0));
        if let Some(next) = block.with("open_bit", open)
            && next != block
            && let Some(w) = state.worlds.get_mut(world)
            && w.set_block(pos, next).is_err()
        {
            warn!(x = pos.x, y = pos.y, z = pos.z, "could not open the barrel");
        }
        return;
    }
    if block.is(Class::Chest) || block.is(Class::EnderChest) || block.is(Class::ShulkerBox) {
        let packet = Packet::BlockEvent(BlockEvent {
            position: [pos.x, pos.y, pos.z],
            event_type: BLOCK_EVENT_CHEST,
            event_data: i32::try_from(viewers).unwrap_or(i32::MAX),
        });
        state.broadcast(world, pos.chunk(), &packet, None);
    }
}

#[cfg(test)]
pub(in crate::tick) mod tests {
    use super::*;
    use crate::item::Kind as ItemKind;
    use crate::world::{Chunk as Terrain, ChunkPos, Kind as WorldKind};

    /// A world with the chunk at `key` loaded and empty (air).
    fn world_with(key: Key) -> World {
        let mut world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        world.insert_chunk(key.1, Terrain::new());
        world
    }

    fn chest_entity(items: &[(usize, Stack)], pair: Option<BlockPos>) -> BlockEntity {
        let mut e = BlockEntity::new(Kind::Chest { pair }, 27);
        for (slot, s) in items {
            e.items[*slot] = s.clone();
        }
        e.name = Some("Loot".to_owned());
        e.rest
            .insert("Lock".to_owned(), Value::String("key".to_owned()));
        e
    }

    #[test]
    fn a_saved_chunk_reads_and_writes_back_byte_for_byte() {
        let key = (0, ChunkPos::new(1, -2));
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let mut chunk = Chunk::default();
        let a = BlockPos::new(16, 3, -32);
        let b = BlockPos::new(17, 3, -32);
        chunk.entities.insert(
            a,
            chest_entity(&[(0, stick.clone()), (26, stick.clone())], Some(b)),
        );
        chunk.entities.insert(b, chest_entity(&[], Some(a)));
        chunk.entities.insert(
            BlockPos::new(18, 3, -32),
            BlockEntity::read(
                Compound::from([
                    ("id".to_owned(), Value::String("Furnace".to_owned())),
                    ("BurnTime".to_owned(), Value::Short(7)),
                ]),
                BlockPos::new(18, 3, -32),
                &Registry::default(),
            ),
        );
        chunk.spills.push(Spill {
            pos: BlockPos::new(19, 3, -31),
            items: vec![stick.clone()],
        });
        let list = chunk.write();
        let read = Chunk::read(key, &list, &Registry::default());
        assert_eq!(read, chunk);
        assert_eq!(read.write(), list);
        // The chunk's blocks are not loaded: nothing is checked against them.
        let world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        let mut store = BlockEntities::default();
        store.load(key, &list, &world, &Registry::default());
        assert_eq!(
            store.unload(key),
            None,
            "unchanged chunks hand nothing back"
        );
        store.load(key, &list, &world, &Registry::default());
        assert!(store.get_mut(0, a).is_some());
        assert_eq!(store.take_changed(), vec![(key, list.clone())]);
        assert_eq!(store.take_changed(), vec![]);
        assert_eq!(store.unload(key), None);
        assert_eq!(store.sizes(), (0, 0, 0, 0), "nothing stays active");
    }

    #[test]
    fn a_loaded_block_entity_without_its_block_is_removed_and_its_items_wait_to_drop() {
        let key = (0, ChunkPos::new(0, 0));
        let mut world = world_with(key);
        let (chest, furnace, frame) = (
            BlockPos::new(1, 2, 3),
            BlockPos::new(2, 2, 3),
            BlockPos::new(3, 2, 3),
        );
        world
            .set_block(furnace, block::Kind::Furnace.default_state())
            .unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let framed = BlockEntity::new(
            Kind::ItemFrame(Box::new(decor::Frame {
                item: stick.clone(),
                rotation: 0,
                drop_chance: 1.0,
                glow: false,
            })),
            0,
        );
        let mut cooking = BlockEntity::read(
            Compound::from([("id".to_owned(), Value::String("Furnace".to_owned()))]),
            furnace,
            &Registry::default(),
        );
        cooking.items[0] = stick.clone();
        let list = vec![
            chest_entity(&[(0, stick.clone()), (26, stick.clone())], None).write(chest),
            cooking.write(furnace),
            framed.write(frame),
        ];
        let mut store = BlockEntities::default();
        store.load(key, &list, &world, &Registry::default());
        assert!(store.get(0, chest).is_none(), "air takes no chest");
        assert!(store.get(0, frame).is_none());
        assert!(store.get(0, furnace).is_some(), "a furnace keeps its own");
        let spills = &store.chunks[&key].spills;
        assert_eq!(
            spills,
            &vec![
                Spill {
                    pos: chest,
                    items: vec![stick.clone(), stick.clone()],
                },
                Spill {
                    pos: frame,
                    items: vec![stick.clone()],
                },
            ],
            "every item waits to drop"
        );
        assert!(store.spilling.contains(&key));
        let changed = store.take_changed();
        assert_eq!(changed.len(), 1, "the chunk is saved without them");
        // Read back, the spills stay and nothing more is removed.
        let mut again = BlockEntities::default();
        again.load(key, &changed[0].1, &world, &Registry::default());
        assert_eq!(again.chunks[&key], store.chunks[&key]);
        assert!(again.take_changed().is_empty());
    }

    /// A saved shulker box holding a stack its slots do not take (an Anchor before the limit)
    /// loads without it: the stack waits as a spill, a name over the limit stays as saved and is
    /// not shown, and the chunk is saved again that way.
    #[test]
    fn a_loaded_shulker_box_spills_what_its_slots_do_not_take() {
        let key = (0, ChunkPos::new(0, 0));
        let mut world = world_with(key);
        let pos = BlockPos::new(1, 2, 3);
        let shulker = block::State::all()
            .find(|b| b.is(Class::ShulkerBox))
            .unwrap();
        world.set_block(pos, shulker).unwrap();
        let stick = Stack::new(ItemKind::Stick.default_type(), 5);
        let big = stick
            .with_extra(Some(item::Extra {
                nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![1; 4 << 10]))]),
                ..item::Extra::default()
            }))
            .unwrap();
        let mut entity = BlockEntity::new(Kind::ShulkerBox { facing: 1 }, 27);
        entity.items[0] = stick.clone();
        entity.items[1] = big.clone();
        let long = "n".repeat(crate::tile::MAX_NAME_BYTES + 1);
        entity.name = Some(long.clone());
        let list = vec![entity.write(pos)];
        let mut store = BlockEntities::default();
        store.load(key, &list, &world, &Registry::default());
        let loaded = store.get(0, pos).unwrap();
        assert_eq!(loaded.items[0], stick);
        assert!(loaded.items[1].is_empty());
        assert_eq!(loaded.name, None);
        assert_eq!(
            loaded.rest.get("CustomName"),
            Some(&Value::String(long.clone()))
        );
        assert_eq!(
            store.chunks[&key].spills,
            vec![Spill {
                pos,
                items: vec![big],
            }]
        );
        let changed = store.take_changed();
        assert_eq!(changed.len(), 1, "the chunk is saved without it");
        assert_eq!(
            changed[0].1[0].get("CustomName"),
            Some(&Value::String(long))
        );
    }

    #[test]
    fn entries_outside_the_chunk_or_duplicated_are_kept_raw() {
        let key = (0, ChunkPos::new(0, 0));
        let outside = Compound::from([
            ("id".to_owned(), Value::String("Chest".to_owned())),
            ("x".to_owned(), Value::Int(99)),
            ("y".to_owned(), Value::Int(0)),
            ("z".to_owned(), Value::Int(0)),
        ]);
        let inside = chest_entity(&[], None).write(BlockPos::new(1, 2, 3));
        let list = vec![inside.clone(), inside.clone(), outside.clone()];
        let chunk = Chunk::read(key, &list, &Registry::default());
        assert_eq!(chunk.entities.len(), 1);
        assert_eq!(chunk.raw, vec![inside, outside]);
        assert_eq!(
            Chunk::read(key, &chunk.write(), &Registry::default()),
            chunk
        );
    }

    #[test]
    fn clients_get_the_client_keys_of_typed_block_entities_only() {
        let key = (0, ChunkPos::new(0, 0));
        let (a, b, c) = (
            BlockPos::new(1, 2, 3),
            BlockPos::new(2, 2, 3),
            BlockPos::new(3, 2, 3),
        );
        let mut chunk = Chunk::default();
        chunk.entities.insert(a, chest_entity(&[], Some(b)));
        let furnace = Compound::from([("id".to_owned(), Value::String("Furnace".to_owned()))]);
        chunk
            .entities
            .insert(b, BlockEntity::read(furnace, b, &Registry::default()));
        let unknown = Compound::from([("id".to_owned(), Value::String("Mystery".to_owned()))]);
        chunk
            .entities
            .insert(c, BlockEntity::read(unknown, c, &Registry::default()));
        let mut store = BlockEntities::default();
        let world = World::new("be", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        store.load(key, &chunk.write(), &world, &Registry::default());
        let sent = store.client_nbt(key);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].get("pairx"), Some(&Value::Int(2)));
        assert_eq!(
            sent[0].get("CustomName"),
            Some(&Value::String("Loot".to_owned()))
        );
        assert!(!sent[0].contains_key("Items") && !sent[0].contains_key("Lock"));
        assert!(
            !sent[1].contains_key("BurnTime"),
            "PMMP sends no furnace timers"
        );
        let bytes = store.client_bytes(0, key.1, usize::try_from((2 + 64) >> 4).unwrap());
        let mut r = crate::protocol::Reader::new(&bytes, 1 << 20, 0);
        for want in &sent {
            assert_eq!(
                &r.nbt(crate::nbt::Encoding::NetworkLittleEndian).unwrap(),
                want
            );
        }
        assert!(store.client_bytes(0, key.1, 0).is_empty());
    }

    #[test]
    fn a_pair_that_is_not_a_neighbour_is_dropped() {
        let pos = BlockPos::new(1, 2, 3);
        let mut nbt = chest_entity(&[], None).write(pos);
        nbt.insert("pairx".to_owned(), Value::Int(5));
        nbt.insert("pairz".to_owned(), Value::Int(3));
        assert_eq!(
            BlockEntity::read(nbt, pos, &Registry::default()).pair(),
            None
        );
    }

    /// One entity of every typed kind with data, as Anchor writes it.
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one fixture per block entity kind"
    )]
    fn every_kind() -> Vec<BlockEntity> {
        let stick = Stack::new(ItemKind::Stick.default_type(), 1);
        let book = Stack::new(ItemKind::WritableBook.default_type(), 1);
        let mut furnace = furnace::Furnace::new(FurnaceType::BlastFurnace);
        (furnace.burn, furnace.cook, furnace.max_burn, furnace.xp) = (5, 6, 1600, 2);
        let kinds = vec![
            (
                Kind::Furnace(furnace),
                vec![stick.clone(), Stack::EMPTY, stick.clone()],
            ),
            (
                Kind::BrewingStand(brewing::Brewing::read(&mut Compound::from([
                    ("CookTime".to_owned(), Value::Short(9)),
                    ("FuelAmount".to_owned(), Value::Short(3)),
                    ("FuelTotal".to_owned(), Value::Short(20)),
                ]))),
                vec![
                    stick.clone(),
                    Stack::EMPTY,
                    Stack::EMPTY,
                    Stack::EMPTY,
                    Stack::EMPTY,
                ],
            ),
            (
                Kind::Campfire(campfire::Campfire {
                    times: [10, 0, 0, 0],
                }),
                vec![stick.clone(), Stack::EMPTY, Stack::EMPTY, Stack::EMPTY],
            ),
            (Kind::EnchantTable, vec![]),
            (
                Kind::Beacon {
                    primary: 1,
                    secondary: 10,
                },
                vec![],
            ),
            (
                Kind::Sign(Box::new(decor::Sign {
                    front: decor::SignSide {
                        lines: ["hi".into(), String::new(), String::new(), String::new()],
                        colour: 7,
                        glowing: true,
                    },
                    back: decor::SignSide::default(),
                    waxed: true,
                    hanging: true,
                })),
                vec![],
            ),
            (Kind::Bed { colour: 3 }, vec![]),
            (
                Kind::Banner(decor::Banner {
                    base: 4,
                    patterns: vec![("bo".into(), 2)],
                    ty: 0,
                }),
                vec![],
            ),
            (
                Kind::FlowerPot {
                    plant: Some(block::Kind::Dandelion.default_state()),
                },
                vec![],
            ),
            (
                Kind::ItemFrame(Box::new(decor::Frame {
                    item: stick.clone(),
                    rotation: 3,
                    drop_chance: 0.5,
                    glow: true,
                })),
                vec![],
            ),
            (
                Kind::Jukebox {
                    record: Stack::new(
                        item::Type::from_name("minecraft:music_disc_13", 0, None, &Compound::new())
                            .unwrap(),
                        1,
                    ),
                },
                vec![],
            ),
            (Kind::Lectern { book, page: 0 }, vec![]),
            (
                Kind::ChiseledBookshelf { last: 2 },
                vec![
                    Stack::EMPTY,
                    Stack::new(ItemKind::Book.default_type(), 1),
                    Stack::EMPTY,
                    Stack::EMPTY,
                    Stack::EMPTY,
                    Stack::EMPTY,
                ],
            ),
            (
                Kind::Skull {
                    skull: 4,
                    rotation: 9,
                },
                vec![],
            ),
            (
                Kind::Cauldron(decor::Cauldron {
                    container: 1,
                    potion: 5,
                    colour: Some(-1),
                }),
                vec![],
            ),
        ];
        kinds
            .into_iter()
            .map(|(kind, items)| {
                let mut e = BlockEntity::new(kind, 0);
                e.items = items;
                e.rest.insert("isMovable".to_owned(), Value::Byte(1));
                e
            })
            .collect()
    }

    #[test]
    fn every_kind_round_trips_its_saved_form_byte_for_byte() {
        let pos = BlockPos::new(5, 70, -9);
        for e in every_kind() {
            let saved = e.write(pos);
            let read = BlockEntity::read(saved.clone(), pos, &Registry::default());
            assert_eq!(read, e, "{saved:?}");
            assert_eq!(read.write(pos), saved);
            let mut a = crate::protocol::Writer::new(Vec::new(), 0);
            let mut b = crate::protocol::Writer::new(Vec::new(), 0);
            a.nbt(&saved, crate::nbt::Encoding::LittleEndian).unwrap();
            b.nbt(&read.write(pos), crate::nbt::Encoding::LittleEndian)
                .unwrap();
            assert_eq!(a.as_bytes(), b.as_bytes());
            assert!(!matches!(read.kind, Kind::Other(_)), "{saved:?}");
        }
    }

    /// A chunk's block entity list through the commit's encoding (pages, sealed) and the load's
    /// decoding: what a load reads back.
    pub(in crate::tick) fn committed(list: &[Compound]) -> Vec<Compound> {
        use crate::storage::codec::ExtraRecord;
        use crate::storage::seal::{MAX_RECORD, seal_encoded, unseal};
        let mut read = Vec::new();
        for page in ExtraRecord::pages(list, MAX_RECORD).expect("the list encodes") {
            let sealed = seal_encoded(b"key", &page).expect("the page seals");
            let ExtraRecord(entries) =
                unseal::<ExtraRecord<'_>>(b"key", &sealed).expect("the page reads back");
            read.extend(entries.into_owned());
        }
        read
    }

    /// A chunk of 3,000 chests, each full of enchanted named diamond swords (vanilla play): reading
    /// its list back takes more than one record's reader may allocate, so pages cut by bytes
    /// alone did not read back. Every page reads back.
    #[test]
    fn a_chunk_of_chests_full_of_enchanted_swords_reads_back() {
        let sword = Stack::new(ItemKind::DiamondSword.default_type(), 1)
            .with_extra(Some(item::Extra {
                custom_name: Some("Excalibur".to_owned()),
                enchantments: vec![
                    (item::Enchantment::Sharpness, 5),
                    (item::Enchantment::Unbreaking, 3),
                    (item::Enchantment::Vanishing, 1),
                    (item::Enchantment::FireAspect, 2),
                    (item::Enchantment::Knockback, 2),
                    (item::Enchantment::Mending, 1),
                ],
                ..item::Extra::default()
            }))
            .unwrap();
        let list: Vec<Compound> = (0..3_000)
            .map(|i| {
                let pos = BlockPos::new(i & 15, -64 + (i >> 8), (i >> 4) & 15);
                chest_entity(
                    &(0..27).map(|s| (s, sword.clone())).collect::<Vec<_>>(),
                    None,
                )
                .write(pos)
            })
            .collect();
        let alloc: usize = list
            .iter()
            .map(|c| {
                let mut w = crate::protocol::Writer::new(Vec::new(), 0);
                w.nbt_within(c, crate::nbt::Encoding::LittleEndian, usize::MAX)
                    .unwrap()
                    .alloc
            })
            .sum();
        assert!(alloc > crate::storage::seal::DECODE_BUDGET, "{alloc}");
        assert_eq!(committed(&list), list);
    }

    fn food(name: &str, count: u8) -> Stack {
        Stack::new(
            item::Type::from_name(name, 0, None, &Compound::new()).unwrap(),
            count,
        )
    }

    /// A server with `slow-empty-worlds` set to `slow`, no players, the block entities of the
    /// default world's spawn chunk loaded: the state and that chunk.
    fn slow_server(slow: bool) -> (State, Key) {
        let state = super::super::tests::new_state(
            super::super::tests::settings(|c| c.developer.slow_empty_worlds = slow),
            crate::event::Listeners::default(),
        );
        let key = (0, state.worlds[0].spawn().chunk());
        (state, key)
    }

    /// Loads the (empty) block entities of the chunk at `key`.
    fn load_chunk(state: &mut State, key: Key) {
        let world = &state.worlds[key.0];
        state
            .rules
            .block_entities
            .load(key, &[], world, &Registry::default());
    }

    /// Puts at `pos` a furnace with beef and coal, active.
    fn furnace(state: &mut State, world: WorldIndex, pos: BlockPos) {
        state.worlds[world]
            .set_block(pos, block::Kind::Furnace.default_state())
            .unwrap();
        let mut e = BlockEntity::new(
            Kind::Furnace(furnace::Furnace::new(FurnaceType::Furnace)),
            3,
        );
        e.items[0] = food("minecraft:beef", 8);
        e.items[1] = food("minecraft:coal", 8);
        state.rules.block_entities.insert(world, pos, e);
    }

    fn cook(state: &State, world: WorldIndex, pos: BlockPos) -> u16 {
        match &state.rules.block_entities.get(world, pos).unwrap().kind {
            Kind::Furnace(f) => f.cook,
            Kind::Campfire(c) => c.times[0],
            other => panic!("{other:?}"),
        }
    }

    /// `slow-empty-worlds`: the tick passes over the active block entities of an asleep world in
    /// one step, and runs those of an awake world.
    #[test]
    fn the_tick_passes_over_an_asleep_world_in_one_step() {
        let (mut state, key) = slow_server(true);
        load_chunk(&mut state, key);
        let base = BlockPos::new(key.1.x * 16, 0, key.1.z * 16);
        for i in 0..1_000 {
            let at = BlockPos::new(base.x + (i & 15), i >> 8, base.z + ((i >> 4) & 15));
            furnace(&mut state, 0, at);
        }
        let mut other = World::new("b", WorldKind::Temporary, 1, BlockPos::new(0, 0, 0));
        other.insert_chunk(ChunkPos::new(0, 0), Terrain::new());
        state.add_world(other);
        load_chunk(&mut state, (1, ChunkPos::new(0, 0)));
        let lone = BlockPos::new(1, 0, 1);
        furnace(&mut state, 1, lone);
        state.awake = vec![super::super::Pace::Asleep, super::super::Pace::Every];
        let first = *state.rules.block_entities.active.first().unwrap();
        assert_eq!(first.0, 0);
        assert_eq!(next(&state, first), Some((1, lone)), "world 0 passed over");
        super::tick(&mut state);
        assert!(cook(&state, 1, lone) > 0, "the awake world's furnace cooks");
        assert_eq!(cook(&state, 0, first.1), 0, "the asleep world's do not");
    }

    /// `slow-empty-worlds`: a campfire (an interval block entity) in a world without players
    /// cooks on each of the world's wakes, whatever its stagger; in a world that runs every tick
    /// it cooks every [`campfire::INTERVAL`] ticks as before.
    #[test]
    fn a_campfire_in_a_slowed_world_cooks_on_each_wake() {
        for (slow, cooked) in [(false, 300), (true, 3 * 10)] {
            let (mut state, key) = slow_server(slow);
            load_chunk(&mut state, key);
            let base = BlockPos::new(key.1.x * 16, 0, key.1.z * 16);
            let pos = (0..16)
                .map(|x| BlockPos::new(base.x + x, 0, base.z))
                .find(|&p| stagger(p, campfire::INTERVAL) != 0)
                .unwrap();
            let lit = block::Kind::Campfire
                .default_state()
                .with("extinguished", PropertyValue::Byte(0))
                .unwrap();
            state.worlds[0].set_block(pos, lit).unwrap();
            let mut e = BlockEntity::new(Kind::Campfire(campfire::Campfire::default()), 4);
            e.items[0] = food("minecraft:beef", 1);
            state.rules.block_entities.insert(0, pos, e);
            let mut output = super::super::Output::default();
            for _ in 0..300 {
                super::super::tests::tick(&mut state, &mut Vec::new(), &mut output);
            }
            assert_eq!(cook(&state, 0, pos), cooked, "slow {slow}");
        }
    }

    #[test]
    fn lit_names_swap_the_prefix() {
        let furnace = block::Kind::Furnace.default_state();
        let lit = lit_name(furnace, true).unwrap();
        assert!(lit.starts_with("minecraft:lit_"), "{lit}");
        let back = interact::props::renamed(furnace, &lit).unwrap();
        assert_eq!(
            interact::props::renamed(back, &lit_name(back, false).unwrap()),
            Some(furnace)
        );
    }
}
