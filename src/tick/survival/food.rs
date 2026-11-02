//! Food, xp and consumables.

use tracing::warn;

use super::attributes::{EXHAUSTION, FOOD, LEVEL, MOVEMENT, SATURATION, XP};
use super::damage::{self, Damage};
use super::effects::{self, Active};
use super::{
    EYE_HEIGHT, MAX_FOOD, MAX_SATURATION, Survival, broadcast_to_viewers, difficulty, feet,
    game_mode, player_random, sound, world_seed,
};
use crate::block;
use crate::config::{Difficulty, GameMode};
use crate::effect::Type as Effect;
use crate::event::{ExhaustCause, PlayerExhaust, PlayerItemConsume, PlayerXpChange, RegainReason};
use crate::item::{Class, Consumable, Enchantment, Food, Stack, Type};
use crate::math::{self, Vec3};
use crate::player::{Id, level_from_xp, xp_to_complete_level, xp_to_reach_level};
use crate::protocol::consts::ActorEvent;
use crate::protocol::packet::{self, Packet};
use crate::random;
use crate::tick::{Phase, Player, SessionId, State, entity, inventory};
use crate::world::{BlockPos, MAX_Y, MIN_Y, Sound};

/// The random stream key of a chorus fruit's teleport.
const CHORUS_FRUIT: u64 = random::tag("chorus-fruit");
/// The most blocks a chorus fruit teleports in each direction.
const CHORUS_RADIUS: i32 = 8;
/// The spots a chorus fruit tries.
const CHORUS_TRIES: u32 = 16;
/// The share of the eating time that may be missing between the start and the end packets: the
/// two packets arrive with network jitter, so a quarter of the time is forgiven.
const CONSUME_TOLERANCE_DIVISOR: u64 = 4;
/// Ticks between eating animations.
const EATING_ANIMATION_TICKS: u64 = 4;
/// Exhaustion that costs one saturation or food point.
const EXHAUSTION_PER_POINT: f64 = 4.0;
/// Ticks of one food cycle.
pub(super) const FOOD_CYCLE: u32 = 80;
/// The most xp points a player collects in all; the points its level and progress hold reach
/// further, to the top level.
pub(super) const MAX_LIFETIME_XP: u32 = i32::MAX.cast_unsigned();
/// The largest xp progress: just below a full bar, which is the next level.
const MAX_PROGRESS: f64 = 1.0 - f64::EPSILON / 2.0;
/// The most residue stacks a listener may leave after eating or drinking.
const MAX_RESIDUE: usize = 4;
/// The random stream key of the mending item an orb repairs.
const MENDING: u64 = random::tag("mending");
/// Durability one xp point repairs with mending.
const MENDING_PER_POINT: u32 = 2;
/// The random stream key of a poisonous potato's poison.
const POISONOUS_POTATO: u64 = random::tag("poisonous-potato");
/// The random stream key of a raw chicken's Hunger effect.
const RAW_CHICKEN: u64 = random::tag("raw-chicken");
/// The random stream key of rotten flesh's Hunger effect.
const ROTTEN_FLESH: u64 = random::tag("rotten-flesh");
/// Ticks after an orb pickup before the next.
const XP_PICKUP_COOLDOWN: u8 = 2;

/// The item a player started eating or drinking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Using {
    /// The engine tick it started.
    pub(super) since: u64,
    /// The item type held then.
    pub(super) item: Type,
}

impl Using {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self { since, item } = self;
        h.u64(*since);
        h.u64(u64::from(item.runtime_id().cast_unsigned()));
    }
}

crate::tick::checkpoint::fields!(Using { since, item });

/// What an item cooldown is kept under: its cooldown tag, or the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CooldownKey {
    /// A tag shared by several items.
    Tag(&'static str),
    /// One item type, by runtime ID.
    Item(i16),
}

impl CooldownKey {
    /// Hashes the key (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        match self {
            Self::Tag(t) => h.bytes(t.as_bytes()),
            Self::Item(i) => h.u64(u64::from(i.cast_unsigned())),
        }
    }
}

