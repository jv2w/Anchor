//! Recipes a server adds (PLAN.md 8.3, 10.8): shaped and shapeless crafting, the furnace family,
//! the stonecutter, the smithing table and the brewing stand. They are checked when the server
//! is built, sent in `CraftingData` after PMMP's recipes and judged by the same rules (a shaped
//! recipe matches its exact shape or its mirror image, as vanilla's; brewing tries PMMP's
//! container changes, then the server's, then PMMP's potion types, then the server's, as PMMP's
//! registration order does).

use super::ItemRef;
use crate::crafting::{FurnaceType, ShapelessType};
use crate::item;

/// The most rows and columns a shaped recipe has (the crafting table's 3x3).
pub(crate) const MAX_SIDE: usize = 3;
/// The most ingredients a shapeless recipe has.
pub(crate) const MAX_INGREDIENTS: usize = 9;
/// The most result stacks a crafting recipe has.
pub(crate) const MAX_RESULTS: usize = 9;

/// What a recipe cell takes.
#[derive(Debug, Clone, Copy)]
pub enum Ingredient {
    /// This item, whatever its count (PMMP `ExactRecipeIngredient`): its type and meta, with no
    /// damage on an item that wears.
    #[doc(alias = "ExactRecipeIngredient::__construct")]
    Item(ItemRef),
    /// Any item of this one's name, whatever its meta (PMMP `MetaWildcardRecipeIngredient`).
    AnyMeta(ItemRef),
    /// Any item with this tag (PMMP `TagWildcardRecipeIngredient`).
    Tag(item::Tag),
}

/// The kinds of recipe.
#[derive(Debug, Clone)]
pub(crate) enum Kind {
    /// Rows of the pattern, the key of each cell character, the results.
    Shaped {
        rows: Vec<String>,
        keys: Vec<(char, Ingredient)>,
        results: Vec<(ItemRef, u8)>,
    },
    Shapeless {
        kind: ShapelessType,
        ingredients: Vec<Ingredient>,
        results: Vec<(ItemRef, u8)>,
    },
    Furnace {
        kind: FurnaceType,
        input: Ingredient,
        result: (ItemRef, u8),
    },
    SmithingTransform {
        template: Ingredient,
        input: Ingredient,
        addition: Ingredient,
        result: ItemRef,
    },
    /// A potion type change.
    Brewing {
        input: Ingredient,
        reagent: Ingredient,
        output: ItemRef,
    },
    /// A potion container change: the input's item name becomes the output's.
    BrewingContainer {
        input: ItemRef,
        reagent: Ingredient,
        output: ItemRef,
    },
}

/// A recipe for [`ServerBuilder::recipe`](crate::ServerBuilder::recipe).
///
/// ```
/// use anchor::custom::{Ingredient, ItemRef, Recipe};
/// use anchor::item::Kind;
///
/// let stick = ItemRef::Item(Kind::Stick.default_type());
/// let diamond = ItemRef::Item(Kind::Diamond.default_type());
/// // A diamond hoe's shape: mirrored, it is the same recipe.
/// let hoe = Recipe::shaped(
///     &["DD", " S", " S"],
///     &[('D', Ingredient::Item(diamond)), ('S', Ingredient::Item(stick))],
///     &[(ItemRef::Item(Kind::DiamondHoe.default_type()), 1)],
/// );
/// ```
#[derive(Debug, Clone)]
pub struct Recipe(pub(crate) Kind);

impl Recipe {
    /// A shaped crafting recipe (PMMP `ShapedRecipe`): `rows` of up to three cells each (all
    /// rows the same width, at most three rows), each cell a character of `keys` or a space for
    /// an empty cell, making `results`. The shape may sit anywhere in the grid and may be
    /// mirrored left to right, as vanilla's.
    #[doc(alias = "ShapedRecipe::__construct")]
    pub fn shaped(rows: &[&str], keys: &[(char, Ingredient)], results: &[(ItemRef, u8)]) -> Self {
        Self(Kind::Shaped {
            rows: rows.iter().map(|r| (*r).to_owned()).collect(),
            keys: keys.to_vec(),
            results: results.to_vec(),
        })
    }

    /// A shapeless crafting recipe (PMMP `ShapelessRecipe`): exactly these ingredients in any
    /// cells, one item each, making `results`.
    #[doc(alias = "ShapelessRecipe::__construct")]
    pub fn shapeless(ingredients: &[Ingredient], results: &[(ItemRef, u8)]) -> Self {
        Self(Kind::Shapeless {
            kind: ShapelessType::Crafting,
            ingredients: ingredients.to_vec(),
            results: results.to_vec(),
        })
    }

    /// A stonecutter recipe (PMMP `ShapelessRecipe` of type `STONECUTTER`): one `input` item cut
    /// into `result`, as many times as the player asks.
    pub fn stonecutter(input: Ingredient, result: (ItemRef, u8)) -> Self {
        Self(Kind::Shapeless {
            kind: ShapelessType::Stonecutter,
            ingredients: vec![input],
            results: vec![result],
        })
    }

    /// The block of a shapeless or stonecutter recipe; `None` for the other kinds.
    #[doc(alias = "ShapelessRecipe::getType")]
    pub fn shapeless_type(&self) -> Option<ShapelessType> {
        match &self.0 {
            Kind::Shapeless { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// A smithing table recipe that turns `input` into one `result` (dragonfly
    /// `SmithingTransformRecipe`; PMMP reads no smithing recipes): the template, the input and
    /// the addition are used up, and the result keeps the input's damage, enchantments and name.
    pub fn smithing_transform(
        template: Ingredient,
        input: Ingredient,
        addition: Ingredient,
        result: ItemRef,
    ) -> Self {
        Self(Kind::SmithingTransform {
            template,
            input,
            addition,
            result,
        })
    }

    /// A brewing recipe that changes the potion type (PMMP `PotionTypeRecipe`): a bottle `input`
    /// gets `reagent` and becomes one `output`. The input and the reagent name an item (a tag is
    /// refused, as PMMP's packet cannot carry one).
    #[doc(alias = "PotionTypeRecipe::__construct")]
    pub fn brewing(input: Ingredient, reagent: Ingredient, output: ItemRef) -> Self {
        Self(Kind::Brewing {
            input,
            reagent,
            output,
        })
    }

    /// A brewing recipe that changes the potion container (PMMP `PotionContainerChangeRecipe`,
    /// such as a potion into a splash potion with gunpowder): a bottle of `input`'s item name
    /// gets `reagent` and becomes the item of `output`'s name with the bottle's meta (its potion
    /// type) and NBT. The reagent names an item.
    #[doc(alias = "PotionContainerChangeRecipe::__construct")]
    pub fn brewing_container(input: ItemRef, reagent: Ingredient, output: ItemRef) -> Self {
        Self(Kind::BrewingContainer {
            input,
            reagent,
            output,
        })
    }

    /// A furnace-family recipe (PMMP `FurnaceRecipe`): one `input` cooked into `result` in a
    /// block of `kind` (furnace, blast furnace, smoker or campfire).
    #[doc(alias = "FurnaceRecipe::__construct")]
    pub fn furnace(kind: FurnaceType, input: Ingredient, result: (ItemRef, u8)) -> Self {
        Self(Kind::Furnace {
            kind,
            input,
            result,
        })
    }
}
