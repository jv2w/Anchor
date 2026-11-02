//! The recipe lookups on [`Context`] (PMMP `CraftingManager` and `FurnaceRecipeManager`): what a
//! crafting grid, a furnace or a brewing stand makes of given items, over PMMP's recipes (unless
//! `vanilla-recipes` is off) and the server's own
//! ([`ServerBuilder::recipe`](crate::ServerBuilder::recipe)).
//!
//! Recipes themselves stay internal: PMMP's are generated static tables and a server's own are
//! [`custom::Recipe`](crate::custom::Recipe) values fixed at build. PMMP's recipes are looked up
//! through generated item name indexes, and the server's own through indexes by the item's name
//! (and tags) built at start, so none scans every recipe; the server's own come after PMMP's.

use crate::Context;
use crate::crafting::{FurnaceType, Recipe, ShapelessType};
use crate::item;

impl Context<'_> {
    /// What the crafting grid `grid` makes, or `None` if no recipe matches it (PMMP
    /// `CraftingManager::matchRecipe` and `CraftingRecipe::getResultsFor`). `grid` is the cells
    /// row by row: 4 for the inventory's 2x2 grid, 9 for a crafting table's 3x3; any other
    /// length matches nothing. A shaped recipe matches its shape anywhere in the grid, mirrored
    /// too; a shapeless one its ingredients in any cells, one item each. The first match wins:
    /// PMMP's recipes in network ID order (shaped, then shapeless), then the server's own.
    #[doc(alias = "CraftingManager::matchRecipe")]
    pub fn crafting_result(&self, grid: &[item::Stack]) -> Option<Vec<item::Stack>> {
        let side = match grid.len() {
            4 => 2,
            9 => 3,
            _ => return None,
        };
        let first = grid.iter().find(|s| !s.is_empty())?.item()?;
        let custom = &self.state.custom;
        custom
            .crafting_candidates(first)
            .find_map(|id| {
                let results = match custom.recipe(id)? {
                    Recipe::Shaped(r) if r.matches(grid, side) => r.results,
                    Recipe::Shapeless(r)
                        if r.kind == ShapelessType::Crafting && r.matches(grid) =>
                    {
                        r.results
                    }
                    _ => return None,
                };
                Some(
                    results
                        .iter()
                        .map(|s| s.to_item())
                        .collect::<Option<Vec<_>>>(),
                )
            })
            .flatten()
    }

    /// What a block of `kind` cooks one `input` item into, or `None` if no recipe of that block
    /// takes it (PMMP `FurnaceRecipeManager::match`, then `FurnaceRecipe::getResult`): PMMP's
    /// recipes first, then the server's own.
    #[doc(alias = "FurnaceRecipeManager::match")]
    pub fn furnace_result(&self, kind: FurnaceType, input: &item::Stack) -> Option<item::Stack> {
        let custom = &self.state.custom;
        let recipe = custom.furnace_at(custom.furnace_recipe(kind, input)?)?;
        recipe.result.to_item()
    }

    /// What brewing `reagent` into the potion `input` makes, or `None` if no recipe takes them
    /// (PMMP `CraftingManager::matchBrewingRecipe`, then `BrewingRecipe::getResultFor`): a
    /// container change (a potion into a splash potion) first, then a potion type change; of
    /// each, PMMP's recipes first, then the server's own.
    #[doc(alias = "CraftingManager::matchBrewingRecipe")]
    pub fn brewing_result(
        &self,
        input: &item::Stack,
        reagent: &item::Stack,
    ) -> Option<item::Stack> {
        self.state.custom.brew(input, reagent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::custom::tests::{ORE, RUBY, SWORD};
    use crate::custom::{Ingredient, ItemRef, Recipe as Own};
    use crate::item::{Kind, Stack, Type};
    use crate::tick::entity::Customs;
    use crate::tick::{Setup, Shared, State};
    use crate::world::{Flat, Generator, World};

    /// A state whose server has a shaped and a shapeless crafting recipe and a blast furnace
    /// recipe of its own.
    fn state() -> State {
        let ruby = ItemRef::Custom(&RUBY);
        let stick = ItemRef::Item(Kind::Stick.default_type());
        let builder = crate::Server::builder()
            .custom_item(&RUBY)
            .custom_item(&SWORD)
            .custom_block(&ORE)
            .recipe(Own::shaped(
                &["R ", "RS"],
                &[
                    ('R', Ingredient::Item(ruby)),
                    ('S', Ingredient::Item(stick)),
                ],
                &[(ItemRef::Custom(&SWORD), 1)],
            ))
            .recipe(Own::shapeless(
                &[
                    Ingredient::Item(ruby),
                    Ingredient::AnyMeta(ItemRef::Item(Kind::Coal.default_type())),
                ],
                &[(ItemRef::Item(Kind::Diamond.default_type()), 2)],
            ))
            .recipe(Own::furnace(
                FurnaceType::BlastFurnace,
                Ingredient::Item(ItemRef::CustomBlock(&ORE)),
                (ruby, 3),
            ));
        let mut custom = builder.setup.custom;
        custom.finish().unwrap();
        let shared = Shared::new(
            crate::protocol::batch::Settings::default(),
            &custom,
            &Customs::default(),
        )
        .unwrap();
        let world = World::new("world", crate::world::Kind::Persistent, 0, Flat.spawn(0));
        State::new(
            crate::tick::Settings::new(&Config::default()),
            shared,
            Setup {
                custom,
                ..Setup::default()
            },
            vec![world],
            0,
        )
    }

    fn named(name: &str) -> Type {
        Type::from_name(name, 0, None, &crate::nbt::Compound::new()).unwrap()
    }

    fn block(kind: crate::block::Kind) -> Type {
        match kind.default_state().item() {
            crate::block::ItemForm::Item(t) => Some(t),
            crate::block::ItemForm::None | crate::block::ItemForm::BlockEntity => None,
        }
        .expect("the block has an item of its own")
    }

    fn one(t: Type) -> Stack {
        Stack::new(t, 1)
    }

    // pmmp: CraftingManager::matchRecipe
    #[test]
    fn crafting_grids_make_what_their_recipe_makes() {
        let mut state = state();
        let cx = Context { state: &mut state };
        let made = |grid: &[Stack]| cx.crafting_result(grid);
        let e = || Stack::EMPTY;
        let log = || one(block(crate::block::Kind::OakLog));
        let planks = || one(block(crate::block::Kind::OakPlanks));
        let c = || one(block(crate::block::Kind::CrimsonPlanks));
        let s = || one(Kind::Stick.default_type());
        let coal = || one(Kind::Coal.default_type());
        // Shaped 1x1 and 1x2 anywhere in the inventory's grid.
        assert_eq!(
            made(&[e(), e(), e(), log()]),
            Some(vec![Stack::new(block(crate::block::Kind::OakPlanks), 4)])
        );
        assert_eq!(
            made(&[e(), planks(), e(), planks()]),
            Some(vec![Stack::new(Kind::Stick.default_type(), 4)])
        );
        // An axe's shape, as laid out and mirrored, in a crafting table's grid.
        let axe = Some(vec![one(named("minecraft:wooden_axe"))]);
        assert_eq!(made(&[c(), c(), e(), c(), s(), e(), e(), s(), e()]), axe);
        assert_eq!(made(&[e(), c(), c(), e(), s(), c(), e(), s(), e()]), axe);
        // A shapeless recipe with a tag ingredient, the tagged item first.
        let gunpowder = one(named("minecraft:gunpowder"));
        let blaze = one(named("minecraft:blaze_powder"));
        assert_eq!(
            made(&[coal(), gunpowder, e(), blaze]),
            Some(vec![Stack::new(named("minecraft:fire_charge"), 3)])
        );
        // The server's own recipes, after PMMP's.
        let ruby = || one(cx.custom_item(&RUBY).unwrap());
        let sword = Some(vec![one(cx.custom_item(&SWORD).unwrap())]);
        assert_eq!(made(&[ruby(), e(), ruby(), s()]), sword);
        assert_eq!(made(&[e(), ruby(), s(), ruby()]), sword, "mirrored");
        assert_eq!(
            made(&[coal(), e(), e(), ruby()]),
            Some(vec![Stack::new(Kind::Diamond.default_type(), 2)])
        );
        // Nothing matches: no recipe, an empty grid, a grid of another size.
        assert_eq!(made(&[s(), e(), e(), e()]), None);
        assert_eq!(made(&[e(), e(), e(), e()]), None);
        assert_eq!(made(&[log(), e(), e()]), None);
        assert_eq!(made(&[]), None);
    }

    // pmmp: FurnaceRecipeManager::match
    #[test]
    fn furnaces_cook_by_their_kind_and_the_servers_own_recipes() {
        let mut state = state();
        let cx = Context { state: &mut state };
        let raw = one(Kind::RawIron.default_type());
        let ingot = Some(one(Kind::IronIngot.default_type()));
        assert_eq!(cx.furnace_result(FurnaceType::Furnace, &raw), ingot);
        assert_eq!(cx.furnace_result(FurnaceType::BlastFurnace, &raw), ingot);
        assert_eq!(cx.furnace_result(FurnaceType::Smoker, &raw), None);
        assert_eq!(cx.furnace_result(FurnaceType::Furnace, &Stack::EMPTY), None);
        let ore = one(cx.item_of(ItemRef::CustomBlock(&ORE)).unwrap());
        let rubies = Some(Stack::new(cx.custom_item(&RUBY).unwrap(), 3));
        assert_eq!(cx.furnace_result(FurnaceType::BlastFurnace, &ore), rubies);
        assert_eq!(cx.furnace_result(FurnaceType::Furnace, &ore), None);
    }

    // pmmp: CraftingManager::matchBrewingRecipe
    #[test]
    fn brewing_makes_what_the_reagent_makes_of_the_potion() {
        let mut state = state();
        let cx = Context { state: &mut state };
        let water = one(Kind::Potion.default_type());
        let wart = one(named("minecraft:nether_wart"));
        let awkward = cx.brewing_result(&water, &wart).unwrap();
        assert_ne!(awkward, water);
        let gunpowder = one(named("minecraft:gunpowder"));
        let splash = cx.brewing_result(&awkward, &gunpowder).unwrap();
        assert_eq!(
            splash.item().map(Type::name),
            Some("minecraft:splash_potion")
        );
        assert_eq!(splash.meta(), awkward.meta());
        let stick = one(Kind::Stick.default_type());
        assert_eq!(cx.brewing_result(&water, &stick), None);
        assert_eq!(cx.brewing_result(&Stack::EMPTY, &wart), None);
    }
}
