//! The custom content of one server: what the builder registered, checked
//! and numbered when the server is built, and every lookup the engine needs of it.
//!
//! Numbers follow registration order, so they are the same on every start: custom items and
//! custom blocks' items take the runtime IDs after the last vanilla one, custom blocks the block
//! IDs from 10,000 (dragonfly), custom recipes the network IDs after PMMP's. Every custom state
//! gets its network hash; one equal to another state's refuses the build (rename the block).

use std::collections::BTreeMap;

use super::creative::{self, CreativeInventory, Resolved};
use super::recipe::{Kind as RecipeKind, MAX_INGREDIENTS, MAX_RESULTS, MAX_SIDE};
use super::{Block, Category, Creative, Drops, Error, Ingredient, Item, ItemRef, Recipe};
use crate::block;
use crate::crafting::{self, FurnaceType, ShapelessType};
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
/// The most custom recipes.
pub(crate) const MAX_RECIPES: usize = 4096;
/// Why a recipe whose ingredient names no item is refused.
const UNKNOWN_INGREDIENT: &str = "an ingredient names an unknown or unregistered item";

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
    /// The network IDs of the custom crafting-table recipes, ascending, by the item names the
    /// first item of a grid can have ([`Shaped::first_cells`], every ingredient of a shapeless
    /// one).
    crafting_by_name: BTreeMap<&'static str, Vec<u32>>,
    /// The network IDs of the custom crafting-table recipes, ascending, by the tags the first
    /// item of a grid can have.
    crafting_by_tag: BTreeMap<item::Tag, Vec<u32>>,
    furnace: Vec<crafting::Furnace>,
    /// The custom furnace recipes whose input names an item (exact and any-meta inputs), by the
    /// item's name, in registration order.
    furnace_by_name: BTreeMap<&'static str, Vec<usize>>,
    /// The custom furnace recipes whose input is an item tag, by tag, in registration order.
    furnace_by_tag: BTreeMap<item::Tag, Vec<usize>>,
    smithing: Vec<crafting::SmithingTransform>,
    brewing: Vec<Brewing>,
    /// The custom potion type recipes by their reagent's item name, in registration order.
    brewing_by_reagent: BTreeMap<&'static str, Vec<usize>>,
    containers: Vec<ContainerChange>,
    /// The custom potion container change recipes by their reagent's item name, in
    /// registration order.
    containers_by_reagent: BTreeMap<&'static str, Vec<usize>>,
    /// The default creative inventory as the builder set it.
    creative_inventory: CreativeInventory,
    /// The alternative creative inventories as the builder registered them, in order.
    alternatives: Vec<(&'static str, CreativeInventory)>,
    /// The creative inventories: the default one, then the alternatives in registration order.
    creatives: Creatives,
    /// The index in [`Self::creatives`] of each alternative's name.
    creative_names: BTreeMap<&'static str, u16>,
    /// What each custom recipe network ID is, from [`Self::first_recipe_id`] on.
    numbered: Vec<Numbered>,
    /// PMMP's recipes are left out (`vanilla-recipes = false`).
    without_vanilla: bool,
    /// The recipe book is hidden (`recipe-book = false`).
    without_book: bool,
}

