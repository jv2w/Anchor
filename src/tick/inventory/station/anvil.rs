//! Anvil rules (PMMP has none; dragonfly `handleCraftRecipeOptional`): renaming, repairing with the
//! material or with a second item of the same kind, combining enchantments, and the level cost,
//! which grows with the items' prior work (`RepairCost`).

use crate::item::{self, Enchantment, Kind, Stack, ToolTier};
use crate::nbt::Value;

/// The cost at which the anvil refuses outside creative mode ("too expensive").
pub(in crate::tick) const TOO_EXPENSIVE: u32 = 40;
/// The tag key of an item's prior work.
const REPAIR_COST: &str = "RepairCost";

/// What an anvil makes of its input and material.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Outcome {
    /// The result, as many as the input.
    pub(in crate::tick) result: Stack,
    /// Material items used up: the repair count, or the whole material.
    pub(in crate::tick) material_used: u8,
    /// The level cost.
    pub(in crate::tick) cost: u32,
}

/// Why the anvil does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Refused {
    /// No input.
    NoInput,
    /// The material does not repair or combine with the input.
    Mismatch,
    /// The input is not damaged.
    Repaired,
    /// Nothing would change.
    Nothing,
    /// The cost reached [`TOO_EXPENSIVE`] outside creative mode.
    TooExpensive,
    /// The result would be over a stack limit (its saved size, a name over 1,024 bytes).
    TooLarge(item::Error),
}

/// An item's prior work (`RepairCost`, dragonfly `AnvilCost`).
pub(in crate::tick) fn repair_cost(stack: &Stack) -> u32 {
    match stack.extra().and_then(|e| e.nbt.get(REPAIR_COST)) {
        Some(Value::Int(c)) => u32::try_from(*c).unwrap_or(0),
        _ => 0,
    }
}

/// The stack with prior work `cost`.
fn with_repair_cost(stack: &Stack, cost: u32) -> Result<Stack, Refused> {
    let mut extra = stack.extra().cloned().unwrap_or_default();
    extra.nbt.insert(
        REPAIR_COST.to_owned(),
        Value::Int(i32::try_from(cost).unwrap_or(i32::MAX)),
    );
    stack.with_extra(Some(extra)).map_err(Refused::TooLarge)
}

/// The durability left (dragonfly `Durability`), 0 for items that do not wear.
fn durability(stack: &Stack) -> i64 {
    let max = stack
        .item()
        .and_then(item::Type::max_durability)
        .unwrap_or(0);
    i64::from(max) - i64::from(stack.damage())
}

/// The stack with `left` durability.
#[expect(
    clippy::expect_used,
    reason = "invariant: an item keeps a damage from 0 to its durability"
)]
fn with_durability(stack: &Stack, left: i64) -> Stack {
    let max = i64::from(
        stack
            .item()
            .and_then(item::Type::max_durability)
            .unwrap_or(0),
    );
    let damage = (max - left).clamp(0, max);
    stack
        .with_damage(u16::try_from(damage).unwrap_or(0))
        .expect("invariant: the damage is within the durability")
}

/// Whether `material` repairs `input` (vanilla repair materials: a tool's tier material, an
/// armour piece's material, leather, phantom membrane for elytra, planks for shields).
fn repairs(input: item::Type, material: item::Type) -> bool {
    let kind = material.kind();
    let tag = |name: &str| item::Tag::find(name).is_some_and(|t| t.contains(material));
    let tier = |t: ToolTier| match t {
        ToolTier::Wood => tag("minecraft:planks"),
        ToolTier::Stone => tag("minecraft:stone_tool_materials"),
        ToolTier::Copper => kind == Some(Kind::CopperIngot),
        ToolTier::Iron => kind == Some(Kind::IronIngot),
        ToolTier::Gold => kind == Some(Kind::GoldIngot),
        ToolTier::Diamond => kind == Some(Kind::Diamond),
        ToolTier::Netherite => kind == Some(Kind::NetheriteIngot),
    };
    if input.armor().is_none()
        && let Some(t) = input.tool_tier()
    {
        return tier(t);
    }
    let name = input.name().strip_prefix("minecraft:").unwrap_or_default();
    if name == "turtle_helmet" {
        return kind == Some(Kind::Scute);
    }
    if name == "elytra" {
        return kind == Some(Kind::PhantomMembrane);
    }
    if name == "shield" {
        return tag("minecraft:planks");
    }
    if input.armor().is_none() {
        return false;
    }
    let by_prefix = [
        ("leather_", Some(Kind::Leather)),
        ("chainmail_", Some(Kind::IronIngot)),
        ("iron_", Some(Kind::IronIngot)),
        ("golden_", Some(Kind::GoldIngot)),
        ("diamond_", Some(Kind::Diamond)),
        ("netherite_", Some(Kind::NetheriteIngot)),
        ("copper_", Some(Kind::CopperIngot)),
    ];
    by_prefix
        .iter()
        .any(|(prefix, want)| name.starts_with(prefix) && kind == *want)
}

