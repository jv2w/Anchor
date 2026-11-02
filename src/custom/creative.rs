//! Creative inventories: what the creative inventory screen lists. A server has a default
//! inventory (the vanilla list, or the one given to
//! [`ServerBuilder::creative_inventory`](crate::ServerBuilder::creative_inventory), then the
//! places custom items and blocks ask for) and may register named alternatives that a player is
//! switched to while the server runs
//! ([`Context::set_creative_inventory`](crate::Context::set_creative_inventory)). Every inventory
//! is fixed when the server is built and encoded once at start.
//!
//! The packet follows the official server's: an entry's group is numbered when the
//! group first shows in its category, later entries of the same category and group join it
//! wherever they are, a category's entries without a group share one anonymous group until an
//! entry of a named group of that category starts a new one, and a named group's icon carries
//! the group's index ([`item::GROUP_WORKAROUND`]). Two groups are the same group when their names
//! and icons are equal. Creative item network IDs count from 1, as the official server sends them.

use std::collections::BTreeMap;

use super::{Category, Creative, Error, ItemRef, Registry, Result};
use crate::item::{self, CreativeEntry as Entry, Stack, Type};
use crate::nbt::Value;
use crate::protocol::{self, packet};

/// The most alternative creative inventories.
pub(crate) const MAX_ALTERNATIVES: usize = 64;

/// The most changes of one creative inventory.
const MAX_CHANGES: usize = 65_536;

/// The most entries of one creative inventory (the vanilla list and every custom item fit).
pub(crate) const MAX_ENTRIES: usize = 8192;

/// The longest creative group or inventory name, in bytes.
const MAX_NAME_BYTES: usize = 128;

/// A creative inventory to register: the vanilla list or an empty one, then the changes made to
/// it in order. The server resolves it when it is built (where an item that is unknown, or
/// custom and not registered, refuses the build) and never changes it afterwards. The default is
/// the vanilla list.
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
// api-class: CreativeInventory
pub struct CreativeInventory {
    vanilla: bool,
    changes: Vec<Change>,
}

impl CreativeInventory {
    /// An inventory without entries.
    pub fn empty() -> Self {
        Self {
            vanilla: false,
            changes: Vec::new(),
        }
    }

    /// The vanilla creative inventory without the Education Edition items.
    pub fn vanilla() -> Self {
        Self {
            vanilla: true,
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

    /// Removes every entry.
    #[doc(alias = "CreativeInventory::clear")]
    #[must_use]
    pub fn clear(mut self) -> Self {
        self.changes.push(Change::Clear);
        self
    }

    /// Removes the first entry of `item`'s type and damage (the NBT is not compared); nothing if
    /// there is none.
    #[doc(alias = "CreativeInventory::remove")]
    #[doc(alias = "CreativeInventoryEntry::matchesItem")]
    #[must_use]
    pub fn remove(mut self, item: impl Into<CreativeItem>) -> Self {
        self.changes.push(Change::Remove(item.into()));
        self
    }

    /// The inventory with its changes applied, then the `places` custom items and blocks ask for
    /// (the default inventory's only). Fails on an unknown or unregistered item, an empty or
    /// too long group name, or too many entries or changes.
    pub(crate) fn resolve(
        &self,
        custom: &Registry,
        places: &[(Type, Creative)],
    ) -> Result<Resolved> {
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

impl Default for CreativeInventory {
    fn default() -> Self {
        Self::vanilla()
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

/// A named group of creative entries, shown folded under its icon. The name is a translation key
/// such as `itemGroup.name.sword` or plain text; it must not be empty (checked when the server
/// is built).
#[derive(Debug, Clone)]
// api-class: CreativeGroup
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

    /// The group's icon.
    #[doc(alias = "CreativeGroup::getIcon")]
    pub fn icon(&self) -> &CreativeItem {
        &self.icon
    }

    /// The group's name.
    #[doc(alias = "CreativeGroup::getName")]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

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
    /// The vanilla inventory unchanged.
    pub(crate) fn vanilla() -> Self {
        Self::new(vanilla_entries())
    }

    /// The same inventory registered as the alternative `name`.
    pub(crate) fn with_name(mut self, name: &'static str) -> Self {
        self.name = Some(name);
        self
    }

    /// The `CreativeContent` packet.
    pub(crate) fn content(&self) -> &packet::CreativeContent {
        &self.content
    }

    /// The name of an alternative inventory; `None` for the default one.
    pub(crate) fn name(&self) -> Option<&'static str> {
        self.name
    }

    /// The stack of creative item network ID `network_id`, one item.
    pub(crate) fn stack(&self, network_id: u32) -> Option<&Stack> {
        self.stacks
            .get(usize::try_from(network_id).ok()?.checked_sub(1)?)
    }

    /// The entries' stacks, by network ID from 1.
    pub(crate) fn stacks(&self) -> &[Stack] {
        &self.stacks
    }

    /// Numbers `entries` from 1 and groups them by the rule in the module docs.
    #[doc(alias = "CreativeInventoryCache::buildPacket")]
    #[expect(
        clippy::expect_used,
        reason = "invariant: an inventory has at most `MAX_ENTRIES` entries, so at most as many \
                  groups"
    )]
    fn new(entries: Vec<Entry>) -> Self {
        let mut groups: Vec<protocol::CreativeGroup> = Vec::new();
        // A named group: its name and its icon without the group index.
        let mut distinct: Vec<(&'static str, protocol::ItemStack)> = Vec::new();
        // The group index of each (category, distinct group); `None` is the anonymous group.
        let mut index: BTreeMap<(u8, Option<usize>), u32> = BTreeMap::new();
        let mut stacks = Vec::with_capacity(entries.len());
        let mut items = Vec::with_capacity(entries.len());
        for (id, entry) in (1_u32..).zip(entries) {
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
                let at = u32::try_from(groups.len()).expect("a group count fits a u32");
                groups.push(match key.and_then(|k| distinct.get(k)) {
                    Some((name, icon)) => {
                        let mut icon = icon.clone();
                        icon.nbt_data.insert(
                            item::GROUP_WORKAROUND.to_owned(),
                            Value::Int(i32::try_from(at).expect("a group index fits an i32")),
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
}

/// `name` if it can name a creative group or inventory ([`is_name`]).
pub(crate) fn checked_name(name: &'static str) -> Result<&'static str> {
    if !is_name(name) {
        return Err(Error::Creative {
            reason: "a creative group or inventory name that is empty or too long",
        });
    }
    Ok(name)
}

/// Whether `name` can name a creative group or inventory: not empty, at most [`MAX_NAME_BYTES`]
/// bytes.
pub(crate) fn is_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES
}

/// The stack a player takes for `item` (one) and its network form.
fn resolve_item(custom: &Registry, item: &CreativeItem) -> Result<(Stack, protocol::ItemStack)> {
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

/// The error of an inventory with more than [`MAX_ENTRIES`] entries.
fn too_many_entries() -> Error {
    Error::TooMany {
        what: "entries of a creative inventory",
        limit: MAX_ENTRIES,
    }
}

/// The vanilla entries, as generated, without the Education items ([`item::is_education`]). The
/// item table keeps those, so a stack that still exists (an old save, a world's element block
/// broken) stays readable to the client.
fn vanilla_entries() -> Vec<Entry> {
    (0..)
        .map_while(item::creative_entry)
        .filter(|e| !e.stack.item().is_some_and(|t| item::is_education(t.name())))
        .collect()
}
