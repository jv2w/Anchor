//! Hunger, experience and consumables (PMMP `HungerManager`, `ExperienceManager`,
//! `ExperienceUtils`, `Human::consumeObject`, `Player::useHeldItem`/`consumeHeldItem`).

use super::attributes::{EXHAUSTION, EXPERIENCE, HUNGER, LEVEL, SATURATION};
use super::damage::{self, Damage};
use super::effects::{self, Active};
use super::{
    MAX_FOOD, MAX_SATURATION, Survival, broadcast_to_viewers, difficulty, feet, game_mode,
    movement, player_random, sound, world_seed,
};
use crate::config::{Difficulty, GameMode};
use crate::effect::Type as Effect;
use crate::item::{Class, Consumable, Enchantment, Food, Stack, Type};
use crate::math;
use crate::protocol::consts::{ActorEvent, LevelEvent, SoundEvent};
use crate::protocol::packet::{self, Packet};
use crate::random;
use crate::tick::{Phase, Player, SessionId, State, entity, inventory};

/// Exhaustion that costs one saturation or food point (PMMP `HungerManager::exhaust`).
const EXHAUSTION_PER_POINT: f64 = 4.0;
/// Ticks of one food cycle (PMMP `HungerManager::tick`).
const FOOD_CYCLE: u32 = 80;
/// The highest experience level (PMMP `Attribute::EXPERIENCE_LEVEL` maximum).
pub(super) const MAX_LEVEL: u32 = 24_791;
/// The most experience points a player holds (PMMP `Limits::INT32_MAX`).
const MAX_XP: u64 = i32::MAX.cast_unsigned() as u64;
/// Ticks an item takes to eat or drink (Bedrock: 1.6 s; dried kelp 0.8 s).
const CONSUME_TICKS: u64 = 32;
/// Ticks dried kelp takes to eat.
const DRIED_KELP_TICKS: u64 = 16;
/// The share of the eating time that must pass between the start and the end packets: the two
/// packets arrive with network jitter, so a quarter of the time is forgiven. PMMP checks no time
/// at all, so a client could eat instantly.
const CONSUME_TOLERANCE_DIVISOR: u64 = 4;
/// Ticks between eating animations (PMMP `Player::onUpdate`).
const EATING_ANIMATION_TICKS: u64 = 4;

/// Ticks after an orb pickup before the next (PMMP `ExperienceManager::resetXpCooldown`).
const XP_PICKUP_COOLDOWN: u8 = 2;
/// Durability one experience point repairs with mending (PMMP `onPickupXp`).
const MENDING_PER_POINT: u32 = 2;
/// What a slice of cake restores (PMMP `BaseCake`).
const CAKE_FOOD: f64 = 2.0;
const CAKE_SATURATION: f64 = 0.4;

const POISONOUS_POTATO: u64 = random::tag("poisonous-potato");
const RAW_CHICKEN: u64 = random::tag("raw-chicken");
const ROTTEN_FLESH: u64 = random::tag("rotten-flesh");
const CHORUS_FRUIT: u64 = random::tag("chorus-fruit");
const MENDING: u64 = random::tag("mending");

/// The item a player started eating or drinking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Using {
    /// The engine tick it started.
    pub(super) since: u64,
    /// The item type held then.
    pub(super) item: Type,
}

impl Using {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self { since, item } = self;
        h.u64(*since);
        h.u64(u64::from(item.runtime_id().cast_unsigned()));
    }
}

/// What an item cooldown is kept under: its cooldown tag, or the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CooldownKey {
    /// A tag shared by several items.
    Tag(&'static str),
    /// One item type, by runtime ID.
    Item(i16),
}

impl CooldownKey {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        match self {
            Self::Tag(t) => h.bytes(t.as_bytes()),
            Self::Item(i) => h.u64(u64::from(i.cast_unsigned())),
        }
    }
}

