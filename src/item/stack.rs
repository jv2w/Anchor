//! Item stacks: immutable values (PLAN.md 8.1, 10.7).
//!
//! A stack is an item type, a count, a damage (durability used, or the stack state PMMP keeps in
//! the meta) and optional extra data (custom name, lore, enchantments and every other NBT key,
//! kept as received). Changing a stack makes a new value; the extra data is shared until changed.
//!
//! A stack has two outside forms, both built from the same item NBT `tag` ([`Stack::tag`]):
//!
//! - network (PMMP `TypeConverter::coreItemStackToNet`): runtime ID, the meta, the network ID of
//!   the block state a block item is sent with (PLAN.md 10.4) and the tag, with the contents of
//!   a stored container reduced to what the client shows;
//! - disk (PMMP `SavedItemStackData`, Bedrock form): `{Name, Count, Damage, Block?, tag?}`, plus
//!   `Slot` where a list stores it.
//!
//! In the tag, `display.Name`, `display.Lore`, `ench` and an item's durability (`Damage`) are
//! read into the stack's fields; every other key, and any of these that is not in the exact form
//! Anchor writes, stays in [`Extra::nbt`] untouched, so a stack Anchor wrote reads back equal and
//! writes back byte for byte. NBT that is part of the item type (an ominous banner's `Type`) is
//! not kept in the stack: the type adds it when the tag is written.

use std::sync::Arc;

use super::{Enchantment, Type};
use crate::nbt::{self, Compound, List, Value};
use crate::protocol::{self, ItemType};

/// The tag key of a custom name and lore.
const TAG_DISPLAY: &str = "display";
/// The custom name in [`TAG_DISPLAY`].
const TAG_DISPLAY_NAME: &str = "Name";
/// The lore in [`TAG_DISPLAY`].
const TAG_DISPLAY_LORE: &str = "Lore";
/// The enchantment list: compounds of `id` and `lvl`, both `TAG_Short`.
const TAG_ENCH: &str = "ench";
/// The durability used by an item that wears (`TAG_Int`, written only when above 0).
const TAG_DAMAGE: &str = "Damage";
/// Block entity data of a block item; the client never reads it (PMMP strips it).
const TAG_BLOCK_ENTITY: &str = "BlockEntityTag";
/// The items a container block item holds (a shulker box).
const TAG_ITEMS: &str = "Items";

/// A stack of items, or nothing (an empty slot).
#[doc(alias = "Item")]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stack {
    /// `None` for an empty stack.
    item: Option<Type>,
    /// 1 to the type's maximum stack size; 0 exactly when `item` is `None`.
    count: u8,
    damage: u16,
    extra: Option<Arc<Extra>>,
}

/// The data a stack carries besides its type, count and damage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Extra {
    /// The name shown instead of the item's name (`display.Name`).
    pub custom_name: Option<String>,
    /// Lines under the name (`display.Lore`).
    pub lore: Vec<String>,
    /// Enchantments and their levels (`ench`), in the order applied.
    pub enchantments: Vec<(Enchantment, u8)>,
    /// Every other NBT key of the stack, kept as received (book pages, fireworks, patterns…).
    pub nbt: Compound,
}

impl Stack {
    /// The empty stack.
    pub const EMPTY: Self = Self {
        item: None,
        count: 0,
        damage: 0,
        extra: None,
    };

    /// `count` items of `item` without damage or extra data; empty when `count` is 0. The count
    /// is limited to the type's maximum stack size.
    pub fn new(item: Type, count: u8) -> Self {
        if count == 0 {
            return Self::EMPTY;
        }
        Self {
            item: Some(item),
            count: count.min(item.max_stack_size().max(1)),
            damage: 0,
            extra: None,
        }
    }

    /// Whether the stack holds nothing.
    pub fn is_empty(&self) -> bool {
        self.item.is_none()
    }

    /// The item type, `None` for an empty stack.
    pub fn item(&self) -> Option<Type> {
        self.item
    }

    /// How many items the stack holds (0 for an empty stack).
    pub fn count(&self) -> u8 {
        self.count
    }