/// A tag by its text, found again among the vanilla items' cooldown tags; a tag no vanilla item
/// has (a custom item's) makes the checkpoint opaque.
impl crate::tick::checkpoint::Codec for CooldownKey {
    fn encode(&self, w: &mut crate::tick::checkpoint::Writer) {
        match self {
            Self::Tag(tag) => {
                w.u8(0);
                if vanilla_tag(tag).is_none() {
                    w.opaque("custom item cooldown tag", 1);
                }
                w.text(tag);
            }
            Self::Item(id) => {
                w.u8(1);
                id.encode(w);
            }
        }
    }

    fn decode(
        r: &mut crate::tick::checkpoint::Reader<'_>,
    ) -> crate::tick::checkpoint::Result<Self> {
        use crate::tick::checkpoint::{Codec, Error};
        match r.u8()? {
            0 => vanilla_tag(&String::decode(r)?)
                .map(Self::Tag)
                .ok_or(Error::Format("cooldown tag")),
            1 => Ok(Self::Item(Codec::decode(r)?)),
            _ => Err(Error::Format("cooldown key")),
        }
    }
}

/// Where a mending item sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worn {
    /// The held item.
    Held,
    /// The off-hand item.
    OffHand,
    /// The armour piece in a slot.
    Armor(usize),
}

/// Starts the player eating or drinking the item it holds, if it may now. True when the held item
/// is a consumable.
pub(super) fn start_eating(state: &mut State, session: SessionId) -> bool {
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
    let s = &player.survival;
    let start = !has_cooldown(state.tick, s, item)
        && (!item.food().is_some_and(Food::requires_food) || can_eat(state, player, mode))
        && medicine_cure(item).is_none_or(|cured| s.effects.contains_key(&cured));
    let tick = state.tick;
    if let Some(player) = state.players.get_mut(&session) {
        player.survival.using = start.then_some(Using { since: tick, item });
    }
    true
}

/// Finishes the eating or drinking of the item the player holds when it clicks in the air: it
/// finishes once the eating time has passed, and stops either way. False, changing nothing, if it
/// does not eat the held item.
pub(super) fn finish_eating(state: &mut State, session: SessionId) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let Some(using) = player.survival.using else {
        return false;
    };
    let item = using.item;
    if player.inventory.held().item() != Some(item) {
        return false;
    }
    if player.survival.is_dead() || game_mode(state, player) == GameMode::Spectator {
        return true;
    }
    let elapsed = state.tick.saturating_sub(using.since);
    let need = u64::from(item.use_ticks());
    if elapsed >= need - need / CONSUME_TOLERANCE_DIVISOR {
        consume(state, session, item);
    }
    if let Some(player) = state.players.get_mut(&session) {
        let s = &mut player.survival;
        s.using = None;
        if s.switches.food {
            s.mark(FOOD);
        }
    }
    true
}

/// Shows viewers the eating animation every 4 ticks while the player eats; a changed held item
/// stops eating.
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