/// Adds exhaustion and spends saturation, then food, for every 4 points (PMMP
/// `HungerManager::exhaust`); nothing while hunger is off for the player.
pub(super) fn exhaust(s: &mut Survival, amount: f64) {
    if !s.motion.hunger || amount.is_nan() || amount <= 0.0 {
        return;
    }
    s.exhaustion += amount;
    while s.exhaustion >= EXHAUSTION_PER_POINT {
        s.exhaustion -= EXHAUSTION_PER_POINT;
        if s.saturation > 0.0 {
            s.saturation = if s.saturation > 1.0 {
                s.saturation - 1.0
            } else {
                0.0
            };
            s.mark(SATURATION);
        } else if s.food > 0.0 {
            set_food(s, s.food - 1.0);
        }
    }
    s.mark(EXHAUSTION);
}

/// Sets the food level, restarting the food cycle when it crosses 17, 6 or 0 (PMMP
/// `HungerManager::setFood`).
fn set_food(s: &mut Survival, food: f64) {
    let food = if food < 0.0 { 0.0 } else { food };
    if [17.0, 6.0, 0.0].iter().any(|&b| (s.food > b) != (food > b)) {
        s.food_timer = 0;
    }
    s.food = food;
    s.mark(HUNGER);
}

/// Adds food and saturation within their limits (PMMP `addFood`, `addSaturation`).
pub(super) fn add_food(s: &mut Survival, food: f64, saturation: f64) {
    let f = s.food + food;
    set_food(s, if f > MAX_FOOD { MAX_FOOD } else { f });
    s.saturation = math::clamp(s.saturation + saturation, 0.0, MAX_SATURATION);
    s.mark(SATURATION);
}

/// One tick of hunger (PMMP `HungerManager::tick`): peaceful refills food and heals; a full bar
/// heals every 80 ticks for 6 exhaustion; an empty bar starves (easy down to 10 health, normal
/// to 1, hard to death); food at 6 or below stops sprinting.
pub(super) fn tick(state: &mut State, session: SessionId, enabled: bool) {
    let peaceful = difficulty(state) == Difficulty::Peaceful;
    let starve = {
        let Some(player) = state.players.get_mut(&session) else {
            return;
        };
        let s = &mut player.survival;
        if !enabled || s.is_dead() {
            return;
        }
        player.data_dirty = true;
        s.food_timer += 1;
        if s.food_timer >= FOOD_CYCLE {
            s.food_timer = 0;
        }
        let mut heal = 0.0;
        if peaceful && s.food_timer % 10 == 0 {
            if s.food < MAX_FOOD {
                set_food(s, s.food + 1.0);
            }
            if s.food_timer % 20 == 0 && s.health < s.max_health() {
                heal += 1.0;
            }
        }
        let mut starve = false;
        if s.food_timer == 0 {
            if s.food >= 18.0 {
                if s.health + heal < s.max_health() {
                    heal += 1.0;
                    exhaust(s, 6.0);
                }
            } else if s.food <= 0.0 {
                starve = true;
            }
        }
        if s.food <= 6.0 && s.motion.sprinting {
            s.motion.sprinting = false;
            s.mark(super::attributes::MOVEMENT);
        }
        if heal > 0.0 {
            damage::heal_player(player, heal);
        }
        starve
    };
    if starve {
        let health = state
            .players
            .get(&session)
            .map_or(0.0, |p| p.survival.health);
        let hurts = match difficulty(state) {
            Difficulty::Easy => health > 10.0,
            Difficulty::Normal => health > 1.0,
            Difficulty::Hard => true,
            Difficulty::Peaceful => false,
        };
        if hurts {
            damage::damage(state, session, 1.0, Damage::Starvation);
        }
    }
}

/// The experience needed to reach `level` from 0 (PMMP `ExperienceUtils::getXpToReachLevel`).
pub(super) fn xp_to_reach(level: u32) -> u64 {
    let l = u64::from(level);
    if level <= 16 {
        l * l + 6 * l
    } else if level <= 31 {
        // 2.5 l² - 40.5 l + 360, exact in halves: (5 l² - 81 l + 720) / 2, rounded down.
        (5 * l * l + 720 - 81 * l) / 2
    } else {
        // 4.5 l² - 162.5 l + 2220 = (9 l² - 325 l + 4440) / 2.
        (9 * l * l + 4440 - 325 * l) / 2
    }
}

