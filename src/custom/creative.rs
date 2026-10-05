//! Creative inventories (PMMP `CreativeInventory`, PLAN.md 10.4, 10.8): what the creative
//! inventory screen lists. A server has a default inventory (PMMP's list, or the one given to
//! [`ServerBuilder::creative_inventory`](crate::ServerBuilder::creative_inventory), then the
//! places custom items and blocks ask for) and may register named alternatives that a player is
//! switched to while the server runs
//! ([`Context::set_creative_inventory`](crate::Context::set_creative_inventory)). Every inventory
//! is fixed when the server is built and encoded once at start.
//!
//! The packet follows PMMP's `CreativeInventoryCache`: an entry's group is numbered when the
//! group first shows in its category, later entries of the same category and group join it
//! wherever they are, a category's entries without a group share one anonymous group until an
//! entry of a named group of that category starts a new one, and a named group's icon carries
//! the group's index ([`item::GROUP_WORKAROUND`]). Two groups are the same group when their names
//! and icons are equal. Creative item network IDs count from 1, as BDS sends them.

use std::collections::BTreeMap;

use super::{Category, Creative, Error, ItemRef, Registry};
use crate::item::{self, CreativeEntry as Entry, Stack, Type};
use crate::nbt::Value;
use crate::protocol::{self, packet};

/// The most entries of one creative inventory (PMMP's 1,633 and every custom item fit).
pub(crate) const MAX_ENTRIES: usize = 8192;
/// The most changes of one creative inventory.
const MAX_CHANGES: usize = 65_536;
/// The most alternative creative inventories.
pub(crate) const MAX_ALTERNATIVES: usize = 64;
/// The longest creative group or inventory name, in bytes.
pub(crate) const MAX_NAME: usize = 128;

/// What a creative entry or a group's icon shows: an item named before the server numbers the
/// custom items, or a whole stack with its NBT (an enchanted book's enchantments). Its count
/// does not matter: an entry is one item.
#[derive(Debug, Clone)]
pub enum CreativeItem {
    /// An item type, vanilla or custom.
    Item(ItemRef),
    /// A stack of a vanilla item type.
    Stack(Stack),
}

impl From<ItemRef> for CreativeItem {
    fn from(item: ItemRef) -> Self {
        Self::Item(item)
    }
}

impl From<Stack> for CreativeItem {
    fn from(stack: Stack) -> Self {
        Self::Stack(stack)
    }
}

/// A named group of creative entries, shown folded under its icon (PMMP `CreativeGroup`).
/// The name is a translation key such as `itemGroup.name.sword` or plain text; it must not be
/// empty (checked when the server is built).
#[derive(Debug, Clone)]
// pmmp-class: CreativeGroup
pub struct CreativeGroup {
    name: &'static str,
    icon: CreativeItem,
}

impl CreativeGroup {
    /// The group `name` shown with `icon`.
    #[doc(alias = "CreativeGroup::__construct")]
    pub fn new(name: &'static str, icon: impl Into<CreativeItem>) -> Self {
        Self {
            name,
            icon: icon.into(),
        }
    }

    /// The group's name.
    #[doc(alias = "CreativeGroup::getName")]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The group's icon.
    #[doc(alias = "CreativeGroup::getIcon")]
    pub fn icon(&self) -> &CreativeItem {
        &self.icon
    }
}

/// One change of a creative inventory, applied in order when the server is built.
#[derive(Debug, Clone)]
enum Change {
    Add {
        item: CreativeItem,
        category: Category,
        group: Option<CreativeGroup>,
    },
    Remove(CreativeItem),
    Clear,
}

/// A creative inventory to register (PMMP `CreativeInventory`): PMMP's list or an empty one,
/// then the changes made to it in order. The server resolves it when it is built (where an
/// item that is unknown, or custom and not registered, refuses the build) and never changes it
/// afterwards. The default is PMMP's list.
///
/// ```
/// use anchor::custom::{Category, CreativeGroup, CreativeInventory, ItemRef};
/// use anchor::item::Kind;
///
/// let sword = ItemRef::Item(Kind::DiamondSword.default_type());
/// // A kit menu: two swords in one group, then an apple.
/// let kits = CreativeInventory::empty()
///     .add(sword, Category::Equipment, Some(CreativeGroup::new("Swords", sword)))
///     .add(
///         ItemRef::Item(Kind::IronSword.default_type()),
///         Category::Equipment,
///         Some(CreativeGroup::new("Swords", sword)),
///     )
///     .add(ItemRef::Item(Kind::Apple.default_type()), Category::Items, None);
/// ```
#[derive(Debug, Clone)]
// pmmp-class: CreativeInventory
pub struct CreativeInventory {
    vanilla: bool,
    changes: Vec<Change>,
}