/// Finishes eating or drinking the held item: check, then apply. The residue (a bowl, a bottle)
/// takes the held slot when the stack is used up, else goes into the inventory or drops at the
/// feet; if the drops would find no room (entity limit) nothing is consumed. Creative players
/// use nothing up and get no residue. The residue event monitors see is the one given.
fn consume(state: &mut State, session: SessionId, item: Type) {
    let Some(held) = consumable_now(state, session, item) else {
        return;
    };
    let residue: Option<Stack> = item
        .consumable()
        .and_then(Consumable::residue)
        .map(|t| Stack::new(t, 1));
    let Ok(mut event) = super::ask(
        state,
        || PlayerItemConsume {
            player: Id::new(session),
            item: held.clone(),
            residue: residue.iter().cloned().collect(),
            cancelled: false,
        },
        PlayerItemConsume::is_cancelled,
        |state, _| consumable_now(state, session, item).is_some_and(|h| h == held),
    ) else {
        return;
    };
    // A listener's residue is checked like external input; the first stack takes the residue's
    // place, the others go into the inventory or drop at the feet.
    let (residue, extra) = match &event {
        Some(e)
            if e.residue.len() != usize::from(residue.is_some())
                || e.residue.first() != residue.as_ref() =>
        {
            let mut chosen = super::valid_stacks(&e.residue, MAX_RESIDUE).into_iter();
            (chosen.next(), chosen.collect())
        }
        _ => (residue, Vec::new()),
    };
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let finite = super::has_finite_resources(player.survival.mode());
    let (world, at) = (player.world, feet(player));
    let (residue, extra) = if finite {
        (residue, extra)
    } else {
        (None, Vec::new())
    };
    let used_up = held.count() <= 1;
    let mut to_give = extra;
    let mut replacement = None;
    if let Some(residue) = residue {
        if used_up {
            replacement = Some(residue);
        } else {
            to_give.insert(0, residue);
        }
    }
    // Every stack that gives no room back may need an item entity: ask before anything changes.
    let has_room =
        entity::can_create(state, world, at) && entity::has_room(state, world, to_give.len());
    if !to_give.is_empty() && !has_room {
        return;
    }
    if let Some(e) = &mut event {
        e.residue = replacement.iter().chain(&to_give).cloned().collect();
    }
    if finite && let Some(player) = state.players.get_mut(&session) {
        let remaining = held.with_count(held.count().saturating_sub(1));
        inventory::set_held(player, replacement.unwrap_or(remaining));
    }
    for stack in to_give {
        let Some(player) = state.players.get_mut(&session) else {
            break;
        };
        let left = inventory::give(player, stack);
        if !left.is_empty() && !entity::drop_item(state, world, at, left) {
            warn!(
                session = session.get(),
                "a residue stack was not dropped although the world had room"
            );
        }
    }
    apply(state, session, item);
    super::watch(state, event.as_ref());
}

