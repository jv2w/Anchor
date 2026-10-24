//! Station windows' request actions: enchanting, anvil, smithing, stonecutter, loom, beacon
//! payment and trades.
//!
//! The executor hands a station everything it does not do itself: [`takes_craft_recipe`] says
//! whether a `CraftRecipe` belongs to the open station (an enchanting option, a stonecutter or
//! smithing recipe, a trade offer) and [`takes_auto_craft`] whether a `CraftRecipeAuto` does (a
//! quick trade), [`action`] runs such actions and the station-only ones (`CraftRecipeOptional`,
//! `CraftLoomRecipe`, `BeaconPayment`) with the executor's slot operations, [`crafting()`] lets
//! consume actions through, and [`finish`] checks at the end of the request that exactly the
//! craft's inputs were consumed, from the station's own slots (a quick trade's also from the main
//! inventory).
//!
//! A station craft computes its result from the window's slots as the request has left them and
//! puts it into the created output; what it does outside the slots (levels spent, a new
//! enchanting seed, a damaged anvil, a beacon's effects, a counted trade) is an [`Effect`] the
//! commit applies ([`apply`]). What the checks need from outside the inventory (the player's
//! level, the beacon's pyramid, the trade offers) is read before the request runs
//! ([`context`]).

mod anvil;
mod enchant;
#[cfg(test)]
mod tests;

pub(super) use self::enchant::Choice;
use super::super::{SessionId, State, block_entity, dispatch, interact, survival, trade};
use super::request::{Created, Error, Exec, Made, Result};
use super::{SlotRef, Station, WindowKind};
use crate::block;
use crate::crafting::{self, Ingredient, Recipe, ShapelessType};
use crate::event::{EnchantOption, PlayerEnchantingOptionsRequest};
use crate::item::{self, Stack};
use crate::nbt::{Compound, List, Value};
use crate::protocol::packet::{Packet, PlayerEnchantOptions};
use crate::protocol::{
    EnchantmentInstance, EnchantmentOption, ItemEnchantments, ItemStackRequest, StackRequestAction,
};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::BlockPos;
pub(in crate::tick) use enchant::enchant as enchant_item;

/// The most patterns a loom puts on a banner (Bedrock).
const MAX_LOOM_PATTERNS: usize = 6;
/// The chance in 100 that using an anvil damages it (dragonfly: 12%).
const ANVIL_DAMAGE_PERCENT: u32 = 12;
/// The random stream purpose of anvil damage.
const ANVIL: u64 = random::tag("anvil-damage");

/// What a station window shows besides its slots: the enchanting options for its input now.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Shown {
    /// The input the options were made for, one of it.
    input: Stack,
    /// The options with their recipe network IDs, at most three.
    options: Vec<(u32, enchant::Choice)>,
}

crate::tick::checkpoint::fields!(Shown { input, options });

impl Shown {
    /// The options shown, with their recipe network IDs (tests).
    #[cfg(test)]
    pub(super) fn options(&self) -> &[(u32, Choice)] {
        &self.options
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::super::Fnv) {
        let Self { input, options } = self;
        super::hash_stack(h, input);
        h.usize(options.len());
        for (id, c) in options {
            h.u64(u64::from(*id));
            h.u64(u64::from(c.level));
            h.bytes(c.name.as_bytes());
            for (e, l) in &c.enchantments {
                h.u64(u64::from(e.id()) << 8 | u64::from(*l));
            }
        }
    }
}

/// What a request's station actions need from outside the inventory.
#[derive(Debug, Default)]
pub(super) struct Context {
    /// The player's experience level: 0 with `experience = false`, so a survival player takes no
    /// enchanting option or anvil work, which all cost levels.
    level: u32,
    /// The request's filter strings (an anvil's new name).
    filter: Vec<String>,
    /// The pyramid levels of the open beacon.
    beacon_level: u8,
    /// The trade offers the request's `CraftRecipe` and `CraftRecipeAuto` actions name.
    offers: Vec<(u32, trade::Offer)>,
    /// The server's custom content: its recipes and creative items.
    pub(super) custom: std::sync::Arc<crate::custom::Registry>,
    /// The player's creative inventory, by its index in the custom content.
    pub(super) creative: u16,
}