impl Default for CreativeInventory {
    fn default() -> Self {
        Self::vanilla()
    }
}

impl CreativeInventory {
    /// PMMP's creative inventory: 1,633 entries in 107 groups.
    pub fn vanilla() -> Self {
        Self {
            vanilla: true,
            changes: Vec::new(),
        }
    }

    /// An inventory without entries.
    pub fn empty() -> Self {
        Self {
            vanilla: false,
            changes: Vec::new(),
        }
    }

    /// Adds `item` at the end, in `category` and in `group` (none: the category's anonymous
    /// group).
    #[doc(alias = "CreativeInventory::add")]
    #[must_use]
    pub fn add(
        mut self,
        item: impl Into<CreativeItem>,
        category: Category,
        group: Option<CreativeGroup>,
    ) -> Self {
        self.changes.push(Change::Add {
            item: item.into(),
            category,
            group,
        });
        self
    }

    /// Removes the first entry of `item`'s type and damage (PMMP
    /// `CreativeInventoryEntry::matchesItem`: the NBT is not compared); nothing if there is none.
    #[doc(alias = "CreativeInventory::remove")]
    #[doc(alias = "CreativeInventoryEntry::matchesItem")]
    #[must_use]
    pub fn remove(mut self, item: impl Into<CreativeItem>) -> Self {
        self.changes.push(Change::Remove(item.into()));
        self
    }

    /// Removes every entry.
    #[doc(alias = "CreativeInventory::clear")]
    #[must_use]
    pub fn clear(mut self) -> Self {
        self.changes.push(Change::Clear);
        self
    }

    /// The inventory with its changes applied, then the `places` custom items and blocks ask for
    /// (the default inventory's only). Fails on an unknown or unregistered item, an empty or
    /// too long group name, or too many entries or changes.
    pub(crate) fn resolve(
        &self,
        custom: &Registry,
        places: &[(Type, Creative)],
    ) -> Result<Resolved, Error> {
        if self.changes.len() > MAX_CHANGES {
            return Err(Error::TooMany {
                what: "changes of a creative inventory",
                limit: MAX_CHANGES,
            });
        }
        let mut entries = if self.vanilla {
            vanilla_entries()
        } else {
            Vec::new()
        };
        if !custom.has_education_items() {
            entries.retain(|e| !e.stack.item().is_some_and(|t| item::is_education(t.name())));
        }
        for change in &self.changes {
            match change {
                Change::Add {
                    item,
                    category,
                    group,
                } => {
                    let (stack, net) = resolve_item(custom, item)?;
                    let group = match group {
                        Some(g) => Some((checked_name(g.name)?, resolve_item(custom, &g.icon)?.1)),
                        None => None,
                    };
                    entries.push(Entry {
                        stack,
                        net,
                        category: category.id(),
                        group,
                    });
                }
                Change::Remove(item) => {
                    let (want, _) = resolve_item(custom, item)?;
                    if let Some(at) = entries.iter().position(|e| {
                        e.stack.item() == want.item() && e.stack.damage() == want.damage()
                    }) {
                        entries.remove(at);
                    }
                }
                Change::Clear => entries.clear(),
            }
            if entries.len() > MAX_ENTRIES {
                return Err(too_many_entries());
            }
        }
        // A custom place's group shows the first custom item of that category and group.
        let mut icons: Vec<(Category, &'static str, protocol::ItemStack)> = Vec::new();
        for &(t, place) in places {
            let stack = Stack::new(t, 1);
            let net = stack.to_network();
            let group = place.group.map(|name| {
                let first = icons
                    .iter()
                    .find(|(c, n, _)| *c == place.category && *n == name);
                let icon = if let Some((_, _, icon)) = first {
                    icon.clone()
                } else {
                    icons.push((place.category, name, net.clone()));
                    net.clone()
                };
                (name, icon)
            });
            entries.push(Entry {
                stack,
                net,
                category: place.category.id(),
                group,
            });
        }
        if entries.len() > MAX_ENTRIES {
            return Err(too_many_entries());
        }
        Ok(Resolved::new(entries))
    }
}

/// The error of an inventory with more than [`MAX_ENTRIES`] entries.
fn too_many_entries() -> Error {
    Error::TooMany {
        what: "entries of a creative inventory",
        limit: MAX_ENTRIES,
    }
}

/// `name` if it can name a creative group or inventory: not empty, at most [`MAX_NAME`] bytes.
pub(crate) fn checked_name(name: &'static str) -> Result<&'static str, Error> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Error::Creative {
            reason: "a creative group or inventory name that is empty or too long",
        });
    }
    Ok(name)
}

