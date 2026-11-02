//! The custom content of one server: what the builder registered, checked
//! and numbered when the server is built, and every lookup the engine needs of it.
//!
//! Numbers follow registration order, so they are the same on every start: custom items and
//! custom blocks' items take the runtime IDs after the last vanilla one, custom blocks the block
//! IDs from 10,000 (dragonfly). Every custom state gets its network hash; one equal to another
//! state's refuses the build (rename the block).

use std::collections::BTreeMap;

use super::creative::{self, CreativeInventory, Resolved};
use super::{Block, Category, Creative, Drops, Error, Item, ItemRef, Result};
use crate::block;
use crate::item::{self, Stack, Type};
use crate::nbt::{Compound, Value};
use crate::protocol::consts::{CreativeCategory, ItemEntryVersion};
use crate::protocol::{BlockEntry, ItemEntry, packet};

/// The first block ID of custom blocks (dragonfly: 10,000 plus the block's index).
const FIRST_BLOCK_ID: i32 = 10_000;
/// The most states of all custom blocks together.
const MAX_ALL_STATES: usize = 65_536;
/// The most custom items and blocks together.
pub(crate) const MAX_ENTRIES: usize = 4096;
/// The longest identifier, in bytes.
const MAX_IDENTIFIER: usize = 128;

/// The custom content of a server.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    /// The items and blocks in registration order.
    entries: Vec<Entry>,
    /// The item type of each entry (a block's item for a block), by entry; empty until
    /// [`Self::finish`].
    types: Vec<Type>,
    /// The entry of each identifier.
    by_name: BTreeMap<&'static str, usize>,
    /// The default creative inventory as the builder set it.
    creative_inventory: CreativeInventory,
    /// The alternative creative inventories as the builder registered them, in order.
    alternatives: Vec<(&'static str, CreativeInventory)>,
    /// The creative inventories: the default one, then the alternatives in registration order.
    creatives: Creatives,
    /// The index in [`Self::creatives`] of each alternative's name.
    creative_names: BTreeMap<&'static str, u16>,
}

impl Registry {
    /// The item type an [`ItemRef`] names, `None` for a vanilla block kind with no item of its
    /// own ([`block::ItemForm`] other than `Item`) or a custom item or block that is not
    /// registered (or before [`Self::finish`]).
    pub(crate) fn resolve(&self, item: ItemRef) -> Option<Type> {
        match item {
            ItemRef::Item(t) => Some(t),
            ItemRef::Block(kind) => match kind.default_state().item() {
                block::ItemForm::Item(t) => Some(t),
                block::ItemForm::None | block::ItemForm::BlockEntity => None,
            },
            ItemRef::Custom(i) => self.by_identifier(i.identifier()),
            ItemRef::CustomBlock(b) => self.by_identifier(b.identifier()),
        }
    }

    /// The custom item type (or custom block item) of `identifier`.
    pub(crate) fn by_identifier(&self, identifier: &str) -> Option<Type> {
        let at = *self.by_name.get(identifier)?;
        self.types.get(at).copied()
    }

    /// The registered custom item `item` as an item type.
    pub(crate) fn item_type(&self, item: &Item) -> Option<Type> {
        self.by_identifier(item.identifier())
            .filter(|t| t.custom_item().is_some())
    }

    /// The default state of the registered custom block `block`.
    pub(crate) fn block_state(&self, block: &Block) -> Option<block::State> {
        self.by_identifier(block.identifier())
            .filter(|t| t.custom_block().is_some())?
            .block()
    }

    /// The custom state a disk palette entry `{name, states, version}` names.
    pub(crate) fn find_state_nbt(&self, entry: &Compound) -> Option<block::State> {
        let Some(Value::String(name)) = entry.get("name") else {
            return None;
        };
        let t = self.by_identifier(name)?;
        let b = t.custom_block()?;
        let empty = Compound::new();
        let states = match entry.get("states") {
            Some(Value::Compound(states)) => states,
            Some(_) => return None,
            None => &empty,
        };
        block::State::custom(b, b.index_of(states)?, t.runtime_id())
    }

