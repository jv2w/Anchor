//! Item types: every item type Anchor knows, in Bedrock form, with its 1.26.50
//! runtime ID, the lookups from the forms clients and saved data use, and the item registry and
//! creative inventory sent at spawn.
//!
//! The types are PMMP's item types (its item registry, the item form of every block state and
//! every name PMMP parses) merged by Bedrock form, with PMMP's data for each:
//! PMMP type and class, stack size, durability, tool, armour, food, fuel, enchanting, cooldown;
//! and the enchantment, item tag and tool tier tables. All data lives in static tables generated
//! by `cargo xtask gen`, so nothing is built at start and a lookup never allocates.

mod creative;
mod registry;
mod stack;
mod table;

pub(crate) use registry::SHIELD_RUNTIME_ID;
#[cfg(test)]
pub(crate) use stack::MAX_STORED_BYTES;
pub use stack::{Error, Extra, MAX_NBT_BYTES, Result, Stack};
pub use table::{Class, Enchantment, Kind, ToolTier, Trait};

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::block::{self, ToolType};
use crate::custom;
use crate::effect;
use crate::nbt::{self, StaticCompound};
use crate::protocol::{self, ItemType, packet};

/// An item type: a Bedrock item name with its meta value and, for a block item, the block state
/// it places.
///
/// It is a small `Copy` value: a number into Anchor's static item table, or a custom item or the
/// item of a custom block the server registered ([`custom::Item`], [`custom::Block`]), which
/// carries its definition and the runtime ID the server gave it. Only the server makes custom
/// types ([`Context::custom_item`](crate::Context::custom_item)).
#[derive(Clone, Copy)]
pub struct Type(Repr);

/// What a [`Type`] is.
#[derive(Clone, Copy)]
enum Repr {
    /// An index of the static item table.
    Vanilla(u16),
    /// A custom item and its runtime ID.
    Item(&'static custom::Item, i16),
    /// A custom block's item, its runtime ID and the network ID of the block's default state
    /// (the block it places and is sent with).
    Block(&'static custom::Block, i16, u32),
}

/// One row of the item table.
#[derive(Debug)]
struct Data {
    name: &'static str,
    meta: u16,
    block: Option<block::State>,
    /// The network ID of the block state the item is sent with; 0 for items without a block.
    block_network_id: u32,
    nbt: Option<StaticCompound>,
    runtime_id: i16,
}

impl Data {
    const fn new(
        name: &'static str,
        meta: u16,
        block: Option<block::State>,
        block_network_id: u32,
        nbt: Option<StaticCompound>,
        runtime_id: i16,
    ) -> Self {
        Self {
            name,
            meta,
            block,
            block_network_id,
            nbt,
            runtime_id,
        }
    }
}

impl Type {
    /// The type with this table index. Only generated tables call it, with indices of the item
    /// table.
    #[inline]
    pub(crate) const fn from_index(index: u16) -> Self {
        Self(Repr::Vanilla(index))
    }

    /// The registered custom item `item` with the runtime ID the server gave it. Only the custom
    /// registry calls it.
    pub(crate) const fn from_custom_item(item: &'static custom::Item, runtime_id: i16) -> Self {
        Self(Repr::Item(item, runtime_id))
    }

    /// The item of the registered custom block `block`, with the runtime ID the server gave it;
    /// `None` if the block has no default state (a registered block always has one).
    pub(crate) fn from_custom_block(
        block: &'static custom::Block,
        runtime_id: i16,
    ) -> Option<Self> {
        let placed = block::State::custom(block, 0, runtime_id)?;
        Some(Self(Repr::Block(block, runtime_id, placed.network_id())))
    }

    /// The custom item this is, `None` for any other type.
    #[inline]
    pub fn custom_item(self) -> Option<&'static custom::Item> {
        match self.0 {
            Repr::Item(item, _) => Some(item),
            Repr::Vanilla(_) | Repr::Block(..) => None,
        }
    }

    /// The custom block this is the item of, `None` for any other type.
    #[inline]
    pub fn custom_block(self) -> Option<&'static custom::Block> {
        match self.0 {
            Repr::Block(block, ..) => Some(block),
            Repr::Vanilla(_) | Repr::Item(..) => None,
        }
    }

    /// The index into the static tables, `None` for a custom type.
    #[inline]
    fn vanilla(self) -> Option<usize> {
        match self.0 {
            Repr::Vanilla(i) => Some(usize::from(i)),
            Repr::Item(..) | Repr::Block(..) => None,
        }
    }

    #[inline]
    fn data(index: u16) -> &'static Data {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a Type is only made from an index of the item table"
        )]
        &table::ITEMS[usize::from(index)]
    }

    /// The Bedrock item name, such as `minecraft:stick`, or a custom item's or block's
    /// identifier.
    #[inline]
    pub fn name(self) -> &'static str {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).name,
            Repr::Item(item, _) => item.identifier(),
            Repr::Block(block, ..) => block.identifier(),
        }
    }

    /// The meta (damage) value that tells apart types sharing a name, such as potion kinds; 0
    /// for custom types.
    #[inline]
    pub fn meta(self) -> u16 {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).meta,
            Repr::Item(..) | Repr::Block(..) => 0,
        }
    }

    /// The block state a block item places.
    pub fn block(self) -> Option<block::State> {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).block,
            Repr::Item(..) => None,
            Repr::Block(block, runtime_id, _) => block::State::custom(block, 0, runtime_id),
        }
    }

    /// The network ID of the block state a block item is sent with, 0 for other items. It is the
    /// state BDS sends with the item (the vanilla creative state), which may differ from the
    /// state the item places.
    #[inline]
    pub(crate) fn block_network_id(self) -> u32 {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).block_network_id,
            Repr::Item(..) => 0,
            Repr::Block(_, _, network_id) => network_id,
        }
    }

    /// The 1.26.50 network runtime ID of the name; for a custom type, the one the server gave it
    /// (after every vanilla runtime ID, in registration order).
    #[inline]
    pub fn runtime_id(self) -> i16 {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).runtime_id,
            Repr::Item(_, id) | Repr::Block(_, id, _) => id,
        }
    }

    /// NBT that is part of the type rather than of a stack, such as an ominous banner's `Type`.
    pub(crate) fn nbt(self) -> Option<StaticCompound> {
        match self.0 {
            Repr::Vanilla(i) => Self::data(i).nbt,
            Repr::Item(..) | Repr::Block(..) => None,
        }
    }

    /// The type of an item in saved Bedrock form: its name, meta, the block state of a block item
    /// and the stack NBT (empty if none).
    ///
    /// Among the types of the name and block, it is the one with this meta. Where several have
    /// it (a black banner and an ominous banner are both `minecraft:banner` meta 0), it is the
    /// one with the most type NBT that is part of `nbt`. Where none has it and the name and block
    /// have a single type, it is that type, as PMMP reads such a meta as stack state (a tool's
    /// damage, a firework star's colour); the caller keeps the meta with the stack. A block item
    /// needs its block state, since several states can share an item name. Anything else is
    /// `None`.
    #[doc(alias = "ItemDeserializer::deserializeType")]
    pub fn from_name(
        name: &str,
        meta: u16,
        block: Option<block::State>,
        nbt: &nbt::Compound,
    ) -> Option<Self> {
        let index = table::NAMES
            .binary_search_by(|(n, _)| (*n).cmp(name))
            .ok()?;
        let (_, runtime_id) = table::NAMES.get(index)?;
        Self::lookup(
            *runtime_id,
            |e| Self::from_index(e.3).block() == block,
            Some(meta),
            nbt,
        )
    }

    /// The type of an item in network form, as a client sends it: runtime ID, meta, the network
    /// ID of the block a block item is sent with (0 for others) and the stack NBT. The rules are
    /// those of [`Type::from_name`]; a meta beyond 16 bits matches only as stack state.
    #[doc(alias = "ItemTranslator::fromNetworkId")]
    pub fn from_network(
        runtime_id: i32,
        meta: u32,
        block_runtime_id: i32,
        nbt: &nbt::Compound,
    ) -> Option<Self> {
        let runtime_id = i16::try_from(runtime_id).ok()?;
        let block = block_runtime_id.cast_unsigned();
        Self::lookup(runtime_id, |e| e.1 == block, u16::try_from(meta).ok(), nbt)
    }

    /// The type of an item in network form whose block a client may send differently (P6:
    /// BDS matches requests by network ID, count and stack ID, not by the block runtime ID):
    /// [`Type::from_network`] with the block, else the one type of the runtime ID and meta with
    /// any block; `None` if several types could be meant.
    #[cfg(test)]
    pub(crate) fn from_network_any_block(
        runtime_id: i32,
        meta: u32,
        block_runtime_id: i32,
        nbt: &nbt::Compound,
    ) -> Option<Self> {
        Self::from_network(runtime_id, meta, block_runtime_id, nbt).or_else(|| {
            let runtime_id = i16::try_from(runtime_id).ok()?;
            let meta = u16::try_from(meta).ok();
            let all = equal_range(&table::BY_NETWORK, |e| e.0.cmp(&runtime_id));
            let mut exact = all.iter().filter(|e| Some(e.2) == meta);
            let only = match all {
                [one] => Some(one),
                _ => None,
            };
            let one = exact.next().or(only)?;
            if exact.next().is_some() {
                return None;
            }
            Some(Self::from_index(one.3))
        })
    }

    /// The lookup rules of [`Type::from_name`] among the types of a runtime ID that `block`
    /// accepts; `meta` is `None` when it cannot be a type's meta. A runtime ID has few types (at
    /// most the kinds of potion), so the scans are short.
    fn lookup(
        runtime_id: i16,
        block: impl Fn(&(i16, u32, u16, u16)) -> bool,
        meta: Option<u16>,
        nbt: &nbt::Compound,
    ) -> Option<Self> {
        let all = equal_range(&table::BY_NETWORK, |e| e.0.cmp(&runtime_id));
        let candidates = || all.iter().filter(|e| block(e));
        let exact = || candidates().filter(|e| Some(e.2) == meta);
        let found = match exact().count() {
            // Where none has the meta, a single type of the name and block takes it as stack state.
            0 => {
                let mut only = candidates();
                let one = only.next()?;
                if only.next().is_some() {
                    return None;
                }
                one
            }
            1 => exact().next()?,
            // The generator checked that type NBT sizes differ here, so the best is unique.
            _ => exact()
                .filter(|e| {
                    Self::from_index(e.3)
                        .nbt()
                        .is_none_or(|need| nbt::contains(nbt, need))
                })
                .max_by_key(|e| Self::from_index(e.3).nbt().map_or(0, <[_]>::len))?,
        };
        Some(Self::from_index(found.3))
    }
}

