//! Item stacks: immutable values.
//!
//! A stack is an item type, a count, a damage (durability used, or the stack state kept in the
//! meta) and optional extra data (custom name, lore, enchantments and every other NBT key, kept
//! as received). Changing a stack makes a new value; the extra data is shared until changed.
//!
//! A stack has two outside forms, both built from the same item NBT `tag` ([`Stack::tag`]):
//!
//! - network: runtime ID, the meta, the network ID of the block state a block item is sent with
//!   and the tag, with the contents of a stored container reduced to what the client shows;
//! - disk (Bedrock form): `{Name, Count, Damage, Block?, tag?}`, plus `Slot` where a list stores
//!   it.
//!
//! In the tag, `display.Name`, `display.Lore`, the entries of `ench` and an item's durability
//! (`Damage`) are read into the stack's fields; every other key, every `ench` entry of an
//! enchantment Anchor does not know, and any of these that is not in the exact form Anchor
//! writes, stays in [`Extra::nbt`] untouched, so a stack Anchor wrote reads back equal and writes
//! back byte for byte. Extra data a stack is given is read the same way ([`Stack::with_extra`]),
//! so a stack in memory is the stack its outside forms read back as. A `Damage` of 0 is read too,
//! as no wear, and not written back, so it stacks with a new item. NBT that is
//! part of the item type (an ominous banner's `Type`) is not kept in the stack: the type adds it
//! when the tag is written.
//!
//! Limits: a stack is refused where its extra data is set ([`Stack::with_extra`],
//! and the conversions from outside forms) if its saved form is over [`MAX_NBT_BYTES`] or its
//! custom name over [`MAX_NAME_BYTES`]. The limits are sized so that
//! everything the game stores whole fits its record: a player's `MAX_SAVED_STACKS` (`tick::inventory`) stacks and user
//! data in its record, a container's 27 stacks in one NBT root, and a shulker box's 27 stacks of
//! at most [`MAX_STORED_BYTES`] with its name in one stack.

use std::fmt;
use std::sync::Arc;

use tracing::warn;

use super::{Enchantment, Type};
use crate::nbt::{self, Compound, Cost, Encoding, List, Value};
use crate::protocol::{self, Writer};
use crate::tile::MAX_NAME_BYTES;

/// The highest enchantment level a stack keeps: the most its saved `lvl`, a `TAG_Short`, holds.
pub const MAX_ENCHANTMENT_LEVEL: u16 = 32_767;

/// The most a stack's saved form may take from a reader's allocation budget to read back: four
/// times its bytes, so `MAX_SAVED_STACKS` of them read within the 64 MiB a record's reader may allocate. Only
/// NBT of many small compounds comes near it: each takes a tree node (an enchantment takes about
/// 56 times its 16 bytes).
const MAX_NBT_ALLOC: usize = 4 * MAX_NBT_BYTES;
/// The most bytes a stack's saved form may take: its Bedrock item compound with the `Slot` byte
/// an item list adds, at the largest damage, as little-endian NBT. With `MAX_SAVED_STACKS` (`tick::inventory`) such stacks and its user
/// data (16 values of 64 KiB) a player's record stays under 15 MiB, and a container's 27 under
/// 2.6 MiB (one NBT root takes at most 4 MiB).
pub const MAX_NBT_BYTES: usize = 96 << 10;
/// What a stored stack may take from a reader's budget, as [`MAX_NBT_ALLOC`] counts: a 28th of
/// it (13.7 KiB), so 27 of them, a 1,024-byte name and the box's own keys read back within it.
/// The largest vanilla items fit (a firework rocket of seven stars of eight colours, 12.9 KB;
/// armour of nine enchantments with the longest name, 12.4 KB; a custom block item of the
/// longest state, about 7.5 KB); a stack of more enchantments than survival gives (a book of
/// all 18 that go together, 20.4 KB) does not.
const MAX_STORED_ALLOC: usize = MAX_NBT_ALLOC / 28;
/// The most bytes a stack a shulker box holds may take, saved as [`MAX_NBT_BYTES`] counts: 3 KiB,
/// so 27 of them, a 1,024-byte name and the box's own keys (82.4 KiB) make one stack. The largest
/// vanilla items (a firework rocket of seven stars, about 0.9 KB; a custom block item of the
/// longest state, about 2.5 KB) fit.
pub(crate) const MAX_STORED_BYTES: usize = MAX_NBT_BYTES / 32;