    /// The `ItemRegistry` packet: the vanilla list, then the custom items.
    pub(crate) fn item_registry(&self) -> packet::ItemRegistry {
        let mut registry = item::registry();
        registry.items.extend(self.item_entries());
        registry
    }

    /// The `StartGame` block definitions of the custom blocks, in registration order.
    pub(crate) fn to_block_entries(&self) -> Vec<BlockEntry> {
        let mut out = Vec::new();
        let mut block_id = FIRST_BLOCK_ID;
        for entry in &self.entries {
            if let Entry::Block(b) = entry {
                out.push(BlockEntry {
                    name: b.identifier().to_owned(),
                    properties: b.entry_nbt(block_id),
                });
                block_id += 1;
            }
        }
        out
    }

    /// The creative inventories: the default one (index 0), then the alternatives in
    /// registration order.
    pub(crate) fn creatives(&self) -> &[Resolved] {
        &self.creatives.0
    }

    /// The `CreativeContent` packet of the default creative inventory.
    #[cfg(test)]
    pub(crate) fn creative_content(&self) -> packet::CreativeContent {
        self.creatives()
            .first()
            .map(|c| c.content().clone())
            .unwrap_or_default()
    }

    /// The stack of the creative item with this network ID in creative inventory `inventory`
    /// ([`Self::creatives`]), one item. O(1).
    pub(crate) fn creative_stack(&self, inventory: u16, network_id: u32) -> Option<Stack> {
        self.creatives()
            .get(usize::from(inventory))?
            .stack(network_id)
            .cloned()
    }

    /// The index in [`Self::creatives`] of the alternative creative inventory `name`, or of the
    /// default one for `None`; `None` for a name that is not registered.
    pub(crate) fn creative_index(&self, name: Option<&str>) -> Option<u16> {
        match name {
            Some(name) => self.creative_names.get(name).copied(),
            None => Some(0),
        }
    }