/// Reads what the stations need for `request` of `session` (the executor sees only the inventory).
pub(super) fn context(state: &State, session: SessionId, request: &ItemStackRequest) -> Context {
    let Some(player) = state.players.get(&session) else {
        return Context::default();
    };
    let kind = player.inventory.window.as_ref().map(|w| w.kind);
    // Only a payment reads the pyramid, which takes up to a few hundred block reads.
    let pays = || {
        request
            .actions
            .iter()
            .any(|a| matches!(a, StackRequestAction::BeaconPayment(_)))
    };
    let beacon_level = match kind {
        Some(WindowKind::Station {
            world,
            pos,
            kind: Station::Beacon,
        }) if pays() => block_entity::beacon_level(state, world, pos),
        _ => 0,
    };
    let offers = if matches!(kind, Some(WindowKind::Trade { .. })) {
        request
            .actions
            .iter()
            .filter_map(|a| match a {
                StackRequestAction::CraftRecipe(c) => Some(c.recipe_network_id),
                StackRequestAction::AutoCraftRecipe(c) => Some(c.recipe_network_id),
                _ => None,
            })
            .take(super::request::MAX_ACTIONS)
            .filter_map(|id| Some((id, trade::offer(state, session, id)?)))
            .collect()
    } else {
        Vec::new()
    };
    Context {
        level: if state.settings.experience {
            survival::xp_level(player)
        } else {
            0
        },
        filter: request.filter_strings.iter().take(1).cloned().collect(),
        beacon_level,
        offers,
        custom: std::sync::Arc::clone(&state.custom),
        creative: player.inventory.creative,
    }
}

/// A station craft in progress in a request, with what it must consume.
#[derive(Debug, Clone)]
pub(super) enum Craft {
    /// An enchanting option: the input (one) and the lapis it costs.
    Enchant {
        input: Stack,
        lapis: u8,
        /// The levels the option takes (its index plus one).
        cost: u32,
    },
    /// A stonecutter recipe `times` times.
    Stonecutter { ingredient: Ingredient, times: u8 },
    /// A smithing recipe: one of each of the three slots.
    Smithing {
        input: Stack,
        material: Stack,
        template: Stack,
    },
    /// An anvil: the whole input and `material` of the material.
    Anvil {
        input: Stack,
        material: Stack,
        material_used: u8,
        cost: u32,
    },
    /// A loom pattern on `times` banners, with as many dyes.
    Loom {
        banner: Stack,
        dye: Stack,
        times: u8,
    },
    /// A beacon payment of one item.
    Beacon { primary: i32, secondary: i32 },
    /// A trade `times` times.
    Trade {
        net_id: u32,
        offer: trade::Offer,
        times: u8,
        payments: Payments,
    },
}

/// Where a trade's payments come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Payments {
    /// `CraftRecipe`: the two payment slots, the first payment from the first.
    Slots,
    /// `CraftRecipeAuto` (the quick trade: the trade button, a right click on the arrow): the
    /// payment slots and the main inventory, in any mix.
    Anywhere,
}

/// What a committed station craft does besides moving items.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Effect {
    /// Enchanted: `cost` levels taken and a new enchanting seed.
    Enchanted { cost: u32 },
    /// An anvil used: `cost` levels taken; with finite resources it may be damaged.
    Anvil { cost: u32, finite: bool },
    /// Beacon effects chosen.
    Beacon { primary: i32, secondary: i32 },
    /// `times` trades of offer `net_id`.
    Traded { net_id: u32, times: u32 },
}

/// The kind of the open station, if one is open.
fn station(exec: &Exec<'_>) -> Option<Station> {
    match exec.window?.kind {
        WindowKind::Station { kind, .. } => Some(kind),
        _ => None,
    }
}

/// Whether a trade window is open.
fn trading(exec: &Exec<'_>) -> bool {
    exec.window
        .is_some_and(|w| matches!(w.kind, WindowKind::Trade { .. }))
}

/// Whether the open window's station handles `CraftRecipeAuto` itself (a quick trade).
pub(super) fn takes_auto_craft(exec: &Exec<'_>) -> bool {
    trading(exec)
}

/// Whether the open window's station handles `CraftRecipe` itself (enchanting options, trade
/// offers, stonecutter and smithing recipes; P6).
pub(super) fn takes_craft_recipe(exec: &Exec<'_>) -> bool {
    trading(exec)
        || matches!(
            station(exec),
            Some(Station::Enchant | Station::Stonecutter | Station::Smithing)
        )
}

/// Whether a station craft is in progress, so consume actions are allowed.
pub(super) fn crafting(exec: &Exec<'_>) -> bool {
    exec.station.is_some()
}

/// Whether a destroy action is a beacon's payment (dragonfly: the client destroys the payment
/// after `BeaconPayment`), taken as a consume.
pub(super) fn takes_destroy(exec: &Exec<'_>) -> bool {
    matches!(exec.station, Some(Craft::Beacon { .. }))
}