/// The experience from `level` to the next (PMMP `ExperienceUtils::getXpToCompleteLevel`).
pub(super) fn xp_to_complete(level: u32) -> u64 {
    let l = u64::from(level);
    if level <= 15 {
        2 * l + 7
    } else if level <= 30 {
        5 * l - 38
    } else {
        9 * l - 158
    }
}

/// The level and the progress through it that `xp` points make. PMMP solves the level's
/// quadratic and keeps the fraction of the root, which is not the share of points collected in
/// the level; Anchor keeps the share, as the client shows it.
pub(super) fn level_of(xp: u64) -> (u32, f64) {
    let (mut lo, mut hi) = (0, MAX_LEVEL);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if xp_to_reach(mid) <= xp {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let into = xp.saturating_sub(xp_to_reach(lo));
    let need = xp_to_complete(lo);
    #[expect(
        clippy::cast_precision_loss,
        reason = "invariant: experience points stay below 2^31, exact in f64"
    )]
    let progress = if lo >= MAX_LEVEL || need == 0 {
        0.0
    } else {
        into as f64 / need as f64
    };
    (lo, progress)
}

/// The player's current experience points (PMMP `getCurrentTotalXp`).
pub(super) fn current_xp(s: &Survival) -> u64 {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: the points into a level are below 2^31 and progress is in [0, 1)"
    )]
    let into = (xp_to_complete(s.xp_level) as f64 * s.xp_progress) as u64;
    xp_to_reach(s.xp_level) + into
}

/// Sets the player's level and progress from a total.
fn set_xp(s: &mut Survival, total: u64) {
    let (level, progress) = level_of(total.min(MAX_XP));
    s.xp_level = level;
    s.xp_progress = progress;
    s.mark(LEVEL | EXPERIENCE);
}

/// Adds experience points (PMMP `ExperienceManager::addXp` without its sound): the lifetime
/// total stays below 2^31.
pub(super) fn add_experience(player: &mut Player, amount: u32) {
    let s = &mut player.survival;
    let room = i32::MAX.cast_unsigned().saturating_sub(s.xp_lifetime);
    let amount = amount.min(room);
    if amount == 0 {
        return;
    }
    set_xp(s, current_xp(s) + u64::from(amount));
    s.xp_lifetime = s.xp_lifetime.saturating_add(amount);
    player.data_dirty = true;
}

/// Counts down the orb pickup cooldown (PMMP `ExperienceManager::tick`).
pub(super) fn xp_tick(s: &mut Survival) {
    s.xp_cooldown = s.xp_cooldown.saturating_sub(1);
}

/// Where a mending item sits (PMMP `onPickupXp`'s equipment keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worn {
    Held,
    OffHand,
    Armor(usize),
}