    /// The damage (durability used) or meta state.
    pub fn damage(&self) -> u16 {
        self.damage
    }

    /// The extra data, if any.
    pub fn extra(&self) -> Option<&Extra> {
        self.extra.as_deref()
    }

    /// The most items a stack of this type holds (0 for an empty stack).
    pub fn max_count(&self) -> u8 {
        self.item.map_or(0, Type::max_stack_size)
    }

    /// The same stack with `count` items, limited to the maximum stack size; empty when 0.
    #[must_use]
    pub fn with_count(&self, count: u8) -> Self {
        match self.item {
            Some(item) if count > 0 => Self {
                count: count.min(item.max_stack_size().max(1)),
                ..self.clone()
            },
            _ => Self::EMPTY,
        }
    }

    /// The same stack with `damage`.
    #[must_use]
    pub fn with_damage(&self, damage: u16) -> Self {
        if self.is_empty() {
            return Self::EMPTY;
        }
        Self {
            damage,
            ..self.clone()
        }
    }

    /// The same stack with `extra` (`None` removes it).
    #[must_use]
    pub fn with_extra(&self, extra: Option<Extra>) -> Self {
        if self.is_empty() {
            return Self::EMPTY;
        }
        Self {
            extra: extra.filter(|e| *e != Extra::default()).map(Arc::new),
            ..self.clone()
        }
    }

    /// Whether `other` can join this stack: the same type, damage and extra data (PMMP
    /// `Item::canStackWith`). Empty stacks stack with nothing.
    pub fn can_stack_with(&self, other: &Self) -> bool {
        self.item.is_some()
            && self.item == other.item
            && self.damage == other.damage
            && self.extra == other.extra
    }

    /// The custom name, or "" (PMMP `Item::getCustomName`).
    pub(crate) fn custom_name(&self) -> &str {
        self.extra()
            .and_then(|e| e.custom_name.as_deref())
            .unwrap_or_default()
    }

    /// The meta of the stack's outside forms: 0 for an item that wears (its durability is in the
    /// tag), the stack state where PMMP keeps one in the meta (a firework star's colour), the
    /// type's meta otherwise.
    pub(crate) fn meta(&self) -> u16 {
        match self.item {
            None => 0,
            Some(item) if item.max_durability().is_some() => 0,
            Some(_) if self.damage != 0 => self.damage,
            Some(item) => item.meta(),
        }
    }

    /// `count` items of `item` from the parts of an outside form: its meta and item NBT tag.
    /// The meta is the stack state where it is not the type's (an item that wears reads an old
    /// meta as damage, as PMMP does); the tag's known keys are read into the fields. Empty when
    /// `count` is 0; the count is limited to the maximum stack size.
    pub(crate) fn from_parts(item: Type, count: u8, meta: u16, tag: Option<Compound>) -> Self {
        let stack = Self::new(item, count);
        if stack.is_empty() {
            return stack;
        }
        let durability = item.max_durability();
        let (damage, extra) = split_tag(item, durability, tag.unwrap_or_default());
        let damage = match durability {
            // PMMP `ItemDeserializer`: an old meta is the damage of an undamaged item.
            Some(max) => damage.unwrap_or(if meta > 0 { meta.min(max) } else { 0 }),
            None if meta != item.meta() => meta,
            None => 0,
        };
        stack.with_damage(damage).with_extra(Some(extra))
    }