impl fmt::Debug for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Type")
            .field("name", &self.name())
            .field("meta", &self.meta())
            .finish()
    }
}

impl PartialEq for Type {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (Repr::Vanilla(a), Repr::Vanilla(b)) => a == b,
            // A server gives each custom type its own runtime ID.
            (Repr::Item(a, x), Repr::Item(b, y)) => x == y && a.identifier() == b.identifier(),
            (Repr::Block(a, x, _), Repr::Block(b, y, _)) => {
                x == y && a.identifier() == b.identifier()
            }
            _ => false,
        }
    }
}

impl Eq for Type {}

impl PartialOrd for Type {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Types of the table come first, in table order; then custom types by runtime ID and
/// identifier (never by address, so the order is the same on every run).
impl Ord for Type {
    fn cmp(&self, other: &Self) -> Ordering {
        let key = |t: &Self| match t.0 {
            Repr::Vanilla(i) => (0, i32::from(i), ""),
            Repr::Item(item, id) => (1, i32::from(id), item.identifier()),
            Repr::Block(block, id, _) => (1, i32::from(id), block.identifier()),
        };
        key(self).cmp(&key(other))
    }
}

impl Hash for Type {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self.0 {
            Repr::Vanilla(i) => i.hash(h),
            Repr::Item(_, id) | Repr::Block(_, id, _) => id.hash(h),
        }
    }
}

/// The data of a custom block's item: a plain block item of 64.
static CUSTOM_BLOCK_ITEM: Info = Info::custom(64, 0, 1, None);

/// [`Trait`] bits of every PMMP item (`ItemEnchantmentHandling`).
const TRAITS_ITEM: u32 = 1 << Trait::ItemEnchantmentHandling as u32;
/// [`Trait`] bits of a PMMP food item (`Food`: consumable, food source, releasable).
const TRAITS_FOOD: u32 = TRAITS_ITEM
    | 1 << Trait::Consumable as u32
    | 1 << Trait::ConsumableItem as u32
    | 1 << Trait::FoodSource as u32
    | 1 << Trait::FoodSourceItem as u32
    | 1 << Trait::Releasable as u32;

/// A row of the item data table: PMMP's data for an item type. A custom item holds one of its
/// own ([`Info::custom`]).
#[derive(Debug)]
pub(crate) struct Info {
    kind: Option<Kind>,
    /// The first of the type's classes in [`table::CLASS_CHAINS`] and their count.
    class: (u16, u8),
    /// [`Trait`] bits.
    traits: u32,
    max_stack: u8,
    /// 0 for items without durability.
    max_durability: u16,
    fire_proof: bool,
    attack_points: u8,
    defense_points: u8,
    tool_type: ToolType,
    harvest_level: u8,
    efficiency: f64,
    tier: Option<ToolTier>,
    armor: Option<Armor>,
    food: Option<Food>,
    consumable: Option<Consumable>,
    fuel_time: u16,
    fuel_residue: Option<Type>,
    enchantability: u8,
    primary_enchantments: EnchantmentSet,
    enchantments: EnchantmentSet,
    cooldown: Option<Cooldown>,
    throw_force: Option<f64>,
    potion: Option<effect::Potion>,
}

impl Info {
    /// The row of a custom item: a plain PMMP item (enchantability 1, like every `Item`) with
    /// this stack size, durability (0: none), attack damage and food. A food item is a PMMP
    /// `Food`: consumed whole, leaving nothing, with no effects.
    pub(crate) const fn custom(
        max_stack: u8,
        max_durability: u16,
        attack_points: u8,
        food: Option<Food>,
    ) -> Self {
        let edible = food.is_some();
        Self {
            kind: None,
            class: (0, 0),
            traits: if edible { TRAITS_FOOD } else { TRAITS_ITEM },
            max_stack,
            max_durability,
            fire_proof: false,
            attack_points,
            defense_points: 0,
            tool_type: ToolType::NONE,
            harvest_level: 0,
            efficiency: 1.0,
            tier: None,
            armor: None,
            food,
            consumable: if edible {
                Some(Consumable::new(None, Some((0, 0))))
            } else {
                None
            },
            fuel_time: 0,
            fuel_residue: None,
            enchantability: 1,
            primary_enchantments: EnchantmentSet(0),
            enchantments: EnchantmentSet(0),
            cooldown: None,
            throw_force: None,
            potion: None,
        }
    }

    /// The most items a stack holds.
    pub(crate) const fn max_stack(&self) -> u8 {
        self.max_stack
    }

    /// The durability; 0 for none.
    #[inline]
    pub(crate) const fn max_durability(&self) -> u16 {
        self.max_durability
    }

    /// What eating restores.
    pub(crate) const fn food(&self) -> Option<Food> {
        self.food
    }

    /// The damage of a hit.
    pub(crate) const fn attack_points(&self) -> u8 {
        self.attack_points
    }
}

/// The state index sentinel of [`table::TYPE_PLACES`]: none.
const NO_STATE: u16 = 0xffff;

/// An armour slot (PMMP `ArmorInventory` slots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArmorSlot {
    /// Slot 0.
    Head,
    /// Slot 1.
    Chest,
    /// Slot 2.
    Legs,
    /// Slot 3.
    Feet,
}

/// What an armour item is (PMMP `ArmorTypeInfo`; defence is [`Type::defense_points`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Armor {
    slot: u8,
    toughness: u8,
}

impl Armor {
    const fn new(slot: u8, toughness: u8) -> Self {
        Self { slot, toughness }
    }

    /// The slot it is worn in.
    #[doc(alias = "Armor::getArmorSlot")]
    pub fn slot(self) -> ArmorSlot {
        match self.slot {
            0 => ArmorSlot::Head,
            1 => ArmorSlot::Chest,
            2 => ArmorSlot::Legs,
            _ => ArmorSlot::Feet,
        }
    }

    /// The armour toughness (PMMP keeps it but does not use it).
    #[doc(alias = "ArmorTypeInfo::getToughness")]
    pub fn toughness(self) -> u8 {
        self.toughness
    }
}

/// What eating an item restores (PMMP `FoodSource`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Food {
    points: u8,
    saturation: f64,
    requires_hunger: bool,
}

impl Food {
    pub(crate) const fn new(points: u8, saturation: f64, requires_hunger: bool) -> Self {
        Self {
            points,
            saturation,
            requires_hunger,
        }
    }

    /// The food points restored.
    #[doc(alias = "FoodSource::getFoodRestore")]
    pub fn food_restore(self) -> u8 {
        self.points
    }

    /// The saturation restored.
    #[doc(alias = "FoodSource::getSaturationRestore")]
    pub fn saturation_restore(self) -> f64 {
        self.saturation
    }

    /// Whether the player must be hungry to eat it.
    #[doc(alias = "FoodSource::requiresHunger")]
    pub fn requires_hunger(self) -> bool {
        self.requires_hunger
    }
}

/// What consuming an item leaves and does (PMMP `ConsumableItem`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// pmmp-class: Consumable
pub struct Consumable {
    residue: Option<Type>,
    /// The first of the effects in [`table::CONSUME_EFFECTS`] and their count; `None` if PMMP
    /// draws them at random.
    effects: Option<(u16, u8)>,
}

impl Consumable {
    const fn new(residue: Option<Type>, effects: Option<(u16, u8)>) -> Self {
        Self { residue, effects }
    }

    /// The item left after consuming one (a bowl, a glass bottle).
    #[doc(alias = "ConsumableItem::getResidue")]
    pub fn residue(self) -> Option<Type> {
        self.residue
    }

    /// The effects consuming one gives; `None` if PMMP draws them at random (a poisonous potato
    /// poisons six times in ten), which the eating rules work out.
    #[doc(alias = "Consumable::getAdditionalEffects")]
    pub fn effects(self) -> Option<&'static [effect::Instance]> {
        let (start, count) = self.effects?;
        let start = usize::from(start);
        table::CONSUME_EFFECTS.get(start..start.saturating_add(usize::from(count)))
    }
}

/// The cooldown after using an item (PMMP `Item::getCooldownTicks`, `getCooldownTag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cooldown {
    ticks: u16,
    tag: Option<&'static str>,
}

impl Cooldown {
    const fn new(ticks: u16, tag: Option<&'static str>) -> Self {
        Self { ticks, tag }
    }

    /// The cooldown in ticks.
    #[doc(alias = "Item::getCooldownTicks")]
    pub fn ticks(self) -> u16 {
        self.ticks
    }

    /// The tag items sharing the cooldown have; `None`: the item type alone.
    #[doc(alias = "Item::getCooldownTag")]
    pub fn tag(self) -> Option<&'static str> {
        self.tag
    }
}

/// A set of enchantments, as bits by [`Enchantment`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EnchantmentSet(u32);