    /// The name of the creative inventory at `index` ([`Self::creative_index`]); `None` for the
    /// default one.
    pub(crate) fn creative_name(&self, index: u16) -> Option<&'static str> {
        self.creatives().get(usize::from(index))?.name()
    }

    /// Registers a custom item (checked by [`Self::finish`]).
    pub(crate) fn add_item(&mut self, item: &'static Item) {
        self.entries.push(Entry::Item(item));
    }

    /// Registers a custom block.
    pub(crate) fn add_block(&mut self, block: &'static Block) {
        self.entries.push(Entry::Block(block));
    }

    /// Sets the default creative inventory.
    pub(crate) fn set_creative_inventory(&mut self, inventory: CreativeInventory) {
        self.creative_inventory = inventory;
    }

    /// Registers an alternative creative inventory.
    pub(crate) fn add_creative_inventory(
        &mut self,
        name: &'static str,
        inventory: CreativeInventory,
    ) {
        self.alternatives.push((name, inventory));
    }

    /// Checks every registration and numbers the content (when the server is built). On failure
    /// the registry is left as it was and the error names the first registration at fault.
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(Error::TooMany {
                what: "custom items and blocks",
                limit: MAX_ENTRIES,
            });
        }
        let mut by_name = BTreeMap::new();
        let mut types = Vec::with_capacity(self.entries.len());
        // The runtime IDs stay far below `i16::MAX`: at most `MAX_ENTRIES` follow the vanilla
        // ones.
        for (runtime_id, (index, entry)) in
            (first_runtime_id()..).zip(self.entries.iter().enumerate())
        {
            let identifier = entry.identifier();
            if !is_identifier(identifier) {
                return Err(Error::Identifier { identifier });
            }
            if by_name.insert(identifier, index).is_some() {
                return Err(Error::Duplicate { identifier });
            }
            let reason = match entry {
                Entry::Item(i) => i.check(),
                Entry::Block(b) => b.check(),
            };
            if let Some(reason) = reason {
                return Err(Error::Definition { identifier, reason });
            }
            if let Some(Creative {
                group: Some(group), ..
            }) = entry.creative()
                && !creative::is_name(group)
            {
                return Err(Error::Definition {
                    identifier,
                    reason: "a creative group name that is empty or too long",
                });
            }
            types.push(match *entry {
                Entry::Item(item) => Type::from_custom_item(item, runtime_id),
                Entry::Block(block) => Type::from_custom_block(block, runtime_id),
            });
        }
        self.check_states(&types)?;
        let places: Vec<(Type, Creative)> = self
            .entries
            .iter()
            .zip(&types)
            .filter_map(|(entry, t)| Some((*t, entry.creative()?)))
            .collect();
        let mut finished = Self {
            entries: self.entries.clone(),
            types,
            by_name,
            ..Self::default()
        };
        finished.resolve_creatives(&self.creative_inventory, &self.alternatives, &places)?;
        // Drops name registered items only, each count within its item's stack size (a count the
        // stack cannot hold would be cut when dropped; counts of 0 the definition check refused).
        for entry in &finished.entries {
            if let Entry::Block(b) = entry
                && let Drops::Items(items) = b.dropped()
            {
                for &(item, count) in items {
                    let t = finished.resolve(item).ok_or(Error::Unregistered {
                        identifier: b.identifier(),
                    })?;
                    if count > t.max_stack_size() {
                        return Err(Error::Definition {
                            identifier: b.identifier(),
                            reason: "a drop count outside its item's stack size",
                        });
                    }
                }
            }
        }
        *self = finished;
        Ok(())
    }

    /// Resolves the default creative inventory (with the custom `places`) and the alternatives
    /// (each name once) against the numbered custom items.
    fn resolve_creatives(
        &mut self,
        default: &CreativeInventory,
        alternatives: &[(&'static str, CreativeInventory)],
        places: &[(Type, Creative)],
    ) -> Result<()> {
        if alternatives.len() > creative::MAX_ALTERNATIVES {
            return Err(Error::TooMany {
                what: "alternative creative inventories",
                limit: creative::MAX_ALTERNATIVES,
            });
        }
        let mut resolved = Vec::with_capacity(alternatives.len() + 1);
        resolved.push(default.resolve(self, places)?);
        let mut names = BTreeMap::new();
        for (index, (name, inventory)) in (1_u16..).zip(alternatives) {
            if names.insert(creative::checked_name(name)?, index).is_some() {
                return Err(Error::Duplicate { identifier: name });
            }
            resolved.push(inventory.resolve(self, &[])?.with_name(name));
        }
        self.creatives = Creatives(resolved);
        self.creative_names = names;
        Ok(())
    }

    /// Checks that every custom state's network ID is its own: no other custom state's and no
    /// vanilla state's.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a checked block has from 1 to `MAX_STATES` states, each of which \
                  can be made"
    )]
    fn check_states(&self, types: &[Type]) -> Result<()> {
        let mut states = Vec::new();
        for (entry, t) in self.entries.iter().zip(types) {
            let Entry::Block(b) = *entry else {
                continue;
            };
            let count = u16::try_from(b.state_count()).expect("a state count fits a u16");
            if states.len() + usize::from(count) > MAX_ALL_STATES {
                return Err(Error::TooMany {
                    what: "custom block states",
                    limit: MAX_ALL_STATES,
                });
            }
            for index in 0..count {
                let state = block::State::custom(b, index, t.runtime_id())
                    .expect("a state index below the state count makes a state");
                states.push((state.network_id(), state));
            }
        }
        states.sort_by_key(|(id, _)| *id);
        for w in states.windows(2) {
            if let [(a, s), (b, _)] = w
                && a == b
            {
                return Err(Error::HashCollision {
                    identifier: s.name(),
                });
            }
        }
        if let Some((_, s)) = states
            .iter()
            .find(|(id, _)| block::is_vanilla_network_id(*id))
        {
            return Err(Error::HashCollision {
                identifier: s.name(),
            });
        }
        Ok(())
    }

    /// The custom item entries of `ItemRegistry`, in runtime ID order.
    fn item_entries(&self) -> impl Iterator<Item = ItemEntry> + '_ {
        self.entries
            .iter()
            .zip(&self.types)
            .map(|(entry, t)| match entry {
                Entry::Item(i) => ItemEntry {
                    name: i.identifier().to_owned(),
                    runtime_id: t.runtime_id(),
                    component_based: true,
                    version: i32::from(ItemEntryVersion::DataDriven),
                    data: i.to_components(),
                },
                Entry::Block(b) => ItemEntry {
                    name: b.identifier().to_owned(),
                    runtime_id: t.runtime_id(),
                    component_based: false,
                    // As vanilla block items have.
                    version: i32::from(ItemEntryVersion::None),
                    data: Compound::new(),
                },
            })
    }
}