    /// The item NBT `tag`: the extra NBT with the custom name, lore, enchantments, durability and
    /// the type's own NBT put back in; `None` when it would be empty.
    pub(crate) fn tag(&self) -> Option<Compound> {
        let item = self.item?;
        let mut tag = self.extra().map(|e| e.nbt.clone()).unwrap_or_default();
        if let Some(static_nbt) = item.nbt() {
            for (key, value) in nbt::compound(static_nbt) {
                tag.insert(key, value);
            }
        }
        if item.max_durability().is_some() && self.damage > 0 {
            tag.insert(TAG_DAMAGE.to_owned(), Value::Int(i32::from(self.damage)));
        }
        if let Some(extra) = self.extra() {
            if extra.custom_name.is_some() || !extra.lore.is_empty() {
                let display = match tag.remove(TAG_DISPLAY) {
                    Some(Value::Compound(display)) => display,
                    _ => Compound::new(),
                };
                let mut display = display;
                if let Some(name) = &extra.custom_name {
                    display.insert(TAG_DISPLAY_NAME.to_owned(), Value::String(name.clone()));
                }
                if !extra.lore.is_empty() {
                    let lore = Value::List(List::String(extra.lore.clone()));
                    display.insert(TAG_DISPLAY_LORE.to_owned(), lore);
                }
                tag.insert(TAG_DISPLAY.to_owned(), Value::Compound(display));
            }
            if !extra.enchantments.is_empty() {
                let list = extra
                    .enchantments
                    .iter()
                    .map(|&(e, level)| {
                        Compound::from([
                            ("id".to_owned(), Value::Short(i16::from(e.id()))),
                            ("lvl".to_owned(), Value::Short(i16::from(level))),
                        ])
                    })
                    .collect();
                tag.insert(TAG_ENCH.to_owned(), Value::List(List::Compound(list)));
            }
        }
        (!tag.is_empty()).then_some(tag)
    }

    /// The network form (PMMP `TypeConverter::coreItemStackToNet`): runtime ID, meta, count, the
    /// network ID of the block state a block item is sent with, and the tag without block entity
    /// data and with stored items reduced to their type, count and name. The air stack for an
    /// empty stack.
    pub(crate) fn to_network(&self) -> protocol::ItemStack {
        let Some(item) = self.item else {
            return protocol::ItemStack::default();
        };
        let mut nbt_data = self.tag().unwrap_or_default();
        nbt_data.remove(TAG_BLOCK_ENTITY);
        if let Some(Value::List(List::Compound(items))) = nbt_data.get_mut(TAG_ITEMS) {
            for stored in items.iter_mut() {
                visible_only(stored);
            }
        }
        protocol::ItemStack {
            item_type: ItemType {
                network_id: i32::from(item.runtime_id()),
                metadata_value: u32::from(self.meta()),
            },
            // The network field is the hash's bit pattern as an int32.
            block_runtime_id: item.block_network_id().cast_signed(),
            count: u16::from(self.count),
            nbt_data,
            can_be_placed_on: Vec::new(),
            can_break: Vec::new(),
            blocking_tick: 0,
        }
    }

    /// The network form with its stack network ID (`ItemInstance`); an empty stack has ID 0.
    pub(crate) fn to_instance(&self, stack_network_id: i32) -> protocol::ItemInstance {
        protocol::ItemInstance {
            stack_network_id: if self.is_empty() { 0 } else { stack_network_id },
            stack: self.to_network(),
        }
    }