/// Applies what consuming an item does: food and saturation with the burp sound, the item's
/// effects, then what the item itself does (milk, honey, chorus fruit, medicine).
fn apply(state: &mut State, session: SessionId, item: Type) {
    let tick = state.tick;
    let seed = world_seed(state, session);
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
        sound::at_feet(state, session, Sound::Burp);
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

/// Teleports the player to a random safe spot within [`CHORUS_RADIUS`] blocks: [`CHORUS_TRIES`]
/// tries; each goes down from a random block to a solid one with two free blocks above.
fn chorus_teleport(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let origin = BlockPos::containing(feet(player));
    let world_index = player.world;
    let mut r = player_random(seed, tick, player, CHORUS_FRUIT);
    let Some(world) = state.worlds.get(world_index) else {
        return;
    };
    let min_y = origin.y.min(MAX_Y) - CHORUS_RADIUS;
    for _ in 0..CHORUS_TRIES {
        let x = r.range(origin.x - CHORUS_RADIUS, origin.x + CHORUS_RADIUS);
        let mut y = r.range(min_y, min_y + 2 * CHORUS_RADIUS);
        let z = r.range(origin.z - CHORUS_RADIUS, origin.z + CHORUS_RADIUS);
        let solid = |y: i32| {
            world
                .block(BlockPos::new(x, y, z))
                .is_some_and(block::State::is_solid)
        };
        while y >= MIN_Y && !solid(y) {
            y -= 1;
        }
        if y < MIN_Y {
            continue;
        }
        let free = |y: i32| {
            world
                .block(BlockPos::new(x, y, z))
                .is_some_and(|b| !b.is_solid() && !b.is(block::Class::Liquid))
        };
        if !free(y + 1) || !free(y + 2) {
            continue;
        }
        // The feet stand on the block; a teleport takes the eyes.
        let eye = Vec3::new(
            f64::from(x) + 0.5,
            f64::from(y) + 1.0 + EYE_HEIGHT,
            f64::from(z) + 0.5,
        );
        super::teleport(state, session, world_index, eye);
        return;
    }
}

/// Counts one tick of food: peaceful refills food and heals; a full bar heals every 80 ticks for
/// 6 exhaustion; an empty bar starves (easy down to 10 health, normal to 1, hard to death); food
/// at 6 or below stops sprinting.
pub(super) fn tick(state: &mut State, session: SessionId) {
    let world = state.players.get(&session).map_or(0, |p| p.world);
    let peaceful = difficulty(state, world) == Difficulty::Peaceful;
    let (peaceful_heal, full, starve) = {
        let Some(player) = state.players.get_mut(&session) else {
            return;
        };
        let s = &mut player.survival;
        if s.is_dead() {
            return;
        }
        player.data_dirty = true;
        s.food_timer += 1;
        if s.food_timer >= FOOD_CYCLE {
            s.food_timer = 0;
        }
        let mut peaceful_heal = false;
        if peaceful && s.food_timer % 10 == 0 {
            if s.food < MAX_FOOD {
                set_food(s, math::min(s.food + 1.0, MAX_FOOD));
            }
            peaceful_heal = s.food_timer % 20 == 0 && s.health < s.max_health();
        }
        let (mut full, mut starve) = (false, false);
        if s.food_timer == 0 {
            if s.food >= 18.0 {
                full = true;
            } else if s.food <= 0.0 {
                starve = true;
            }
        }
        if s.food <= 6.0 && s.motion.sprinting {
            s.motion.sprinting = false;
            s.mark(MOVEMENT);
        }
        (peaceful_heal, full, starve)
    };
    if peaceful_heal {
        damage::heal(state, session, 1.0, RegainReason::Saturation);
    }
    // Checked after the peaceful heal: health it filled costs no exhaustion.
    if full
        && state
            .players
            .get(&session)
            .is_some_and(|p| p.survival.health < p.survival.max_health())
    {
        damage::heal(state, session, 1.0, RegainReason::Saturation);
        exhaust_as(state, session, 6.0, ExhaustCause::HealthRegen);
    }
    if starve {
        let health = state
            .players
            .get(&session)
            .map_or(0.0, |p| p.survival.health);
        let hurts = match difficulty(state, world) {
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

/// Adds exhaustion for `cause`: while food runs for the player, the [`PlayerExhaust`] listeners
/// may cancel it or change the amount (a value that is not positive adds nothing). An amount of 0
/// is still announced (walking).
pub(super) fn exhaust_as(state: &mut State, session: SessionId, amount: f64, cause: ExhaustCause) {
    let hungry = |state: &State| {
        state
            .players
            .get(&session)
            .is_some_and(|p| p.survival.motion.food)
    };
    if !amount.is_finite() || amount < 0.0 || !hungry(state) {
        return;
    }
    let Ok(event) = super::ask(
        state,
        || PlayerExhaust {
            player: Id::new(session),
            amount,
            cause,
            cancelled: false,
        },
        PlayerExhaust::is_cancelled,
        |state, _| hungry(state),
    ) else {
        return;
    };
    let amount = event.as_ref().map_or(amount, |e| e.amount);
    if let Some(player) = state.players.get_mut(&session) {
        exhaust(&mut player.survival, amount);
    }
    super::watch(state, event.as_ref());
}

/// Adds exhaustion and spends saturation, then food, for every 4 points; nothing while food is
/// off for the player. A huge amount must not loop for ever: the rounds stop once saturation and
/// food are spent, and the rest of the exhaustion is kept below 4 at once (exact, as the
/// subtractions were).
pub(super) fn exhaust(s: &mut Survival, amount: f64) {
    if !s.motion.food || amount.is_nan() || amount <= 0.0 {
        return;
    }
    s.exhaustion += amount;
    let points = (s.exhaustion / EXHAUSTION_PER_POINT).floor();
    let mut rounds = math::min(points, s.saturation.ceil() + s.food.ceil());
    s.exhaustion %= EXHAUSTION_PER_POINT;
    while rounds >= 1.0 {
        rounds -= 1.0;
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

/// Adds food and saturation within their limits; nothing with `food = false` (eating and the
/// saturation effect then fill nothing).
pub(super) fn add_food(s: &mut Survival, food: f64, saturation: f64) {
    if !s.switches.food {
        return;
    }
    let f = s.food + food;
    set_food(s, if f > MAX_FOOD { MAX_FOOD } else { f });
    s.saturation = math::clamp(s.saturation + saturation, 0.0, MAX_SATURATION);
    s.mark(SATURATION);
}

/// Sets the food level, restarting the food cycle when it crosses 17, 6 or 0.
pub(super) fn set_food(s: &mut Survival, food: f64) {
    let food = if food < 0.0 { 0.0 } else { food };
    if [17.0, 6.0, 0.0].iter().any(|&b| (s.food > b) != (food > b)) {
        s.food_timer = 0;
    }
    s.food = food;
    s.mark(FOOD);
}

/// Picks up an xp orb: mending repairs one damaged item (2 durability a point), the rest is
/// added with its sound, and the pickup cooldown starts. False while the cooldown runs or the
/// player cannot pick anything up.
pub(super) fn pickup_xp(state: &mut State, session: SessionId, amount: u32) -> bool {
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
    // The candidates: the held item, the off-hand item, then the armour, each a durable item
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
            && stack
                .enchantment_level(Enchantment::Mending)
                .is_some_and(|level| level > 0)
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
    gain(state, session, amount);
    let Some(player) = state.players.get_mut(&session) else {
        return true;
    };
    player.survival.xp_cooldown = XP_PICKUP_COOLDOWN;
    let s = &player.survival;
    let (level, total) = (s.xp_level, current_xp(s));
    if level / 5 > old_level / 5 {
        sound::at_feet(state, session, Sound::XpLevelUp { level });
    } else if total > old_total {
        sound::at_feet(state, session, Sound::XpCollect);
    }
    true
}

/// Adds `amount` xp points the game gives: the [`PlayerXpChange`] listeners may cancel it or
/// choose the level and progress it ends at (the level at most the top level, the progress below
/// a full bar). The points ever collected grow by `amount` unless it is cancelled. The event
/// monitors see is the level and progress applied.
fn gain(state: &mut State, session: SessionId, amount: u32) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let s = &player.survival;
    let amount = amount.min(lifetime_room(s));
    if amount == 0 {
        return;
    }
    let (old_level, old_progress, total) = (s.xp_level, s.xp_progress, current_xp(s));
    let (level, progress) = level_from_xp(total + u64::from(amount));
    let Ok(mut event) = super::ask(
        state,
        || PlayerXpChange {
            player: Id::new(session),
            old_level,
            old_progress,
            new_level: Some(level),
            new_progress: Some(progress),
            cancelled: false,
        },
        PlayerXpChange::is_cancelled,
        |state, _| state.players.contains_key(&session),
    ) else {
        return;
    };
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let chosen = event
        .as_ref()
        .map(|e| (e.new_level, e.new_progress))
        .filter(|&(l, p)| l != Some(level) || p.map(f64::to_bits) != Some(progress.to_bits()));
    match chosen {
        Some((l, p)) => {
            let s = &mut player.survival;
            if let Some(l) = l {
                s.xp_level = l;
            }
            if let Some(p) = p {
                s.xp_progress = math::clamp(p, 0.0, MAX_PROGRESS);
            }
            s.xp_lifetime = s.xp_lifetime.saturating_add(amount);
            s.mark(LEVEL | XP);
            player.data_dirty = true;
        }
        None => add_xp(player, amount),
    }
    if let Some(e) = &mut event {
        let s = &player.survival;
        e.new_level = Some(s.xp_level);
        e.new_progress = Some(s.xp_progress);
    }
    super::watch(state, event.as_ref());
}

/// Adds xp points without a sound: the points ever collected stay at most [`MAX_LIFETIME_XP`].
pub(super) fn add_xp(player: &mut Player, amount: u32) {
    let s = &mut player.survival;
    let amount = amount.min(lifetime_room(s));
    if amount == 0 {
        return;
    }
    set_xp(s, current_xp(s) + u64::from(amount));
    s.xp_lifetime = s.xp_lifetime.saturating_add(amount);
    player.data_dirty = true;
}

/// Sets the player's level and progress from a total, at most the points of the top level.
pub(super) fn set_xp(s: &mut Survival, total: u64) {
    let (level, progress) = level_from_xp(total);
    s.xp_level = level;
    s.xp_progress = progress;
    s.mark(LEVEL | XP);
}

/// The player's current xp points.
pub(super) fn current_xp(s: &Survival) -> u64 {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: the points into a level are below 2^31 and progress is in [0, 1)"
    )]
    let into = (xp_to_complete_level(s.xp_level) as f64 * s.xp_progress) as u64;
    xp_to_reach_level(s.xp_level) + into
}

/// Counts down the orb pickup cooldown.
pub(super) fn xp_tick(s: &mut Survival) {
    s.xp_cooldown = s.xp_cooldown.saturating_sub(1);
}

/// Empties the player's xp after death and returns the points [`death_xp`] gives, which the
/// caller drops only in the modes and settings that let orbs exist. With `xp = false` the level a
/// save kept stays. No [`PlayerXpChange`] is raised: the death event carries the loss.
pub(super) fn take_death_xp(s: &mut Survival) -> u32 {
    if !s.switches.xp {
        return 0;
    }
    let drop = death_xp(s);
    s.xp_level = 0;
    s.xp_progress = 0.0;
    s.mark(LEVEL | XP);
    drop
}

/// The xp points a dead player drops if the game lets it drop any: 7 per level, at most 100,
/// none with `xp = false`. The caller drops none in creative or without orbs (`xp-orbs = false`).
pub(super) fn death_xp(s: &Survival) -> u32 {
    if s.switches.xp {
        s.xp_level.saturating_mul(7).min(100)
    } else {
        0
    }
}

/// Starts the item's own cooldown, if it has one.
pub(super) fn reset_cooldown(tick: u64, player: &mut Player, item: Type) {
    if let Some(c) = item.cooldown() {
        start_cooldown(tick, player, item, u32::from(c.ticks()));
    }
}

/// Starts a cooldown of `ticks` (more than 0) for the item and tells the client (the item's name
/// and the ticks); ended cooldowns are dropped first, so the map holds at most one entry per
/// cooldown key.
pub(super) fn start_cooldown(tick: u64, player: &mut Player, item: Type, ticks: u32) {
    let s = &mut player.survival;
    s.cooldowns.retain(|_, &mut end| end > tick);
    s.cooldowns
        .insert(cooldown_key(item), tick.saturating_add(u64::from(ticks)));
    player.data_dirty = true;
    player.send_packet(Packet::ClientStartItemCooldown(
        packet::ClientStartItemCooldown {
            category: item.name().to_owned(),
            duration: i32::try_from(ticks).unwrap_or(i32::MAX),
        },
    ));
}

/// Whether the item's cooldown runs.
pub(super) fn has_cooldown(tick: u64, s: &Survival, item: Type) -> bool {
    cooldown_end(tick, s, item).is_some()
}

/// The engine tick the item's cooldown ends, while it runs.
pub(super) fn cooldown_end(tick: u64, s: &Survival, item: Type) -> Option<u64> {
    s.cooldowns
        .get(&cooldown_key(item))
        .copied()
        .filter(|&end| end > tick)
}

/// Repairs the mending item at `slot` with up to `amount` points (2 durability each); returns
/// the points left (the points used are the repair halved, rounded up).
#[expect(
    clippy::expect_used,
    reason = "invariant: the item wears (its caller checks) and keeps less damage than it has"
)]
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
    let repaired = stack
        .with_damage(u16::try_from(damage - repair).unwrap_or(0))
        .expect("an item that wears keeps less damage than it has");
    match slot {
        Worn::Held => inventory::set_held(player, repaired),
        Worn::OffHand => inventory::set_offhand(player, repaired),
        Worn::Armor(i) => inventory::set_armor(player, i, repaired),
    }
    player.data_dirty = true;
    amount.saturating_sub(repair.div_ceil(MENDING_PER_POINT))
}