impl EnchantmentSet {
    /// Whether the set holds the enchantment.
    pub fn contains(self, e: Enchantment) -> bool {
        self.0 & (1 << e as u32) != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The enchantments of the set, in [`Enchantment`] order.
    pub fn iter(self) -> impl Iterator<Item = Enchantment> {
        Enchantment::all()
            .iter()
            .copied()
            .filter(move |e| self.contains(*e))
    }
}

/// A row of the enchantment table.
#[derive(Debug)]
struct EnchantmentData {
    id: u8,
    text: &'static str,
    rarity: u8,
    max_level: u8,
    available: bool,
    incompatible: EnchantmentSet,
    /// (min, max) enchanting power of each level from 1.
    power: &'static [(u16, u16)],
}

impl EnchantmentData {
    const fn new(
        id: u8,
        text: &'static str,
        rarity: u8,
        max_level: u8,
        available: bool,
        incompatible: EnchantmentSet,
        power: &'static [(u16, u16)],
    ) -> Self {
        Self {
            id,
            text,
            rarity,
            max_level,
            available,
            incompatible,
            power,
        }
    }
}

impl Enchantment {
    #[inline]
    fn data(self) -> &'static EnchantmentData {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the generated enchantment table has a row per enchantment"
        )]
        &table::ENCHANTMENT_DATA[self as usize]
    }

    /// Every enchantment, in name order.
    pub fn all() -> &'static [Self] {
        &table::ENCHANTMENTS
    }

    /// The enchantment with this save and network ID, or `None`.
    #[doc(alias = "EnchantmentIdMap::fromId")]
    pub fn from_id(id: i16) -> Option<Self> {
        let id = usize::try_from(id).ok()?;
        table::ENCHANTMENTS_BY_ID.get(id).copied().flatten()
    }

    /// The save and network ID.
    #[doc(alias = "EnchantmentIdMap::toId")]
    pub fn id(self) -> u8 {
        self.data().id
    }

    /// The translation key of the name, such as `enchantment.damage.all`.
    #[doc(alias = "Enchantment::getName")]
    pub fn text(self) -> &'static str {
        self.data().text
    }

    /// The weight of the enchantment in random choices (PMMP `Rarity`: common 10, uncommon 5,
    /// rare 2, mythic 1).
    #[doc(alias = "Enchantment::getRarity")]
    pub fn rarity(self) -> u8 {
        self.data().rarity
    }

    /// The highest level.
    #[doc(alias = "Enchantment::getMaxLevel")]
    pub fn max_level(self) -> u8 {
        self.data().max_level
    }

    /// Whether enchanting tables and anvils offer it (PMMP `AvailableEnchantmentRegistry`).
    #[doc(alias = "AvailableEnchantmentRegistry::isRegistered")]
    pub fn is_available(self) -> bool {
        self.data().available
    }

    /// The other enchantments an item cannot have with this one.
    pub fn incompatible(self) -> EnchantmentSet {
        self.data().incompatible
    }

    /// Whether an item can have both enchantments. An enchantment is compatible with itself
    /// here; PMMP leaves combining the same enchantment to the anvil rules.
    #[doc(alias = "Enchantment::isCompatibleWith")]
    pub fn is_compatible_with(self, other: Self) -> bool {
        !self.incompatible().contains(other)
    }

    /// The least enchanting power for the level at an enchanting table, or `None` past the
    /// highest level.
    #[doc(alias = "Enchantment::getMinEnchantingPower")]
    pub fn min_enchanting_power(self, level: u8) -> Option<u16> {
        let at = usize::from(level).checked_sub(1)?;
        self.data().power.get(at).map(|p| p.0)
    }

    /// The most enchanting power for the level at an enchanting table, or `None` past the
    /// highest level.
    #[doc(alias = "Enchantment::getMaxEnchantingPower")]
    pub fn max_enchanting_power(self, level: u8) -> Option<u16> {
        let at = usize::from(level).checked_sub(1)?;
        self.data().power.get(at).map(|p| p.1)
    }
}

/// An item tag (PMMP `ItemTagToIdMap`), such as `minecraft:logs`: recipes name a tag to accept
/// any of its items.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(u8);

impl Tag {
    /// The tag at this index of the tag table. Only generated tables call it.
    #[inline]
    pub(crate) const fn from_index(index: u8) -> Self {
        Self(index)
    }

    /// The tag with this name, or `None`.
    pub fn find(name: &str) -> Option<Self> {
        let at = table::TAG_NAMES.binary_search(&name).ok()?;
        u8::try_from(at).ok().map(Self)
    }

    /// The name.
    #[inline]
    pub fn name(self) -> &'static str {
        table::TAG_NAMES
            .get(usize::from(self.0))
            .copied()
            .unwrap_or_default()
    }

    /// Whether the item type has the tag.
    pub fn contains(self, item: Type) -> bool {
        let Some(index) = item.vanilla() else {
            return false;
        };
        let set = table::TYPE_TAGS
            .get(index)
            .and_then(|s| table::TAG_SETS.get(usize::from(*s)));
        set.is_some_and(|set| set & (1 << self.0) != 0)
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Tag").field(&self.name()).finish()
    }
}

impl Kind {
    /// Every kind, in name order.
    pub fn all() -> &'static [Self] {
        &table::KINDS
    }

    /// The `VanillaItems` name, lowercase, such as `iron_ingot`.
    #[inline]
    pub fn name(self) -> &'static str {
        table::KIND_NAMES
            .get(self as usize)
            .copied()
            .unwrap_or_default()
    }

    /// The item type `VanillaItems` gives for the kind (a kind with variants, such as potions,
    /// has more types).
    #[doc(alias = "VanillaItems")]
    pub fn default_type(self) -> Type {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the generated tables have a default type per kind"
        )]
        table::KIND_DEFAULTS[self as usize]
    }
}

impl Class {
    /// The PMMP class name, such as `Food`.
    #[inline]
    pub fn name(self) -> &'static str {
        table::CLASS_NAMES
            .get(self as usize)
            .copied()
            .unwrap_or_default()
    }
}

impl Trait {
    /// The PMMP name without `Trait`, such as `Releasable`.
    #[inline]
    pub fn name(self) -> &'static str {
        table::TRAIT_NAMES
            .get(self as usize)
            .copied()
            .unwrap_or_default()
    }
}

impl ToolTier {
    #[inline]
    fn data(self) -> (ToolTier, u8, u16, u8, u8, u8) {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the generated tier table has a row per tier, in enum order"
        )]
        table::TOOL_TIERS[self as usize]
    }

    /// The harvest level a tool of this tier has.
    #[doc(alias = "ToolTier::getHarvestLevel")]
    pub fn harvest_level(self) -> u8 {
        self.data().1
    }

    /// The durability of a tool of this tier.
    #[doc(alias = "ToolTier::getMaxDurability")]
    #[inline]
    pub fn max_durability(self) -> u16 {
        self.data().2
    }

    /// The base attack points of this tier's tools.
    #[doc(alias = "ToolTier::getBaseAttackPoints")]
    pub fn base_attack_points(self) -> u8 {
        self.data().3
    }

    /// The base mining efficiency of this tier's tools.
    #[doc(alias = "ToolTier::getBaseEfficiency")]
    pub fn base_efficiency(self) -> u8 {
        self.data().4
    }

    /// The enchantability of this tier's tools.
    #[doc(alias = "ToolTier::getEnchantability")]
    pub fn enchantability(self) -> u8 {
        self.data().5
    }
}