/// An orb pickup (PMMP `ExperienceManager::onPickupXp`): mending repairs one damaged item (2
/// durability a point), the rest is added with its sound, and the pickup cooldown starts.
/// False while the cooldown runs or the player cannot pick anything up.
pub(super) fn pickup_experience(state: &mut State, session: SessionId, amount: u32) -> bool {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    if player.phase != Phase::Spawned
        || player.survival.is_dead()
        || player.survival.xp_cooldown > 0
    {
        return false;
    }
    let mut amount = amount;
    // PMMP's candidates: the held item, the off-hand item, then the armour, each a durable item
    // with mending; one is picked at random.
    let inv = &player.inventory;
    let worn = std::iter::once((Worn::Held, inv.held()))
        .chain(std::iter::once((Worn::OffHand, inv.offhand())))
        .chain(
            inv.armor()
                .iter()
                .enumerate()
                .map(|(i, s)| (Worn::Armor(i), s)),
        );
    let mut candidates = [None; 6];
    let mut n = 0;
    for (slot, stack) in worn {
        let durable = stack.item().and_then(Type::max_durability).is_some();
        if durable
            && damage::level_of(stack, Enchantment::Mending) > 0
            && let Some(c) = candidates.get_mut(n)
        {
            *c = Some(slot);
            n += 1;
        }
    }
    if n > 0 {
        let last = i32::try_from(n - 1).unwrap_or(0);
        let pick = player_random(seed, tick, player, MENDING).range(0, last);
        let slot = usize::try_from(pick)
            .ok()
            .and_then(|i| candidates.get(i).copied().flatten());
        if let Some(slot) = slot {
            amount = mend(player, slot, amount);
        }
    }
    let s = &player.survival;
    let (old_level, old_total) = (s.xp_level, current_xp(s));
    add_experience(player, amount);
    player.survival.xp_cooldown = XP_PICKUP_COOLDOWN;
    let s = &player.survival;
    let (level, total) = (s.xp_level, current_xp(s));
    if level / 5 > old_level / 5 {
        sound::plain(
            state,
            session,
            SoundEvent::LevelUp,
            sound::level_up_data(level),
        );
    } else if total > old_total {
        sound::level_event(state, session, LevelEvent::SoundExperienceOrbPickup);
    }
    true
}

/// Repairs the mending item at `slot` with up to `amount` points (2 durability each); returns
/// the points left (PMMP: the points used are the repair halved, rounded up).
fn mend(player: &mut Player, slot: Worn, amount: u32) -> u32 {
    let inv = &player.inventory;
    let stack = match slot {
        Worn::Held => inv.held(),
        Worn::OffHand => inv.offhand(),
        Worn::Armor(i) => match inv.armor().get(i) {
            Some(s) => s,
            None => return amount,
        },
    };
    let damage = u32::from(stack.damage());
    if damage == 0 {
        return amount;
    }
    let repair = damage.min(amount.saturating_mul(MENDING_PER_POINT));
    let repaired = stack.with_damage(u16::try_from(damage - repair).unwrap_or(0));
    match slot {
        Worn::Held => inventory::set_held(player, repaired),
        Worn::OffHand => inventory::set_offhand(player, repaired),
        Worn::Armor(i) => inventory::set_armor(player, i, repaired),
    }
    player.data_dirty = true;
    amount.saturating_sub(repair.div_ceil(MENDING_PER_POINT))
}

/// Empties the player's experience after death (PMMP `Player::onDeath`); returns the points to
/// drop: 7 per level, at most 100 (PMMP `Human::getXpDropAmount`).
pub(super) fn take_death_xp(s: &mut Survival, finite: bool) -> u32 {
    let drop = if finite {
        s.xp_level.saturating_mul(7).min(100)
    } else {
        0
    };
    s.xp_level = 0;
    s.xp_progress = 0.0;
    s.mark(LEVEL | EXPERIENCE);
    drop
}

/// Whether the player can eat food that needs hunger (PMMP `Human::canEat`, `Player::canEat`).
fn can_eat(state: &State, s: &Survival, mode: GameMode) -> bool {
    s.food < MAX_FOOD || difficulty(state) == Difficulty::Peaceful || mode == GameMode::Creative
}

/// Ticks eating `item` takes.
fn consume_ticks(item: Type) -> u64 {
    if item.is(Class::DriedKelp) {
        DRIED_KELP_TICKS
    } else {
        CONSUME_TICKS
    }
}

/// Whether the item is eaten or drunk by holding use (PMMP `ConsumableItem` and `Releasable`).
fn is_consumable(item: Type) -> bool {
    item.food().is_some() || item.consumable().is_some()
}

/// The cooldown key of an item, if it has a cooldown (PMMP `Item::getCooldownTag`).
fn cooldown_of(item: Type) -> Option<(CooldownKey, u16)> {
    let c = item.cooldown()?;
    let key = c
        .tag()
        .map_or(CooldownKey::Item(item.runtime_id()), CooldownKey::Tag);
    Some((key, c.ticks()))
}