/// The held stack if the player can finish eating or drinking `item` now: it still holds it, has
/// room to eat (food that needs food) and its cooldown has ended.
fn consumable_now(state: &State, session: SessionId, item: Type) -> Option<Stack> {
    let player = state.players.get(&session)?;
    let mode = player.survival.mode();
    let held = player.inventory.held();
    let ok = held.item() == Some(item)
        && (!item.food().is_some_and(Food::requires_food) || can_eat(state, player, mode))
        && !has_cooldown(state.tick, &player.survival, item);
    ok.then(|| held.clone())
}

/// Whether the player can eat food that needs food.
fn can_eat(state: &State, player: &Player, mode: GameMode) -> bool {
    let s = &player.survival;
    s.food < MAX_FOOD
        || difficulty(state, player.world) == Difficulty::Peaceful
        || mode == GameMode::Creative
}

/// Whether the item is eaten or drunk by holding use.
fn is_consumable(item: Type) -> bool {
    item.food().is_some() || item.consumable().is_some()
}

/// What an item's cooldown is kept under: its cooldown tag, else the item.
fn cooldown_key(item: Type) -> CooldownKey {
    item.cooldown()
        .and_then(crate::item::Cooldown::tag)
        .map_or(CooldownKey::Item(item.runtime_id()), CooldownKey::Tag)
}