/// A request action only a station takes.
pub(super) fn action(exec: &mut Exec<'_>, action: &StackRequestAction) -> Result<()> {
    if exec.station.is_some() {
        return Err(Error::Crafting);
    }
    let craft = match (action, station(exec)) {
        (StackRequestAction::CraftRecipe(a), Some(Station::Enchant)) => {
            begin_enchant(exec, a.recipe_network_id)?
        }
        (StackRequestAction::CraftRecipe(a), Some(Station::Stonecutter)) => {
            begin_stonecutter(exec, a.recipe_network_id, a.number_of_crafts)?
        }
        (StackRequestAction::CraftRecipe(a), Some(Station::Smithing)) => {
            begin_smithing(exec, a.recipe_network_id)?
        }
        (StackRequestAction::CraftRecipe(a), None) if trading(exec) => begin_trade(
            exec,
            a.recipe_network_id,
            a.number_of_crafts,
            Payments::Slots,
        )?,
        // The client's ingredient list is not used: the consumes are checked against the offer.
        (StackRequestAction::AutoCraftRecipe(a), None) if trading(exec) => begin_trade(
            exec,
            a.recipe_network_id,
            a.number_of_crafts,
            Payments::Anywhere,
        )?,
        (StackRequestAction::CraftRecipeOptional(a), Some(Station::Anvil)) => {
            begin_anvil(exec, a.filter_string_index)?
        }
        (StackRequestAction::CraftLoomRecipe(a), Some(Station::Loom)) => {
            begin_loom(exec, &a.pattern, a.times_crafted)?
        }
        (StackRequestAction::BeaconPayment(a), Some(Station::Beacon)) => {
            begin_beacon(exec, a.primary_effect, a.secondary_effect)?
        }
        _ => return Err(Error::Unsupported),
    };
    exec.station = Some(craft);
    Ok(())
}

/// The stack in the window's own slot `i` as the request has left it.
fn ui(exec: &Exec<'_>, i: u8) -> Result<Stack> {
    exec.slot(SlotRef::Ui(i)).map(|s| s.stack)
}

/// `CraftRecipe` on an enchanting table: the option with that recipe network ID enchants the
/// input (PMMP `ItemStackRequestExecutor`, `EnchantingTransaction`): the option must be shown
/// for the input there now (PMMP shows new options as the input changes, so old IDs fail); with
/// finite resources the player needs the option's level, and the lapis spent is the option's
/// index plus one (P6).
fn begin_enchant(exec: &mut Exec<'_>, id: u32) -> Result<Craft> {
    let window = exec.window.ok_or(Error::NoSlot)?;
    let (index, choice) = window
        .station
        .options
        .iter()
        .enumerate()
        .find_map(|(i, (option_id, c))| (*option_id == id).then_some((i, c.clone())))
        .ok_or(Error::Recipe)?;
    let input = ui(exec, 0)?.with_count(1);
    if input.is_empty() || input != window.station.input {
        return Err(Error::Recipe);
    }
    if !exec.creative && exec.ctx.level < u32::from(choice.level) {
        return Err(Error::Recipe);
    }
    let cost = u8::try_from(index + 1).map_err(|_| Error::Recipe)?;
    let result = enchant::enchant(&input, &choice.enchantments).map_err(|_| Error::Recipe)?;
    exec.set_created(Some(Created::of(&result, 1)))?;
    if exec.describe {
        exec.made = Some(Made::Enchant {
            choice,
            input: input.clone(),
            output: result,
            cost,
        });
    }
    Ok(Craft::Enchant {
        input,
        lapis: cost,
        cost: u32::from(cost),
    })
}

/// `CraftRecipe` on a stonecutter (dragonfly `handleStonecutting`; PMMP crafts it as any
/// recipe): a stonecutter recipe `times` times from the input slot.
fn begin_stonecutter(exec: &mut Exec<'_>, id: u32, times: u8) -> Result<Craft> {
    let Some(Recipe::Shapeless(r)) = exec.ctx.custom.recipe(id) else {
        return Err(Error::Recipe);
    };
    let (ShapelessType::Stonecutter, [ingredient], [result]) = (r.kind, r.ingredients, r.results)
    else {
        return Err(Error::Recipe);
    };
    let result = result.to_item().ok_or(Error::Recipe)?;
    if times == 0 || !ingredient.accepts(&ui(exec, 0)?) {
        return Err(Error::Recipe);
    }
    exec.set_created(Some(Created::of(&result, u32::from(times))))?;
    made(exec, result, times);
    Ok(Craft::Stonecutter {
        ingredient: *ingredient,
        times,
    })
}