    /// The stack a client sent in network form, or `None` if Anchor does not know the item or
    /// the count is above the item's stack size. The block a block item is sent with only tells
    /// apart types of the same runtime ID and meta; a client's different block runtime ID for an
    /// item without a block form is not a reason to refuse it (P6 facts 2).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consistency: the M8 packet hooks decode client items (PMMP \
                      `TypeConverter::netItemStackToCore`); the tests check the network form \
                      round-trips"
        )
    )]
    pub(crate) fn from_network(stack: &protocol::ItemStack) -> Option<Self> {
        let ItemType {
            network_id,
            metadata_value,
        } = stack.item_type;
        if network_id == 0 || stack.count == 0 {
            return Some(Self::EMPTY);
        }
        let nbt = &stack.nbt_data;
        let item =
            Type::from_network_any_block(network_id, metadata_value, stack.block_runtime_id, nbt)?;
        let count = u8::try_from(stack.count)
            .ok()
            .filter(|c| *c <= item.max_stack_size())?;
        let meta = u16::try_from(metadata_value).unwrap_or(item.meta());
        let tag = (!nbt.is_empty()).then(|| nbt.clone());
        Some(Self::from_parts(item, count, meta, tag))
    }

    /// The disk form (Bedrock item compound): `{Name, Count, Damage, Block?, tag?}`; the air
    /// compound for an empty stack.
    pub(crate) fn to_nbt(&self) -> Compound {
        let Some(item) = self.item else {
            return Compound::from([
                ("Count".to_owned(), Value::Byte(0)),
                ("Damage".to_owned(), Value::Short(0)),
                ("Name".to_owned(), Value::String(String::new())),
            ]);
        };
        let mut out = Compound::from([
            ("Count".to_owned(), Value::Byte(self.count)),
            ("Damage".to_owned(), Value::Short(self.meta().cast_signed())),
            ("Name".to_owned(), Value::String(item.name().to_owned())),
        ]);
        if let Some(block) = item.block() {
            out.insert(
                "Block".to_owned(),
                Value::Compound(crate::save::state_nbt(block)),
            );
        }
        if let Some(tag) = self.tag() {
            out.insert("tag".to_owned(), Value::Compound(tag));
        }
        out
    }

    /// The disk form as BDS writes it in the NBT it sends (a furnace's `LastFuel`, trade offers):
    /// with `WasPickedUp` 0 (P6 facts 3 and 6).
    pub(crate) fn to_sent_nbt(&self) -> Compound {
        let mut out = self.to_nbt();
        out.insert("WasPickedUp".to_owned(), Value::Byte(0));
        out
    }

    /// The stack a disk item compound holds: empty for air or a count of 0, `None` if Anchor does
    /// not know the item or the compound is not an item. Counts above the stack size are cut to
    /// it.
    pub(crate) fn from_nbt(nbt: &Compound) -> Option<Self> {
        let Some(Value::String(name)) = nbt.get("Name") else {
            return None;
        };
        let count = match nbt.get("Count") {
            Some(Value::Byte(c)) => *c,
            _ => return None,
        };
        if name.is_empty() || name == "minecraft:air" || count == 0 {
            return Some(Self::EMPTY);
        }
        let meta = match nbt.get("Damage") {
            Some(Value::Short(m)) => u16::try_from(*m).ok()?,
            None => 0,
            Some(_) => return None,
        };
        let block = match nbt.get("Block") {
            Some(Value::Compound(b)) => Some(crate::save::find_state(b)?),
            None => None,
            Some(_) => return None,
        };
        let tag = match nbt.get("tag") {
            Some(Value::Compound(t)) => Some(t.clone()),
            None => None,
            Some(_) => return None,
        };
        let empty = Compound::new();
        let item = Type::from_name(name, meta, block, tag.as_ref().unwrap_or(&empty))?;
        Some(Self::from_parts(item, count, meta, tag))
    }
}

/// Reads the known keys of an item tag (PMMP `Item::deserializeCompoundTag`): the durability of
/// an item that wears, the custom name, the lore and the enchantments, each only in the exact
/// form [`Stack::tag`] writes; the rest, without the type's own NBT, is the extra NBT.
fn split_tag(item: Type, durability: Option<u16>, mut tag: Compound) -> (Option<u16>, Extra) {
    if let Some(static_nbt) = item.nbt() {
        for (key, value) in nbt::compound(static_nbt) {
            if tag.get(&key) == Some(&value) {
                tag.remove(&key);
            }
        }
    }
    let mut damage = None;
    if let (Some(max), Some(Value::Int(d))) = (durability, tag.get(TAG_DAMAGE))
        && let Ok(d) = u16::try_from(*d)
        && (1..=max).contains(&d)
    {
        damage = Some(d);
        tag.remove(TAG_DAMAGE);
    }
    let mut extra = Extra::default();
    if let Some(Value::Compound(display)) = tag.get_mut(TAG_DISPLAY) {
        if let Some(Value::String(name)) = display.get(TAG_DISPLAY_NAME)
            && !name.is_empty()
        {
            extra.custom_name = Some(name.clone());
            display.remove(TAG_DISPLAY_NAME);
        }
        if let Some(Value::List(List::String(lore))) = display.get(TAG_DISPLAY_LORE)
            && !lore.is_empty()
        {
            extra.lore.clone_from(lore);
            display.remove(TAG_DISPLAY_LORE);
        }
        if display.is_empty() {
            tag.remove(TAG_DISPLAY);
        }
    }
    if let Some(Value::List(List::Compound(list))) = tag.get(TAG_ENCH)
        && let Some(enchantments) = read_enchantments(list)
    {
        extra.enchantments = enchantments;
        tag.remove(TAG_ENCH);
    }
    extra.nbt = tag;
    (damage, extra)
}