/// The level cost of an enchantment's rarity (dragonfly `Rarity.Cost`: common 1, uncommon 2,
/// rare 4, very rare 8), from its weight.
fn rarity_cost(e: Enchantment) -> u32 {
    match e.rarity() {
        10.. => 1,
        5..=9 => 2,
        2..=4 => 4,
        _ => 8,
    }
}

/// Whether the item is an enchanted book.
fn is_book(stack: &Stack) -> bool {
    stack
        .item()
        .is_some_and(|t| t.is(item::Class::EnchantedBook))
}

/// The anvil's outcome for `input` and `material` with the optional `rename` (dragonfly
/// `handleCraftRecipeOptional`; `creative` lifts the cost limit).
pub(in crate::tick) fn craft(
    input: &Stack,
    material: &Stack,
    rename: Option<&str>,
    creative: bool,
) -> Result<Outcome, Refused> {
    let Some(input_type) = input.item() else {
        return Err(Refused::NoInput);
    };
    let mut result = input.clone();
    let mut anvil_cost = repair_cost(input);
    if !material.is_empty() {
        anvil_cost = anvil_cost.saturating_add(repair_cost(material));
    }
    let (mut action, mut rename_cost, mut repaired) = (0u32, 0u32, 0u8);
    if let Some(material_type) = material.item() {
        if repairs(input_type, material_type) {
            let max = i64::from(input_type.max_durability().unwrap_or(0));
            let mut delta = (max - durability(input)).min(max / 4);
            if delta <= 0 {
                return Err(Refused::Repaired);
            }
            while delta > 0 && repaired < material.count() {
                result = with_durability(&result, durability(&result) + delta);
                action += 1;
                repaired += 1;
                delta = (max - durability(&result)).min(max / 4);
            }
        } else {
            let book =
                is_book(material) && material.extra().is_some_and(|e| !e.enchantments.is_empty());
            let durable = input_type.max_durability().is_some();
            if !book && (Some(input_type) != material.item() || !durable) {
                return Err(Refused::Mismatch);
            }
            if durable && !book {
                let max = i64::from(input_type.max_durability().unwrap_or(0));
                let left = (durability(input) + durability(material) + max * 12 / 100).min(max);
                if left > durability(input) {
                    result = with_durability(&result, left);
                    action += 2;
                }
            }
            let (compatible, incompatible);
            (result, compatible, incompatible, action) =
                merge(input, material, result, action, book)?;
            // dragonfly refuses this only for items that do not wear, and so charges a level for
            // nothing on others; vanilla (Java `AnvilMenu`) refuses it for every item.
            if incompatible && !compatible {
                return Err(Refused::Mismatch);
            }
        }
    }
    if let Some(name) = rename {
        rename_cost = 1;
        action += rename_cost;
        let mut extra = result.extra().cloned().unwrap_or_default();
        extra.custom_name = Some(name.to_owned()).filter(|n| !n.is_empty());
        result = result.with_extra(Some(extra)).map_err(Refused::TooLarge)?;
    }
    let mut cost = action.saturating_add(anvil_cost);
    if cost == 0 {
        return Err(Refused::Nothing);
    }
    if rename_cost == action && rename_cost > 0 && cost >= TOO_EXPENSIVE {
        cost = TOO_EXPENSIVE - 1;
    }
    if cost >= TOO_EXPENSIVE && !creative {
        return Err(Refused::TooExpensive);
    }
    let mut updated = repair_cost(&result);
    if !material.is_empty() && updated < repair_cost(material) {
        updated = repair_cost(material);
    }
    if rename_cost != action || rename_cost == 0 {
        updated = updated.saturating_mul(2).saturating_add(1);
    }
    let result = with_repair_cost(&result, updated)?;
    let material_used = if repaired > 0 {
        repaired
    } else {
        material.count()
    };
    Ok(Outcome {
        result,
        material_used,
        cost,
    })
}