/// `CraftRecipe` on a smithing table (dragonfly `handleSmithing`; PMMP has no smithing): the
/// input, material and template must be the recipe's; a transform turns the input into the
/// result's item keeping its data (damage, enchantments, name), a trim adds the armour trim of
/// the template's pattern and the material (`Trim {Material, Pattern}`).
fn begin_smithing(exec: &mut Exec<'_>, id: u32) -> Result<Craft> {
    let (input, material, template) = (ui(exec, 0)?, ui(exec, 1)?, ui(exec, 2)?);
    let one = input.with_count(1);
    let result = match exec.ctx.custom.recipe(id) {
        Some(Recipe::SmithingTransform(r))
            if r.input.accepts(&input)
                && r.addition.accepts(&material)
                && r.template.accepts(&template) =>
        {
            Stack::from_parts(r.result.item, 1, r.result.meta, one.tag())
                .map_err(|_| Error::Recipe)?
        }
        Some(Recipe::SmithingTrim(r))
            if r.input.accepts(&input)
                && r.addition.accepts(&material)
                && r.template.accepts(&template) =>
        {
            let pattern = template
                .item()
                .and_then(crafting::trim_pattern)
                .ok_or(Error::Recipe)?;
            let trim_material = material
                .item()
                .and_then(crafting::trim_material)
                .ok_or(Error::Recipe)?;
            let mut extra = one.extra().cloned().unwrap_or_default();
            extra.nbt.insert(
                "Trim".to_owned(),
                Value::Compound(Compound::from([
                    (
                        "Material".to_owned(),
                        Value::String(trim_material.to_owned()),
                    ),
                    ("Pattern".to_owned(), Value::String(pattern.to_owned())),
                ])),
            );
            one.with_extra(Some(extra)).map_err(|_| Error::Recipe)?
        }
        _ => return Err(Error::Recipe),
    };
    if result.is_empty() {
        return Err(Error::Recipe);
    }
    exec.set_created(Some(Created::of(&result, 1)))?;
    made(exec, result, 1);
    Ok(Craft::Smithing {
        input: one,
        material: material.with_count(1),
        template: template.with_count(1),
    })
}

/// `CraftRecipeOptional` on an anvil ([`anvil::craft`]): the request's filter string at the
/// action's index is the new name.
fn begin_anvil(exec: &mut Exec<'_>, filter_index: i32) -> Result<Craft> {
    let (input, material) = (ui(exec, 0)?, ui(exec, 1)?);
    let rename = if exec.ctx.filter.is_empty() {
        None
    } else {
        let index = usize::try_from(filter_index).map_err(|_| Error::Recipe)?;
        Some(exec.ctx.filter.get(index).ok_or(Error::Recipe)?.as_str())
    };
    let out = anvil::craft(&input, &material, rename, exec.creative).map_err(|_| Error::Recipe)?;
    if !exec.creative && exec.ctx.level < out.cost {
        return Err(Error::Recipe);
    }
    exec.set_created(Some(Created::of(&out.result, 1)))?;
    Ok(Craft::Anvil {
        input,
        material: material.with_count(out.material_used),
        material_used: out.material_used,
        cost: out.cost,
    })
}

/// `CraftLoomRecipe` (dragonfly `handleLoomCraft`; PMMP has no loom): the pattern goes on
/// `times` banners in the dye's colour. Patterns that need a pattern item cannot be made (PMMP
/// has no banner pattern items); an ominous banner takes none; a banner holds at most six.
fn begin_loom(exec: &mut Exec<'_>, pattern: &str, times: u8) -> Result<Craft> {
    let (banner, dye) = (ui(exec, 0)?, ui(exec, 1)?);
    let banner_type = banner.item().ok_or(Error::Recipe)?;
    let colour = dye
        .item()
        .and_then(item::dye_colour)
        .and_then(crate::tile::DyeColour::from_id)
        .ok_or(Error::Recipe)?;
    if times == 0
        || banner_type.name() != "minecraft:banner"
        || banner_type.nbt().is_some()
        || banner.count() < times
        || dye.count() < times
        || block::BannerPattern::from_id(pattern).is_none()
        || needs_pattern_item(pattern)
    {
        return Err(Error::Recipe);
    }
    let mut tag = banner.tag().unwrap_or_default();
    let mut list = match tag.remove("Patterns") {
        Some(Value::List(List::Compound(list))) => list,
        _ => Vec::new(),
    };
    if list.len() >= MAX_LOOM_PATTERNS {
        return Err(Error::Recipe);
    }
    list.push(Compound::from([
        ("Pattern".to_owned(), Value::String(pattern.to_owned())),
        ("Color".to_owned(), Value::Int(colour.banner_id())),
    ]));
    tag.insert("Patterns".to_owned(), Value::List(List::Compound(list)));
    let result =
        Stack::from_parts(banner_type, 1, banner.meta(), Some(tag)).map_err(|_| Error::Recipe)?;
    exec.set_created(Some(Created::of(&result, u32::from(times))))?;
    made(exec, result, times);
    Ok(Craft::Loom {
        banner: banner.with_count(1),
        dye: dye.with_count(1),
        times,
    })
}