/// The effect a medicine cures, by its item meta: eye drops blindness, tonic nausea, antidote
/// poison, elixir weakness. Drinking it starts only while that effect is active.
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

/// The cooldown tag `tag` of a vanilla item, as the item table holds it.
fn vanilla_tag(tag: &str) -> Option<&'static str> {
    crate::item::Kind::all()
        .iter()
        .filter_map(|k| k.default_type().cooldown()?.tag())
        .find(|t| *t == tag)
}

/// The xp points the player can still collect before the points ever collected reach
/// [`MAX_LIFETIME_XP`].
fn lifetime_room(s: &Survival) -> u32 {
    MAX_LIFETIME_XP.saturating_sub(s.xp_lifetime)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::player::MAX_LEVEL;

    #[test]
    fn exhaustion_spends_saturation_then_food() {
        let mut s = Survival::default();
        s.motion.food = true;
        s.saturation = 1.5;
        exhaust(&mut s, 4.0);
        assert_eq!((s.saturation, s.food, s.exhaustion), (0.5, 20.0, 0.0));
        exhaust(&mut s, 4.0);
        assert_eq!((s.saturation, s.food), (0.0, 20.0));
        exhaust(&mut s, 9.0);
        assert_eq!((s.food, s.exhaustion), (18.0, 1.0));
        s.motion.food = false;
        exhaust(&mut s, 100.0);
        assert_eq!((s.food, s.exhaustion), (18.0, 1.0));
    }

    #[test]
    fn a_huge_exhaustion_spends_food_once_and_returns() {
        let mut s = Survival::default();
        s.motion.food = true;
        exhaust(&mut s, 1e300);
        assert_eq!((s.saturation, s.food), (0.0, 0.0));
        assert!((0.0..EXHAUSTION_PER_POINT).contains(&s.exhaustion));
    }

    #[test]
    fn xp_at_the_top_level_keeps_the_level() {
        let mut s = Survival {
            xp_level: MAX_LEVEL,
            ..Survival::default()
        };
        let top = current_xp(&s);
        assert!(
            top > u64::from(MAX_LIFETIME_XP),
            "the top level holds more points than a lifetime"
        );
        set_xp(&mut s, top + 1);
        assert_eq!((s.xp_level, s.xp_progress), (MAX_LEVEL, 0.0));
        set_xp(&mut s, top - 1);
        assert_eq!(s.xp_level, MAX_LEVEL - 1);
    }

    #[test]
    fn the_lifetime_room_ends_at_the_most_points() {
        let nearly = Survival {
            xp_lifetime: MAX_LIFETIME_XP - 5,
            ..Survival::default()
        };
        assert_eq!(lifetime_room(&nearly), 5);
        let all = Survival {
            xp_lifetime: MAX_LIFETIME_XP,
            ..Survival::default()
        };
        assert_eq!(lifetime_room(&all), 0);
    }
}