/// Whether the item's cooldown runs (PMMP `Player::hasItemCooldown`).
pub(super) fn has_cooldown(tick: u64, s: &Survival, item: Type) -> bool {
    cooldown_of(item)
        .and_then(|(key, _)| s.cooldowns.get(&key))
        .is_some_and(|&end| end > tick)
}

/// Starts the item's cooldown (PMMP `Player::resetItemCooldown`) and tells the client
/// (`NetworkSession::onItemCooldownChanged`: the item's name and the ticks); ended cooldowns are
/// dropped first, so the map holds at most one entry per cooldown key.
pub(super) fn reset_cooldown(tick: u64, player: &mut Player, item: Type) {
    let s = &mut player.survival;
    s.cooldowns.retain(|_, &mut end| end > tick);
    if let Some((key, ticks)) = cooldown_of(item) {
        s.cooldowns
            .insert(key, tick.saturating_add(u64::from(ticks)));
        player.data_dirty = true;
        player.send(Packet::ClientStartItemCooldown(
            packet::ClientStartItemCooldown {
                category: item.name().to_owned(),
                duration: i32::from(ticks),
            },
        ));
    }
}

/// The player uses its held item in the air (PMMP `InGamePacketHandler` click-air): the first
/// use starts eating or drinking, the next finishes it once the eating time has passed. True when
/// the held item is a consumable.
pub(super) fn use_consumable(state: &mut State, session: SessionId) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let mode = game_mode(state, player);
    let Some(item) = player.inventory.held().item() else {
        return false;
    };
    if !is_consumable(item) {
        return false;
    }
    if player.survival.is_dead() || mode == GameMode::Spectator {
        return true;
    }
    let using = player.survival.using;
    match using {
        Some(using) if using.item == item => {
            let elapsed = state.tick.saturating_sub(using.since);
            let need = consume_ticks(item);
            if elapsed >= need - need / CONSUME_TOLERANCE_DIVISOR {
                consume(state, session, item);
            }
            if let Some(player) = state.players.get_mut(&session) {
                player.survival.using = None;
                player.survival.mark(HUNGER);
            }
        }
        _ => {
            let s = &player.survival;
            let start = !has_cooldown(state.tick, s, item)
                && (!item.food().is_some_and(Food::requires_hunger) || can_eat(state, s, mode))
                && medicine_cure(item).is_none_or(|cured| s.effects.contains_key(&cured));
            let tick = state.tick;
            if let Some(player) = state.players.get_mut(&session) {
                player.survival.using = start.then_some(Using { since: tick, item });
            }
        }
    }
    true
}

/// Finishes eating or drinking the held item (PMMP `Player::consumeHeldItem`): check, then
/// apply. The residue (a bowl, a bottle) takes the held slot when the stack is used up, else
/// goes into the inventory or drops at the feet; if it can go nowhere (entity limit) nothing is
/// consumed.
fn consume(state: &mut State, session: SessionId, item: Type) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let mode = super::game_mode_of(&state.settings, player);
    let held = player.inventory.held().clone();
    if held.item() != Some(item) {
        return;
    }
    let s = &player.survival;
    if item.food().is_some_and(Food::requires_hunger)
        && !(s.food < MAX_FOOD
            || state.settings.difficulty == Difficulty::Peaceful
            || mode == GameMode::Creative)
    {
        return;
    }
    if has_cooldown(state.tick, s, item) {
        return;
    }
    let residue = item
        .consumable()
        .and_then(Consumable::residue)
        .map(|t| Stack::new(t, 1));
    let finite = mode != GameMode::Creative;
    if finite {
        let remaining = held.with_count(held.count().saturating_sub(1));
        match residue {
            Some(residue) if remaining.is_empty() => inventory::set_held(player, residue),
            Some(residue) => {
                let left = inventory::give(player, residue);
                if !left.is_empty() {
                    let (world, at) = (player.world, feet(player));
                    if !entity::drop_item(state, world, at, left) {
                        return;
                    }
                }
                if let Some(player) = state.players.get_mut(&session) {
                    inventory::set_held(player, remaining);
                }
            }
            None => inventory::set_held(player, remaining),
        }
    }
    apply(state, session, item);
}