/// Whether a banner pattern needs its pattern item in the loom (dragonfly
/// `BannerPatternType.Item`).
fn needs_pattern_item(pattern: &str) -> bool {
    matches!(
        pattern,
        "bri" | "cre" | "cbo" | "flo" | "moj" | "sku" | "glb" | "pig" | "flw" | "gus"
    )
}

/// `BeaconPayment` (dragonfly `handleBeaconPayment`; PMMP keeps only the effect IDs): the effects
/// must be allowed for the beacon's pyramid, the secondary only at level 4. The payment item is
/// checked in [`finish`], on what the request consumed.
fn begin_beacon(exec: &mut Exec<'_>, primary: i32, secondary: i32) -> Result<Craft> {
    let level = exec.ctx.beacon_level;
    if !block_entity::beacon_effect_allowed(primary, level)
        || !block_entity::beacon_effect_allowed(secondary, level)
        || (level < 4 && secondary != 0)
    {
        return Err(Error::Recipe);
    }
    Ok(Craft::Beacon { primary, secondary })
}

/// `CraftRecipe` (BDS, P6) or `CraftRecipeAuto` in a trade window: the offer with that network
/// ID, `times` times; its payments come from where `payments` says.
fn begin_trade(exec: &mut Exec<'_>, net_id: u32, times: u8, payments: Payments) -> Result<Craft> {
    let offer = exec
        .ctx
        .offers
        .iter()
        .find_map(|(id, o)| (*id == net_id).then(|| o.clone()))
        .ok_or(Error::Recipe)?;
    // At most the offer's uses left, capped at 256 by trading; the action's count is a byte.
    if times == 0
        || u32::from(times) > offer.max_times
        || offer.sell.is_empty()
        || offer.buy_a.is_empty()
    {
        return Err(Error::Recipe);
    }
    exec.set_created(Some(Created::of(&offer.sell, u32::from(times))))?;
    if exec.describe {
        exec.made = Some(Made::Trade {
            net_id,
            inputs: [offer.buy_a.clone(), offer.buy_b.clone()],
            output: offer.sell.clone(),
            times: u32::from(times),
        });
    }
    Ok(Craft::Trade {
        net_id,
        offer,
        times,
        payments,
    })
}

/// Describes a station recipe making `result` `times` times for the events (its inputs come from
/// what the request consumes).
fn made(exec: &mut Exec<'_>, result: Stack, times: u8) {
    if exec.describe {
        exec.made = Some(Made::Craft {
            inputs: Vec::new(),
            outputs: vec![result],
            times: u32::from(times),
        });
    }
}

/// What the consume actions took from the window's own slot `i`: the count, and whether every
/// stack was `like` (the same item, damage and data).
fn taken_from(exec: &Exec<'_>, i: u8, like: impl Fn(&Stack) -> bool) -> (u32, bool) {
    let mut count = 0u32;
    let mut all = true;
    for (r, stack) in &exec.consumed {
        if *r == Some(SlotRef::Ui(i)) {
            count += u32::from(stack.count());
            all &= like(stack);
        }
    }
    (count, all)
}

/// Whether every consume action took from the window's own slots in `allowed`.
fn only_from(exec: &Exec<'_>, allowed: &[u8]) -> bool {
    exec.consumed
        .iter()
        .all(|(r, _)| matches!(r, Some(SlotRef::Ui(i)) if allowed.contains(i)))
}

/// Whether `stack` is `want`'s item with its damage and data (the count aside).
fn same(want: &Stack) -> impl Fn(&Stack) -> bool + '_ {
    move |s: &Stack| s.can_stack_with(want)
}

/// Whether a trade payment `stack` is the offer's `want` ([`trade::is_payment`]).
fn pays(want: &Stack, damage: trade::Damage) -> impl Fn(&Stack) -> bool + '_ {
    move |s: &Stack| trade::is_payment(want, damage, s)
}