/// The stack a player takes for `item` (one) and its network form.
fn resolve_item(
    custom: &Registry,
    item: &CreativeItem,
) -> Result<(Stack, protocol::ItemStack), Error> {
    let stack = match item {
        CreativeItem::Item(r) => custom.resolve(*r).map(|t| Stack::new(t, 1)),
        CreativeItem::Stack(s) => Some(s.with_count(1)).filter(|s| !s.is_empty()),
    }
    .ok_or(Error::Creative {
        reason: "an entry or icon names an unknown or unregistered item, or no item",
    })?;
    let net = stack.to_network();
    Ok((stack, net))
}

/// PMMP's entries, as generated.
fn vanilla_entries() -> Vec<Entry> {
    (0..).map_while(item::creative_entry).collect()
}

/// A creative inventory as the server holds it: the stack of each creative item network ID and
/// the `CreativeContent` packet.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The name of an alternative inventory; `None` for the default one.
    name: Option<&'static str>,
    /// The stack of network ID `i + 1` at `i`.
    stacks: Vec<Stack>,
    content: packet::CreativeContent,
}

impl Resolved {
    /// PMMP's inventory unchanged.
    pub(crate) fn vanilla() -> Self {
        Self::new(vanilla_entries())
    }

    /// Numbers `entries` from 1 and groups them by PMMP's rule (see the module).
    #[doc(alias = "CreativeInventoryCache::buildPacket")]
    fn new(entries: Vec<Entry>) -> Self {
        let mut groups: Vec<protocol::CreativeGroup> = Vec::new();
        // A named group: its name and its icon without the group index.
        let mut distinct: Vec<(&'static str, protocol::ItemStack)> = Vec::new();
        // The group index of each (category, distinct group); `None` is the anonymous group.
        let mut index: BTreeMap<(u8, Option<usize>), u32> = BTreeMap::new();
        let mut stacks = Vec::with_capacity(entries.len());
        let mut items = Vec::with_capacity(entries.len());
        for (id, entry) in (1u32..).zip(entries) {
            let Entry {
                stack,
                net,
                category,
                group,
            } = entry;
            let key = group.map(|g| {
                distinct.iter().position(|d| *d == g).unwrap_or_else(|| {
                    distinct.push(g);
                    distinct.len() - 1
                })
            });
            if key.is_some() {
                // A named group ends the category's anonymous group.
                index.remove(&(category, None));
            }
            let group_index = *index.entry((category, key)).or_insert_with(|| {
                let at = u32::try_from(groups.len()).unwrap_or(u32::MAX);
                groups.push(match key.and_then(|k| distinct.get(k)) {
                    Some((name, icon)) => {
                        let mut icon = icon.clone();
                        icon.nbt_data.insert(
                            item::GROUP_WORKAROUND.to_owned(),
                            Value::Int(i32::try_from(at).unwrap_or(i32::MAX)),
                        );
                        protocol::CreativeGroup {
                            category,
                            name: (*name).to_owned(),
                            icon,
                        }
                    }
                    None => protocol::CreativeGroup {
                        category,
                        name: String::new(),
                        icon: protocol::ItemStack::default(),
                    },
                });
                at
            });
            stacks.push(stack);
            items.push(protocol::CreativeItem {
                creative_item_network_id: id,
                item: net,
                group_index,
            });
        }
        Self {
            name: None,
            stacks,
            content: packet::CreativeContent { groups, items },
        }
    }

    /// The same inventory registered as the alternative `name`.
    pub(crate) fn with_name(mut self, name: &'static str) -> Self {
        self.name = Some(name);
        self
    }

    /// The name of an alternative inventory; `None` for the default one.
    pub(crate) fn name(&self) -> Option<&'static str> {
        self.name
    }

    /// The `CreativeContent` packet.
    pub(crate) fn content(&self) -> &packet::CreativeContent {
        &self.content
    }

    /// The entries' stacks, by network ID from 1.
    pub(crate) fn stacks(&self) -> &[Stack] {
        &self.stacks
    }

    /// The stack of creative item network ID `network_id`, one item.
    pub(crate) fn stack(&self, network_id: u32) -> Option<&Stack> {
        self.stacks
            .get(usize::try_from(network_id).ok()?.checked_sub(1)?)
    }
}