/// The effect a medicine cures (PMMP `MedicineType::getCuredEffect`, Bedrock meta from PMMP
/// `MedicineTypeIds`): eye drops blindness, tonic nausea, antidote poison, elixir weakness.
/// Drinking it starts only while that effect is active (PMMP `Medicine::canStartUsingItem`).
fn medicine_cure(item: Type) -> Option<Effect> {
    if !item.is(Class::Medicine) {
        return None;
    }
    match item.meta() {
        0 => Some(Effect::Blindness),
        1 => Some(Effect::Nausea),
        2 => Some(Effect::Poison),
        3 => Some(Effect::Weakness),
        _ => None,
    }
}

/// A slice of cake (PMMP `BaseCake` through `Human::consumeObject`): false if the player
/// cannot eat it now.
pub(super) fn eat_cake(state: &mut State, session: SessionId) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let mode = game_mode(state, player);
    if player.phase != Phase::Spawned
        || player.survival.is_dead()
        || mode == GameMode::Spectator
        || !can_eat(state, &player.survival, mode)
    {
        return false;
    }
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    add_food(&mut player.survival, CAKE_FOOD, CAKE_SATURATION);
    player.data_dirty = true;
    sound::plain(state, session, SoundEvent::Burp, -1);
    true
}

/// What consuming an item does (PMMP `Human::applyConsumptionResults`, `Consumable::onConsume`):
/// food and saturation with the burp sound, the item's effects, then what the item itself does
/// (milk, honey, chorus fruit, medicine).
fn apply(state: &mut State, session: SessionId, item: Type) {
    let tick = state.tick;
    let seed = super::world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    reset_cooldown(tick, player, item);
    player.data_dirty = true;
    if let Some(food) = item.food() {
        add_food(
            &mut player.survival,
            f64::from(food.food_restore()),
            food.saturation_restore(),
        );
    }
    let fixed = item.consumable().and_then(Consumable::effects);
    let mut random_effect = None;
    if fixed.is_none() {
        if item.is(Class::PoisonousPotato) {
            let mut r = player_random(seed, tick, player, POISONOUS_POTATO);
            if r.range(0, 100) > 40 {
                random_effect = Some((Effect::Poison, Active::new(100, 0)));
            }
        } else if item.is(Class::RawChicken) {
            let mut r = player_random(seed, tick, player, RAW_CHICKEN);
            if r.range(0, 9) < 3 {
                random_effect = Some((Effect::Hunger, Active::new(600, 0)));
            }
        } else if item.is(Class::RottenFlesh) {
            let mut r = player_random(seed, tick, player, ROTTEN_FLESH);
            if r.unit() <= 0.8 {
                random_effect = Some((Effect::Hunger, Active::new(600, 0)));
            }
        }
    }
    for (effect, active) in fixed
        .map(effects::of_instances)
        .into_iter()
        .flatten()
        .chain(random_effect)
    {
        effects::add(state, session, effect, active);
    }
    if item.food().is_some() {
        sound::plain(state, session, SoundEvent::Burp, -1);
    }
    if let Some(cured) = medicine_cure(item) {
        effects::remove(state, session, cured);
    } else if item.is(Class::MilkBucket) {
        effects::clear(state, session);
    } else if item.is(Class::HoneyBottle) {
        effects::remove(state, session, Effect::Poison);
    } else if item.is(Class::ChorusFruit) {
        chorus_teleport(state, session);
    }
}