/// Whether the consume actions of a quick trade took exactly `times` of the offer's payments,
/// from the payment slots and the main inventory. A stack that pays both payments (the same item
/// twice) counts towards whichever still needs it.
fn paid_anywhere(exec: &Exec<'_>, offer: &trade::Offer, times: u32) -> bool {
    let want_a = u32::from(offer.buy_a.count()) * times;
    let want_b = u32::from(offer.buy_b.count()) * times;
    let (pays_a, pays_b) = (
        pays(&offer.buy_a, offer.damage_a),
        pays(&offer.buy_b, offer.damage_b),
    );
    let (mut only_a, mut only_b, mut either) = (0u32, 0u32, 0u32);
    for (r, stack) in &exec.consumed {
        if !matches!(r, Some(SlotRef::Ui(0 | 1) | SlotRef::Main(_))) {
            return false;
        }
        let count = u32::from(stack.count());
        let total = match (pays_a(stack), want_b > 0 && pays_b(stack)) {
            (true, true) => &mut either,
            (true, false) => &mut only_a,
            (false, true) => &mut only_b,
            (false, false) => return false,
        };
        *total = total.saturating_add(count);
    }
    only_a <= want_a
        && only_b <= want_b
        && only_a.saturating_add(only_b).saturating_add(either) == want_a + want_b
}

/// The station's end-of-request checks: the craft consumed exactly its inputs; its effect waits
/// for the commit.
pub(super) fn finish(exec: &mut Exec<'_>) -> Result<()> {
    let Some(craft) = exec.station.take() else {
        return Ok(());
    };
    let exact = |exec: &Exec<'_>, i: u8, want: u32, like: &dyn Fn(&Stack) -> bool| {
        let (count, all) = taken_from(exec, i, like);
        count == want && all
    };
    let ok = match &craft {
        Craft::Enchant { input, lapis, cost } => {
            let lapis_type = item::Kind::LapisLazuli.default_type();
            let (spent, all_lapis) = taken_from(exec, 1, |s| s.item() == Some(lapis_type));
            let lapis_ok = if exec.creative {
                all_lapis
            } else {
                all_lapis && spent == u32::from(*lapis)
            };
            exec.effects.push(Effect::Enchanted { cost: *cost });
            only_from(exec, &[0, 1]) && exact(exec, 0, 1, &same(input)) && lapis_ok
        }
        Craft::Stonecutter { ingredient, times } => {
            only_from(exec, &[0]) && exact(exec, 0, u32::from(*times), &|s| ingredient.accepts(s))
        }
        Craft::Smithing {
            input,
            material,
            template,
        } => {
            only_from(exec, &[0, 1, 2])
                && exact(exec, 0, 1, &same(input))
                && exact(exec, 1, 1, &same(material))
                && exact(exec, 2, 1, &same(template))
        }
        Craft::Anvil {
            input,
            material,
            material_used,
            cost,
        } => {
            let finite = !exec.creative;
            exec.effects.push(Effect::Anvil {
                cost: if finite { *cost } else { 0 },
                finite,
            });
            only_from(exec, &[0, 1])
                && exact(exec, 0, u32::from(input.count()), &same(input))
                && exact(exec, 1, u32::from(*material_used), &same(material))
        }
        Craft::Loom { banner, dye, times } => {
            only_from(exec, &[0, 1])
                && exact(exec, 0, u32::from(*times), &same(banner))
                && exact(exec, 1, u32::from(*times), &same(dye))
        }
        Craft::Beacon { primary, secondary } => {
            exec.effects.push(Effect::Beacon {
                primary: *primary,
                secondary: *secondary,
            });
            only_from(exec, &[0])
                && exact(exec, 0, 1, &|s| {
                    s.item().is_some_and(block_entity::beacon_payment)
                })
        }
        Craft::Trade {
            net_id,
            offer,
            times,
            payments,
        } => {
            let times = u32::from(*times);
            exec.effects.push(Effect::Traded {
                net_id: *net_id,
                times,
            });
            let b = u32::from(offer.buy_b.count()) * times;
            match payments {
                Payments::Slots => {
                    only_from(exec, &[0, 1])
                        && exact(
                            exec,
                            0,
                            u32::from(offer.buy_a.count()) * times,
                            &pays(&offer.buy_a, offer.damage_a),
                        )
                        && (b == 0 || exact(exec, 1, b, &pays(&offer.buy_b, offer.damage_b)))
                        && (b != 0 || taken_from(exec, 1, |_| true).0 == 0)
                }
                Payments::Anywhere => paid_anywhere(exec, offer, times),
            }
        }
    };
    if ok { Ok(()) } else { Err(Error::Consumed) }
}

