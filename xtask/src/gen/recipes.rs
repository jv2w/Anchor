//! `src/crafting/table.rs`: the recipe tables (PLAN.md 10.4) from the PMMP recipe dump
//! (`xtask/data/pmmp/recipes.json`). Every item must resolve to an Anchor item type or a known
//! item name, or generation fails.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::str_lit;
use super::data::{Data, PMMP_DIR, Tag, bedrock_state, read_json, static_compound};
use super::emit::HEADER;

type R<T> = Result<T, String>;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/crafting/table.rs";

/// PMMP's `FurnaceType` cases in declaration order, which is the order `CraftingDataCache` sends
/// them in: (dump name, Rust variant).
const FURNACE_TYPES: [(&str, &str); 5] = [
    ("furnace", "Furnace"),
    ("blast_furnace", "BlastFurnace"),
    ("smoker", "Smoker"),
    ("campfire", "Campfire"),
    ("soul_campfire", "SoulCampfire"),
];

/// The `ShapelessRecipeType` cases PMMP's recipes use: (dump name, Rust variant). PMMP has no
/// smithing or cartography table recipes of this kind.
const SHAPELESS_TYPES: [(&str, &str); 2] = [
    ("crafting_table", "Crafting"),
    ("stonecutter", "Stonecutter"),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipes {
    shaped: Vec<ShapedJson>,
    shapeless: Vec<ShapelessJson>,
    furnace: Vec<FurnaceJson>,
    potion_type: Vec<PotionTypeJson>,
    potion_container_change: Vec<PotionContainerJson>,
    smithing_transform: Vec<SmithingTransformJson>,
    smithing_trim: Vec<SmithingTrimJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapedJson {
    width: u8,
    height: u8,
    grid: Vec<CellJson>,
    results: Vec<Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellJson {
    x: u8,
    y: u8,
    ingredient: Option<Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapelessJson {
    block: String,
    ingredients: Vec<Json>,
    results: Vec<Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FurnaceJson {
    block: String,
    input: Json,
    result: Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PotionTypeJson {
    input: Json,
    ingredient: Json,
    output: Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PotionContainerJson {
    input: String,
    ingredient: Json,
    output: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SmithingTransformJson {
    template: Json,
    input: Json,
    addition: Json,
    result: Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SmithingTrimJson {
    template: Json,
    input: Json,
    addition: Json,
}

/// Generates the file text (unformatted).
pub(crate) fn generate(root: &Path, data: &Data) -> R<String> {
    let recipes: Recipes = read_json(root, PMMP_DIR, "recipes.json")?;
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Recipes generated from the PMMP recipe dump (PLAN.md 10.4).\n\n\
         use super::{{Exact, Furnace, FurnaceType, Ingredient, Name, PotionContainerChange, \
         PotionType, Shaped, Shapeless, ShapelessType, SmithingTransform, SmithingTrim, Stack}};\n\
         use crate::item;\n\n"
    );

    table(
        &mut out,
        "Shaped crafting recipes, in dump order.",
        "SHAPED",
        "Shaped",
        &recipes.shaped,
        |r| shaped(data, r).map_err(|e| format!("shaped recipe: {e}")),
    )?;
    table(
        &mut out,
        "Shapeless recipes, in dump order.",
        "SHAPELESS",
        "Shapeless",
        &recipes.shapeless,
        |r| shapeless(data, r).map_err(|e| format!("shapeless recipe: {e}")),
    )?;
    // Grouped by furnace type in PMMP's order; dump order within a type.
    let mut furnaces = Vec::with_capacity(recipes.furnace.len());
    for (name, _) in FURNACE_TYPES {
        furnaces.extend(recipes.furnace.iter().filter(|r| r.block == name));
    }
    if furnaces.len() != recipes.furnace.len() {
        return Err("a furnace recipe has an unknown block".to_owned());
    }
    table(
        &mut out,
        "Furnace recipes, by furnace type in PMMP's `FurnaceType` order, then in dump order.",
        "FURNACE",
        "Furnace",
        &furnaces,
        |r| furnace(data, r).map_err(|e| format!("furnace recipe: {e}")),
    )?;
    brewing_and_smithing(&mut out, data, &recipes)?;
    let mut ticks = Vec::with_capacity(FURNACE_TYPES.len());
    for (name, _) in FURNACE_TYPES {
        let t = data
            .tables
            .furnace_types
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("no cook time for {name}"))?;
        ticks.push(t.cook_ticks.to_string());
    }
    if ticks.len() != data.tables.furnace_types.len() {
        return Err("PMMP has a furnace type Anchor does not know".to_owned());
    }
    put!(
        out,
        "/// The cook time of each furnace type in ticks, in `FurnaceType` order (PMMP \
         `FurnaceType::getCookDurationTicks`).\npub(super) static COOK_TICKS: [u16; {}] = [{}];\n",
        ticks.len(),
        ticks.join(",")
    );
    Ok(out)
}

/// Appends the brewing and smithing tables.
fn brewing_and_smithing(out: &mut String, data: &Data, recipes: &Recipes) -> R<()> {
    table(
        out,
        "Potion type (brewing) recipes, in dump order.",
        "POTION_TYPE",
        "PotionType",
        &recipes.potion_type,
        |r| {
            Ok(format!(
                "PotionType {{ input: {}, reagent: {}, output: {} }}",
                exact(data, &r.input)?,
                exact(data, &r.ingredient)?,
                stack(data, &r.output)?
            ))
        },
    )?;
    table(
        out,
        "Potion container change recipes, in dump order.",
        "POTION_CONTAINER_CHANGE",
        "PotionContainerChange",
        &recipes.potion_container_change,
        |r| {
            Ok(format!(
                "PotionContainerChange {{ input: {}, reagent: {}, output: {} }}",
                name(data, &r.input)?,
                exact(data, &r.ingredient)?,
                name(data, &r.output)?
            ))
        },
    )?;
    table(
        out,
        "Smithing transform recipes, in dump order.",
        "SMITHING_TRANSFORM",
        "SmithingTransform",
        &recipes.smithing_transform,
        |r| {
            Ok(format!(
                "SmithingTransform {{ template: {}, input: {}, addition: {}, result: {} }}",
                ingredient(data, &r.template)?,
                ingredient(data, &r.input)?,
                ingredient(data, &r.addition)?,
                stack(data, &r.result)?
            ))
        },
    )?;
    table(
        out,
        "Smithing trim recipes, in dump order.",
        "SMITHING_TRIM",
        "SmithingTrim",
        &recipes.smithing_trim,
        |r| {
            Ok(format!(
                "SmithingTrim {{ template: {}, input: {}, addition: {} }}",
                ingredient(data, &r.template)?,
                ingredient(data, &r.input)?,
                ingredient(data, &r.addition)?
            ))
        },
    )
}

/// Appends a `static` array with a row for each recipe.
fn table<T>(
    out: &mut String,
    doc: &str,
    name: &str,
    row_type: &str,
    recipes: &[T],
    row: impl Fn(&T) -> R<String>,
) -> R<()> {
    put!(
        out,
        "/// {doc}\npub(super) static {name}: [{row_type}; {}] = [",
        recipes.len()
    );
    for r in recipes {
        put!(out, "{},", row(r)?);
    }
    out.push_str("];\n\n");
    Ok(())
}

/// A shaped recipe; the grid becomes row-major (PMMP `getIngredient(column, row)` order).
fn shaped(data: &Data, r: &ShapedJson) -> R<String> {
    let width = usize::from(r.width);
    let size = width * usize::from(r.height);
    if size == 0 || r.grid.len() != size {
        return Err(format!(
            "grid of {} cells for {}x{}",
            r.grid.len(),
            r.width,
            r.height
        ));
    }
    let mut cells: Vec<Option<String>> = vec![None; size];
    for cell in &r.grid {
        if cell.x >= r.width || cell.y >= r.height {
            return Err(format!("cell ({}, {}) outside the grid", cell.x, cell.y));
        }
        let slot = cells
            .get_mut(usize::from(cell.y) * width + usize::from(cell.x))
            .ok_or("cell outside the grid")?;
        if slot.is_some() {
            return Err(format!("cell ({}, {}) twice", cell.x, cell.y));
        }
        *slot = Some(match &cell.ingredient {
            Some(i) => format!("Some({})", ingredient(data, i)?),
            None => "None".to_owned(),
        });
    }
    let grid = cells
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or("grid has a gap")?;
    Ok(format!(
        "Shaped {{ width: {}, height: {}, grid: &[{}], results: {} }}",
        r.width,
        r.height,
        grid.join(","),
        results(data, &r.results)?
    ))
}

fn shapeless(data: &Data, r: &ShapelessJson) -> R<String> {
    let (_, kind) = SHAPELESS_TYPES
        .iter()
        .find(|(name, _)| *name == r.block)
        .ok_or_else(|| format!("unknown block {}", r.block))?;
    if r.ingredients.is_empty() {
        return Err("no ingredients".to_owned());
    }
    let ingredients = r
        .ingredients
        .iter()
        .map(|i| ingredient(data, i))
        .collect::<R<Vec<_>>>()?;
    Ok(format!(
        "Shapeless {{ kind: ShapelessType::{kind}, ingredients: &[{}], results: {} }}",
        ingredients.join(","),
        results(data, &r.results)?
    ))
}

fn furnace(data: &Data, r: &FurnaceJson) -> R<String> {
    let (_, kind) = FURNACE_TYPES
        .iter()
        .find(|(name, _)| *name == r.block)
        .ok_or_else(|| format!("unknown block {}", r.block))?;
    Ok(format!(
        "Furnace {{ kind: FurnaceType::{kind}, input: {}, result: {} }}",
        ingredient(data, &r.input)?,
        stack(data, &r.result)?
    ))
}

/// A non-empty list of result stacks.
fn results(data: &Data, list: &[Json]) -> R<String> {
    if list.is_empty() {
        return Err("no results".to_owned());
    }
    let stacks = list.iter().map(|s| stack(data, s)).collect::<R<Vec<_>>>()?;
    Ok(format!("&[{}]", stacks.join(",")))
}

/// A recipe ingredient (`{"exact": stack}`, `{"any_meta": name}` or `{"tag": tag}`).
fn ingredient(data: &Data, json: &Json) -> R<String> {
    let obj = json.as_object().ok_or("ingredient is not an object")?;
    let mut entries = obj.iter();
    let (Some((kind, value)), None) = (entries.next(), entries.next()) else {
        return Err("ingredient must have exactly one kind".to_owned());
    };
    match kind.as_str() {
        "exact" => Ok(format!("Ingredient::Exact({})", exact(data, json)?)),
        "any_meta" => {
            let item = value.as_str().ok_or("any_meta name is not a string")?;
            Ok(format!("Ingredient::AnyMeta({})", name(data, item)?))
        }
        "tag" => {
            let tag = value.as_str().ok_or("tag is not a string")?;
            let index = data
                .tables
                .item_tags
                .iter()
                .position(|t| t.tag == tag)
                .ok_or_else(|| format!("item tag {tag} is not in PMMP's tag map"))?;
            Ok(format!("Ingredient::Tag(item::Tag::from_index({index}))"))
        }
        other => Err(format!("unknown ingredient kind {other}")),
    }
}

/// An exact ingredient (`{"exact": stack}`) with the meta PMMP sends for it
/// (`TypeConverter::coreRecipeIngredientToNet`): the legacy meta of the block state for a block
/// item, the stack meta otherwise, and the stack's own meta and NBT, which crafting matches
/// (PMMP `ExactRecipeIngredient::accepts`). PMMP always sends a count of 1, so the dump must
/// hold 1.
fn exact(data: &Data, json: &Json) -> R<String> {
    let value = json.get("exact").ok_or("ingredient is not exact")?;
    let s = data.stack(value)?;
    if s.count != 1 {
        return Err(format!("exact ingredient with count {}", s.count));
    }
    let meta = match value.get("block") {
        Some(block) => data.recipe_meta(&bedrock_state(block)?)?,
        None => i32::from(s.meta),
    };
    Ok(format!(
        "Exact::new({}, {meta}, {}, {})",
        s.item,
        s.meta,
        nbt(s.nbt.as_ref())
    ))
}

/// A result stack.
fn stack(data: &Data, json: &Json) -> R<String> {
    let s = data.stack(json)?;
    Ok(format!(
        "Stack::new({}, {}, {}, {})",
        s.item,
        s.meta,
        s.count,
        nbt(s.nbt.as_ref())
    ))
}

/// An item name with its 1.26.50 runtime ID from the reference list. PMMP sends names it knows
/// without an item type for them too, such as `minecraft:lodestone_compass`.
fn name(data: &Data, name: &str) -> R<String> {
    let id = data.runtime_id(name)?;
    Ok(format!("Name::new({}, {id})", str_lit(name)))
}

/// Static NBT of a stack, if it has any.
fn nbt(c: Option<&BTreeMap<String, Tag>>) -> String {
    c.map_or_else(
        || "None".to_owned(),
        |c| format!("Some({})", static_compound(c)),
    )
}