impl Type {
    #[inline]
    fn info(self) -> &'static Info {
        match self.0 {
            Repr::Vanilla(i) =>
            {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "invariant: the generated tables have a data row index per item \
                              type, and each indexes the data table"
                )]
                &table::INFO[usize::from(table::TYPE_INFO[usize::from(i)])]
            }
            Repr::Item(item, _) => item.info(),
            Repr::Block(..) => &CUSTOM_BLOCK_ITEM,
        }
    }

    /// The PMMP item type (`VanillaItems` entry); `None` for block items, whose block state has
    /// a [`block::Kind`].
    #[doc(alias = "Item::getTypeId")]
    #[inline]
    pub fn kind(self) -> Option<Kind> {
        self.info().kind
    }

    /// The PMMP class and its parents, leaf first, ending with [`Class::Item`].
    #[inline]
    pub fn classes(self) -> &'static [Class] {
        match self.0 {
            Repr::Vanilla(_) => {}
            Repr::Item(item, _) => return item.classes(),
            Repr::Block(..) => return &[Class::ItemBlock, Class::Item],
        }
        let (start, count) = self.info().class;
        let start = usize::from(start);
        table::CLASS_CHAINS
            .get(start..start.saturating_add(usize::from(count)))
            .unwrap_or_default()
    }

    /// The PMMP class, such as [`Class::Sword`].
    #[inline]
    pub fn class(self) -> Class {
        self.classes().first().copied().unwrap_or(Class::Item)
    }

    /// Whether the item is of the PMMP class or a subclass of it (`instanceof`).
    #[inline]
    pub fn is(self, class: Class) -> bool {
        self.classes().contains(&class)
    }

    /// Whether the item's PMMP class has the trait or interface, such as [`Trait::Releasable`].
    #[inline]
    pub fn has_trait(self, t: Trait) -> bool {
        self.info().traits & (1 << t as u32) != 0
    }

    /// The most items a stack holds.
    #[doc(alias = "Item::getMaxStackSize")]
    #[inline]
    pub fn max_stack_size(self) -> u8 {
        self.info().max_stack
    }

    /// The durability of a new item; `None` for items that do not wear.
    #[doc(alias = "Durable::getMaxDurability")]
    #[inline]
    pub fn max_durability(self) -> Option<u16> {
        Some(self.info().max_durability).filter(|d| *d > 0)
    }

    /// Whether the item survives fire and lava as a dropped item.
    #[doc(alias = "Item::isFireProof")]
    #[inline]
    pub fn is_fire_proof(self) -> bool {
        self.info().fire_proof
    }

    /// The damage a hit with the item deals (1 for items that are not weapons).
    #[doc(alias = "Item::getAttackPoints")]
    pub fn attack_points(self) -> u8 {
        self.info().attack_points
    }

    /// The armour points the item gives when worn.
    #[doc(alias = "Item::getDefensePoints")]
    pub fn defense_points(self) -> u8 {
        self.info().defense_points
    }

    /// The block tool types the item counts as.
    #[doc(alias = "Item::getBlockToolType")]
    #[inline]
    pub fn block_tool_type(self) -> ToolType {
        self.info().tool_type
    }

    /// The harvest level the item has as a tool (0 for other items).
    #[doc(alias = "Item::getBlockToolHarvestLevel")]
    pub fn block_tool_harvest_level(self) -> u8 {
        self.info().harvest_level
    }

    /// The mining efficiency on blocks of its tool type, before an Efficiency enchantment (1 for
    /// items that are not tools).
    #[doc(alias = "Item::getMiningEfficiency")]
    pub fn mining_efficiency(self) -> f64 {
        self.info().efficiency
    }

    /// The tier of a tiered tool.
    #[doc(alias = "TieredTool::getTier")]
    pub fn tool_tier(self) -> Option<ToolTier> {
        self.info().tier
    }

    /// What the item is as armour.
    pub fn armor(self) -> Option<Armor> {
        self.info().armor
    }

    /// What eating the item restores.
    pub fn food(self) -> Option<Food> {
        self.info().food
    }

    /// What consuming the item leaves and does.
    pub fn consumable(self) -> Option<Consumable> {
        self.info().consumable
    }

    /// The ticks one item burns as fuel; 0 if it is not fuel.
    #[doc(alias = "Item::getFuelTime")]
    #[inline]
    pub fn fuel_time_ticks(self) -> u16 {
        self.info().fuel_time
    }

    /// The item left after burning one as fuel (a lava bucket leaves a bucket).
    #[doc(alias = "Item::getFuelResidue")]
    pub fn fuel_residue(self) -> Option<Self> {
        self.info().fuel_residue
    }

    /// How well the item takes enchantments at an enchanting table.
    #[doc(alias = "Item::getEnchantability")]
    pub fn enchantability(self) -> u8 {
        self.info().enchantability
    }

    /// The enchantments an enchanting table can give the item (for an unenchanted item).
    #[doc(alias = "AvailableEnchantmentRegistry::getPrimaryEnchantmentsForItem")]
    pub fn primary_enchantments(self) -> EnchantmentSet {
        self.info().primary_enchantments
    }

    /// The enchantments the item can have (an anvil, a command).
    #[doc(alias = "AvailableEnchantmentRegistry::getAllEnchantmentsForItem")]
    pub fn enchantments(self) -> EnchantmentSet {
        self.info().enchantments
    }

    /// The cooldown after using the item.
    pub fn cooldown(self) -> Option<Cooldown> {
        self.info().cooldown
    }

    /// The speed a thrown item leaves with (snowballs, potions, pearls).
    #[doc(alias = "ProjectileItem::getThrowForce")]
    pub fn throw_force(self) -> Option<f64> {
        self.info().throw_force
    }

    /// The potion type a potion, splash potion or lingering potion holds.
    #[doc(alias = "Potion::getType")]
    pub fn potion(self) -> Option<effect::Potion> {
        self.info().potion
    }

    /// Whether the item has the tag.
    pub fn has_tag(self, tag: Tag) -> bool {
        tag.contains(self)
    }

    /// The block state placing the item on the `clicked` face of a block puts down, before the
    /// placing rules turn it to face the player; `None` if the item places nothing. Items with a
    /// wall form (signs, torches, banners) put that down against a side of a block.
    #[doc(alias = "Item::getBlock")]
    pub fn placed_block(self, clicked: Option<block::Face>) -> Option<block::State> {
        let index = match self.0 {
            Repr::Vanilla(i) => usize::from(i),
            Repr::Item(..) => return None,
            Repr::Block(..) => return self.block(),
        };
        let state = |table: &[u16]| {
            table
                .get(index)
                .copied()
                .filter(|s| *s != NO_STATE)
                .map(block::State::from_index)
        };
        let floor = state(&table::TYPE_PLACES);
        match clicked {
            Some(block::Face::Up | block::Face::Down) | None => floor,
            Some(_) => state(&table::TYPE_PLACES_ON_WALL).or(floor),
        }
    }
}

/// The run of `sorted` that `cmp` finds equal, by binary search.
fn equal_range<T>(sorted: &[T], cmp: impl Fn(&T) -> std::cmp::Ordering) -> &[T] {
    let start = sorted.partition_point(|e| cmp(e).is_lt());
    let rest = sorted.get(start..).unwrap_or_default();
    let len = rest.partition_point(|e| cmp(e).is_eq());
    rest.get(..len).unwrap_or_default()
}

/// The network form of a stack of `item` with meta `meta` and stack NBT `nbt`, as PMMP's
/// `TypeConverter` writes it: the runtime ID and meta, the network ID of the block a block item
/// places, and the NBT. The meta is the stack's, which differs from the type's where PMMP reads
/// meta as stack state (a firework star's colour).
pub(crate) fn network_stack(
    item: Type,
    meta: u16,
    count: u16,
    nbt: Option<StaticCompound>,
) -> protocol::ItemStack {
    protocol::ItemStack {
        item_type: ItemType {
            network_id: i32::from(item.runtime_id()),
            metadata_value: u32::from(meta),
        },
        // The network field is the hash's bit pattern as an int32.
        block_runtime_id: item.block_network_id().cast_signed(),
        count,
        nbt_data: nbt.map(nbt::compound).unwrap_or_default(),
        can_be_placed_on: Vec::new(),
        can_break: Vec::new(),
        blocking_tick: 0,
    }
}

/// One entry of the generated item registry.
#[derive(Debug)]
struct RegistryEntry {
    name: &'static str,
    runtime_id: i16,
    component_based: bool,
    version: i32,
    data: StaticCompound,
}

impl RegistryEntry {
    const fn new(
        name: &'static str,
        runtime_id: i16,
        component_based: bool,
        version: i32,
        data: StaticCompound,
    ) -> Self {
        Self {
            name,
            runtime_id,
            component_based,
            version,
            data,
        }
    }
}

/// Whether the item named `name` is an Education Edition item, never offered (creative inventory,
/// commands): PMMP's
/// chemistry items and blocks (elements, compounds, bleach, ice bombs, medicine, the chemistry
/// tables, the heat block, coloured and underwater torches, hardened glass) and the reference
/// list's other Education entries (balloons, sparklers, glow sticks, rapid fertilizer,
/// underwater TNT and the old chemistry names). `name` may leave out `minecraft:`. The one
/// place that says which items they are.
pub(crate) fn is_education(name: &str) -> bool {
    const PREFIXES: [&str; 3] = ["element_", "colored_torch_", "hard_"];
    const NAMES: [&str; 15] = [
        "balloon",
        "bleach",
        "chemical_heat",
        "chemistry_table",
        "compound",
        "compound_creator",
        "glow_stick",
        "ice_bomb",
        "lab_table",
        "material_reducer",
        "medicine",
        "rapid_fertilizer",
        "sparkler",
        "underwater_tnt",
        "underwater_torch",
    ];
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    PREFIXES.iter().any(|p| name.starts_with(p)) || NAMES.contains(&name)
}

/// The `ItemRegistry` packet: every item of the 1.26.50 reference list with its component data,
/// in runtime ID order. The client does not depend on the order: PMMP sends the
/// list by name and dragonfly in Go map order. Built once when the server is built.
pub(crate) fn registry() -> packet::ItemRegistry {
    packet::ItemRegistry {
        items: registry::ENTRIES
            .iter()
            .map(|e| protocol::ItemEntry {
                name: e.name.to_owned(),
                runtime_id: e.runtime_id,
                component_based: e.component_based,
                version: e.version,
                data: nbt::compound(e.data),
            })
            .collect(),
    }
}

/// A stack in a generated table: the type, the network meta (the type's, or stack state such as
/// a firework star's colour) and the stack NBT.
#[derive(Debug)]
struct StaticStack {
    item: Type,
    meta: u16,
    nbt: Option<StaticCompound>,
}

impl StaticStack {
    const fn new(item: Type, meta: u16, nbt: Option<StaticCompound>) -> Self {
        Self { item, meta, nbt }
    }

    /// The network form of one item of this stack.
    fn to_network(&self) -> protocol::ItemStack {
        network_stack(self.item, self.meta, 1, self.nbt)
    }
}

/// A creative group in a generated table: its `CreativeContent` category, name and icon (`None`
/// for an anonymous group, sent with an empty name and the air stack as PMMP does).
#[derive(Debug)]
struct CreativeGroup {
    category: u8,
    name: &'static str,
    icon: Option<StaticStack>,
}

impl CreativeGroup {
    const fn new(category: u8, name: &'static str, icon: Option<StaticStack>) -> Self {
        Self {
            category,
            name,
            icon,
        }
    }
}

/// A creative item in a generated table: its group index and stack.
#[derive(Debug)]
struct CreativeItem {
    group: u32,
    stack: StaticStack,
}

impl CreativeItem {
    const fn new(group: u32, stack: StaticStack) -> Self {
        Self { group, stack }
    }
}

/// The NBT key PMMP's `CreativeInventoryCache` sets on a group icon to the group's index (its
/// workaround for a 1.21.60 client glitch).
pub(crate) const GROUP_WORKAROUND: &str = "___GroupBugWorkaround___";

/// The generated `CreativeContent` packet: PMMP's creative inventory with its groups, as PMMP's
/// `CreativeInventoryCache` builds it (a group icon carries [`GROUP_WORKAROUND`] set to its group
/// index). Creative item network IDs count from 1 as BDS sends them (P6), where PMMP
/// counts from 0. The server builds its inventories from [`creative_entry`]; this is what it
/// builds of PMMP's unchanged.
#[cfg(test)]
pub(crate) fn creative_content() -> packet::CreativeContent {
    packet::CreativeContent {
        groups: creative::GROUPS
            .iter()
            .map(|g| protocol::CreativeGroup {
                category: g.category,
                name: g.name.to_owned(),
                icon: g
                    .icon
                    .as_ref()
                    .map(StaticStack::to_network)
                    .unwrap_or_default(),
            })
            .collect(),
        items: (1u32..)
            .zip(&creative::ITEMS)
            .map(|(id, c)| protocol::CreativeItem {
                creative_item_network_id: id,
                item: c.stack.to_network(),
                group_index: c.group,
            })
            .collect(),
    }
}