/// Block entity data of a block item; the client never reads it, so the network form drops it.
const TAG_BLOCK_ENTITY: &str = "BlockEntityTag";
/// The durability used by an item that wears (`TAG_Int`, written only when above 0).
const TAG_DAMAGE: &str = "Damage";
/// The tag key of a custom name and lore.
const TAG_DISPLAY: &str = "display";
/// The lore in [`TAG_DISPLAY`].
const TAG_DISPLAY_LORE: &str = "Lore";
/// The custom name in [`TAG_DISPLAY`].
const TAG_DISPLAY_NAME: &str = "Name";
/// The enchantment list: compounds of `id` and `lvl`, both `TAG_Short`.
const TAG_ENCH: &str = "ench";
/// The items a container block item holds (a shulker box).
const TAG_ITEMS: &str = "Items";
/// The key in [`Extra::nbt`] that keeps a stack with its owner on death: a `TAG_Byte` of 1.
pub const TAG_KEEP_ON_DEATH: &str = "minecraft:keep_on_death";
/// The key in [`Extra::nbt`] of an armour piece's trim: a compound of [`TAG_TRIM_MATERIAL`] and
/// [`TAG_TRIM_PATTERN`], as smithing writes it.
pub const TAG_TRIM: &str = "Trim";
/// The trim's material in [`TAG_TRIM`]: a `TAG_String`, one of
/// [`TRIM_MATERIALS`](super::TRIM_MATERIALS).
pub const TAG_TRIM_MATERIAL: &str = "Material";
/// The trim's pattern in [`TAG_TRIM`]: a `TAG_String`, one of
/// [`TRIM_PATTERNS`](super::TRIM_PATTERNS).
pub const TAG_TRIM_PATTERN: &str = "Pattern";
/// The key in [`Extra::nbt`] that keeps an item that wears from wearing: a `TAG_Byte` other than
/// 0.
pub const TAG_UNBREAKABLE: &str = "Unbreakable";

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
            count: count.min(item.max_stack_size()),
            damage: 0,
            extra: None,
        }
    }

    /// The same stack with `count` items, limited to the maximum stack size; empty when 0.
    #[doc(alias = "Item::setCount")]
    #[must_use]
    pub fn with_count(&self, count: u8) -> Self {
        match self.item {
            Some(item) if count > 0 => Self {
                count: count.min(item.max_stack_size()),
                ..self.clone()
            },
            _ => Self::EMPTY,
        }
    }

    /// The same stack with `damage`: for an item that wears, the durability used, 0 to its
    /// [`max_durability`](Type::max_durability); for another, the stack state its saved form
    /// keeps (a firework star's colour). Fails with [`Error::InvalidDamage`] for a damage the
    /// saved form would not read back; `self` stays as it is.
    #[doc(alias = "Durable::setDamage")]
    pub fn with_damage(&self, damage: u16) -> Result<Self> {
        let Some(item) = self.item else {
            return Ok(Self::EMPTY);
        };
        if !keeps_damage(item, damage) {
            return Err(Error::InvalidDamage);
        }
        Ok(Self {
            damage,
            ..self.clone()
        })
    }

    /// The same stack with `extra` (`None` removes it), read as its outside forms read the tag
    /// it makes: keys of [`Extra::nbt`] in the exact form of a name, lore, enchantments or (for
    /// an item that wears) a durability become those fields and that damage, where the fields do
    /// not already set them. Fails if the stack would be over a limit ([`MAX_NBT_BYTES`],
    /// [`MAX_NAME_BYTES`]) or an enchantment level is outside 1 to [`MAX_ENCHANTMENT_LEVEL`];
    /// `self` stays as it is.
    pub fn with_extra(&self, extra: Option<Extra>) -> Result<Self> {
        let Some(item) = self.item else {
            return Ok(Self::EMPTY);
        };
        let (fields, nbt) = match extra {
            Some(mut extra) => {
                // Checked before the levels go into the tag, where one that does not read back
                // would stay as a raw `ench` entry.
                check_levels(&extra)?;
                let nbt = std::mem::take(&mut extra.nbt);
                (Some(Arc::new(extra)), nbt)
            }
            None => (None, Compound::new()),
        };
        let given = Self {
            item: self.item,
            count: self.count,
            damage: self.damage,
            extra: fields,
        };
        Self::from_parts(item, self.count, given.meta(), given.tag_with(item, nbt))
    }

    /// `count` items of `item` from the parts of an outside form: its meta and item NBT tag.
    /// The meta is the stack state where it is not the type's (an item that wears reads an old
    /// meta as damage); the tag's known keys are read into the fields. Empty when
    /// `count` is 0; the count is limited to the maximum stack size. Fails as
    /// [`Stack::with_damage`] and [`Stack::with_extra`] do.
    pub(crate) fn from_parts(
        item: Type,
        count: u8,
        meta: u16,
        tag: Option<Compound>,
    ) -> Result<Self> {
        let stack = Self::new(item, count);
        if stack.is_empty() {
            return Ok(stack);
        }
        let (damage, extra) = split_tag(item, tag.unwrap_or_default());
        let damage = match item.max_durability() {
            // An old meta is the damage of an undamaged item.
            Some(max) => damage.unwrap_or(meta.min(max)),
            None if meta != item.meta() => meta,
            None => 0,
        };
        Self {
            extra: Some(extra).filter(|e| *e != Extra::default()).map(Arc::new),
            ..stack.with_damage(damage)?
        }
        .checked()
    }

    /// The inverse of [`Stack::to_network`] for tests: the stack of a network form, or `None` if
    /// Anchor does not know the item or the count is above the item's stack size. The block a
    /// block item is sent with only tells apart types of the same runtime ID and meta.
    #[cfg(test)]
    pub(crate) fn from_network(stack: &protocol::ItemStack) -> Option<Self> {
        let protocol::ItemType {
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
        Self::from_parts(item, count, meta, tag).ok()
    }

    /// The stack a disk item compound ([`Stack::to_nbt`]) holds, knowing the vanilla items and
    /// blocks only, for code without a server such as a data converter run before it starts;
    /// [`Context::stack_from_nbt`](crate::Context::stack_from_nbt) knows the server's custom ones
    /// too. Empty for air or a count of 0; `None` for an unknown item, a compound that is no item
    /// or a stack over a limit. A count above the item's stack size is cut to it.
    pub fn from_nbt(nbt: &Compound) -> Option<Self> {
        Self::read_nbt(nbt, None)
    }

    /// [`Stack::from_nbt`] knowing the server's custom items and blocks too: a custom item and a
    /// custom block's item read back by their name, whatever state a custom block's item saved.
    pub(crate) fn from_nbt_with(nbt: &Compound, custom: &crate::custom::Registry) -> Option<Self> {
        Self::read_nbt(nbt, Some(custom))
    }

    /// Whether the stack holds nothing.
    #[doc(alias = "Item::isNull")]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.item.is_none()
    }

    /// The item type, `None` for an empty stack.
    #[inline]
    pub fn item(&self) -> Option<Type> {
        self.item
    }

    /// How many items the stack holds (0 for an empty stack).
    #[doc(alias = "Item::getCount")]
    #[inline]
    pub fn count(&self) -> u8 {
        self.count
    }

    /// The damage (durability used) or meta state.
    #[doc(alias = "Durable::getDamage")]
    #[inline]
    pub fn damage(&self) -> u16 {
        self.damage
    }

    /// The extra data, if any.
    pub fn extra(&self) -> Option<&Extra> {
        self.extra.as_deref()
    }

    /// The level of enchantment `e` on the stack, `None` without it.
    pub(crate) fn enchantment_level(&self, e: Enchantment) -> Option<u16> {
        self.extra()?
            .enchantments
            .iter()
            .find(|(x, _)| *x == e)
            .map(|&(_, level)| level)
    }

    /// Whether `other` is this stack: `==`, but extra data both stacks share is the same at once,
    /// without walking its NBT (`Extra` holds floats, so it is not `Eq` and `Arc`'s `==` compares
    /// the contents). Floats compare by their bits in `==`, so a NaN equals itself.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.item == other.item
            && self.count == other.count
            && self.damage == other.damage
            && match (&self.extra, &other.extra) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
                (None, None) => true,
                _ => false,
            }
    }

    /// The most items a stack of this type holds (0 for an empty stack).
    #[inline]
    pub fn max_count(&self) -> u8 {
        self.item.map_or(0, Type::max_stack_size)
    }

    /// Whether `other` can join this stack: the same type, damage and extra data.
    /// Empty stacks stack with nothing.
    #[doc(alias = "Item::canStackWith")]
    #[inline]
    pub fn can_stack_with(&self, other: &Self) -> bool {
        self.item.is_some()
            && self.item == other.item
            && self.damage == other.damage
            && self.extra == other.extra
    }

    /// Whether a shulker box's slots take the stack: not a shulker box, and at most
    /// [`MAX_STORED_BYTES`], so that the box with everything it holds is one stack within
    /// [`MAX_NBT_BYTES`]. An empty stack fits.
    pub(crate) fn can_enter_shulker_box(&self) -> bool {
        let Some(item) = self.item else {
            return true;
        };
        let shulker = item
            .block()
            .is_some_and(|b| b.is(crate::block::Class::ShulkerBox));
        // A stack without extra data is its type's form, which every type keeps within the
        // limit (`every_type_without_extra_data_fits_a_shulker_box_slot`), so it is not written.
        !shulker
            && (self.extra.is_none()
                || self
                    .stored_cost(MAX_STORED_BYTES, MAX_STORED_ALLOC)
                    .is_some())
    }

    /// The custom name, or "".
    pub(crate) fn custom_name(&self) -> &str {
        self.extra()
            .and_then(|e| e.custom_name.as_deref())
            .unwrap_or_default()
    }

    /// The meta of the stack's outside forms: 0 for an item that wears (its durability is in the
    /// tag), the stack state where one is kept in the meta (a firework star's colour), the
    /// type's meta otherwise.
    pub(crate) fn meta(&self) -> u16 {
        match self.item {
            Some(item) if item.max_durability().is_some() => 0,
            Some(_) if self.damage != 0 => self.damage,
            Some(item) => item.meta(),
            None => 0,
        }
    }

    /// The item NBT `tag`: the extra NBT with the custom name, lore, enchantments, durability and
    /// the type's own NBT put back in; `None` when it would be empty.
    pub(crate) fn tag(&self) -> Option<Compound> {
        let item = self.item?;
        self.tag_with(
            item,
            self.extra().map(|e| e.nbt.clone()).unwrap_or_default(),
        )
    }

    /// The network form: runtime ID, meta, count, the network ID of the block state a block item
    /// is sent with, and the tag without block entity data and with stored items reduced to
    /// their type, count and name. The air stack for an empty stack.
    pub(crate) fn to_network(&self) -> protocol::ItemStack {
        let Some(item) = self.item else {
            return protocol::ItemStack::default();
        };
        // Built from references, so block entity data and the hidden data of stored items are
        // never copied only to be dropped.
        let nbt = self.extra().map_or_else(Compound::new, |extra| {
            extra
                .nbt
                .iter()
                .filter_map(|(key, value)| {
                    let sent = match (key.as_str(), value) {
                        (TAG_BLOCK_ENTITY, _) => return None,
                        (TAG_ITEMS, Value::List(List::Compound(items))) => {
                            Value::List(List::Compound(items.iter().map(visible_only).collect()))
                        }
                        _ => value.clone(),
                    };
                    Some((key.clone(), sent))
                })
                .collect()
        });
        let nbt_data = self.tag_with(item, nbt).unwrap_or_default();
        super::network_stack_with(item, self.meta(), u16::from(self.count), nbt_data)
    }

    /// The network form with its stack network ID (`ItemInstance`); an empty stack has ID 0.
    pub(crate) fn to_instance(&self, stack_network_id: i32) -> protocol::ItemInstance {
        protocol::ItemInstance {
            stack_network_id: if self.is_empty() { 0 } else { stack_network_id },
            stack: self.to_network(),
        }
    }

    /// The disk form (Bedrock item compound) Anchor saves the stack in, as in an item list:
    /// `{Name, Count, Damage, Block?, tag?}`; the air compound for an empty stack.
    /// [`Context::stack_from_nbt`](crate::Context::stack_from_nbt) reads it back.
    #[doc(alias = "Item::nbtSerialize")]
    pub fn to_nbt(&self) -> Compound {
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

    /// The disk form as the official server writes it in the NBT it sends (trade offers): with
    /// `WasPickedUp` 0 (P6).
    pub(crate) fn to_sent_nbt(&self) -> Compound {
        let mut out = self.to_nbt();
        out.insert("WasPickedUp".to_owned(), Value::Byte(0));
        out
    }

    /// The stack if it is within the limits and its enchantment levels read back. One without
    /// extra data is its type's form (at most a custom block state's 2,293 bytes of NBT), within
    /// them.
    fn checked(self) -> Result<Self> {
        let Some(extra) = &self.extra else {
            return Ok(self);
        };
        check_levels(extra)?;
        if self.custom_name().len() > MAX_NAME_BYTES {
            return Err(Error::NameTooLong);
        }
        if self.stored_cost(MAX_NBT_BYTES, MAX_NBT_ALLOC).is_none() {
            return Err(Error::TooLarge);
        }
        Ok(self)
    }

    /// What the stack takes in an item list (its saved form with a `Slot` byte) at the largest
    /// damage it can take (an item that wears writes its damage, an int, only when above 0);
    /// `None` if that is over `bytes`, reads back with more than `alloc` of a reader's budget, or
    /// cannot be written. Writing stops at `bytes`, so a huge stack costs no more than that.
    fn stored_cost(&self, bytes: usize, alloc: usize) -> Option<Cost> {
        let worn = self.item.is_some_and(|i| i.max_durability().is_some()) && self.damage == 0;
        let mut nbt = if worn {
            Self {
                damage: 1,
                ..self.clone()
            }
            .to_nbt()
        } else {
            self.to_nbt()
        };
        nbt.insert("Slot".to_owned(), Value::Byte(0));
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt_within(&nbt, Encoding::LittleEndian, bytes)
            .ok()
            .filter(|c| c.alloc <= alloc)
    }

    /// The stack of a disk item compound, knowing the vanilla items and blocks and, with
    /// `custom`, the server's own.
    fn read_nbt(nbt: &Compound, custom: Option<&crate::custom::Registry>) -> Option<Self> {
        let Some(Value::String(name)) = nbt.get("Name") else {
            return None;
        };
        let Some(Value::Byte(count)) = nbt.get("Count") else {
            return None;
        };
        if name.is_empty() || name == "minecraft:air" || *count == 0 {
            return Some(Self::EMPTY);
        }
        let meta = match nbt.get("Damage") {
            Some(Value::Short(m)) => u16::try_from(*m).ok()?,
            Some(_) => return None,
            None => 0,
        };
        let block = match nbt.get("Block") {
            Some(Value::Compound(b)) => Some(b),
            Some(_) => return None,
            None => None,
        };
        let tag = match nbt.get("tag") {
            Some(Value::Compound(t)) => Some(t.clone()),
            Some(_) => return None,
            None => None,
        };
        let empty = Compound::new();
        let tag_or_empty = tag.as_ref().unwrap_or(&empty);
        let vanilla = match block {
            Some(b) => crate::save::find_state(b)
                .and_then(|state| Type::from_name(name, meta, Some(state), tag_or_empty)),
            None => Type::from_name(name, meta, None, tag_or_empty),
        };
        // A custom item is known by its name alone: a custom block's item may have saved a state
        // its block no longer has since its definition changed.
        let item = vanilla.or_else(|| custom?.by_identifier(name))?;
        // performance: saved data is outside input, so a stack read back is checked as
        // a new one is: one more write of its NBT, only for a stack with extra data.
        match Self::from_parts(item, *count, meta, tag) {
            Ok(stack) => Some(stack),
            Err(e) => {
                warn!(
                    item = name.as_str(),
                    error = &e as &(dyn std::error::Error + 'static),
                    "a saved stack is refused and reads as unknown"
                );
                None
            }
        }
    }

    /// The item NBT tag of `item` with `tag` as the extra NBT: the type's own NBT where the extra
    /// NBT does not hold its key (it holds one only where the stack differs from its type: a
    /// firework rocket with explosions), the durability, and the fields of the extra data, the
    /// enchantments before the `ench` entries reading left in it; `None` when it would be empty.
    fn tag_with(&self, item: Type, mut tag: Compound) -> Option<Compound> {
        if let Some(static_nbt) = item.nbt() {
            for (key, value) in static_nbt {
                if !tag.contains_key(*key) {
                    tag.insert((*key).to_owned(), value.to_value());
                }
            }
        }
        if item.max_durability().is_some() && self.damage > 0 {
            tag.insert(TAG_DAMAGE.to_owned(), Value::Int(i32::from(self.damage)));
        }
        if let Some(extra) = self.extra() {
            if extra.custom_name.is_some() || !extra.lore.is_empty() {
                let mut display = match tag.remove(TAG_DISPLAY) {
                    Some(Value::Compound(display)) => display,
                    _ => Compound::new(),
                };
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
                let mut list: Vec<Compound> = extra
                    .enchantments
                    .iter()
                    .map(|&(e, level)| {
                        // `checked` keeps every level at most `MAX_ENCHANTMENT_LEVEL`, a short.
                        Compound::from([
                            ("id".to_owned(), Value::Short(e.id())),
                            ("lvl".to_owned(), Value::Short(level.cast_signed())),
                        ])
                    })
                    .collect();
                if let Some(Value::List(List::Compound(left))) = tag.remove(TAG_ENCH) {
                    list.extend(left);
                }
                tag.insert(TAG_ENCH.to_owned(), Value::List(List::Compound(list)));
            }
        }
        (!tag.is_empty()).then_some(tag)
    }
}

/// The data a stack carries besides its type, count and damage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Extra {
    /// The name shown instead of the item's name (`display.Name`).
    #[doc(alias = "Item::getCustomName")]
    pub custom_name: Option<String>,
    /// Lines under the name (`display.Lore`).
    #[doc(alias = "Item::getLore")]
    pub lore: Vec<String>,
    /// Enchantments and their levels (the `ench` entries of enchantments Anchor knows), in the
    /// order applied. A level is from 1 to [`MAX_ENCHANTMENT_LEVEL`].
    #[doc(alias = "Item::getEnchantments")]
    pub enchantments: Vec<(Enchantment, u16)>,
    /// Every other NBT key of the stack, kept as received (book pages, fireworks, patterns…),
    /// and the `ench` entries of enchantments Anchor does not know.
    #[doc(alias = "Item::getNamedTag")]
    pub nbt: Compound,
}

