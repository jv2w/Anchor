//! The custom content of one server (PLAN.md 8.3, 10.8): what the builder registered, checked
//! and numbered when the server is built, and every lookup the engine needs of it.
//!
//! Numbers follow registration order, so they are the same on every start: custom items and
//! custom blocks' items take the runtime IDs after the last vanilla one, custom blocks the block
//! IDs from 10,000 (dragonfly), custom recipes the network IDs after PMMP's. Every custom state
//! gets its network hash; one equal to another state's refuses the build (rename the block).

use std::collections::BTreeMap;

use super::recipe::{Kind as RecipeKind, MAX_INGREDIENTS, MAX_RESULTS, MAX_SIDE};
use super::{Block, Category, Creative, Drops, Error, Ingredient, Item, ItemRef, Recipe};
use crate::block::{self, PropertyValue};
use crate::crafting::{self, FurnaceType, ShapelessType};
use crate::item::{self, Stack, Type};
use crate::nbt::{Compound, Value};
use crate::protocol::{self, BlockEntry, ItemEntry, packet};

/// The most custom items and blocks together.
pub(crate) const MAX_ENTRIES: usize = 4096;
/// The most custom recipes.
pub(crate) const MAX_RECIPES: usize = 4096;
/// The most states of all custom blocks together.
const MAX_ALL_STATES: usize = 65_536;
/// The longest identifier, in bytes.
const MAX_IDENTIFIER: usize = 128;
/// The first block ID of custom blocks (dragonfly: 10,000 plus the block's index).
const FIRST_BLOCK_ID: i32 = 10_000;
/// `ItemEntry.Version` of a component-based item (gophertunnel `ItemEntryVersionDataDriven`).
const VERSION_DATA_DRIVEN: i32 = 1;
/// `ItemEntry.Version` of a block's item (gophertunnel `ItemEntryVersionNone`, as vanilla
/// block items have).
const VERSION_NONE: i32 = 2;
/// The meta of an ingredient that matches every meta.
const WILDCARD_META: i32 = 0x7fff;

/// One registration.
#[derive(Debug, Clone, Copy)]
enum Entry {
    Item(&'static Item),
    Block(&'static Block),
}

impl Entry {
    fn identifier(self) -> &'static str {
        match self {
            Self::Item(i) => i.identifier(),
            Self::Block(b) => b.identifier(),
        }
    }

    fn creative(self) -> Option<Creative> {
        match self {
            Self::Item(i) => i.creative_place(),
            Self::Block(b) => b.creative_place(),
        }
    }
}

/// A server's own shaped recipe, owning its cells.
#[derive(Debug)]
struct Shaped {
    width: u8,
    height: u8,
    grid: Vec<Option<crafting::Ingredient>>,
    results: Vec<crafting::Stack>,
}

/// A server's own shapeless recipe.
#[derive(Debug)]
struct Shapeless {
    ingredients: Vec<crafting::Ingredient>,
    results: Vec<crafting::Stack>,
}

/// Where a custom recipe network ID points.
#[derive(Debug, Clone, Copy)]
enum Numbered {
    Shaped(usize),
    Shapeless(usize),
    Furnace(usize),
}

/// The custom content of a server.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    /// The items and blocks in registration order.
    entries: Vec<Entry>,
    /// The recipes in registration order, until [`Self::finish`] resolves them.
    recipes: Vec<Recipe>,
    /// The item type of each entry (a block's item for a block), by entry; empty until
    /// [`Self::finish`].
    types: Vec<Type>,
    /// The entry of each identifier.
    by_name: BTreeMap<&'static str, usize>,
    shaped: Vec<Shaped>,
    shapeless: Vec<Shapeless>,
    furnace: Vec<crafting::Furnace>,
    /// The custom furnace recipes whose input names an item (exact and any-meta inputs), by the
    /// item's name, in registration order.
    furnace_by_name: BTreeMap<&'static str, Vec<usize>>,
    /// The custom furnace recipes whose input is an item tag, by tag, in registration order.
    furnace_by_tag: BTreeMap<item::Tag, Vec<usize>>,
    /// The custom entries of the creative inventory, in registration order: (type, place).
    creative: Vec<(Type, Creative)>,
    /// What each custom recipe network ID is, from [`Self::first_recipe_id`] on.
    numbered: Vec<Numbered>,
}

impl Registry {
    /// Registers a custom item (checked by [`Self::finish`]).
    pub(crate) fn add_item(&mut self, item: &'static Item) {
        self.entries.push(Entry::Item(item));
    }

    /// Registers a custom block.
    pub(crate) fn add_block(&mut self, block: &'static Block) {
        self.entries.push(Entry::Block(block));
    }

