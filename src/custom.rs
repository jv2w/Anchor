//! Custom content: items and blocks a server adds beyond vanilla, its
//! own recipes, its creative inventories, and villager trade offers as typed values.
//!
//! Items and blocks are `static` definitions ([`Item`], [`Block`]) given to the builder
//! ([`ServerBuilder::custom_item`], [`ServerBuilder::custom_block`]); recipes are values
//! ([`ServerBuilder::recipe`]). [`ServerBuilder::build`](crate::ServerBuilder::build) checks
//! them all and numbers them in registration order: runtime IDs after the vanilla items, network
//! hashes for every block state, recipe IDs after PMMP's. The engine then sends them in the
//! start-game packets (`StartGame` block definitions, `ItemRegistry`, `CreativeContent`,
//! `CraftingData`); textures and models come from a resource pack.
//!
//! A custom item or block state is an [`item::Type`](crate::item::Type) or
//! [`block::State`](crate::block::State) like a vanilla one: the server makes them
//! ([`Context::custom_item`](crate::Context::custom_item),
//! [`Context::custom_block`](crate::Context::custom_block)) and every rule reads their data
//! from the definition. Saved worlds keep custom blocks and items by name: when a definition is
//! gone, its blocks stay as they were saved and show as `minecraft:info_update`.

mod block;
mod creative;
mod item;
mod offer;
mod recipe;
mod registry;
#[cfg(test)]
pub(crate) mod tests;

use std::fmt;

pub use self::block::{Behaviour, Block, Drops, Material, Property, RenderMethod};
pub use self::creative::{CreativeGroup, CreativeInventory, CreativeItem};
pub use self::item::Item;
pub(crate) use self::offer::DEFAULT_PRICE_MULTIPLIER;
pub use self::offer::{MAX_TRADE_COUNTER, Offer};
pub use self::recipe::{Ingredient, Recipe};
pub(crate) use self::registry::{Registry, is_identifier};
pub use crate::crafting::{FurnaceType, ShapelessType};

use crate::ServerBuilder;

/// A creative inventory tab.
#[doc(alias = "CreativeCategory")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Building blocks.
    Construction,
    /// Natural blocks and plants.
    Nature,
    /// Tools, weapons and armour.
    Equipment,
    /// Everything else.
    Items,
}

/// Where a custom item or block's item shows in the default creative inventory, after its own
/// entries: a tab and an optional group (a vanilla group name such as `itemGroup.name.sword`, or
/// a name of the resource pack). Custom entries with the same tab and group share one group shown
/// with the first of them, as one PMMP `CreativeGroup` would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Creative {
    category: Category,
    group: Option<&'static str>,
}

impl Creative {
    /// In `category`, in no group.
    pub const fn new(category: Category) -> Self {
        Self {
            category,
            group: None,
        }
    }

    /// The same place in the group `group`.
    #[must_use]
    pub const fn with_group(mut self, group: &'static str) -> Self {
        self.group = Some(group);
        self
    }
}