impl Registry {
    /// Whether the recipe book is shown and its quick crafting (`CraftRecipeAuto`) accepted.
    pub(crate) fn has_recipe_book(&self) -> bool {
        !self.without_book
    }

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
            None => &empty,
            Some(_) => return None,
        };
        block::State::custom(b, b.index_of(states)?, t.runtime_id())
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
            None => Some(0),
            Some(name) => self.creative_names.get(name).copied(),
        }
    }

    /// The name of the creative inventory at `index` ([`Self::creative_index`]); `None` for the
    /// default one.
    pub(crate) fn creative_name(&self, index: u16) -> Option<&'static str> {
        self.creatives().get(usize::from(index))?.name()
    }

    /// The first network ID after every recipe, the custom ones too: enchanting options and
    /// trade offers count from here.
    #[expect(
        clippy::expect_used,
        reason = "invariant: at most `MAX_RECIPES` recipes are numbered"
    )]
    pub(crate) fn first_special_id(&self) -> u32 {
        let custom = u32::try_from(self.numbered.len()).expect("a recipe count fits a u32");
        self.first_recipe_id() + custom
    }

    /// The recipe with this network ID: PMMP's, then the custom ones. Without PMMP's recipes
    /// the custom ones start at 1, so no ID reaches PMMP's table.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a numbered recipe is an index of its table"
    )]
    pub(crate) fn recipe(&self, network_id: u32) -> Option<crafting::Recipe<'_>> {
        let first = self.first_recipe_id();
        if network_id < first {
            return crafting::recipe(network_id);
        }
        let at = usize::try_from(network_id - first).ok()?;
        Some(match *self.numbered.get(at)? {
            Numbered::Shaped(i) => crafting::Recipe::Shaped(self.shaped[i].view()),
            Numbered::Shapeless(i) => crafting::Recipe::Shapeless(self.shapeless[i].view()),
            Numbered::Furnace(_) => crafting::Recipe::Furnace,
            Numbered::SmithingTransform(i) => {
                crafting::Recipe::SmithingTransform(&self.smithing[i])
            }
        })
    }

    /// The network IDs of the crafting recipes that can take `first` as the first item of a
    /// grid, ascending: PMMP's from its index (none without them), then the custom ones from
    /// theirs, by its name and its tags; the caller checks them one by one.
    pub(crate) fn crafting_candidates(&self, first: Type) -> impl Iterator<Item = u32> {
        let vanilla = (!self.without_vanilla)
            .then(|| crafting::crafting_candidates(first.name()))
            .into_iter()
            .flatten();
        let mut custom: Vec<u32> = self
            .crafting_by_name
            .get(first.name())
            .into_iter()
            .flatten()
            .copied()
            .chain(
                self.crafting_by_tag
                    .iter()
                    .filter(|(tag, _)| tag.contains(first))
                    .flat_map(|(_, ids)| ids.iter().copied()),
            )
            .collect();
        custom.sort_unstable();
        custom.dedup();
        vanilla.chain(custom)
    }

    /// What brewing `reagent` into the potion `input` makes (PMMP
    /// `CraftingManager::matchBrewingRecipe`, then `BrewingRecipe::getResultFor`): the first
    /// container change that takes them, PMMP's then the server's, else the first potion type
    /// change, PMMP's then the server's. Each kind is looked up by the reagent's item name.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a reagent index holds indices of its recipe table"
    )]
    pub(crate) fn brew<'a>(&'a self, input: &Stack, reagent: &Stack) -> Option<Stack> {
        let reagent_name = reagent.item()?.name();
        let input_name = input.item()?.name();
        let listed = |index: &'a BTreeMap<&'static str, Vec<usize>>| {
            index.get(reagent_name).map_or(&[][..], Vec::as_slice)
        };
        let vanilla = !self.without_vanilla;
        vanilla
            .then(|| crafting::brew_container_change(input, reagent))
            .flatten()
            .or_else(|| {
                listed(&self.containers_by_reagent)
                    .iter()
                    .map(|&i| &self.containers[i])
                    .filter(|r| r.input.name() == input_name && r.reagent.accepts(reagent))
                    .find_map(|r| {
                        crafting::change_container(input, |meta, tag| {
                            self.item_by_name(r.output.name(), meta, None, tag)
                        })
                    })
            })
            .or_else(|| {
                vanilla
                    .then(|| crafting::brew_potion_type(input, reagent))
                    .flatten()
            })
            .or_else(|| {
                listed(&self.brewing_by_reagent)
                    .iter()
                    .map(|&i| &self.brewing[i])
                    .find(|r| r.reagent.accepts(reagent) && r.input.accepts(input))
                    .and_then(|r| r.output.to_item())
            })
    }

    /// The index of the furnace recipe of `kind` that takes `input`: PMMP's first (PMMP
    /// `FurnaceRecipeManager::match` takes the first registered), then the custom ones, which
    /// count on from PMMP's. The custom ones are looked up by the input's name and tags.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a furnace index holds indices of the furnace table"
    )]
    pub(crate) fn furnace_recipe(&self, kind: FurnaceType, input: &Stack) -> Option<u16> {
        let vanilla = (!self.without_vanilla)
            .then(|| crafting::furnace_recipe(kind, input))
            .flatten();
        vanilla.or_else(|| {
            let item = input.item()?;
            let takes = |&at: &usize| {
                let r = &self.furnace[at];
                r.kind == kind && r.input.accepts(input)
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
            u16::try_from(self.vanilla_furnaces().checked_add(at)?).ok()
        })
    }

    /// The furnace recipe at `index` ([`Self::furnace_recipe`]).
    pub(crate) fn furnace_at(&self, index: u16) -> Option<crafting::Furnace> {
        let vanilla = (!self.without_vanilla)
            .then(|| crafting::furnace_at(index).copied())
            .flatten();
        vanilla.or_else(|| {
            let at = usize::from(index).checked_sub(self.vanilla_furnaces())?;
            self.furnace.get(at).copied()
        })
    }

    /// The `CraftingData` packet: PMMP's recipes (unless left out), then the custom ones with
    /// the network IDs after them in the same network forms as PMMP's, and the custom brewing
    /// recipes after PMMP's. With the recipe book hidden every recipe is locked.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a numbered recipe is an index of its table"
    )]
    pub(crate) fn crafting_data(&self) -> packet::CraftingData {
        let mut data = if self.without_vanilla {
            packet::CraftingData {
                clear_recipes: true,
                ..packet::CraftingData::default()
            }
        } else {
            crafting::crafting_data()
        };
        for (id, numbered) in (self.first_recipe_id()..).zip(&self.numbered) {
            match *numbered {
                Numbered::Shaped(i) => {
                    data.shaped_recipes.push(self.shaped[i].view().network(id));
                }
                Numbered::Shapeless(i) => {
                    data.shapeless_recipes
                        .push(self.shapeless[i].view().network(id));
                }
                Numbered::Furnace(i) => {
                    data.shapeless_recipes.push(self.furnace[i].network(id));
                }
                Numbered::SmithingTransform(i) => {
                    data.smithing_transform_recipes
                        .push(self.smithing[i].network(id));
                }
            }
        }
        data.potion_recipes.extend(
            self.brewing
                .iter()
                .map(|r| crafting::potion_recipe(&r.input, &r.reagent, &r.output)),
        );
        data.potion_container_change_recipes
            .extend(self.containers.iter().map(|r| {
                crafting::potion_container_change_recipe(
                    r.input.runtime_id(),
                    &r.reagent,
                    r.output.runtime_id(),
                )
            }));
        if self.without_book {
            for r in &mut data.shaped_recipes {
                r.unlock_requirement = Some(crafting::locked());
            }
            for r in &mut data.shapeless_recipes {
                r.unlock_requirement = Some(crafting::locked());
            }
        }
        data
    }

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

    /// Leaves PMMP's recipes out (`vanilla-recipes = false`, before [`Self::finish`]): only the
    /// registered recipes exist, numbered from 1, in every lookup and in `CraftingData`.
    pub(crate) fn drop_vanilla_recipes(&mut self) {
        self.without_vanilla = true;
    }

    /// Hides the recipe book (`recipe-book = false`): every recipe goes out locked
    /// ([`crafting::locked`]) and the book's quick crafting is refused ([`Self::has_recipe_book`]);
    /// the crafting grid still crafts.
    pub(crate) fn hide_recipe_book(&mut self) {
        self.without_book = true;
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
            recipes: Vec::new(),
            types,
            by_name,
            without_vanilla: self.without_vanilla,
            without_book: self.without_book,
            ..Self::default()
        };
        finished.resolve_creatives(&self.creative_inventory, &self.alternatives, &places)?;
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
        for (index, recipe) in self.recipes.iter().enumerate() {
            finished.add_resolved(index, recipe)?;
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
    ) -> Result<(), Error> {
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
    fn check_states(&self, types: &[Type]) -> Result<(), Error> {
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
                    data: i.components(),
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

    /// The network ID of the first custom recipe: after every PMMP recipe, or 1 without them.
    fn first_recipe_id(&self) -> u32 {
        if self.without_vanilla {
            1
        } else {
            crafting::first_special_id()
        }
    }

    /// How many furnace recipes come before the custom ones: PMMP's, or none without them.
    fn vanilla_furnaces(&self) -> usize {
        if self.without_vanilla {
            0
        } else {
            crafting::furnace_count()
        }
    }

    /// Checks and resolves the recipe at `index` of the registrations into the tables.
    fn add_resolved(&mut self, index: usize, recipe: &Recipe) -> Result<(), Error> {
        let fail = |reason| Error::Recipe { index, reason };
        let id = self.first_special_id();
        let numbered = match &recipe.0 {
            RecipeKind::Shaped {
                rows,
                keys,
                results,
            } => {
                let shaped = self.shaped(rows, keys, results, fail)?;
                for first in shaped.first_cells() {
                    index_ingredient(
                        &mut self.crafting_by_name,
                        &mut self.crafting_by_tag,
                        first,
                        id,
                    );
                }
                self.shaped.push(shaped);
                Numbered::Shaped(self.shaped.len() - 1)
            }
            RecipeKind::Shapeless {
                kind,
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
                    .ok_or(fail(UNKNOWN_INGREDIENT))?;
                let results = self.results(results).map_err(fail)?;
                if *kind == ShapelessType::Crafting {
                    for ingredient in &ingredients {
                        index_ingredient(
                            &mut self.crafting_by_name,
                            &mut self.crafting_by_tag,
                            ingredient,
                            id,
                        );
                    }
                }
                self.shapeless.push(Shapeless {
                    kind: *kind,
                    ingredients,
                    results,
                });
                Numbered::Shapeless(self.shapeless.len() - 1)
            }
            RecipeKind::SmithingTransform {
                template,
                input,
                addition,
                result,
            } => {
                let unknown = || fail(UNKNOWN_INGREDIENT);
                let recipe = crafting::SmithingTransform {
                    template: self.ingredient(*template).ok_or_else(unknown)?,
                    input: self.ingredient(*input).ok_or_else(unknown)?,
                    addition: self.ingredient(*addition).ok_or_else(unknown)?,
                    result: self.result((*result, 1)).map_err(fail)?,
                };
                self.smithing.push(recipe);
                Numbered::SmithingTransform(self.smithing.len() - 1)
            }
            RecipeKind::Brewing {
                input,
                reagent,
                output,
            } => return self.add_brewing(*input, *reagent, *output, fail),
            RecipeKind::BrewingContainer {
                input,
                reagent,
                output,
            } => return self.add_container_change(*input, *reagent, *output, fail),
            RecipeKind::Furnace {
                kind,
                input,
                result,
            } => {
                let input = self
                    .ingredient(*input)
                    .ok_or(fail("the input names an unknown or unregistered item"))?;
                let result = self.result(*result).map_err(fail)?;
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

    /// The table form of a shaped recipe: `rows` of cells keyed by `keys`, making `results`.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the sides are checked to be at most `MAX_SIDE`"
    )]
    fn shaped(
        &self,
        rows: &[String],
        keys: &[(char, Ingredient)],
        results: &[(ItemRef, u8)],
        fail: impl Fn(&'static str) -> Error,
    ) -> Result<Shaped, Error> {
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
            let (_, ingredient) = found.next().ok_or_else(|| fail("a cell has no key"))?;
            if found.next().is_some() {
                return Err(fail("a key is given twice"));
            }
            grid.push(Some(
                self.ingredient(*ingredient)
                    .ok_or_else(|| fail(UNKNOWN_INGREDIENT))?,
            ));
        }
        if keys.iter().any(|(k, _)| *k == ' ') {
            return Err(fail("a space is a key"));
        }
        // PMMP `ShapedRecipe` refuses a key its shape does not use.
        if keys
            .iter()
            .any(|(k, _)| !rows.iter().any(|r| r.contains(*k)))
        {
            return Err(fail("a key is not in the shape"));
        }
        if grid.iter().all(Option::is_none) {
            return Err(fail("the shape has no ingredient"));
        }
        let results = self.results(results).map_err(&fail)?;
        Ok(Shaped {
            width: u8::try_from(width).expect("a side is at most `MAX_SIDE`"),
            height: u8::try_from(height).expect("a side is at most `MAX_SIDE`"),
            grid,
            results,
        })
    }

    /// Checks and adds a potion type recipe, indexed by its reagent.
    fn add_brewing(
        &mut self,
        input: Ingredient,
        reagent: Ingredient,
        output: ItemRef,
        fail: impl Fn(&'static str) -> Error,
    ) -> Result<(), Error> {
        let input = self.brewing_ingredient(input, &fail)?;
        let reagent = self.brewing_ingredient(reagent, &fail)?;
        let output = self
            .one(output)
            .ok_or_else(|| fail("the output names an unknown or unregistered item"))?;
        index_by(&mut self.brewing_by_reagent, &reagent, self.brewing.len());
        self.brewing.push(Brewing {
            input,
            reagent,
            output,
        });
        Ok(())
    }

    /// Checks and adds a potion container change recipe, indexed by its reagent.
    fn add_container_change(
        &mut self,
        input: ItemRef,
        reagent: Ingredient,
        output: ItemRef,
        fail: impl Fn(&'static str) -> Error,
    ) -> Result<(), Error> {
        let unknown = || fail("an item is unknown or unregistered");
        let (input, output) = (
            self.resolve(input).ok_or_else(unknown)?,
            self.resolve(output).ok_or_else(unknown)?,
        );
        let reagent = self.brewing_ingredient(reagent, &fail)?;
        index_by(
            &mut self.containers_by_reagent,
            &reagent,
            self.containers.len(),
        );
        self.containers.push(ContainerChange {
            input,
            reagent,
            output,
        });
        Ok(())
    }

    /// Adds a checked furnace recipe and indexes it by its input: its place in the table.
    fn add_furnace(&mut self, recipe: crafting::Furnace) -> usize {
        let at = self.furnace.len();
        index_ingredient(
            &mut self.furnace_by_name,
            &mut self.furnace_by_tag,
            &recipe.input,
            at,
        );
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
                    crafting::WILDCARD_META
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

    /// The table form of a brewing ingredient, which names an item (PMMP's packet carries a
    /// runtime ID and meta, no tag).
    fn brewing_ingredient(
        &self,
        ingredient: Ingredient,
        fail: impl Fn(&'static str) -> Error,
    ) -> Result<crafting::Ingredient, Error> {
        let i = self
            .ingredient(ingredient)
            .ok_or_else(|| fail(UNKNOWN_INGREDIENT))?;
        match i.name() {
            Some(_) => Ok(i),
            None => Err(fail("a brewing ingredient is a tag")),
        }
    }

    /// The table form of a one-item result.
    fn one(&self, item: ItemRef) -> Option<crafting::Stack> {
        self.result((item, 1)).ok()
    }

    /// The table form of recipe results: 1 to [`MAX_RESULTS`] stacks, or why they are refused.
    fn results(&self, results: &[(ItemRef, u8)]) -> Result<Vec<crafting::Stack>, &'static str> {
        if results.is_empty() || results.len() > MAX_RESULTS {
            return Err("no result or more than 9");
        }
        results.iter().map(|&r| self.result(r)).collect()
    }

    /// The table form of one recipe result, its count within its stack size, or why it is
    /// refused.
    fn result(&self, (item, count): (ItemRef, u8)) -> Result<crafting::Stack, &'static str> {
        let t = self
            .resolve(item)
            .ok_or("a result names an unknown or unregistered item")?;
        if count == 0 || count > t.max_stack_size() {
            return Err("a result count outside its stack size");
        }
        Ok(crafting::Stack {
            item: t,
            meta: t.meta(),
            count: u16::from(count),
            nbt: None,
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

/// A server's own shaped recipe, owning its cells.
#[derive(Debug)]
struct Shaped {
    width: u8,
    height: u8,
    grid: Vec<Option<crafting::Ingredient>>,
    results: Vec<crafting::Stack>,
}

impl Shaped {
    /// The ingredients the first item of a grid (row by row) can be: the leftmost and the
    /// rightmost of the top row with ingredients, as the shape is laid out and mirrored.
    fn first_cells(&self) -> impl Iterator<Item = &crafting::Ingredient> {
        self.grid
            .chunks(usize::from(self.width))
            .find(|row| row.iter().any(Option::is_some))
            .into_iter()
            .flat_map(|row| {
                let mut cells = row.iter().flatten();
                [cells.next(), cells.next_back()]
            })
            .flatten()
    }

    /// The recipe as the crafting rules see it.
    fn view(&self) -> crafting::Shaped<'_> {
        crafting::Shaped {
            width: self.width,
            height: self.height,
            grid: &self.grid,
            results: &self.results,
        }
    }
}

/// A server's own shapeless or stonecutter recipe.
#[derive(Debug)]
struct Shapeless {
    kind: ShapelessType,
    ingredients: Vec<crafting::Ingredient>,
    results: Vec<crafting::Stack>,
}

impl Shapeless {
    /// The recipe as the crafting rules see it.
    fn view(&self) -> crafting::Shapeless<'_> {
        crafting::Shapeless {
            kind: self.kind,
            ingredients: &self.ingredients,
            results: &self.results,
        }
    }
}

/// A server's own potion type recipe (PMMP `PotionTypeRecipe`).
#[derive(Debug)]
struct Brewing {
    input: crafting::Ingredient,
    reagent: crafting::Ingredient,
    output: crafting::Stack,
}

/// A server's own potion container change recipe (PMMP `PotionContainerChangeRecipe`).
#[derive(Debug)]
struct ContainerChange {
    input: Type,
    reagent: crafting::Ingredient,
    output: Type,
}

/// Where a custom recipe network ID points.
#[derive(Debug, Clone, Copy)]
enum Numbered {
    Shaped(usize),
    Shapeless(usize),
    Furnace(usize),
    SmithingTransform(usize),
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

/// Adds `at` to the index of what `ingredient` takes: its item name, or its tag. An entry is
/// added once however many times it is given in a row.
fn index_ingredient<T: Copy + PartialEq>(
    names: &mut BTreeMap<&'static str, Vec<T>>,
    tags: &mut BTreeMap<item::Tag, Vec<T>>,
    ingredient: &crafting::Ingredient,
    at: T,
) {
    let list = match ingredient {
        crafting::Ingredient::Exact(e) => names.entry(e.item.name()).or_default(),
        crafting::Ingredient::AnyMeta(n) => names.entry(n.name).or_default(),
        crafting::Ingredient::Tag(tag) => tags.entry(*tag).or_default(),
    };
    if list.last() != Some(&at) {
        list.push(at);
    }
}

/// Adds recipe `at` to the index of its reagent's item name (a brewing reagent always names one).
fn index_by(
    index: &mut BTreeMap<&'static str, Vec<usize>>,
    reagent: &crafting::Ingredient,
    at: usize,
) {
    if let Some(name) = reagent.name() {
        index.entry(name).or_default().push(at);
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
    use crate::custom::tests::RUBY;

    /// A registry of `build`'s registrations, finished.
    fn finished(build: impl FnOnce(&mut Registry)) -> Registry {
        let mut r = Registry::default();
        build(&mut r);
        r.finish().expect("the content registers");
        r
    }

    /// The server's own crafting-table recipes are candidates only for the first items their
    /// grids can have: a shaped recipe's leftmost and rightmost top ingredient, every ingredient
    /// of a shapeless one, every item of a tag; other recipes are none, and the IDs ascend.
    #[test]
    fn custom_crafting_candidates_come_from_an_index_of_first_items() {
        let exact = |kind: item::Kind| Ingredient::Item(ItemRef::Item(kind.default_type()));
        let logs = Ingredient::Tag(item::Tag::find("minecraft:logs").expect("a vanilla tag"));
        let out = [(ItemRef::Item(item::Kind::DiamondHoe.default_type()), 1)];
        let r = finished(|r| {
            r.add_item(&RUBY);
            r.add_recipe(Recipe::shaped(
                &["SD"],
                &[
                    ('S', exact(item::Kind::Stick)),
                    ('D', exact(item::Kind::Diamond)),
                ],
                &out,
            ));
            r.add_recipe(Recipe::shapeless(
                &[exact(item::Kind::IronIngot), logs],
                &out,
            ));
            r.add_recipe(Recipe::stonecutter(
                Ingredient::Item(ItemRef::Custom(&RUBY)),
                (ItemRef::Item(item::Kind::Stick.default_type()), 2),
            ));
            r.add_recipe(Recipe::shapeless(&[exact(item::Kind::Diamond)], &out));
            r.drop_vanilla_recipes();
        });
        let candidates = |t: Type| r.crafting_candidates(t).collect::<Vec<_>>();
        assert_eq!(candidates(item::Kind::Stick.default_type()), [1]);
        assert_eq!(candidates(item::Kind::Diamond.default_type()), [1, 4]);
        assert_eq!(candidates(item::Kind::IronIngot.default_type()), [2]);
        let oak = r
            .resolve(ItemRef::Block(block::Kind::OakLog))
            .expect("an oak log is an item");
        assert_eq!(candidates(oak), [2]);
        let ruby = r.resolve(ItemRef::Custom(&RUBY)).expect("ruby registers");
        assert_eq!(candidates(ruby), Vec::<u32>::new(), "a stonecutter recipe");
        assert_eq!(
            candidates(item::Kind::GoldIngot.default_type()),
            Vec::<u32>::new()
        );
    }

    /// A shaped recipe uses every key it gives, as PMMP `ShapedRecipe` requires.
    #[test]
    fn a_shaped_recipe_with_a_key_its_shape_does_not_use_is_refused() {
        let exact = |kind: item::Kind| Ingredient::Item(ItemRef::Item(kind.default_type()));
        let mut r = Registry::default();
        r.add_recipe(Recipe::shaped(
            &["D"],
            &[
                ('D', exact(item::Kind::Diamond)),
                ('S', exact(item::Kind::Stick)),
            ],
            &[(ItemRef::Item(item::Kind::DiamondHoe.default_type()), 1)],
        ));
        assert_eq!(
            r.finish(),
            Err(Error::Recipe {
                index: 0,
                reason: "a key is not in the shape",
            })
        );
    }

    static PLAIN: Block = Block::new("test:changed", "test_changed");
    static LIT: Block =
        Block::new("test:changed", "test_changed").with_properties(&[Property::bool("lit")]);

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