    /// Registers a recipe.
    pub(crate) fn add_recipe(&mut self, recipe: Recipe) {
        self.recipes.push(recipe);
    }

    /// Checks every registration and numbers the content (when the server is built). On failure
    /// the registry is left as it was and the error names the first registration at fault.
    pub(crate) fn finish(&mut self) -> Result<(), Error> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(Error::TooMany {
                what: "custom items and blocks",
                limit: MAX_ENTRIES,
            });
        }
        if self.recipes.len() > MAX_RECIPES {
            return Err(Error::TooMany {
                what: "custom recipes",
                limit: MAX_RECIPES,
            });
        }
        let mut by_name = BTreeMap::new();
        let mut types = Vec::with_capacity(self.entries.len());
        for (next_id, (index, entry)) in (first_runtime_id()..).zip(self.entries.iter().enumerate())
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
                && (group.is_empty() || group.len() > MAX_IDENTIFIER)
            {
                return Err(Error::Definition {
                    identifier,
                    reason: "a creative group name that is empty or too long",
                });
            }
            let id = i16::try_from(next_id).map_err(|_| Error::TooMany {
                what: "item runtime IDs",
                limit: MAX_ENTRIES,
            })?;
            types.push(match *entry {
                Entry::Item(item) => Type::from_custom_item(item, id),
                Entry::Block(block) => {
                    Type::from_custom_block(block, id).ok_or(Error::Definition {
                        identifier,
                        reason: "the block has no states",
                    })?
                }
            });
        }
        self.check_states(&types)?;
        let creative = self
            .entries
            .iter()
            .zip(&types)
            .filter_map(|(entry, t)| Some((*t, entry.creative()?)))
            .collect();
        let finished = Self {
            entries: self.entries.clone(),
            recipes: Vec::new(),
            types,
            creative,
            by_name,
            ..Self::default()
        };
        // Drops name registered items only, each count within its item's stack size (as recipe
        // results are checked: a count the stack cannot hold would be cut when dropped; counts
        // of 0 the definition check refused).
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
        let mut done = finished;
        for (index, recipe) in self.recipes.iter().enumerate() {
            done.add_resolved(index, recipe)?;
        }
        *self = done;
        Ok(())
    }

    /// Checks that every custom state's network ID is its own: no other custom state's and no
    /// vanilla state's.
    fn check_states(&self, types: &[Type]) -> Result<(), Error> {
        let mut states = Vec::new();
        for (entry, t) in self.entries.iter().zip(types) {
            let Entry::Block(b) = *entry else {
                continue;
            };
            let count = b.state_count();
            if states
                .len()
                .saturating_add(usize::try_from(count).unwrap_or(usize::MAX))
                > MAX_ALL_STATES
            {
                return Err(Error::TooMany {
                    what: "custom block states",
                    limit: MAX_ALL_STATES,
                });
            }
            for index in 0..count {
                let index = u16::try_from(index).map_err(|_| Error::Definition {
                    identifier: b.identifier(),
                    reason: "more than 4,096 states",
                })?;
                let state =
                    block::State::custom(b, index, t.runtime_id()).ok_or(Error::Definition {
                        identifier: b.identifier(),
                        reason: "a state cannot be made",
                    })?;
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
            .find(|(id, _)| block::State::from_network_id(*id).is_some())
        {
            return Err(Error::HashCollision {
                identifier: s.name(),
            });
        }
        Ok(())
    }

    /// The item type an [`ItemRef`] names, `None` for a custom item or block that is not
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
    fn by_identifier(&self, identifier: &str) -> Option<Type> {
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
            None => &empty,
            Some(_) => return None,
        };
        if states.len() > super::block::MAX_PROPERTIES {
            return None;
        }
        let mut properties = Vec::with_capacity(states.len());
        for (key, value) in states {
            properties.push((
                key.as_str(),
                match value {
                    Value::Byte(v) => PropertyValue::Byte(*v),
                    Value::Int(v) => PropertyValue::Int(*v),
                    Value::String(v) => PropertyValue::String(v),
                    _ => return None,
                },
            ));
        }
        block::State::custom(b, b.index_of(&properties)?, t.runtime_id())
    }

    /// The item type of a saved item name: a vanilla one by PMMP's rules ([`Type::from_name`]),
    /// else a registered custom item or custom block item.
    pub(crate) fn item_by_name(
        &self,
        name: &str,
        meta: u16,
        block: Option<block::State>,
        nbt: &Compound,
    ) -> Option<Type> {
        Type::from_name(name, meta, block, nbt).or_else(|| self.by_identifier(name))
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
                    version: VERSION_DATA_DRIVEN,
                    data: i.components(),
                },
                Entry::Block(b) => ItemEntry {
                    name: b.identifier().to_owned(),
                    runtime_id: t.runtime_id(),
                    component_based: false,
                    version: VERSION_NONE,
                    data: Compound::new(),
                },
            })
    }

    /// The `ItemRegistry` packet: the vanilla list, then the custom items.
    pub(crate) fn item_registry(&self) -> packet::ItemRegistry {
        let mut registry = item::registry();
        registry.items.extend(self.item_entries());
        registry
    }

    /// The `StartGame` block definitions of the custom blocks, in registration order.
    pub(crate) fn block_entries(&self) -> Vec<BlockEntry> {
        let mut out = Vec::new();
        let mut block_id = FIRST_BLOCK_ID;
        for entry in &self.entries {
            if let Entry::Block(b) = entry {
                out.push(BlockEntry {
                    name: b.identifier().to_owned(),
                    properties: b.entry_nbt(block_id),
                });
                block_id = block_id.saturating_add(1);
            }
        }
        out
    }

    /// The `CreativeContent` packet: PMMP's inventory, then the custom entries that ask for a
    /// place in it. A run of entries of one category and group shares a group, as PMMP's
    /// `CreativeInventoryCache` makes them (a named group's icon is its first item, an anonymous
    /// one has an empty name and the air icon).
    pub(crate) fn creative_content(&self) -> packet::CreativeContent {
        let mut content = item::creative_content();
        let mut previous: Option<(Category, Option<&str>)> = None;
        let mut next_id = u32::try_from(content.items.len()).unwrap_or(u32::MAX);
        for &(t, place) in &self.creative {
            let key = (place.category, place.group);
            let stack = Stack::new(t, 1).to_network();
            if previous != Some(key) {
                content.groups.push(protocol::CreativeGroup {
                    category: place.category.id(),
                    name: place.group.unwrap_or_default().to_owned(),
                    icon: if place.group.is_some() {
                        stack.clone()
                    } else {
                        protocol::ItemStack::default()
                    },
                });
                previous = Some(key);
            }
            next_id = next_id.saturating_add(1);
            let group = u32::try_from(content.groups.len().saturating_sub(1)).unwrap_or(0);
            content.items.push(protocol::CreativeItem {
                creative_item_network_id: next_id,
                item: stack,
                group_index: group,
            });
        }
        content
    }

    /// The stack of the creative item with this network ID ([`item::creative_stack`], then the
    /// custom entries after PMMP's), one item.
    pub(crate) fn creative_stack(&self, network_id: u32) -> Option<Stack> {
        let vanilla = item::creative_count();
        if network_id <= vanilla {
            return item::creative_stack(network_id);
        }
        let at = usize::try_from(network_id - vanilla - 1).ok()?;
        let &(t, _) = self.creative.get(at)?;
        Some(Stack::new(t, 1))
    }

    /// The network ID of the first custom recipe: after every PMMP recipe.
    fn first_recipe_id() -> u32 {
        crafting::first_special_id()
    }

    /// The first network ID after every recipe, the custom ones too: enchanting options and
    /// trade offers count from here.
    pub(crate) fn first_special_id(&self) -> u32 {
        let custom = u32::try_from(self.numbered.len()).unwrap_or(u32::MAX);
        Self::first_recipe_id().saturating_add(custom)
    }

    /// The recipe with this network ID: PMMP's, then the custom ones.
    pub(crate) fn recipe(&self, network_id: u32) -> Option<crafting::Recipe<'_>> {
        let first = Self::first_recipe_id();
        if network_id < first {
            return crafting::recipe(network_id);
        }
        let at = usize::try_from(network_id - first).ok()?;
        Some(match *self.numbered.get(at)? {
            Numbered::Shaped(i) => {
                let r = self.shaped.get(i)?;
                crafting::Recipe::Shaped(crafting::Shaped {
                    width: r.width,
                    height: r.height,
                    grid: &r.grid,
                    results: &r.results,
                })
            }
            Numbered::Shapeless(i) => {
                let r = self.shapeless.get(i)?;
                crafting::Recipe::Shapeless(crafting::Shapeless {
                    kind: ShapelessType::Crafting,
                    ingredients: &r.ingredients,
                    results: &r.results,
                })
            }
            Numbered::Furnace(_) => crafting::Recipe::Furnace,
        })
    }

    /// The index of the furnace recipe of `kind` that takes `input`: PMMP's first (PMMP
    /// `FurnaceRecipeManager::match` takes the first registered), then the custom ones, which
    /// count on from PMMP's. The custom ones are looked up by the input's name and tags.
    pub(crate) fn furnace_recipe(&self, kind: FurnaceType, input: &Stack) -> Option<u16> {
        crafting::furnace_recipe(kind, input).or_else(|| {
            if input.is_empty() {
                return None;
            }
            let item = input.item()?;
            let takes = |&at: &usize| {
                self.furnace
                    .get(at)
                    .is_some_and(|r| r.kind == kind && r.input.accepts(input))
            };
            let named = self
                .furnace_by_name
                .get(item.name())
                .and_then(|list| list.iter().copied().find(takes));
            // At most one list per tag the recipes use (PMMP's tags, a few dozen).
            let tagged = self
                .furnace_by_tag
                .iter()
                .filter(|(tag, _)| tag.contains(item))
                .filter_map(|(_, list)| list.iter().copied().find(takes));
            let at = named.into_iter().chain(tagged).min()?;
            u16::try_from(crafting::furnace_count().checked_add(at)?).ok()
        })
    }

    /// The furnace recipe at `index` ([`Self::furnace_recipe`]).
    pub(crate) fn furnace_at(&self, index: u16) -> Option<crafting::Furnace> {
        crafting::furnace_at(index).copied().or_else(|| {
            let at = usize::from(index).checked_sub(crafting::furnace_count())?;
            self.furnace.get(at).copied()
        })
    }

    /// The `CraftingData` packet: PMMP's recipes, then the custom ones with the network IDs
    /// after them (furnace recipes as shapeless recipes of their block, as PMMP sends them).
    pub(crate) fn crafting_data(&self) -> packet::CraftingData {
        let mut data = crafting::crafting_data();
        let mut id = Self::first_recipe_id();
        for n in &self.numbered {
            match *n {
                Numbered::Shaped(i) => {
                    if let Some(r) = self.shaped.get(i) {
                        let view = crafting::Shaped {
                            width: r.width,
                            height: r.height,
                            grid: &r.grid,
                            results: &r.results,
                        };
                        data.shaped_recipes.push(view.network(id));
                    }
                }
                Numbered::Shapeless(i) => {
                    if let Some(r) = self.shapeless.get(i) {
                        data.shapeless_recipes.push(crafting::shapeless(
                            id,
                            r.ingredients
                                .iter()
                                .map(|i| crafting::descriptor(Some(i)))
                                .collect(),
                            r.results.iter().map(crafting::Stack::network).collect(),
                            ShapelessType::Crafting.block_name(),
                        ));
                    }
                }
                Numbered::Furnace(i) => {
                    if let Some(r) = self.furnace.get(i) {
                        data.shapeless_recipes.push(crafting::shapeless(
                            id,
                            vec![crafting::descriptor(Some(&r.input))],
                            vec![r.result.network()],
                            r.kind.block_name(),
                        ));
                    }
                }
            }
            id = id.saturating_add(1);
        }
        data
    }

    /// Checks and resolves the recipe at `index` of the registrations into the tables.
    fn add_resolved(&mut self, index: usize, recipe: &Recipe) -> Result<(), Error> {
        let fail = |reason| Error::Recipe { index, reason };
        let numbered = match &recipe.0 {
            RecipeKind::Shaped {
                rows,
                keys,
                results,
            } => {
                let height = rows.len();
                let width = rows.first().map_or(0, |r| r.chars().count());
                if height == 0 || height > MAX_SIDE || width == 0 || width > MAX_SIDE {
                    return Err(fail("the shape is empty or larger than 3 by 3"));
                }
                if rows.iter().any(|r| r.chars().count() != width) {
                    return Err(fail("the rows are not all the same width"));
                }
                let mut grid = Vec::with_capacity(width * height);
                for c in rows.iter().flat_map(|r| r.chars()) {
                    if c == ' ' {
                        grid.push(None);
                        continue;
                    }
                    let mut found = keys.iter().filter(|(k, _)| *k == c);
                    let (_, ingredient) = found.next().ok_or(fail("a cell has no key"))?;
                    if found.next().is_some() {
                        return Err(fail("a key is given twice"));
                    }
                    grid.push(Some(self.ingredient(*ingredient).ok_or(fail(
                        "an ingredient names an unknown or unregistered item",
                    ))?));
                }
                if keys.iter().any(|(k, _)| *k == ' ') {
                    return Err(fail("a space is a key"));
                }
                if grid.iter().all(Option::is_none) {
                    return Err(fail("the shape has no ingredient"));
                }
                let results = self.results(results).ok_or(fail(
                    "no result, more than 9, or a result count outside its stack size",
                ))?;
                self.shaped.push(Shaped {
                    width: u8::try_from(width).map_err(|_| fail("the shape is too wide"))?,
                    height: u8::try_from(height).map_err(|_| fail("the shape is too high"))?,
                    grid,
                    results,
                });
                Numbered::Shaped(self.shaped.len() - 1)
            }
            RecipeKind::Shapeless {
                ingredients,
                results,
            } => {
                if ingredients.is_empty() || ingredients.len() > MAX_INGREDIENTS {
                    return Err(fail("no ingredient or more than 9"));
                }
                let ingredients = ingredients
                    .iter()
                    .map(|i| self.ingredient(*i))
                    .collect::<Option<Vec<_>>>()
                    .ok_or(fail("an ingredient names an unknown or unregistered item"))?;
                let results = self.results(results).ok_or(fail(
                    "no result, more than 9, or a result count outside its stack size",
                ))?;
                self.shapeless.push(Shapeless {
                    ingredients,
                    results,
                });
                Numbered::Shapeless(self.shapeless.len() - 1)
            }
            RecipeKind::Furnace {
                kind,
                input,
                result,
            } => {
                let input = self
                    .ingredient(*input)
                    .ok_or(fail("the input names an unknown or unregistered item"))?;
                let result = self
                    .results(std::slice::from_ref(result))
                    .and_then(|r| r.into_iter().next())
                    .ok_or(fail("the result count is outside its stack size"))?;
                Numbered::Furnace(self.add_furnace(crafting::Furnace {
                    kind: *kind,
                    input,
                    result,
                }))
            }
        };
        self.numbered.push(numbered);
        Ok(())
    }

    /// Adds a checked furnace recipe and indexes it by its input: its place in the table.
    fn add_furnace(&mut self, recipe: crafting::Furnace) -> usize {
        let at = self.furnace.len();
        match recipe.input {
            crafting::Ingredient::Exact(e) => {
                self.furnace_by_name
                    .entry(e.item.name())
                    .or_default()
                    .push(at);
            }
            crafting::Ingredient::AnyMeta(n) => {
                self.furnace_by_name.entry(n.name).or_default().push(at);
            }
            crafting::Ingredient::Tag(tag) => {
                self.furnace_by_tag.entry(tag).or_default().push(at);
            }
        }
        self.furnace.push(recipe);
        at
    }

    /// The table form of an ingredient.
    fn ingredient(&self, ingredient: Ingredient) -> Option<crafting::Ingredient> {
        Some(match ingredient {
            Ingredient::Item(r) => {
                let t = self.resolve(r)?;
                // A vanilla block item's legacy meta is not known here: the client is sent any
                // meta of the name, the server still takes only this type.
                let network_meta = if t.block().is_some() && t.custom_block().is_none() {
                    WILDCARD_META
                } else {
                    i32::from(t.meta())
                };
                crafting::Ingredient::Exact(crafting::Exact {
                    item: t,
                    network_meta,
                    meta: t.meta(),
                    nbt: None,
                })
            }
            Ingredient::AnyMeta(r) => {
                let t = self.resolve(r)?;
                crafting::Ingredient::AnyMeta(crafting::Name {
                    name: t.name(),
                    runtime_id: t.runtime_id(),
                })
            }
            Ingredient::Tag(tag) => crafting::Ingredient::Tag(tag),
        })
    }

    /// The table form of recipe results: 1 to 9 stacks, each count within its stack size.
    fn results(&self, results: &[(ItemRef, u8)]) -> Option<Vec<crafting::Stack>> {
        if results.is_empty() || results.len() > MAX_RESULTS {
            return None;
        }
        results
            .iter()
            .map(|&(r, count)| {
                let t = self.resolve(r)?;
                (count > 0 && count <= t.max_stack_size()).then_some(crafting::Stack {
                    item: t,
                    meta: t.meta(),
                    count: u16::from(count),
                    nbt: None,
                })
            })
            .collect()
    }
}

/// The first runtime ID after every vanilla one.
fn first_runtime_id() -> i32 {
    i32::from(item::max_runtime_id()).saturating_add(1)
}

/// Whether `s` is a custom identifier: `namespace:name`, each part 1 or more of `a-z`, `0-9`,
/// `_`, `-`, `.` (and `/` in the name), at most 128 bytes, the namespace not `minecraft`.
fn is_identifier(s: &str) -> bool {
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

impl Category {
    /// The `CreativeContent` category number (1 construction to 4 items).
    pub(crate) const fn id(self) -> u8 {
        match self {
            Self::Construction => 1,
            Self::Nature => 2,
            Self::Equipment => 3,
            Self::Items => 4,
        }
    }
}