/// An item a definition or a recipe names before the server has numbered the custom items: a
/// vanilla item type, a vanilla block's item, a custom item or a custom block's item.
#[derive(Debug, Clone, Copy)]
pub enum ItemRef {
    /// A vanilla item type, such as `item::Kind::Diamond.default_type()`.
    Item(crate::item::Type),
    /// The item of a vanilla block kind's default state; a kind with no item of its own (air,
    /// a bed: [`ItemForm`](crate::block::ItemForm) other than `Item`) is refused.
    Block(crate::block::Kind),
    /// A custom item; it must be registered too.
    Custom(&'static Item),
    /// A custom block's item; the block must be registered too.
    CustomBlock(&'static Block),
}

/// Why custom content is refused: when the server is built ([`ServerBuilder::build`]), or when
/// a villager's offers are changed ([`Context::set_trade_offers`](crate::Context)).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// An identifier is not `namespace:name` (lowercase letters, digits, `_`, `-`, `.`, and `/`
    /// in the name; at most 128 bytes) or is in the `minecraft` namespace.
    Identifier {
        /// The identifier.
        identifier: &'static str,
    },
    /// Two custom items or blocks have the same identifier.
    Duplicate {
        /// The identifier.
        identifier: &'static str,
    },
    /// A definition breaks a rule of its type.
    Definition {
        /// The item or block.
        identifier: &'static str,
        /// What is wrong.
        reason: &'static str,
    },
    /// A state of the block hashes to the network ID of another state; rename the block or
    /// its properties.
    HashCollision {
        /// The block.
        identifier: &'static str,
    },
    /// A block's drops name a vanilla block kind with no item of its own, or a custom item or
    /// block that is not registered.
    Unregistered {
        /// The block.
        identifier: &'static str,
    },
    /// A recipe breaks a rule.
    Recipe {
        /// Its place in registration order (from 0).
        index: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// More registrations than the limit.
    TooMany {
        /// What there are too many of.
        what: &'static str,
        /// The limit.
        limit: usize,
    },
    /// The entity is not a villager NPC.
    NotVillager,
    /// The villager was not spawned: the world does not exist, the position is not finite,
    /// outside the world border or more than 64 blocks above or below the world height, its
    /// chunk is not loaded, or the world is at its entity limit.
    NotSpawned,
    /// An offer or an offer list change breaks a rule.
    Offer {
        /// What is wrong.
        reason: &'static str,
    },
    /// A creative inventory breaks a rule.
    Creative {
        /// What is wrong.
        reason: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identifier { identifier } => write!(
                f,
                "`{identifier}` is not a `namespace:name` identifier outside `minecraft`"
            ),
            Self::Duplicate { identifier } => write!(f, "`{identifier}` is registered twice"),
            Self::Definition { identifier, reason } => write!(f, "`{identifier}`: {reason}"),
            Self::HashCollision { identifier } => write!(
                f,
                "a state of `{identifier}` has the network ID of another state; rename it"
            ),
            Self::Unregistered { identifier } => {
                write!(f, "`{identifier}` drops an unknown or unregistered item")
            }
            Self::Recipe { index, reason } => write!(f, "recipe {index}: {reason}"),
            Self::TooMany { what, limit } => write!(f, "more than {limit} {what}"),
            Self::NotVillager => f.write_str("the entity is not a villager"),
            Self::NotSpawned => f.write_str("the villager cannot be spawned there"),
            Self::Offer { reason } => write!(f, "offer refused: {reason}"),
            Self::Creative { reason } => write!(f, "creative inventory refused: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl ServerBuilder {
    /// Adds the custom item `item` (checked by [`build`](Self::build)). Its runtime ID follows
    /// the vanilla ones in registration order.
    #[must_use]
    pub fn custom_item(mut self, item: &'static Item) -> Self {
        self.setup.custom.add_item(item);
        self
    }

    /// Adds the custom block `block` and its item (checked by [`build`](Self::build)).
    #[must_use]
    pub fn custom_block(mut self, block: &'static Block) -> Self {
        self.setup.custom.add_block(block);
        self
    }

    /// Adds `recipe` after PMMP's recipes (checked by [`build`](Self::build): its size, counts
    /// and items, which must be vanilla or registered custom ones). PMMP takes its recipes the
    /// same way: shaped and shapeless crafting recipes, stonecutter recipes, furnace recipes and
    /// brewing recipes; smithing recipes are dragonfly's.
    #[doc(alias = "CraftingManager::registerShapedRecipe")]
    #[doc(alias = "CraftingManager::registerShapelessRecipe")]
    #[doc(alias = "FurnaceRecipeManager::register")]
    #[doc(alias = "CraftingManager::registerPotionTypeRecipe")]
    #[doc(alias = "CraftingManager::registerPotionContainerChangeRecipe")]
    #[must_use]
    pub fn recipe(mut self, recipe: Recipe) -> Self {
        self.setup.custom.add_recipe(recipe);
        self
    }

    /// Sets the creative inventory every player sees unless switched to an alternative
    /// ([`Context::set_creative_inventory`](crate::Context::set_creative_inventory)): `inventory`
    /// (checked by [`build`](Self::build)), then the custom items and blocks that ask for a
    /// place ([`Creative`]). Without a call it is PMMP's ([`CreativeInventory::vanilla`]).
    #[must_use]
    pub fn creative_inventory(mut self, inventory: CreativeInventory) -> Self {
        self.setup.custom.set_creative_inventory(inventory);
        self
    }

    /// Registers `inventory` as the alternative creative inventory `name` (PMMP: a clone of
    /// `CreativeInventory` given to `Player::setCreativeInventory`), which a player is switched
    /// to while the server runs. It holds what it lists: no custom item or block place is added.
    /// [`build`](Self::build) checks it and refuses an empty, longer than 128 bytes or repeated
    /// name, or more than 64 alternatives.
    #[must_use]
    pub fn alternative_creative_inventory(
        mut self,
        name: &'static str,
        inventory: CreativeInventory,
    ) -> Self {
        self.setup.custom.add_creative_inventory(name, inventory);
        self
    }
}