/// dragonfly `mergeEnchantments`: each of the material's enchantments that the input can have
/// joins the result (equal levels go one up, to the maximum), each conflict with the input's
/// enchantments costs one; returns the result, whether anything joined, whether anything did not,
/// and the cost so far.
fn merge(
    input: &Stack,
    material: &Stack,
    mut result: Stack,
    mut cost: u32,
    book: bool,
) -> Result<(Stack, bool, bool, u32), Refused> {
    let (mut compatible_any, mut incompatible_any) = (false, false);
    let Some(input_type) = input.item() else {
        return Ok((result, false, false, cost));
    };
    let input_enchantments = input
        .extra()
        .map(|e| e.enchantments.clone())
        .unwrap_or_default();
    let material_enchantments = material
        .extra()
        .map(|e| e.enchantments.clone())
        .unwrap_or_default();
    for (e, material_level) in material_enchantments {
        let mut compatible = input_type.enchantments().contains(e) || is_book(input);
        for (other, _) in &input_enchantments {
            if e != *other && !e.is_compatible_with(*other) {
                compatible = false;
                cost += 1;
            }
        }
        if !compatible {
            incompatible_any = true;
            continue;
        }
        compatible_any = true;
        let mut level_now = material_level;
        let mut level_cost = u32::from(material_level);
        if let Some(existing) = input.enchantment_level(e) {
            // A stack holds levels up to `u8::MAX`, past the maximum when a server gave it.
            if existing > level_now
                || (existing == level_now && (level_now == e.max_level() || level_now == u8::MAX))
            {
                incompatible_any = true;
                continue;
            } else if existing == level_now {
                level_now += 1;
            }
            level_cost = u32::from(level_now - existing);
        }
        let mut rarity = rarity_cost(e);
        if book {
            rarity = (rarity / 2).max(1);
        }
        let mut extra = result.extra().cloned().unwrap_or_default();
        extra.set_enchantment(e, level_now);
        result = result.with_extra(Some(extra)).map_err(Refused::TooLarge)?;
        cost += rarity * level_cost;
        if input.count() > 1 {
            cost = TOO_EXPENSIVE;
        }
    }
    Ok((result, compatible_any, incompatible_any, cost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Extra;

    fn enchanted(kind: Kind, list: &[(Enchantment, u8)]) -> Stack {
        Stack::new(kind.default_type(), 1)
            .with_extra(Some(Extra {
                enchantments: list.to_vec(),
                ..Extra::default()
            }))
            .unwrap()
    }

    #[test]
    fn the_anvil_cost_table_follows_dragonfly() {
        let pick = Stack::new(Kind::IronPickaxe.default_type(), 1);
        let max = i64::from(pick.item().unwrap().max_durability().unwrap());
        let worn = with_durability(&pick, max / 10);
        // Rename only: 1 level, prior work stays 0.
        let renamed = craft(&pick, &Stack::EMPTY, Some("Pick"), false).unwrap();
        assert_eq!(renamed.cost, 1);
        assert_eq!(renamed.result.custom_name(), "Pick");
        assert_eq!(repair_cost(&renamed.result), 0);
        // Repair with three ingots: each restores a quarter of the maximum, one level each.
        let ingots = Stack::new(Kind::IronIngot.default_type(), 5);
        let repaired = craft(&worn, &ingots, None, false).unwrap();
        assert_eq!(repaired.material_used, 4, "a quarter at a time until full");
        assert_eq!(durability(&repaired.result), max);
        assert_eq!(repaired.cost, 4);
        assert_eq!(repair_cost(&repaired.result), 1);
        // A second repair costs the prior work too.
        let again = craft(
            &with_durability(&repaired.result, max - 100),
            &ingots,
            None,
            false,
        )
        .unwrap();
        assert_eq!(again.material_used, 2);
        assert_eq!(again.cost, 2 + 1);
        assert_eq!(repair_cost(&again.result), 3);
        // A full item cannot be repaired; a wrong material does nothing.
        assert_eq!(craft(&pick, &ingots, None, false), Err(Refused::Repaired));
        let gold = Stack::new(Kind::GoldIngot.default_type(), 1);
        assert_eq!(craft(&worn, &gold, None, false), Err(Refused::Mismatch));
        assert_eq!(
            craft(&pick, &Stack::EMPTY, None, false),
            Err(Refused::Nothing)
        );
    }

    #[test]
    fn enchantments_combine_by_level_and_rarity() {
        let a = enchanted(Kind::DiamondSword, &[(Enchantment::Sharpness, 2)]);
        let book = enchanted(Kind::EnchantedBook, &[(Enchantment::Sharpness, 2)]);
        let out = craft(&a, &book, None, false).unwrap();
        assert_eq!(
            out.result.extra().unwrap().enchantments,
            vec![(Enchantment::Sharpness, 3)]
        );
        // Common (1), halved for a book (at least 1), times one level up.
        assert_eq!(out.cost, 1);
        // An enchantment that conflicts with the input's joins nothing: refused.
        let helmet = enchanted(Kind::DiamondHelmet, &[(Enchantment::Protection, 1)]);
        let blast = enchanted(Kind::EnchantedBook, &[(Enchantment::BlastProtection, 1)]);
        assert_eq!(craft(&helmet, &blast, None, false), Err(Refused::Mismatch));
        // Two of a kind repair by 12% and merge.
        let sword = Stack::new(Kind::DiamondSword.default_type(), 1);
        let worn = with_durability(&sword, 100);
        let other = with_durability(
            &enchanted(Kind::DiamondSword, &[(Enchantment::Unbreaking, 1)]),
            100,
        );
        let both = craft(&worn, &other, None, false).unwrap();
        let max = i64::from(sword.item().unwrap().max_durability().unwrap());
        assert_eq!(durability(&both.result), 200 + max * 12 / 100);
        assert_eq!(both.cost, 2 + 2);
        // Too expensive outside creative.
        let costly = with_repair_cost(&worn, 39).unwrap();
        assert_eq!(
            craft(&costly, &Stack::EMPTY, Some("x"), false)
                .unwrap()
                .cost,
            39
        );
        assert_eq!(
            craft(&costly, &other, None, false),
            Err(Refused::TooExpensive)
        );
        assert!(craft(&costly, &other, None, true).is_ok());
    }
}