/// Teleports the player to a random safe spot within 8 blocks (PMMP `ChorusFruit::onConsume`):
/// 16 tries; each goes down from a random block to a solid one with two free blocks above.
fn chorus_teleport(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let seed = super::world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let origin = super::block_at(feet(player));
    let world_index = player.world;
    let mut r = player_random(seed, tick, player, CHORUS_FRUIT);
    let Some(world) = state.worlds.get(world_index) else {
        return;
    };
    let min_y = origin.y.min(crate::world::MAX_Y) - 8;
    for _ in 0..16 {
        let x = r.range(origin.x - 8, origin.x + 8);
        let mut y = r.range(min_y, min_y + 16);
        let z = r.range(origin.z - 8, origin.z + 8);
        let solid = |y: i32| {
            world
                .block(crate::world::BlockPos::new(x, y, z))
                .is_some_and(crate::block::State::is_solid)
        };
        while y >= crate::world::MIN_Y && !solid(y) {
            y -= 1;
        }
        if y < crate::world::MIN_Y {
            continue;
        }
        let free = |y: i32| {
            world
                .block(crate::world::BlockPos::new(x, y, z))
                .is_some_and(|b| !b.is_solid() && !b.is(crate::block::Class::Liquid))
        };
        if !free(y + 1) || !free(y + 2) {
            continue;
        }
        let target =
            crate::math::Vec3::new(f64::from(x) + 0.5, f64::from(y) + 1.0, f64::from(z) + 0.5);
        movement::teleport(state, session, target);
        return;
    }
}

/// While the player eats, viewers see the eating animation every 4 ticks (PMMP
/// `ConsumingItemAnimation`); a changed held item stops eating.
pub(super) fn using_tick(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let Some(using) = player.survival.using else {
        return;
    };
    if player.inventory.held().item() != Some(using.item) {
        player.survival.using = None;
        return;
    }
    let elapsed = state.tick.saturating_sub(using.since);
    if elapsed == 0 || elapsed % EATING_ANIMATION_TICKS != 0 {
        return;
    }
    let id = i32::from(using.item.runtime_id());
    let meta = i32::from(using.item.meta());
    let packet = Packet::ActorEvent(packet::ActorEvent {
        entity_runtime_id: player.runtime_id,
        event_type: u8::from(ActorEvent::Feed),
        event_data: (id << 16) | meta,
        fire_at_position: None,
    });
    broadcast_to_viewers(state, session, &packet);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_thresholds_follow_pmmp() {
        // PMMP ExperienceUtils at the three formula boundaries.
        assert_eq!(xp_to_reach(0), 0);
        assert_eq!(xp_to_reach(1), 7);
        assert_eq!(xp_to_reach(16), 352);
        assert_eq!(xp_to_reach(17), 394);
        assert_eq!(xp_to_reach(30), 1395);
        assert_eq!(xp_to_reach(31), 1507);
        assert_eq!(xp_to_reach(32), 1628);
        assert_eq!(xp_to_complete(0), 7);
        assert_eq!(xp_to_complete(15), 37);
        assert_eq!(xp_to_complete(16), 42);
        assert_eq!(xp_to_complete(30), 112);
        assert_eq!(xp_to_complete(31), 121);
        for level in 0..200 {
            assert_eq!(
                xp_to_reach(level + 1) - xp_to_reach(level),
                xp_to_complete(level),
                "level {level}"
            );
            assert_eq!(level_of(xp_to_reach(level)), (level, 0.0));
        }
        let (level, progress) = level_of(xp_to_reach(10) + xp_to_complete(10) / 2);
        assert_eq!(level, 10);
        assert!((progress - 13.0 / 27.0).abs() < 1e-12);
    }

    #[test]
    fn exhaustion_spends_saturation_then_food() {
        let mut s = Survival::default();
        s.motion.hunger = true;
        s.saturation = 1.5;
        exhaust(&mut s, 4.0);
        assert_eq!((s.saturation, s.food, s.exhaustion), (0.5, 20.0, 0.0));
        exhaust(&mut s, 4.0);
        assert_eq!((s.saturation, s.food), (0.0, 20.0));
        exhaust(&mut s, 9.0);
        assert_eq!((s.food, s.exhaustion), (18.0, 1.0));
        s.motion.hunger = false;
        exhaust(&mut s, 100.0);
        assert_eq!((s.food, s.exhaustion), (18.0, 1.0));
    }
}