/// One registration.
#[derive(Debug, Clone, Copy)]
enum Entry {
    Item(&'static Item),
    Block(&'static Block),
}

impl Entry {
    /// The identifier of the item or block.
    fn identifier(self) -> &'static str {
        match self {
            Self::Item(i) => i.identifier(),
            Self::Block(b) => b.identifier(),
        }
    }

    /// Where the item or block goes in the default creative inventory.
    fn creative(self) -> Option<Creative> {
        match self {
            Self::Item(i) => i.creative_place(),
            Self::Block(b) => b.creative_place(),
        }
    }
}

/// The resolved creative inventories; with nothing registered, PMMP's inventory alone.
#[derive(Debug)]
struct Creatives(Vec<Resolved>);

impl Default for Creatives {
    fn default() -> Self {
        Self(vec![Resolved::vanilla()])
    }
}

impl Category {
    /// The `CreativeContent` category number ([`CreativeCategory`]).
    pub(crate) const fn id(self) -> u8 {
        match self {
            Self::Construction => CreativeCategory::Construction as u8,
            Self::Nature => CreativeCategory::Nature as u8,
            Self::Equipment => CreativeCategory::Equipment as u8,
            Self::Items => CreativeCategory::Items as u8,
        }
    }
}

/// The first runtime ID after every vanilla one.
fn first_runtime_id() -> i16 {
    item::max_runtime_id() + 1
}

/// Whether `s` is a custom identifier of an item, a block or an entity type: `namespace:name`,
/// each part 1 or more of `a-z`, `0-9`, `_`, `-`, `.` (and `/` in the name), at most
/// [`MAX_IDENTIFIER`] bytes, the namespace not `minecraft`.
pub(crate) fn is_identifier(s: &str) -> bool {
    let Some((namespace, name)) = s.split_once(':') else {
        return false;
    };
    let part = |p: &str, extra: &[u8]| {
        !p.is_empty()
            && p.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b"_-.".contains(&b)
                    || extra.contains(&b)
            })
    };
    s.len() <= MAX_IDENTIFIER
        && namespace != "minecraft"
        && part(namespace, b"")
        && part(name, b"/")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::custom::Property;

    /// A registry of `build`'s registrations, finished.
    fn finished(build: impl FnOnce(&mut Registry)) -> Registry {
        let mut r = Registry::default();
        build(&mut r);
        r.finish().expect("the content registers");
        r
    }

    /// A block with a `lit` property.
    static LIT: Block =
        Block::new("test:changed", "test_changed").with_properties(&[Property::bool("lit")]);
    /// The same block with no properties.
    static PLAIN: Block = Block::new("test:changed", "test_changed");

    /// A custom block's item saved under one definition of its block reads back under another
    /// with other properties: the item is known by its name.
    #[test]
    fn a_custom_block_item_reads_back_after_its_block_changes() {
        let before = finished(|r| r.add_block(&PLAIN));
        let after = finished(|r| r.add_block(&LIT));
        let item = |r: &Registry, block| r.resolve(ItemRef::CustomBlock(block)).unwrap();
        let saved = Stack::new(item(&before, &PLAIN), 9).to_nbt();
        assert_eq!(
            Stack::from_nbt_with(&saved, &after),
            Some(Stack::new(item(&after, &LIT), 9))
        );
    }
}