/// Applies the effects of the committed request `request` of `session`: levels, the enchanting
/// seed, the anvil's damage, the beacon's effects, the trade's count. Then, if the request
/// changed an enchanting table's input, its options are worked out again and sent (PMMP
/// `EnchantInventory::onSlotChange`; BDS sends the empty list after an enchant, P6).
pub(super) fn apply(
    state: &mut State,
    session: SessionId,
    request: i32,
    effects: Vec<Effect>,
    input_changed: bool,
) {
    let window = state
        .players
        .get(&session)
        .and_then(|p| p.inventory.window.as_ref())
        .map(|w| w.kind);
    for effect in effects {
        match effect {
            Effect::Enchanted { cost } => {
                // PMMP: with finite resources as many of the levels as the player has.
                let finite = state
                    .players
                    .get(&session)
                    .is_some_and(|p| survival::has_finite_resources(survival::game_mode(state, p)));
                if let Some(player) = state.players.get_mut(&session) {
                    if finite {
                        let level = survival::xp_level(player);
                        survival::subtract_levels(player, cost.min(level));
                    }
                    survival::regenerate_enchantment_seed(player);
                }
            }
            Effect::Anvil { cost, finite } => {
                if let Some(player) = state.players.get_mut(&session) {
                    survival::subtract_levels(player, cost);
                }
                if let Some(WindowKind::Station {
                    world,
                    pos,
                    kind: Station::Anvil,
                }) = window
                {
                    use_anvil(state, session, request, world, pos, finite);
                }
            }
            Effect::Beacon { primary, secondary } => {
                if let Some(WindowKind::Station {
                    world,
                    pos,
                    kind: Station::Beacon,
                }) = window
                {
                    block_entity::set_beacon(state, world, pos, primary, secondary);
                }
            }
            Effect::Traded { net_id, times } => {
                trade::traded(state, session, request, net_id, times);
            }
        }
    }
    if input_changed
        && let Some(WindowKind::Station {
            world,
            pos,
            kind: Station::Enchant,
        }) = window
    {
        refresh_options(state, session, world, pos);
    }
}

/// Works out an enchanting table's options for its input again and sends them.
fn refresh_options(state: &mut State, session: SessionId, world: WorldIndex, pos: BlockPos) {
    let shelves = state
        .worlds
        .get(world)
        .map_or(0, |w| enchant::bookshelves(w, pos));
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let seed = survival::enchantment_seed(player);
    let input = enchant_input(state, session);
    let mut choices = enchant::options(&input, shelves, seed);
    if !choices.is_empty() && dispatch::heard::<PlayerEnchantingOptionsRequest>(state) {
        let mut event = PlayerEnchantingOptionsRequest {
            player: crate::player::Id::new(session),
            world: crate::world::Id::of(world),
            pos,
            options: choices.iter().map(option_of).collect(),
            cancelled: false,
        };
        dispatch::call(state, &mut event);
        let moved = dispatch::listened::<PlayerEnchantingOptionsRequest>(state)
            && (window_of(state, session)
                != Some(WindowKind::Station {
                    world,
                    pos,
                    kind: Station::Enchant,
                })
                || enchant_input(state, session) != input);
        if moved {
            return;
        }
        choices = if event.cancelled {
            Vec::new()
        } else {
            event.options.iter().map(choice_of).collect()
        };
        show_options(state, session, &input, choices);
        dispatch::watch(state, &event);
        return;
    }
    show_options(state, session, &input, choices);
}

/// The kind of `session`'s window.
fn window_of(state: &State, session: SessionId) -> Option<WindowKind> {
    state
        .players
        .get(&session)?
        .inventory
        .window
        .as_ref()
        .map(|w| w.kind)
}

/// The input slot of `session`'s station window.
fn enchant_input(state: &State, session: SessionId) -> Stack {
    state
        .players
        .get(&session)
        .and_then(|p| p.inventory.window.as_ref())
        .and_then(|w| w.ui.first())
        .map(|s| s.stack.clone())
        .unwrap_or_default()
}

/// The longest option name a listener may set, in bytes (the client shows a few words).
const MAX_OPTION_NAME: usize = 64;
/// The most enchantments of an option a listener sets (one of each kind is far fewer).
const MAX_OPTION_ENCHANTMENTS: usize = 16;

/// An option as the event shows it.
fn option_of(c: &Choice) -> EnchantOption {
    EnchantOption {
        required_level: c.level,
        name: c.name.clone(),
        enchantments: c.enchantments.clone(),
    }
}