/// One entry of PMMP's creative inventory ([`creative_entry`]).
#[derive(Debug)]
pub(crate) struct CreativeEntry {
    /// The stack a player takes ([`creative_stack`]).
    pub(crate) stack: Stack,
    /// Its network form.
    pub(crate) net: protocol::ItemStack,
    /// Its category number (1 construction to 4 items).
    pub(crate) category: u8,
    /// Its group's name and icon network form (without [`GROUP_WORKAROUND`]); `None` for the
    /// category's anonymous group.
    pub(crate) group: Option<(&'static str, protocol::ItemStack)>,
}

/// Entry `index` (from 0) of PMMP's creative inventory; `None` past the last one.
pub(crate) fn creative_entry(index: usize) -> Option<CreativeEntry> {
    let c = creative::ITEMS.get(index)?;
    let group = creative::GROUPS.get(usize::try_from(c.group).ok()?)?;
    let named = group.icon.as_ref().map(|icon| {
        let mut icon = icon.to_network();
        icon.nbt_data.remove(GROUP_WORKAROUND);
        (group.name, icon)
    });
    Some(CreativeEntry {
        stack: creative_stack(u32::try_from(index).ok()?.checked_add(1)?)?,
        net: c.stack.to_network(),
        category: group.category,
        group: named,
    })
}

/// The highest runtime ID of the vanilla item registry (custom items take the ones after it).
pub(crate) fn max_runtime_id() -> i16 {
    registry::ENTRIES
        .iter()
        .map(|e| e.runtime_id)
        .max()
        .unwrap_or(0)
}

/// The stack of PMMP's creative item with this network ID (from 1, in PMMP's order), one item;
/// `None` for an unknown ID. O(1).
pub(crate) fn creative_stack(network_id: u32) -> Option<Stack> {
    let index = usize::try_from(network_id).ok()?.checked_sub(1)?;
    let c = creative::ITEMS.get(index)?;
    let tag = c.stack.nbt.map(nbt::compound);
    Stack::from_parts(c.stack.item, 1, c.stack.meta, tag).ok()
}

/// The dye colours by PMMP `DyeColorIdMap` ID (white 0 … black 15): the dye item's name and the
/// colour's RGB value (PMMP `DyeColor::getRgbValue`).
const DYES: [(&str, u32); 16] = [
    ("minecraft:white_dye", 0xf0_f0f0),
    ("minecraft:orange_dye", 0xf9_801d),
    ("minecraft:magenta_dye", 0xc7_4ebd),
    ("minecraft:light_blue_dye", 0x3a_b3da),
    ("minecraft:yellow_dye", 0xfe_d83d),
    ("minecraft:lime_dye", 0x80_c71f),
    ("minecraft:pink_dye", 0xf3_8baa),
    ("minecraft:gray_dye", 0x47_4f52),
    ("minecraft:light_gray_dye", 0x9d_9d97),
    ("minecraft:cyan_dye", 0x16_9c9c),
    ("minecraft:purple_dye", 0x89_32b8),
    ("minecraft:blue_dye", 0x3c_44aa),
    ("minecraft:brown_dye", 0x83_5432),
    ("minecraft:green_dye", 0x5e_7c16),
    ("minecraft:red_dye", 0xb0_2e26),
    ("minecraft:black_dye", 0x1d_1d21),
];

/// The dye colour ID (PMMP `DyeColorIdMap`, white 0 … black 15) of a dye item.
#[doc(alias = "Dye::getColor")]
pub(crate) fn dye_colour(item: Type) -> Option<u8> {
    let at = DYES.iter().position(|(name, _)| *name == item.name())?;
    u8::try_from(at).ok()
}

/// The opaque ARGB value of a dye colour ID (PMMP `DyeColor::getRgbValue()->toARGB()`).
pub(crate) fn dye_argb(colour: u8) -> Option<u32> {
    DYES.get(usize::from(colour))
        .map(|(_, rgb)| 0xff00_0000 | rgb)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value as Json;

    use super::*;
    use crate::nbt::{Compound, List, Value};
    use crate::protocol::packet::Packet;

    const REFERENCE: &str = include_str!("../xtask/data/reference/items.json");
    const CREATIVE: &str = include_str!("../xtask/data/pmmp/creative.json");

    fn all() -> impl Iterator<Item = Type> {
        (0..u16::try_from(table::ITEMS.len()).unwrap()).map(Type::from_index)
    }

    fn reference() -> Vec<Json> {
        serde_json::from_str(REFERENCE).unwrap()
    }

    /// Runtime IDs of the reference list by name.
    fn reference_ids() -> BTreeMap<String, i64> {
        reference()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_owned(),
                    e["runtime_id"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    fn block_id(t: Type) -> i32 {
        t.block_network_id().cast_signed()
    }

    /// Converts the dumps' typed NBT JSON (`{"int": 1}`; floats and doubles as bits).
    fn value(json: &Json) -> Value {
        let (kind, v) = json.as_object().unwrap().iter().next().unwrap();
        let int = || v.as_i64().unwrap();
        let ints = || v.as_array().unwrap().iter().map(|x| x.as_i64().unwrap());
        let byte =
            |x: i64| i8::try_from(x).map_or_else(|_| u8::try_from(x).unwrap(), i8::cast_unsigned);
        match kind.as_str() {
            "byte" => Value::Byte(byte(int())),
            "short" => Value::Short(i16::try_from(int()).unwrap()),
            "int" => Value::Int(i32::try_from(int()).unwrap()),
            "long" => Value::Long(int()),
            "float" => Value::Float(f32::from_bits(u32::try_from(int()).unwrap())),
            "double" => Value::Double(f64::from_bits(
                v.as_u64().unwrap_or_else(|| int().cast_unsigned()),
            )),
            "string" => Value::String(v.as_str().unwrap().to_owned()),
            "byte_array" => Value::ByteArray(ints().map(byte).collect()),
            "int_array" => Value::IntArray(ints().map(|x| i32::try_from(x).unwrap()).collect()),
            "long_array" => Value::LongArray(ints().collect()),
            "compound" => Value::Compound(compound(v)),
            "list" => Value::List(list(v)),
            other => panic!("unknown NBT type {other}"),
        }
    }

    fn list(json: &Json) -> List {
        let values: Vec<Value> = json["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(value)
            .collect();
        macro_rules! take {
            ($variant:ident) => {
                List::$variant(
                    values
                        .into_iter()
                        .map(|v| match v {
                            Value::$variant(x) => x,
                            other => panic!("list element {other:?}"),
                        })
                        .collect(),
                )
            };
        }
        match json["type"].as_str().unwrap() {
            "end" => List::End,
            "byte" => take!(Byte),
            "short" => take!(Short),
            "int" => take!(Int),
            "long" => take!(Long),
            "float" => take!(Float),
            "double" => take!(Double),
            "string" => take!(String),
            "byte_array" => take!(ByteArray),
            "int_array" => take!(IntArray),
            "long_array" => take!(LongArray),
            "compound" => take!(Compound),
            "list" => take!(List),
            other => panic!("unknown list type {other}"),
        }
    }

    fn compound(json: &Json) -> Compound {
        json.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), value(v)))
            .collect()
    }

    #[test]
    fn every_type_has_the_runtime_id_of_its_name() {
        let ids = reference_ids();
        assert_eq!(all().count(), 1613);
        for t in all() {
            assert_eq!(Some(&i64::from(t.runtime_id())), ids.get(t.name()), "{t:?}");
        }
    }

    #[test]
    fn registry_is_the_reference_list() {
        let reference = reference();
        let registry = registry();
        assert_eq!(registry.items.len(), 2076);
        assert_eq!(registry.items.len(), reference.len());
        for (entry, want) in registry.items.iter().zip(&reference) {
            assert_eq!(entry.name, want["name"].as_str().unwrap());
            assert_eq!(
                i64::from(entry.runtime_id),
                want["runtime_id"].as_i64().unwrap()
            );
            assert_eq!(
                entry.component_based,
                want["component_based"].as_bool().unwrap()
            );
            assert_eq!(i64::from(entry.version), want["version"].as_i64().unwrap());
            assert_eq!(entry.data, compound(&want["data"]), "{}", entry.name);
        }
    }

    /// Checks a network stack against the dumped stack PMMP sends it for; an icon also carries
    /// PMMP's group workaround index.
    fn check_stack(
        ids: &BTreeMap<String, i64>,
        stack: &protocol::ItemStack,
        want: &Json,
        workaround: Option<u32>,
    ) {
        let name = want["name"].as_str().unwrap();
        assert_eq!(i64::from(stack.item_type.network_id), ids[name], "{name}");
        // BDS sends the ominous banner white (meta 15) where PMMP has it black.
        let ominous = name == "minecraft:banner" && want["nbt"]["Type"]["int"] == 1;
        let meta = match want["meta"].as_u64().unwrap() {
            0 if ominous => 15,
            m => m,
        };
        assert_eq!(u64::from(stack.item_type.metadata_value), meta, "{name}");
        assert_eq!(u64::from(stack.count), want["count"].as_u64().unwrap());
        let mut nbt = want.get("nbt").map(compound).unwrap_or_default();
        if let Some(i) = workaround {
            nbt.insert(
                "___GroupBugWorkaround___".to_owned(),
                Value::Int(i32::try_from(i).unwrap()),
            );
        }
        assert_eq!(stack.nbt_data, nbt, "{name}");
        let item = Type::from_network(
            stack.item_type.network_id,
            stack.item_type.metadata_value,
            stack.block_runtime_id,
            &stack.nbt_data,
        )
        .unwrap();
        assert_eq!(item.name(), name);
        match want.get("block") {
            None => assert_eq!(stack.block_runtime_id, 0, "{name}"),
            Some(block) => {
                // The item places the dumped (PMMP) block and is sent with the vanilla item state
                // where the reference list has one.
                let state = item.block().unwrap();
                assert_eq!(state.name(), block["name"].as_str().unwrap());
                assert_eq!(
                    stack.block_runtime_id.cast_unsigned(),
                    item.block_network_id()
                );
                assert_ne!(item.block_network_id(), 0);
            }
        }
    }

    // pmmp: CreativeInventoryCache::buildPacket
    #[test]
    fn creative_content_is_the_pmmp_inventory() {
        let ids = reference_ids();
        let dump: Vec<Json> = serde_json::from_str(CREATIVE).unwrap();
        // The 61 bare enchanted books PMMP leaves of the enchantments it lacks are left out.
        let dump: Vec<Json> = dump
            .into_iter()
            .filter(|e| {
                e["item"]["name"] != "minecraft:enchanted_book"
                    || e["item"]["nbt"].get("ench").is_some()
            })
            .collect();
        let content = creative_content();
        assert_eq!(content.items.len(), 1694 - 61);
        assert_eq!(content.items.len(), dump.len());
        // Recomputed from PMMP's creative data files with PMMP's group rules.
        assert_eq!(content.groups.len(), 107);
        let mut next_group = 0;
        let mut previous: Option<(&Json, &Json, u32)> = None;
        for (i, (item, want)) in content.items.iter().zip(&dump).enumerate() {
            assert_eq!(item.creative_item_network_id, u32::try_from(i + 1).unwrap());
            check_stack(&ids, &item.item, &want["item"], None);
            let gi = item.group_index;
            let group = &content.groups[usize::try_from(gi).unwrap()];
            let category = match want["category"].as_str().unwrap() {
                "construction" => 1,
                "nature" => 2,
                "equipment" => 3,
                "items" => 4,
                other => panic!("category {other}"),
            };
            assert_eq!(group.category, category);
            // Group indices are given in order of first use.
            if gi == next_group {
                next_group += 1;
            } else {
                assert!(gi < next_group);
            }
            let dumped = &want["group"];
            if dumped.is_null() {
                assert_eq!(group.name, "");
                assert_eq!(group.icon, protocol::ItemStack::default());
            } else {
                assert_eq!(group.name, dumped["name"].as_str().unwrap());
                check_stack(&ids, &group.icon, &dumped["icon"], Some(gi));
            }
            // Consecutive entries of one category and group share the group.
            if let Some((c, g, previous_gi)) = previous
                && c == &want["category"]
                && g == dumped
            {
                assert_eq!(gi, previous_gi);
            }
            previous = Some((&want["category"], dumped, gi));
        }
        assert_eq!(usize::try_from(next_group).unwrap(), content.groups.len());
    }

    #[test]
    fn packets_round_trip_through_the_codec() {
        // PMMP has no shield; the registry still lists it.
        let shield = i32::try_from(reference_ids()["minecraft:shield"]).unwrap();
        let registry = registry();
        let content = creative_content();
        let mut bytes = Vec::new();
        for packet in [
            Packet::ItemRegistry(registry.clone()),
            Packet::CreativeContent(content.clone()),
        ] {
            packet.encode(&mut bytes, shield).unwrap();
        }
        // The decoder takes at most 1,024 list elements (protocol MAX_LIST_LEN), fewer than the
        // full lists, so every element goes back through the decoder in packets of 1,024.
        let mut packets = Vec::new();
        for items in registry.items.chunks(1024) {
            packets.push(Packet::ItemRegistry(packet::ItemRegistry {
                items: items.to_vec(),
            }));
        }
        for items in content.items.chunks(1024) {
            packets.push(Packet::CreativeContent(packet::CreativeContent {
                groups: content.groups.clone(),
                items: items.to_vec(),
            }));
        }
        for packet in packets {
            bytes.clear();
            packet.encode(&mut bytes, shield).unwrap();
            let mut budget = usize::MAX;
            let decoded = Packet::decode(packet.id(), &bytes, &mut budget, shield).unwrap();
            assert_eq!(decoded, packet);
        }
    }

    // pmmp: ItemDeserializer::deserializeType, ItemTranslator::fromNetworkId
    #[test]
    fn lookups_round_trip_for_every_type() {
        for t in all() {
            let nbt = t.nbt().map(nbt::compound).unwrap_or_default();
            assert_eq!(
                Type::from_name(t.name(), t.meta(), t.block(), &nbt),
                Some(t)
            );
            let stack = network_stack(t, t.meta(), 1, t.nbt());
            assert_eq!(
                Type::from_network(
                    stack.item_type.network_id,
                    stack.item_type.metadata_value,
                    stack.block_runtime_id,
                    &stack.nbt_data
                ),
                Some(t)
            );
        }
    }

    #[test]
    fn lookups_follow_pmmp_meta_rules() {
        let empty = Compound::new();
        let stick = Type::from_name("minecraft:stick", 0, None, &empty).unwrap();
        // A name with one type reads any meta as stack state.
        assert_eq!(
            Type::from_name("minecraft:stick", 7, None, &empty),
            Some(stick)
        );
        assert_eq!(
            Type::from_network(i32::from(stick.runtime_id()), u32::MAX, 0, &empty),
            Some(stick)
        );
        // A name with several types needs a meta one of them has.
        assert_eq!(Type::from_name("minecraft:banner", 999, None, &empty), None);
        // Type NBT picks the ominous banner (white, meta 15, as BDS sends it); other NBT leaves
        // the plain white one.
        let ominous: Compound = [("Type".to_owned(), Value::Int(1))].into();
        let plain: Compound = [("Type".to_owned(), Value::Int(0))].into();
        let o = Type::from_name("minecraft:banner", 15, None, &ominous).unwrap();
        let w = Type::from_name("minecraft:banner", 15, None, &plain).unwrap();
        assert!(o.nbt().is_some());
        assert!(w.nbt().is_none());
        assert_eq!(
            Type::from_name("minecraft:banner", 15, None, &empty),
            Some(w)
        );
        // A block item needs its block; an item without a block must not have one.
        let stone = all().find(|t| t.name() == "minecraft:stone").unwrap();
        assert_eq!(Type::from_name("minecraft:stone", 0, None, &empty), None);
        assert_eq!(
            Type::from_network(i32::from(stone.runtime_id()), 0, 0, &empty),
            None
        );
        assert_eq!(
            Type::from_network(i32::from(stick.runtime_id()), 0, block_id(stone), &empty),
            None
        );
    }

    fn by_name(name: &str) -> Type {
        let empty = Compound::new();
        Type::from_name(name, 0, None, &empty).unwrap_or_else(|| panic!("no item {name}"))
    }

    /// PMMP values of well-known items (PMMP `VanillaItems`, `ToolTier`, `Steak`, `Coal`).
    // pmmp: Item::getTypeId, Item::getMaxStackSize, Durable::getMaxDurability, Item::getAttackPoints,
    // pmmp: TieredTool::getTier, Item::getBlockToolType, Item::getEnchantability, ToolTier,
    // pmmp: AvailableEnchantmentRegistry::getPrimaryEnchantmentsForItem,
    // pmmp: AvailableEnchantmentRegistry::getAllEnchantmentsForItem, Item::getBlockToolHarvestLevel,
    // pmmp: Item::getMiningEfficiency, Item::getFuelTime, ToolTier::getHarvestLevel,
    // pmmp: ToolTier::getMaxDurability, ToolTier::getBaseAttackPoints, ToolTier::getBaseEfficiency,
    // pmmp: ToolTier::getEnchantability
    #[test]
    fn item_data_are_pmmp_values() {
        let ingot = by_name("minecraft:iron_ingot");
        assert_eq!(ingot.kind(), Some(Kind::IronIngot));
        assert_eq!(ingot.max_stack_size(), 64);
        assert_eq!(ingot.max_durability(), None);
        assert_eq!(ingot.attack_points(), 1);
        assert_eq!(ingot.class(), Class::Item);
        // PMMP `ToolTier::DIAMOND`: durability 1562, base attack 8.
        let sword = by_name("minecraft:diamond_sword");
        assert_eq!(sword.max_stack_size(), 1);
        assert_eq!(sword.max_durability(), Some(1562));
        assert_eq!(sword.attack_points(), 8);
        assert_eq!(sword.tool_tier(), Some(ToolTier::Diamond));
        assert_eq!(sword.block_tool_type(), ToolType::SWORD);
        assert!(sword.is(Class::TieredTool) && sword.is(Class::Durable));
        assert_eq!(sword.enchantability(), 10);
        assert!(
            sword
                .primary_enchantments()
                .contains(Enchantment::Sharpness)
        );
        assert!(!sword.primary_enchantments().contains(Enchantment::Mending));
        assert!(sword.enchantments().contains(Enchantment::Mending));
        assert!(!sword.enchantments().contains(Enchantment::Protection));
        let pickaxe = by_name("minecraft:wooden_pickaxe");
        assert_eq!(pickaxe.block_tool_type(), ToolType::PICKAXE);
        assert_eq!(pickaxe.block_tool_harvest_level(), 1);
        assert_eq!(pickaxe.mining_efficiency(), 2.0);
        assert_eq!(pickaxe.fuel_time_ticks(), 200);
        assert_eq!(ToolTier::Wood.harvest_level(), 1);
        assert_eq!(ToolTier::Diamond.max_durability(), 1562);
        assert_eq!(ToolTier::Netherite.base_attack_points(), 9);
        assert_eq!(ToolTier::Gold.base_efficiency(), 12);
        assert_eq!(ToolTier::Gold.enchantability(), 22);
    }

    /// PMMP values of food, fuel, armour and other item data.
    // pmmp: FoodSource::getFoodRestore, FoodSource::getSaturationRestore, FoodSource::requiresHunger,
    // pmmp: Consumable, ConsumableItem::getResidue, Consumable::getAdditionalEffects,
    // pmmp: Item::getFuelTime, Item::getFuelResidue, Armor::getArmorSlot, ArmorTypeInfo::getToughness,
    // pmmp: Item::getDefensePoints, Item::isFireProof, Item::getCooldownTicks, Item::getCooldownTag,
    // pmmp: ProjectileItem::getThrowForce, Potion::getType, Item::getBlock
    #[test]
    fn food_fuel_and_armour_are_pmmp_values() {
        // Cooked beef: 8 food, 12.8 saturation.
        let steak = by_name("minecraft:cooked_beef");
        assert_eq!(steak.kind(), Some(Kind::Steak));
        let food = steak.food().unwrap();
        assert_eq!((food.food_restore(), food.saturation_restore()), (8, 12.8));
        assert!(food.requires_hunger());
        assert!(steak.has_trait(Trait::Releasable) && steak.is(Class::Food));
        let consumable = steak.consumable().unwrap();
        assert_eq!(consumable.residue(), None);
        assert_eq!(consumable.effects(), Some(&[][..]));
        let stew = by_name("minecraft:mushroom_stew").consumable().unwrap();
        assert_eq!(stew.residue(), Some(by_name("minecraft:bowl")));
        // A poisonous potato poisons at random; a golden apple always gives its effects.
        let potato = by_name("minecraft:poisonous_potato").consumable().unwrap();
        assert_eq!(potato.effects(), None);
        let apple = by_name("minecraft:golden_apple").consumable().unwrap();
        let effects: Vec<_> = apple
            .effects()
            .unwrap()
            .iter()
            .map(|e| e.effect())
            .collect();
        assert_eq!(
            effects,
            [effect::Type::Regeneration, effect::Type::Absorption]
        );
        assert!(
            !by_name("minecraft:golden_apple")
                .food()
                .unwrap()
                .requires_hunger()
        );
        // Coal burns 1,600 ticks; a lava bucket 20,000 and leaves a bucket.
        assert_eq!(by_name("minecraft:coal").fuel_time_ticks(), 1600);
        let lava = by_name("minecraft:lava_bucket");
        assert_eq!(lava.fuel_time_ticks(), 20_000);
        assert_eq!(lava.fuel_residue(), Some(by_name("minecraft:bucket")));
        // Armour.
        let chest = by_name("minecraft:diamond_chestplate");
        let armor = chest.armor().unwrap();
        assert_eq!((armor.slot(), armor.toughness()), (ArmorSlot::Chest, 2));
        assert_eq!(chest.defense_points(), 8);
        assert_eq!(
            by_name("minecraft:netherite_helmet")
                .armor()
                .map(Armor::slot),
            Some(ArmorSlot::Head)
        );
        assert!(by_name("minecraft:netherite_ingot").is_fire_proof());
        // Cooldowns and throwing.
        let pearl = by_name("minecraft:ender_pearl");
        let cooldown = pearl.cooldown().unwrap();
        assert_eq!(
            (cooldown.ticks(), cooldown.tag()),
            (20, Some("ender_pearl"))
        );
        assert_eq!(pearl.throw_force(), Some(1.5));
        assert_eq!(pearl.max_stack_size(), 16);
        // Potions hold their type.
        let awkward = Type::from_name("minecraft:potion", 4, None, &Compound::new()).unwrap();
        assert_eq!(awkward.potion(), Some(effect::Potion::Awkward));
        assert_eq!(
            awkward.consumable().unwrap().residue(),
            Some(by_name("minecraft:glass_bottle"))
        );
        // A block item has no item kind; its block has a block kind.
        let planks = all().find(|t| t.name() == "minecraft:oak_planks").unwrap();
        assert_eq!(planks.kind(), None);
        assert_eq!(planks.class(), Class::ItemBlock);
        assert_eq!(planks.fuel_time_ticks(), 300);
        assert_eq!(
            planks.block().map(block::State::kind),
            Some(block::Kind::OakPlanks)
        );
        // Placing: seeds put down wheat, a door its lower half, a sign its standing or wall
        // form, a stick nothing.
        let wheat = by_name("minecraft:wheat_seeds").placed_block(None).unwrap();
        assert_eq!(wheat.kind(), block::Kind::Wheat);
        assert_eq!(wheat.value("growth"), Some(block::PropertyValue::Int(0)));
        let door = by_name("minecraft:wooden_door")
            .placed_block(Some(block::Face::Up))
            .unwrap();
        assert_eq!(door.kind(), block::Kind::OakDoor);
        let sign = by_name("minecraft:oak_sign");
        let floor = sign.placed_block(Some(block::Face::Up)).unwrap();
        let wall = sign.placed_block(Some(block::Face::North)).unwrap();
        assert_eq!(
            (floor.kind(), wall.kind()),
            (block::Kind::OakSign, block::Kind::OakWallSign)
        );
        assert_eq!(by_name("minecraft:stick").placed_block(None), None);
    }

    // pmmp: Enchantment, VanillaEnchantments, Enchantment::getMaxLevel, Enchantment::getRarity,
    // pmmp: Enchantment::getName, AvailableEnchantmentRegistry::isRegistered,
    // pmmp: Enchantment::getMinEnchantingPower, Enchantment::getMaxEnchantingPower,
    // pmmp: Enchantment::isCompatibleWith, EnchantmentIdMap::fromId, EnchantmentIdMap::toId
    #[test]
    fn enchantments_are_pmmp_values() {
        assert_eq!(Enchantment::all().len(), 23);
        let sharpness = Enchantment::Sharpness;
        assert_eq!(sharpness.max_level(), 5);
        assert_eq!(sharpness.id(), 9);
        assert_eq!(sharpness.rarity(), 10);
        assert_eq!(sharpness.text(), "enchantment.damage.all");
        assert!(sharpness.is_available());
        // PMMP `SharpnessEnchantment`: level n needs 1 + 11 (n - 1) to that plus 20.
        assert_eq!(sharpness.min_enchanting_power(1), Some(1));
        assert_eq!(sharpness.max_enchanting_power(5), Some(65));
        assert_eq!(sharpness.min_enchanting_power(0), None);
        assert_eq!(sharpness.min_enchanting_power(6), None);
        // PMMP's incompatible groups: protection kinds, infinity and mending, fortune and silk
        // touch.
        assert!(!Enchantment::Protection.is_compatible_with(Enchantment::FireProtection));
        assert!(!Enchantment::Infinity.is_compatible_with(Enchantment::Mending));
        assert!(!Enchantment::SilkTouch.is_compatible_with(Enchantment::Fortune));
        assert!(Enchantment::Sharpness.is_compatible_with(Enchantment::Unbreaking));
        assert!(Enchantment::Sharpness.incompatible().is_empty());
        for e in Enchantment::all() {
            assert_eq!(Enchantment::from_id(i16::from(e.id())), Some(*e));
            assert!(!e.incompatible().contains(*e));
            for other in e.incompatible().iter() {
                assert!(other.incompatible().contains(*e), "{e:?} {other:?}");
            }
            for level in 1..=e.max_level() {
                assert!(e.min_enchanting_power(level) <= e.max_enchanting_power(level));
            }
        }
        assert_eq!(Enchantment::from_id(-1), None);
        assert_eq!(Enchantment::from_id(1000), None);
    }

    // pmmp: VanillaItems
    #[test]
    fn every_type_has_its_data_and_tags_resolve() {
        // PMMP 5.44.3: the item kinds are VanillaItems without air.
        assert!(Kind::all().len() > 250, "{}", Kind::all().len());
        for (i, k) in Kind::all().iter().enumerate() {
            assert_eq!(*k as usize, i);
            assert_eq!(k.default_type().kind(), Some(*k));
            assert!(!k.name().is_empty());
        }
        for t in all() {
            assert!(t.max_stack_size() >= 1);
            assert_eq!(t.classes().last(), Some(&Class::Item));
            assert_eq!(t.class(), t.classes()[0]);
            assert!(t.mining_efficiency() > 0.0);
            // Every block item places a block: the one of its form if it has one.
            if t.kind().is_none() {
                assert!(t.placed_block(None).is_some(), "{t:?}");
            }
            // A block item places a block (bamboo places a bamboo sapling, not its form's state).
            if t.block().is_some() {
                assert!(t.placed_block(None).is_some(), "{t:?}");
            }
            if let Some(c) = t.consumable()
                && let Some(effects) = c.effects()
            {
                assert!(
                    effects
                        .iter()
                        .all(|e| e.duration_ticks() > 0 || e.effect().is_instant())
                );
            }
            for e in t.primary_enchantments().iter() {
                assert!(t.enchantments().contains(e));
            }
        }
        let logs = Tag::find("minecraft:logs").unwrap();
        assert_eq!(logs.name(), "minecraft:logs");
        assert!(logs.contains(all().find(|t| t.name() == "minecraft:oak_log").unwrap()));
        assert!(!logs.contains(by_name("minecraft:stick")));
        assert!(by_name("minecraft:coal").has_tag(Tag::find("minecraft:coals").unwrap()));
        assert_eq!(Tag::find("minecraft:nope"), None);
        assert_eq!(format!("{logs:?}"), "Tag(\"minecraft:logs\")");
    }

    /// The PMMP item dump (`items.json`), one entry per item type in table order.
    fn item_dump() -> Vec<Json> {
        serde_json::from_str(include_str!("../xtask/data/pmmp/items.json")).unwrap()
    }

    /// The PMMP dump's tables (`tables.json`).
    fn dump_tables() -> Json {
        serde_json::from_str(include_str!("../xtask/data/pmmp/tables.json")).unwrap()
    }

    /// The index of the entry named `name` in the dump table `list` (effects, enchantments,
    /// potions): the enums are in the tables' order.
    fn table_index(tables: &Json, list: &str, name: &Json) -> usize {
        tables[list]
            .as_array()
            .unwrap()
            .iter()
            .position(|e| e["name"] == *name)
            .unwrap_or_else(|| panic!("{list} {name}"))
    }

    /// The item type a dumped `{name, meta}` names; `None` for JSON null.
    fn dumped_type(json: &Json) -> Option<Type> {
        if json.is_null() {
            return None;
        }
        let found = all().find(|t| {
            t.name() == json["name"] && u64::from(t.meta()) == json["meta"].as_u64().unwrap()
        });
        Some(found.unwrap_or_else(|| panic!("no item {json}")))
    }

    fn int(json: &Json) -> u64 {
        json.as_u64().unwrap()
    }

    /// Every item type has the identity, stack, tool and armour data of its PMMP dump entry.
    // pmmp: Item::getTypeId, Item::getMaxStackSize, Durable::getMaxDurability, Item::isFireProof,
    // pmmp: Item::getAttackPoints, Item::getDefensePoints, Item::getBlockToolType,
    // pmmp: Item::getBlockToolHarvestLevel, Item::getMiningEfficiency, TieredTool::getTier,
    // pmmp: Armor::getArmorSlot, ArmorTypeInfo::getToughness
    #[test]
    fn every_type_has_its_pmmp_tool_and_armour_data() {
        let dump = item_dump();
        assert_eq!(dump.len(), all().count());
        for (t, entry) in all().zip(&dump) {
            let (bedrock, d) = (&entry["bedrock"], &entry["data"]);
            assert_eq!(t.name(), bedrock["name"]);
            // BDS sends the ominous banner white (meta 15) where PMMP has it black.
            let meta = match int(&bedrock["meta"]) {
                0 if bedrock["nbt"]["Type"]["int"] == 1 => 15,
                m => m,
            };
            assert_eq!(u64::from(t.meta()), meta, "{t:?}");
            assert_eq!(t.kind().map(Kind::name), d["kind"].as_str(), "{t:?}");
            assert_eq!(u64::from(t.max_stack_size()), int(&d["max_stack"]), "{t:?}");
            let durability = t.max_durability().map(u64::from);
            assert_eq!(durability, d["max_durability"].as_u64(), "{t:?}");
            assert_eq!(Some(t.is_fire_proof()), d["fire_proof"].as_bool(), "{t:?}");
            assert_eq!(u64::from(t.attack_points()), int(&d["attack_points"]));
            assert_eq!(u64::from(t.defense_points()), int(&d["defense_points"]));
            let tool = &d["tool"];
            assert_eq!(u64::from(t.block_tool_type().bits()), int(&tool["type"]));
            let level = u64::from(t.block_tool_harvest_level());
            assert_eq!(level, int(&tool["harvest_level"]), "{t:?}");
            assert_eq!(Some(t.mining_efficiency()), tool["efficiency"].as_f64());
            let tier = t.tool_tier().map(|x| format!("{x:?}").to_lowercase());
            assert_eq!(tier.as_deref(), tool["tier"].as_str(), "{t:?}");
            match (t.armor(), &d["armor"]) {
                (None, Json::Null) => {}
                (Some(a), want) => {
                    let slot = match a.slot() {
                        ArmorSlot::Head => 0,
                        ArmorSlot::Chest => 1,
                        ArmorSlot::Legs => 2,
                        ArmorSlot::Feet => 3,
                    };
                    assert_eq!(slot, int(&want["slot"]), "{t:?}");
                    assert_eq!(u64::from(a.toughness()), int(&want["toughness"]), "{t:?}");
                }
                (None, want) => panic!("{t:?} has no armour, PMMP {want}"),
            }
        }
    }

    /// Every item type has the food, consumable and fuel data of its PMMP dump entry.
    // pmmp: FoodSource::getFoodRestore, FoodSource::getSaturationRestore, FoodSource::requiresHunger,
    // pmmp: ConsumableItem::getResidue, Consumable::getAdditionalEffects, Item::getFuelTime,
    // pmmp: Item::getFuelResidue
    #[test]
    fn every_type_has_its_pmmp_food_and_fuel_data() {
        let tables = dump_tables();
        for (t, entry) in all().zip(&item_dump()) {
            let d = &entry["data"];
            match (t.food(), &d["food"]) {
                (None, Json::Null) => {}
                (Some(f), want) => {
                    assert_eq!(u64::from(f.food_restore()), int(&want["food"]), "{t:?}");
                    assert_eq!(Some(f.saturation_restore()), want["saturation"].as_f64());
                    assert_eq!(Some(f.requires_hunger()), want["requires_hunger"].as_bool());
                }
                (None, want) => panic!("{t:?} is no food, PMMP {want}"),
            }
            match (t.consumable(), &d["consumable"]) {
                (None, Json::Null) => {}
                (Some(c), want) => {
                    assert_eq!(c.residue(), dumped_type(&want["residue"]), "{t:?}");
                    match (c.effects(), &want["effects"]) {
                        (None, Json::String(random)) => assert_eq!(random, "random"),
                        (Some(effects), Json::Array(wanted)) => {
                            assert_eq!(effects.len(), wanted.len(), "{t:?}");
                            for (e, w) in effects.iter().zip(wanted) {
                                let at = table_index(&tables, "effects", &w["effect"]);
                                assert_eq!(e.effect(), effect::Type::all()[at], "{t:?}");
                                assert_eq!(u64::from(e.duration_ticks()), int(&w["duration"]));
                                assert_eq!(u64::from(e.amplifier()), int(&w["amplifier"]));
                            }
                        }
                        (got, want) => panic!("{t:?} effects {got:?}, PMMP {want}"),
                    }
                }
                (None, want) => panic!("{t:?} is not consumable, PMMP {want}"),
            }
            let fuel = &d["fuel"];
            assert_eq!(u64::from(t.fuel_time_ticks()), int(&fuel["time"]), "{t:?}");
            assert_eq!(t.fuel_residue(), dumped_type(&fuel["residue"]), "{t:?}");
        }
    }

    /// Every item type has the enchanting, cooldown, throwing, potion and placing data of its
    /// PMMP dump entry.
    // pmmp: Item::getEnchantability, AvailableEnchantmentRegistry::getPrimaryEnchantmentsForItem,
    // pmmp: AvailableEnchantmentRegistry::getAllEnchantmentsForItem, Item::getCooldownTicks,
    // pmmp: Item::getCooldownTag, ProjectileItem::getThrowForce, Potion::getType, Item::getBlock
    #[test]
    fn every_type_has_its_pmmp_use_data() {
        let tables = dump_tables();
        for (t, entry) in all().zip(&item_dump()) {
            let d = &entry["data"];
            assert_eq!(u64::from(t.enchantability()), int(&d["enchantability"]));
            for (set, key) in [
                (t.primary_enchantments(), "primary"),
                (t.enchantments(), "all"),
            ] {
                let wanted: BTreeSet<usize> = d["enchantments"][key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| table_index(&tables, "enchantments", n))
                    .collect();
                for (i, e) in Enchantment::all().iter().enumerate() {
                    assert_eq!(set.contains(*e), wanted.contains(&i), "{t:?} {key} {e:?}");
                }
            }
            match (t.cooldown(), &d["cooldown"]) {
                (None, Json::Null) => {}
                (Some(c), want) => {
                    assert_eq!(u64::from(c.ticks()), int(&want["ticks"]), "{t:?}");
                    assert_eq!(c.tag(), want["tag"].as_str(), "{t:?}");
                }
                (None, want) => panic!("{t:?} has no cooldown, PMMP {want}"),
            }
            assert_eq!(t.throw_force(), d["throw_force"].as_f64(), "{t:?}");
            let potion = &d["potion"];
            let want = (!potion.is_null()).then(|| table_index(&tables, "potions", potion));
            assert_eq!(t.potion(), want.map(|i| effect::Potion::all()[i]), "{t:?}");
            let places = |face| t.placed_block(Some(face)).map(block::State::name);
            let floor = d["places"]["name"].as_str();
            assert_eq!(places(block::Face::Up), floor, "{t:?}");
            if let Some(wall) = d["places_on_wall"]["name"].as_str() {
                assert_eq!(places(block::Face::North), Some(wall), "{t:?}");
            }
        }
    }

    /// Dye items have PMMP `DyeColorIdMap` IDs (white 0 … black 15) and PMMP's RGB values.
    // pmmp: Dye::getColor
    #[test]
    fn dyes_have_pmmp_colour_ids() {
        assert_eq!(dye_colour(by_name("minecraft:white_dye")), Some(0));
        assert_eq!(dye_colour(by_name("minecraft:red_dye")), Some(14));
        assert_eq!(dye_colour(by_name("minecraft:black_dye")), Some(15));
        assert_eq!(dye_colour(by_name("minecraft:stick")), None);
        // PMMP `DyeColor::RED`: (176, 46, 38).
        assert_eq!(dye_argb(14), Some(0xffb0_2e26));
        assert_eq!(dye_argb(16), None);
    }

    /// splitmix64: a fixed-seed generator for the unknown input tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn unknown_network_input_is_none() {
        let mut rng = Rng(0x5eed_0001);
        let types: Vec<Type> = all().collect();
        let ids: BTreeSet<i32> = types.iter().map(|t| i32::from(t.runtime_id())).collect();
        let ominous: Compound = [("Type".to_owned(), Value::Int(1))].into();
        let empty = Compound::new();
        let pick = |rng: &mut Rng| types[usize::try_from(rng.below(1613)).unwrap()];
        for _ in 0..100_000 {
            let base = pick(&mut rng);
            let low = |rng: &mut Rng| i32::try_from(rng.below(64)).unwrap();
            let id = match rng.below(3) {
                0 => i32::from(base.runtime_id()),
                1 => i32::from(base.runtime_id()) + low(&mut rng) - 32,
                _ => i32::from_le_bytes(rng.next().to_le_bytes()[..4].try_into().unwrap()),
            };
            let meta = match rng.below(3) {
                0 => u32::from(base.meta()),
                1 => low(&mut rng).cast_unsigned(),
                _ => u32::try_from(rng.next() >> 32).unwrap(),
            };
            let block = match rng.below(3) {
                0 => block_id(base),
                1 => 0,
                _ => block_id(pick(&mut rng)) ^ low(&mut rng),
            };
            let nbt = if rng.below(2) == 0 { &empty } else { &ominous };
            let found = Type::from_network(id, meta, block, nbt);
            if !ids.contains(&id) {
                assert_eq!(found, None);
            }
            if let Some(t) = found {
                assert_eq!(i32::from(t.runtime_id()), id);
                assert_eq!(block_id(t), block);
                assert!(
                    u16::try_from(meta) == Ok(t.meta()) || {
                        // Only a name and block with one type takes another meta.
                        types
                            .iter()
                            .filter(|o| o.runtime_id() == t.runtime_id() && o.block() == t.block())
                            .count()
                            == 1
                    }
                );
            }
        }
    }
}