/// Why a stack was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Its saved form is over [`MAX_NBT_BYTES`], or of so many tiny elements that reading it back
    /// takes more than four times that, or it cannot be saved at all (a string over 32,767
    /// bytes, nesting deeper than 512).
    TooLarge,
    /// Its custom name is over [`MAX_NAME_BYTES`] bytes, the most a placed block keeps.
    NameTooLong,
    /// Its damage is one its saved form does not read back: for an item that wears, above its
    /// durability; for another, other than 0, one above 32,767, its type's own meta, or any where
    /// its name and block have more types than its own (a potion).
    InvalidDamage,
    /// An enchantment level is 0 or above [`MAX_ENCHANTMENT_LEVEL`], which its saved form does not
    /// read back.
    InvalidEnchantmentLevel,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(
                f,
                "the stack's saved form is over {MAX_NBT_BYTES} bytes, too costly to read back \
                 or not savable"
            ),
            Self::NameTooLong => write!(f, "the custom name is over {MAX_NAME_BYTES} bytes"),
            Self::InvalidDamage => f.write_str("the item cannot keep that damage"),
            Self::InvalidEnchantmentLevel => write!(
                f,
                "an enchantment level is not from 1 to {MAX_ENCHANTMENT_LEVEL}"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Checks that every enchantment level of `extra` is from 1 to [`MAX_ENCHANTMENT_LEVEL`], which
/// its saved form reads back.
fn check_levels(extra: &Extra) -> Result<()> {
    if extra
        .enchantments
        .iter()
        .any(|&(_, level)| !(1..=MAX_ENCHANTMENT_LEVEL).contains(&level))
    {
        return Err(Error::InvalidEnchantmentLevel);
    }
    Ok(())
}

/// Whether the outside forms of `item` keep `damage`: the saved form reads back as the same type
/// with the same damage. An item that wears writes it in its tag, read back up to its durability;
/// another writes it as its meta (a `TAG_Short`), read back as stack state only where it is not
/// the type's own meta and the name and block have no other type ([`Type::from_name`]); a custom
/// type is found by its name alone.
fn keeps_damage(item: Type, damage: u16) -> bool {
    if let Some(max) = item.max_durability() {
        return damage <= max;
    }
    damage == 0
        || (i16::try_from(damage).is_ok()
            && damage != item.meta()
            && Type::from_name(item.name(), damage, item.block(), &Compound::new()).map_or_else(
                || item.custom_item().is_some() || item.custom_block().is_some(),
                |t| t == item,
            ))
}

/// Reads the known keys of an item tag: the durability of an item that wears (0 too, which
/// [`Stack::tag`] leaves out), the custom name, the lore and the enchantments, each only in the
/// exact form [`Stack::tag`] writes; the rest, without the type's own NBT, is the extra NBT.
fn split_tag(item: Type, mut tag: Compound) -> (Option<u16>, Extra) {
    if let Some(static_nbt) = item.nbt() {
        for entry in static_nbt {
            // Compared by bits, without building the type's NBT.
            if nbt::contains(&tag, std::slice::from_ref(entry)) {
                tag.remove(entry.0);
            }
        }
    }
    let mut damage = None;
    if let (Some(max), Some(Value::Int(d))) = (item.max_durability(), tag.get(TAG_DAMAGE))
        && let Ok(d) = u16::try_from(*d)
        && d <= max
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
    if let Some(Value::List(List::Compound(list))) = tag.get_mut(TAG_ENCH) {
        let mut left = Vec::new();
        for entry in std::mem::take(list) {
            match read_enchantment(&entry) {
                Some(enchantment) => extra.enchantments.push(enchantment),
                None => left.push(entry),
            }
        }
        *list = left;
        if list.is_empty() && !extra.enchantments.is_empty() {
            tag.remove(TAG_ENCH);
        }
    }
    extra.nbt = tag;
    (damage, extra)
}

/// The enchantment of an `ench` entry that is exactly `{id, lvl}` of a known enchantment and a
/// level above 0.
fn read_enchantment(entry: &Compound) -> Option<(Enchantment, u16)> {
    if entry.len() != 2 {
        return None;
    }
    let (Some(Value::Short(id)), Some(Value::Short(level))) = (entry.get("id"), entry.get("lvl"))
    else {
        return None;
    };
    let level = u16::try_from(*level).ok().filter(|l| *l > 0)?;
    Some((Enchantment::from_id(*id)?, level))
}

/// What the client shows of a stored item compound: its type, count, slot and custom name.
fn visible_only(stored: &Compound) -> Compound {
    let mut visible: Compound = stored
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "Name" | "Count" | "Damage" | "Block" | "Slot"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(Value::Compound(tag)) = stored.get("tag")
        && let Some(Value::Compound(display)) = tag.get(TAG_DISPLAY)
        && let Some(name) = display.get(TAG_DISPLAY_NAME)
    {
        let display = Compound::from([(TAG_DISPLAY_NAME.to_owned(), name.clone())]);
        let tag = Compound::from([(TAG_DISPLAY.to_owned(), Value::Compound(display))]);
        visible.insert("tag".to_owned(), Value::Compound(tag));
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::item::Kind;

    /// The enchantment ID of looting.
    const LOOTING: i16 = 14;

    fn kind(k: Kind) -> Type {
        k.default_type()
    }

    fn named(stack: &Stack, name: &str) -> Stack {
        stack
            .with_extra(Some(Extra {
                custom_name: Some(name.to_owned()),
                ..Extra::default()
            }))
            .unwrap()
    }

    /// The largest vanilla items a shulker box takes: a firework rocket of seven stars of eight
    /// colours and eight fade colours, and armour with nine enchantments (more than survival
    /// gives one piece) at their highest level, the longest name and its prior work.
    #[test]
    fn the_largest_vanilla_items_fit_a_shulker_box() {
        let star = Compound::from([
            ("FireworkColor".to_owned(), Value::ByteArray(vec![1; 8])),
            ("FireworkFade".to_owned(), Value::ByteArray(vec![1; 8])),
            ("FireworkFlicker".to_owned(), Value::Byte(1)),
            ("FireworkTrail".to_owned(), Value::Byte(1)),
            ("FireworkType".to_owned(), Value::Byte(4)),
        ]);
        let fireworks = Compound::from([
            (
                "Explosions".to_owned(),
                Value::List(List::Compound(vec![star; 7])),
            ),
            ("Flight".to_owned(), Value::Byte(3)),
        ]);
        let rocket = Type::from_name("minecraft:firework_rocket", 0, None, &Compound::new())
            .expect("the rocket is an item");
        let rocket = Stack::new(rocket, 1)
            .with_extra(Some(Extra {
                nbt: Compound::from([("Fireworks".to_owned(), Value::Compound(fireworks))]),
                ..Extra::default()
            }))
            .unwrap();
        let enchantments = [
            Enchantment::Protection,
            Enchantment::FeatherFalling,
            Enchantment::FrostWalker,
            Enchantment::SwiftSneak,
            Enchantment::Unbreaking,
            Enchantment::Mending,
            Enchantment::Thorns,
            Enchantment::Vanishing,
            Enchantment::AquaAffinity,
        ];
        let boots = Stack::new(kind(Kind::NetheriteBoots), 1)
            .with_extra(Some(Extra {
                custom_name: Some("n".repeat(MAX_NAME_BYTES)),
                enchantments: enchantments.map(|e| (e, 5)).to_vec(),
                nbt: Compound::from([("RepairCost".to_owned(), Value::Int(39))]),
                ..Extra::default()
            }))
            .unwrap();
        assert!(rocket.can_enter_shulker_box());
        assert!(boots.can_enter_shulker_box());
    }

    /// Every vanilla type without extra data keeps within what a shulker box's slot takes, at the
    /// largest damage too, so `can_enter_shulker_box` takes such a stack without writing it.
    #[test]
    fn every_type_without_extra_data_fits_a_shulker_box_slot() {
        for i in 0..u16::try_from(crate::item::table::ITEMS.len()).unwrap() {
            let stack = Stack::new(Type::from_index(i), 1);
            assert!(
                stack
                    .stored_cost(MAX_STORED_BYTES, MAX_STORED_ALLOC)
                    .is_some(),
                "{stack:?}"
            );
        }
    }

    #[test]
    fn same_is_equality_with_shared_data_always_the_same() {
        let stick = Stack::new(kind(Kind::Stick), 1);
        let a = named(&stick, "a");
        assert!(a.same(&a.clone()));
        assert!(a.same(&named(&stick, "a")), "equal data apart");
        assert!(!a.same(&named(&stick, "b")));
        assert!(!a.same(&stick) && !stick.same(&a));
        assert!(!a.same(&a.with_count(2)));
        let nan = stick
            .with_extra(Some(Extra {
                nbt: Compound::from([("x".to_owned(), Value::Float(f32::NAN))]),
                ..Extra::default()
            }))
            .unwrap();
        assert_eq!(nan, nan.clone());
        assert!(nan.same(&nan.clone()));
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
        let stack = Stack::new(pick, 1)
            .with_damage(42)
            .unwrap()
            .with_extra(Some(extra))
            .unwrap();
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

    /// A level is any above 0 that the saved `lvl` (a `TAG_Short`) holds: one
    /// above 255 reads back from both forms; 0 and one past a short are refused.
    #[test]
    fn enchantment_levels_are_the_positive_shorts() {
        let sword = Stack::new(kind(Kind::DiamondSword), 1);
        let with = |level| {
            sword.with_extra(Some(Extra {
                enchantments: vec![(Enchantment::Sharpness, level)],
                ..Extra::default()
            }))
        };
        for level in [256, MAX_ENCHANTMENT_LEVEL] {
            let stack = with(level).unwrap();
            assert_eq!(stack.enchantment_level(Enchantment::Sharpness), Some(level));
            assert_eq!(Stack::from_nbt(&stack.to_nbt()).as_ref(), Some(&stack));
            assert_eq!(Stack::from_network(&stack.to_network()), Some(stack));
        }
        for level in [0, MAX_ENCHANTMENT_LEVEL + 1] {
            assert_eq!(with(level), Err(Error::InvalidEnchantmentLevel), "{level}");
        }
    }

    /// An `ench` list with an enchantment Anchor does not know keeps that entry as it is and
    /// reads the others into the fields: the known ones count, and an enchantment added later
    /// keeps the unknown entry after it.
    #[test]
    fn unknown_enchantment_entries_stay_and_known_ones_count() {
        assert_eq!(Enchantment::from_id(LOOTING), None, "looting is not known");
        let entry = |id: i16, level: i16| {
            Compound::from([
                ("id".to_owned(), Value::Short(id)),
                ("lvl".to_owned(), Value::Short(level)),
            ])
        };
        let sharpness = entry(Enchantment::Sharpness.id(), 5);
        let looting = entry(LOOTING, 3);
        let mut disk = Stack::new(kind(Kind::DiamondSword), 1).to_nbt();
        let list = vec![sharpness.clone(), looting.clone()];
        let tag = Compound::from([(TAG_ENCH.to_owned(), Value::List(List::Compound(list)))]);
        disk.insert("tag".to_owned(), Value::Compound(tag));
        let stack = Stack::from_nbt(&disk).expect("a known item reads back");
        assert_eq!(stack.enchantment_level(Enchantment::Sharpness), Some(5));
        assert_eq!(stack.to_nbt(), disk);
        let mut extra = stack.extra().cloned().unwrap();
        extra.enchantments.push((Enchantment::Unbreaking, 1));
        let more = stack.with_extra(Some(extra)).unwrap();
        let unbreaking = entry(Enchantment::Unbreaking.id(), 1);
        let written = more.tag().expect("the tag is written");
        assert_eq!(
            written.get(TAG_ENCH),
            Some(&Value::List(List::Compound(vec![
                sharpness, unbreaking, looting
            ])))
        );
    }

    /// Extra data whose NBT holds a name, enchantments or a durability in the form a tag stores
    /// them is the stack that tag reads back as: the name limit holds for it, the enchantments
    /// count, the damage is the durability used, and the stack reads back equal.
    #[test]
    fn extra_nbt_in_the_form_of_known_keys_is_read_into_the_fields() {
        let sword = Stack::new(kind(Kind::DiamondSword), 1);
        let name = Value::String("n".repeat(MAX_NAME_BYTES + 1));
        let display = Compound::from([(TAG_DISPLAY_NAME.to_owned(), name)]);
        let long = Extra {
            nbt: Compound::from([(TAG_DISPLAY.to_owned(), Value::Compound(display))]),
            ..Extra::default()
        };
        assert_eq!(sword.with_extra(Some(long)), Err(Error::NameTooLong));
        let sharpness = Compound::from([
            ("id".to_owned(), Value::Short(Enchantment::Sharpness.id())),
            ("lvl".to_owned(), Value::Short(5)),
        ]);
        let raw = Extra {
            nbt: Compound::from([
                (
                    TAG_ENCH.to_owned(),
                    Value::List(List::Compound(vec![sharpness])),
                ),
                (TAG_DAMAGE.to_owned(), Value::Int(7)),
            ]),
            ..Extra::default()
        };
        let stack = sword.with_extra(Some(raw)).unwrap();
        assert_eq!(stack.enchantment_level(Enchantment::Sharpness), Some(5));
        assert_eq!(stack.damage(), 7);
        assert_eq!(stack.extra().map(|e| e.nbt.is_empty()), Some(true));
        assert_eq!(Stack::from_nbt(&stack.to_nbt()), Some(stack));
    }

    /// A tag `Damage` of 0 reads as no wear (over an old meta too) and is not written, so such
    /// a stack equals and stacks with a new one.
    #[test]
    fn a_tag_damage_of_zero_is_no_wear_and_is_not_written() {
        let fresh = Stack::new(kind(Kind::DiamondSword), 1);
        for meta in [0, 5] {
            let mut disk = fresh.to_nbt();
            disk.insert("Damage".to_owned(), Value::Short(meta));
            let tag = Compound::from([("Damage".to_owned(), Value::Int(0))]);
            disk.insert("tag".to_owned(), Value::Compound(tag));
            let read = Stack::from_nbt(&disk).unwrap();
            assert_eq!(read, fresh, "meta {meta}");
            assert!(read.can_stack_with(&fresh));
            assert_eq!(read.to_nbt(), fresh.to_nbt(), "no Damage key written");
        }
    }

    // api: Durable::setDamage
    #[test]
    fn a_damage_set_through_the_api_reads_back_from_disk() {
        for i in 0..u16::try_from(crate::item::table::ITEMS.len()).unwrap() {
            let item = Type::from_index(i);
            let max = item.max_durability().unwrap_or(0);
            let damages = [
                1,
                5,
                100,
                item.meta(),
                max,
                max.saturating_add(1),
                32_767,
                32_768,
            ];
            for damage in damages {
                if let Ok(stack) = Stack::new(item, 1).with_damage(damage) {
                    assert_eq!(
                        Stack::from_nbt(&stack.to_nbt()).as_ref(),
                        Some(&stack),
                        "{item:?} {damage}"
                    );
                }
            }
        }
        let refused = [
            (kind(Kind::Potion), 100),
            (kind(Kind::Potion), 5),
            (kind(Kind::Stick), 40_000),
            (kind(Kind::DiamondSword), 5000),
        ];
        for (item, damage) in refused {
            assert_eq!(
                Stack::new(item, 1).with_damage(damage),
                Err(Error::InvalidDamage),
                "{item:?} {damage}"
            );
        }
        let kept = [
            (kind(Kind::Stick), 32_767),
            (kind(Kind::DiamondSword), 1561),
            (kind(Kind::Potion), 0),
        ];
        for (item, damage) in kept {
            let stack = Stack::new(item, 1).with_damage(damage).unwrap();
            assert_eq!(stack.damage(), damage);
            assert_eq!(Stack::from_nbt(&stack.to_nbt()), Some(stack));
        }
    }

    // api: Item, Item::getCount, Item::setCount, Item::isNull, Item::canStackWith,
    // api: Durable::getDamage, Durable::setDamage, Item::getCustomName, Item::getLore,
    // api: Item::getEnchantments, Item::getNamedTag
    /// A stack's getters read what its `with_*` methods set, and a change makes a new value: the
    /// count is kept to the type's stack size and 0 empties the stack, the damage and extra data
    /// are kept apart.
    #[test]
    fn getters_read_what_the_with_methods_set() {
        let pearls = Stack::new(kind(Kind::EnderPearl), 10);
        assert!(!pearls.is_empty() && Stack::EMPTY.is_empty());
        assert_eq!(pearls.count(), 10);
        assert_eq!(pearls.with_count(3).count(), 3);
        assert_eq!(
            pearls.with_count(64).count(),
            16,
            "an ender pearl stacks to 16"
        );
        assert!(pearls.with_count(0).is_empty());
        assert_eq!(pearls.count(), 10, "a change makes a new stack");
        assert!(Stack::EMPTY.with_count(5).is_empty());
        assert!(pearls.can_stack_with(&pearls.with_count(1)));
        assert!(!Stack::EMPTY.can_stack_with(&Stack::EMPTY));

        let sword = Stack::new(kind(Kind::DiamondSword), 1);
        assert_eq!(sword.damage(), 0);
        let worn = sword.with_damage(100).unwrap();
        assert_eq!(worn.damage(), 100);
        assert!(
            !worn.can_stack_with(&sword),
            "a different damage does not stack"
        );
        assert!(Stack::EMPTY.with_damage(5).unwrap().is_empty());

        let extra = Extra {
            custom_name: Some("Edge".to_owned()),
            lore: vec!["sharp".to_owned()],
            enchantments: vec![(Enchantment::Sharpness, 3)],
            nbt: Compound::from([("custom".to_owned(), Value::Byte(1))]),
        };
        let named = worn.with_extra(Some(extra.clone())).unwrap();
        let got = named.extra().expect("the extra data is kept");
        assert_eq!(got.custom_name.as_deref(), Some("Edge"));
        assert_eq!(got.lore, ["sharp"]);
        assert_eq!(got.enchantments, [(Enchantment::Sharpness, 3)]);
        assert_eq!(got.nbt.get("custom"), Some(&Value::Byte(1)));
        assert_eq!(named.damage(), 100);
        assert_eq!(named.with_extra(None).unwrap().extra(), None);
        assert_eq!(
            worn.with_extra(Some(Extra::default())).unwrap().extra(),
            None
        );
        // The tag holds the extra data with the damage and reads back the same fields.
        let back = Stack::from_parts(kind(Kind::DiamondSword), 1, 0, named.tag()).unwrap();
        assert_eq!(back, named);
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
        let kept = Stack::from_parts(stick, 1, 0, Some(tag.clone())).unwrap();
        assert_eq!(kept.extra().unwrap().nbt, tag);
        assert_eq!(kept.tag(), Some(tag));
    }

    /// A firework rocket's type NBT is `Fireworks` with no explosions; a rocket from the creative
    /// inventory carries its own `Fireworks` with explosions, which the stack keeps.
    #[test]
    fn a_stack_keeps_its_own_value_of_a_key_its_type_fixes() {
        let rocket = Type::from_name("minecraft:firework_rocket", 0, None, &Compound::new())
            .expect("the rocket is an item");
        let fixed = nbt::compound(rocket.nbt().expect("the rocket's type has NBT"));
        let explosion = Compound::from([("FireworkType".to_owned(), Value::Byte(1))]);
        let fireworks = Compound::from([
            (
                "Explosions".to_owned(),
                Value::List(List::Compound(vec![explosion])),
            ),
            ("Flight".to_owned(), Value::Byte(2)),
        ]);
        let tag = Compound::from([("Fireworks".to_owned(), Value::Compound(fireworks))]);
        assert_ne!(fixed, tag, "the stack's fireworks differ from the type's");
        let stack = Stack::from_parts(rocket, 1, 0, Some(tag.clone())).unwrap();
        assert_eq!(stack.tag(), Some(tag.clone()));
        assert_eq!(stack.to_network().nbt_data, tag);
        assert_eq!(Stack::from_nbt(&stack.to_nbt()), Some(stack));
        // A plain rocket still gets the type's NBT.
        assert_eq!(Stack::new(rocket, 1).tag(), Some(fixed));
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
        let crate::block::ItemForm::Item(planks) =
            crate::block::Kind::OakPlanks.default_state().item()
        else {
            panic!("oak planks have an item form");
        };
        let planks = Stack::new(planks, 1);
        let mut net = planks.to_network();
        net.block_runtime_id = 12_345;
        assert_eq!(Stack::from_network(&net), Some(planks));
    }

    #[test]
    fn stored_items_are_sent_without_their_hidden_data() {
        let mut inner = named(&Stack::new(kind(Kind::Stick), 2), "x")
            .with_damage(0)
            .unwrap()
            .to_nbt();
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
        let stack = Stack::new(kind(Kind::Stick), 1)
            .with_extra(Some(extra))
            .unwrap();
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

    /// `item` with a byte array of `len` bytes as its extra NBT.
    fn padded(item: Type, len: usize) -> Result<Stack> {
        Stack::new(item, 1).with_extra(Some(Extra {
            nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![1; len]))]),
            ..Extra::default()
        }))
    }

    /// The longest pad that keeps `item` within the stack limit.
    fn longest_pad(item: Type) -> usize {
        let (mut lo, mut hi) = (0, MAX_NBT_BYTES);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if padded(item, mid).is_ok() {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// A stack's saved form, with a `Slot`, at the largest damage, is what the limit counts: one
    /// byte over it, a name over 1,024 bytes, a string NBT cannot hold or many tiny elements
    /// are refused; a stack read from disk over the limit reads as unknown.
    #[test]
    fn the_limits_count_the_saved_form_at_the_largest_damage() {
        let stick = kind(Kind::Stick);
        let pad = longest_pad(stick);
        let largest = padded(stick, pad).unwrap();
        let mut nbt = largest.to_nbt();
        nbt.insert("Slot".to_owned(), Value::Byte(0));
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(&nbt, Encoding::LittleEndian).unwrap();
        assert_eq!(w.as_bytes().len(), MAX_NBT_BYTES);
        assert_eq!(padded(stick, pad + 1), Err(Error::TooLarge));
        assert_eq!(Stack::from_nbt(&largest.to_nbt()), Some(largest));
        let mut over = padded(stick, pad).unwrap().to_nbt();
        if let Some(Value::Compound(tag)) = over.get_mut("tag") {
            tag.insert("pad".to_owned(), Value::ByteArray(vec![1; pad + 1]));
        }
        assert_eq!(Stack::from_nbt(&over), None);
        // An item that wears keeps room for its damage, so wearing never passes the limit.
        let sword = kind(Kind::DiamondSword);
        let sword_pad = longest_pad(sword);
        let new = padded(sword, sword_pad).unwrap();
        let mut worn = new.with_damage(1).unwrap().to_nbt();
        worn.insert("Slot".to_owned(), Value::Byte(0));
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(&worn, Encoding::LittleEndian).unwrap();
        assert_eq!(w.as_bytes().len(), MAX_NBT_BYTES);
        let name = |len: usize| {
            Stack::new(stick, 1).with_extra(Some(Extra {
                custom_name: Some("n".repeat(len)),
                ..Extra::default()
            }))
        };
        assert!(name(MAX_NAME_BYTES).is_ok());
        assert_eq!(name(MAX_NAME_BYTES + 1), Err(Error::NameTooLong));
        let long_string = Extra {
            lore: vec!["l".repeat(40_000)],
            ..Extra::default()
        };
        assert_eq!(
            Stack::new(stick, 1).with_extra(Some(long_string)),
            Err(Error::TooLarge)
        );
        // 20,000 empty compounds take 20 KB but about 480 KB to read back.
        let tiny = Extra {
            nbt: Compound::from([(
                "l".to_owned(),
                Value::List(List::Compound(vec![Compound::new(); 20_000])),
            )]),
            ..Extra::default()
        };
        assert_eq!(
            Stack::new(stick, 1).with_extra(Some(tiny)),
            Err(Error::TooLarge)
        );
        assert_eq!(Stack::EMPTY.with_extra(None), Ok(Stack::EMPTY));
    }

    /// A shulker box takes no shulker box, nothing over a 32nd of the stack's byte limit and
    /// nothing over a 28th of what it may take to read back; 27 of the largest it takes of either
    /// and the longest name make a box item within the limit.
    #[test]
    fn a_shulker_box_takes_a_32nd_of_the_limit_and_a_full_box_fits_one_stack() {
        let stick = kind(Kind::Stick);
        let (mut lo, mut hi) = (0, MAX_STORED_BYTES);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if padded(stick, mid).unwrap().can_enter_shulker_box() {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let most = padded(stick, lo).unwrap();
        assert!(!padded(stick, lo + 1).unwrap().can_enter_shulker_box());
        assert!(Stack::EMPTY.can_enter_shulker_box());
        let shulker = crate::block::State::all()
            .find(|b| b.is(crate::block::Class::ShulkerBox))
            .unwrap();
        let crate::block::ItemForm::Item(box_type) = shulker.item() else {
            panic!("a shulker box item");
        };
        assert!(!Stack::new(box_type, 1).can_enter_shulker_box());
        // Compounds of one key cost little to write and a tree node each to read.
        let small = |n: usize| {
            let one = Compound::from([("k".to_owned(), Value::Byte(0))]);
            Stack::new(stick, 1)
                .with_extra(Some(Extra {
                    nbt: Compound::from([(
                        "l".to_owned(),
                        Value::List(List::Compound(vec![one; n])),
                    )]),
                    ..Extra::default()
                }))
                .unwrap()
        };
        // Each takes more than 700 bytes of the budget: fewer than 20 fit.
        let (mut lo, mut hi) = (0, 20_usize);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if small(mid).can_enter_shulker_box() {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let heaviest = small(lo);
        assert!(heaviest.stored_cost(MAX_STORED_BYTES, usize::MAX).is_some());
        for most in [most, heaviest] {
            let items = (0_u8..27)
                .map(|slot| {
                    let mut nbt = most.to_nbt();
                    nbt.insert("Slot".to_owned(), Value::Byte(slot));
                    nbt
                })
                .collect();
            let full = Stack::new(box_type, 1).with_extra(Some(Extra {
                custom_name: Some("n".repeat(MAX_NAME_BYTES)),
                nbt: Compound::from([(TAG_ITEMS.to_owned(), Value::List(List::Compound(items)))]),
                ..Extra::default()
            }));
            assert!(full.is_ok());
        }
    }
}