/// An option a listener set, kept within what the packet and the client take: the name cut to
/// [`MAX_OPTION_NAME`] bytes at a character, at most [`MAX_OPTION_ENCHANTMENTS`] enchantments,
/// none of level 0.
fn choice_of(o: &EnchantOption) -> Choice {
    let mut end = o.name.len().min(MAX_OPTION_NAME);
    while !o.name.is_char_boundary(end) {
        end -= 1;
    }
    Choice {
        level: o.required_level,
        name: o.name.get(..end).unwrap_or_default().to_owned(),
        enchantments: o
            .enchantments
            .iter()
            .copied()
            .filter(|(_, level)| *level > 0)
            .take(MAX_OPTION_ENCHANTMENTS)
            .collect(),
    }
}

/// Shows `choices` for `input` in `session`'s enchanting window, each with a new recipe network
/// ID after every recipe, the server's own too (P6: BDS never reuses one).
fn show_options(state: &mut State, session: SessionId, input: &Stack, choices: Vec<Choice>) {
    let first = state.custom.first_special_id();
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let inv = &mut player.inventory;
    let mut options = Vec::with_capacity(choices.len());
    for c in choices {
        let id = inv.next_option_id.max(first);
        inv.next_option_id = id.checked_add(1).unwrap_or(first);
        options.push((id, c));
    }
    let packet = options_packet(enchant::slot(input.item()), &options);
    if let Some(w) = inv.window.as_mut() {
        w.station = Shown {
            input: input.with_count(1),
            options,
        };
    }
    player.send_packet(packet);
}

/// `PlayerEnchantOptions` of `options` for an input of enchantment slot `slot` in BDS's shape
/// (P6 `enchant`): the slot flag, and each enchantment in the list of its activation
/// ([`enchant::activation`]). PMMP sends slot 0 and every enchantment in the first list.
pub(super) fn options_packet(slot: i32, options: &[(u32, enchant::Choice)]) -> Packet {
    Packet::PlayerEnchantOptions(PlayerEnchantOptions {
        options: options
            .iter()
            .map(|(id, c)| {
                let mut lists: [Vec<EnchantmentInstance>; 3] = Default::default();
                for (e, l) in &c.enchantments {
                    if let Some(list) = lists.get_mut(enchant::activation(*e)) {
                        list.push(EnchantmentInstance {
                            r#type: e.id(),
                            level: *l,
                        });
                    }
                }
                EnchantmentOption {
                    cost: c.level,
                    enchantments: ItemEnchantments {
                        slot,
                        enchantments: lists,
                    },
                    name: c.name.clone(),
                    recipe_network_id: *id,
                }
            })
            .collect(),
    })
}

/// An anvil was used (dragonfly): with finite resources it takes damage with a 12% chance, one
/// stage at a time (anvil, chipped, damaged, gone); the use or break sound plays.
fn use_anvil(
    state: &mut State,
    session: SessionId,
    request: i32,
    world: WorldIndex,
    pos: BlockPos,
    finite: bool,
) {
    let Some(block) = state.worlds.get(world).and_then(|w| w.block(pos)) else {
        return;
    };
    let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
    // A player may use the anvil in several requests of one tick: the request ID, unique among
    // the requests the server took, keeps each use's draw its own whatever it refused before.
    let mut rng = Random::new(
        seed,
        state.tick,
        random::key(
            ANVIL,
            &[
                session.get(),
                u64::from(request.cast_unsigned()),
                u64::from(pos.x.cast_unsigned()),
                u64::from(pos.y.cast_unsigned()),
                u64::from(pos.z.cast_unsigned()),
            ],
        ),
    );
    let damaged = finite && rng.chance(ANVIL_DAMAGE_PERCENT, 100);
    let next = if damaged {
        match block.name() {
            "minecraft:anvil" => interact::props::renamed(block, "minecraft:chipped_anvil"),
            "minecraft:chipped_anvil" => interact::props::renamed(block, "minecraft:damaged_anvil"),
            _ => Some(block::AIR),
        }
    } else {
        None
    };
    let used = if next == Some(block::AIR) {
        crate::world::Sound::AnvilBreak
    } else {
        crate::world::Sound::AnvilUse
    };
    interact::sound(state, world, interact::block_vec(pos), used);
    if let Some(next) = next {
        if let Some(w) = state.worlds.get_mut(world)
            && w.set_block(pos, next).is_err()
        {
            tracing::warn!(
                x = pos.x,
                y = pos.y,
                z = pos.z,
                "could not damage the anvil"
            );
        }
        if next == block::AIR {
            super::close_viewers(state, world, pos);
        }
    }
}