/// The enchantments of an `ench` list if every entry is exactly `{id, lvl}` of a known
/// enchantment and a level from 1 to 255, and the list is not empty.
fn read_enchantments(list: &[Compound]) -> Option<Vec<(Enchantment, u8)>> {
    if list.is_empty() {
        return None;
    }
    list.iter()
        .map(|entry| {
            if entry.len() != 2 {
                return None;
            }
            let (Some(Value::Short(id)), Some(Value::Short(level))) =
                (entry.get("id"), entry.get("lvl"))
            else {
                return None;
            };
            let level = u8::try_from(*level).ok().filter(|l| *l > 0)?;
            Some((Enchantment::from_id(*id)?, level))
        })
        .collect()
}

/// Reduces a stored item compound to what the client shows of it (PMMP
/// `TypeConverter::stripContainedItemNonVisualNBT`): its type, count, slot and custom name.
fn visible_only(stored: &mut Compound) {
    let name = match stored.get("tag") {
        Some(Value::Compound(tag)) => match tag.get(TAG_DISPLAY) {
            Some(Value::Compound(display)) => display.get(TAG_DISPLAY_NAME).cloned(),
            _ => None,
        },
        _ => None,
    };
    stored.retain(|k, _| matches!(k.as_str(), "Name" | "Count" | "Damage" | "Block" | "Slot"));
    if let Some(name) = name {
        let display = Compound::from([(TAG_DISPLAY_NAME.to_owned(), name)]);
        let tag = Compound::from([(TAG_DISPLAY.to_owned(), Value::Compound(display))]);
        stored.insert("tag".to_owned(), Value::Compound(tag));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Kind;

    fn kind(k: Kind) -> Type {
        k.default_type()
    }

    fn named(stack: &Stack, name: &str) -> Stack {
        stack.with_extra(Some(Extra {
            custom_name: Some(name.to_owned()),
            ..Extra::default()
        }))
    }

    #[test]
    fn every_item_type_round_trips_through_disk_and_network() {
        for i in 0..u16::try_from(crate::item::table::ITEMS.len()).unwrap() {
            let item = Type::from_index(i);
            let stack = Stack::new(item, item.max_stack_size());
            let back = Stack::from_nbt(&stack.to_nbt()).expect("a known item reads back");
            assert_eq!(back.to_nbt(), stack.to_nbt(), "{item:?}");
            let net = stack.to_network();
            let back = Stack::from_network(&net).expect("a known item reads back");
            assert_eq!(back.to_network(), net, "{item:?}");
        }
    }

    #[test]
    fn durability_custom_name_lore_and_enchantments_round_trip() {
        let pick = kind(Kind::DiamondPickaxe);
        let extra = Extra {
            custom_name: Some("Digger".to_owned()),
            lore: vec!["one".to_owned(), "two".to_owned()],
            enchantments: vec![(Enchantment::from_id(15).unwrap(), 5)],
            nbt: Compound::from([("custom".to_owned(), Value::Int(7))]),
        };
        let stack = Stack::new(pick, 1).with_damage(42).with_extra(Some(extra));
        let disk = stack.to_nbt();
        let Some(Value::Compound(tag)) = disk.get("tag") else {
            panic!("the tag is written");
        };
        assert_eq!(tag.get("Damage"), Some(&Value::Int(42)));
        assert_eq!(disk.get("Damage"), Some(&Value::Short(0)));
        assert_eq!(Stack::from_nbt(&disk), Some(stack.clone()));
        let net = stack.to_network();
        assert_eq!(net.item_type.metadata_value, 0);
        assert_eq!(Stack::from_network(&net), Some(stack));
    }

    #[test]
    fn unknown_forms_of_known_keys_stay_in_the_extra_nbt() {
        let stick = kind(Kind::Stick);
        let tag = Compound::from([
            (
                "display".to_owned(),
                Value::Compound(Compound::from([
                    ("Name".to_owned(), Value::Int(3)),
                    ("Lore".to_owned(), Value::List(List::String(Vec::new()))),
                ])),
            ),
            (
                "ench".to_owned(),
                Value::List(List::Compound(vec![Compound::from([(
                    "id".to_owned(),
                    Value::Short(9999),
                )])])),
            ),
        ]);
        let kept = Stack::from_parts(stick, 1, 0, Some(tag.clone()));
        assert_eq!(kept.extra().unwrap().nbt, tag);
        assert_eq!(kept.tag(), Some(tag));
    }

    #[test]
    fn named_stacks_stack_only_with_the_same_name() {
        let stick = Stack::new(kind(Kind::Stick), 3);
        let a = named(&stick, "a");
        assert!(a.can_stack_with(&named(&stick, "a")));
        assert!(!a.can_stack_with(&stick));
        assert_eq!(a.custom_name(), "a");
        assert_eq!(stick.custom_name(), "");
    }

    #[test]
    fn bad_disk_compounds_are_refused_and_air_is_empty() {
        let air = Stack::EMPTY.to_nbt();
        assert_eq!(Stack::from_nbt(&air), Some(Stack::EMPTY));
        let unknown = Compound::from([
            (
                "Name".to_owned(),
                Value::String("minecraft:nope".to_owned()),
            ),
            ("Count".to_owned(), Value::Byte(1)),
        ]);
        assert_eq!(Stack::from_nbt(&unknown), None);
        let wrong = Compound::from([
            (
                "Name".to_owned(),
                Value::String("minecraft:stick".to_owned()),
            ),
            ("Count".to_owned(), Value::Int(1)),
        ]);
        assert_eq!(Stack::from_nbt(&wrong), None);
        let over = Compound::from([
            (
                "Name".to_owned(),
                Value::String("minecraft:stick".to_owned()),
            ),
            ("Count".to_owned(), Value::Byte(200)),
        ]);
        assert_eq!(Stack::from_nbt(&over).map(|s| s.count()), Some(64));
    }

    #[test]
    fn a_block_item_sent_with_another_block_id_is_still_known() {
        let crate::block::ItemForm::Item(table) =
            crate::block::Kind::CraftingTable.default_state().item()
        else {
            panic!("a crafting table has an item form");
        };
        let table = Stack::new(table, 1);
        let mut net = table.to_network();
        net.block_runtime_id = 12345;
        assert_eq!(Stack::from_network(&net), Some(table));
    }

    #[test]
    fn stored_items_are_sent_without_their_hidden_data() {
        let inner = named(&Stack::new(kind(Kind::Stick), 2), "x")
            .with_damage(0)
            .to_nbt();
        let mut inner = inner;
        inner.insert("Slot".to_owned(), Value::Byte(3));
        if let Some(Value::Compound(tag)) = inner.get_mut("tag") {
            tag.insert("secret".to_owned(), Value::Int(1));
        }
        let extra = Extra {
            nbt: Compound::from([
                ("Items".to_owned(), Value::List(List::Compound(vec![inner]))),
                (
                    "BlockEntityTag".to_owned(),
                    Value::Compound(Compound::new()),
                ),
            ]),
            ..Extra::default()
        };
        let stack = Stack::new(kind(Kind::Stick), 1).with_extra(Some(extra));
        let net = stack.to_network().nbt_data;
        assert!(!net.contains_key("BlockEntityTag"));
        let Some(Value::List(List::Compound(items))) = net.get("Items") else {
            panic!("the items are sent");
        };
        let Some(Value::Compound(tag)) = items[0].get("tag") else {
            panic!("the name is sent");
        };
        assert!(!tag.contains_key("secret"));
        assert_eq!(items[0].get("Slot"), Some(&Value::Byte(3)));
    }
}
