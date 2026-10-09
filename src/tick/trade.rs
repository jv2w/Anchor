//! Villager trading: the offer lists of villager NPCs, the trade screen and the trades themselves.
//! No vanilla villager economy: code sets and changes the offers ([`change_offers`]); prices are
//! the real counts.
//!
//! A villager NPC keeps its [`Offers`] in its entity (the entity module saves them with it, in
//! the same commit). This module keeps who trades with which villager ([`Trades`]).
//!
//! - [`open`]: an interaction with a villager opens the trade window (`inventory::open_trade`) and
//!   sends `UpdateTrade` (BDS: no `ContainerOpen`; window type 15, size 0, the villager's tier and
//!   unique ID, entity -1, the profession's display name, new trade UI, demand-based prices, the
//!   offers in network NBT), after `TradeOpen` (cancelled, nothing opens). Offers get network IDs
//!   from a server counter that starts after the last `CraftingData` recipe; reopening an unchanged
//!   list shows the same IDs (BDS), and every change of the list gives it new ones, so a stale
//!   screen's IDs fail.
//! - The inventory executor checks a trade with [`offer`] and counts it with [`traded`] after
//!   its item changes committed (a check/apply pair; `traded` cannot fail after `offer`
//!   succeeded in the same request). A trade counts uses (at most 256 per request), gives the
//!   villager experience and tiers, and drops reward orbs at the villager. Like BDS, no
//!   `UpdateTrade` follows a trade, not even one that reaches a new tier (P6 `tradeup`).
//! - [`closed`]: the inventory module's report that the trade window closed (it returns the
//!   payment slots). [`tick`] closes the window of every player whose villager is gone or in
//!   another world.

mod api;
mod offers;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use tracing::{error, warn};

use super::{SessionId, State, dispatch, entity, inventory};
use crate::entity::Id;
use crate::event::TradeOpen;
use crate::item::{Stack, Type};
use crate::math::Vec3;
use crate::nbt::Encoding;
use crate::protocol::Writer;
use crate::protocol::consts::ContainerType;
use crate::protocol::packet::{Packet, UpdateTrade};
use crate::random::{self, Random};
use crate::save::WorldIndex;

pub(super) use self::api::new_offers;
pub(super) use self::offers::{Damage, Error, MAX_TIER, Offers, Recipe, Result};

/// The most trades one request may repeat.
pub(super) const MAX_REPEAT: u32 = 256;
/// `UpdateTrade.EntityUniqueID` as BDS sends it.
const ANY_ENTITY: i64 = -1;
/// Experience per rewarded trade: 3 plus 0 to 3 (Java `Villager.rewardTradeXp`; BDS drops a
/// random amount).
const REWARD_BASE: u32 = 3;
/// The random part of [`REWARD_BASE`]: `0..REWARD_SPREAD`.
const REWARD_SPREAD: u32 = 4;
/// Reward orbs appear this far above the villager's feet (Java `Villager.rewardTradeXp`).
const REWARD_HEIGHT: f64 = 0.5;
/// The random stream of reward orbs.
const REWARD_RANDOM: u64 = random::tag("trade-reward");
/// The trade screen titles by profession (`EntityVariant`), from BDS 1.26.52's `villager_v2`
/// economy trade tables: unskilled, farmer, fisherman, shepherd, fletcher, librarian,
/// cartographer, cleric, armorer, weaponsmith, toolsmith, butcher, leatherworker, mason.
const DISPLAY_NAMES: [&str; 14] = [
    "entity.villager.unskilled",
    "entity.villager.farmer",
    "entity.villager.fisherman",
    "entity.villager.shepherd",
    "entity.villager.fletcher",
    "entity.villager.librarian",
    "entity.villager.cartographer",
    "entity.villager.cleric",
    "entity.villager.armor",
    "entity.villager.weapon",
    "entity.villager.tool",
    "entity.villager.butcher",
    "entity.villager.leather",
    "entity.villager.mason",
];
/// The title of other professions (the nitwit, 14, has none of its own).
const DEFAULT_DISPLAY_NAME: &str = "entity.villager.name";

/// One trade as the inventory executor sees it: what the player pays (`buy_b` may be empty)
/// and what it gets, per trade.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Offer {
    /// The first payment.
    pub(super) buy_a: Stack,
    /// The second payment, or empty.
    pub(super) buy_b: Stack,
    /// How the first payment's damage matches.
    pub(super) damage_a: Damage,
    /// How the second payment's damage matches.
    pub(super) damage_b: Damage,
    /// What the trade gives.
    pub(super) sell: Stack,
    /// The most trades one request may make now: the uses left, at most [`MAX_REPEAT`].
    pub(super) max_times: u32,
}

/// What trading needs to know of a villager NPC (the entity module's view of one).
#[derive(Debug, Clone, Copy)]
pub(super) struct Villager<'a> {
    /// Its world.
    pub(super) world: WorldIndex,
    /// Its feet.
    pub(super) position: Vec3,
    /// Its unique ID (its runtime ID).
    pub(super) unique_id: u64,
    /// Its profession (`EntityVariant`).
    pub(super) profession: u8,
    /// Its offers.
    pub(super) offers: &'a Offers,
}

/// One player's trading: the villager and the trade window's ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    villager: Id,
    window: u8,
}

/// Who trades with which villager NPC.
#[derive(Debug, Default)]
pub(super) struct Trades {
    /// The trading players: at most one entry per joined player ([`tick`] drops the others).
    sessions: BTreeMap<SessionId, Session>,
    /// `sessions` by villager, to find a villager's traders; derived from `sessions`.
    by_villager: BTreeSet<(Id, SessionId)>,
    /// The next offer network ID to allocate (0 until the first: the first ID after the
    /// recipes).
    next_net_id: u32,
    /// The test stand-ins for villagers and windows.
    #[cfg(test)]
    fixtures: tests::Fixtures,
}

impl Trades {
    /// Starts `session`'s trading with `villager` in window `window`, ending any other.
    fn start(&mut self, session: SessionId, villager: Id, window: u8) {
        self.end(session);
        self.sessions.insert(session, Session { villager, window });
        self.by_villager.insert((villager, session));
    }

    /// Ends `session`'s trading, if any.
    fn end(&mut self, session: SessionId) {
        if let Some(s) = self.sessions.remove(&session) {
            self.by_villager.remove(&(s.villager, session));
        }
    }

    /// The sessions trading with `villager` and their windows.
    fn traders(&self, villager: Id) -> Vec<(SessionId, u8)> {
        self.by_villager
            .range((villager, SessionId::new(0))..=(villager, SessionId::new(u64::MAX)))
            .filter_map(|&(_, s)| self.sessions.get(&s).map(|t| (s, t.window)))
            .collect()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            sessions,
            by_villager: _,
            next_net_id,
            fixtures,
        } = self;
        h.usize(sessions.len());
        for (s, t) in sessions {
            h.u64(s.get());
            h.u64(u64::from(t.villager.index()));
            h.u64(u64::from(t.villager.generation()));
            h.u64(u64::from(t.window));
        }
        h.u64(u64::from(*next_net_id));
        // The stand-in villagers' offers, which the entity module hashes in the game.
        fixtures.hash_into(h);
    }
}

super::checkpoint::fields!(Session { villager, window });

/// The sessions and the next network ID; `by_villager` is `sessions` again.
impl super::checkpoint::Codec for Trades {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self {
            sessions,
            by_villager: _,
            next_net_id,
            #[cfg(test)]
                fixtures: _,
        } = self;
        sessions.encode(w);
        next_net_id.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        let sessions: BTreeMap<SessionId, Session> = super::checkpoint::Codec::decode(r)?;
        let by_villager = sessions.iter().map(|(&s, t)| (t.villager, s)).collect();
        Ok(Self {
            sessions,
            by_villager,
            next_net_id: super::checkpoint::Codec::decode(r)?,
            #[cfg(test)]
            fixtures: tests::Fixtures::default(),
        })
    }
}

/// What trading calls in other modules: the running server's in [`HOOKS`], stand-ins in tests.
#[derive(Clone, Copy)]
struct Hooks {
    /// The villager NPC `id`, if it is one and has a runtime ID.
    villager: for<'a> fn(&'a State, Id) -> Option<Villager<'a>>,
    /// The offers of the villager NPC `id`, to change.
    offers_mut: for<'a> fn(&'a mut State, Id) -> Option<&'a mut Offers>,
    /// Opens the trade window (`inventory::open_trade`).
    open_window: fn(&mut State, SessionId, Id) -> Option<u8>,
    /// Closes the player's window (`inventory::close_window`).
    close_window: fn(&mut State, SessionId),
    /// Drops experience orbs (`entity::drop_experience`).
    drop_experience: fn(&mut State, WorldIndex, Vec3, u32),
    /// Tells a villager's viewers its tier and experience changed from the given ones
    /// (`entity::villager_traded`).
    villager_traded: fn(&mut State, Id, (u8, u32)),
}

/// The hooks of the running server.
const HOOKS: Hooks = Hooks {
    villager: entity_villager,
    offers_mut: entity_offers_mut,
    open_window: inventory::open_trade,
    close_window: inventory::close_window,
    drop_experience: entity::drop_experience,
    villager_traded: entity::villager_traded,
};

/// The villager NPC `id` as the entity module keeps it.
fn entity_villager(state: &State, id: Id) -> Option<Villager<'_>> {
    let (world, position, unique_id, profession, offers) = super::entity::villager(state, id)?;
    Some(Villager {
        world,
        position,
        unique_id,
        profession,
        offers,
    })
}

/// The offers of the villager NPC `id` in the entity module, to change (its chunk is saved).
fn entity_offers_mut(state: &mut State, id: Id) -> Option<&mut Offers> {
    super::entity::offers_mut(state, id)
}

/// `session` opens trading with the villager NPC `villager` (an `interact` on it): opens the
/// trade window and sends `UpdateTrade`. False, with nothing opened, if `villager` is not a
/// villager NPC in the player's world or has no offers.
pub(super) fn open(state: &mut State, session: SessionId, villager: Id) -> bool {
    open_with(state, session, villager, HOOKS)
}

fn open_with(state: &mut State, session: SessionId, villager: Id, hooks: Hooks) -> bool {
    let may_open = |state: &State| {
        let world = state.players.get(&session).map(|p| p.world);
        (hooks.villager)(state, villager)
            .is_some_and(|v| Some(v.world) == world && !v.offers.is_empty())
    };
    if !may_open(state) {
        return false;
    }
    let mut event = dispatch::heard::<TradeOpen>(state).then(|| TradeOpen {
        player: crate::player::Id::new(session),
        villager,
        cancelled: false,
    });
    if let Some(e) = &mut event {
        dispatch::call(state, e);
        if e.cancelled || (dispatch::listened::<TradeOpen>(state) && !may_open(state)) {
            return false;
        }
    }
    let opened = open_now(state, session, villager, hooks);
    if let Some(e) = event.filter(|_| opened) {
        dispatch::watch(state, &e);
    }
    opened
}

/// Opens the trade window of a villager the player may trade with ([`open_with`]).
fn open_now(state: &mut State, session: SessionId, villager: Id, hooks: Hooks) -> bool {
    allocate_net_ids(state, villager, hooks);
    let Some(view) = View::of(state, villager, hooks) else {
        return false;
    };
    let Some(window) = (hooks.open_window)(state, session, villager) else {
        return false;
    };
    state.trades.start(session, villager, window);
    if let Some(p) = state.players.get_mut(&session) {
        p.send_packet(view.packet(window));
    }
    true
}

/// The offer with network ID `net_id` of the villager `session` trades with, if it can be traded
/// now: the ID is from the list as it is now (not an older screen, not another villager's, not
/// a recipe's), the villager is in the player's world (until [`tick`] closes the window), its
/// tier is unlocked and it has uses left.
pub(super) fn offer(state: &State, session: SessionId, net_id: u32) -> Option<Offer> {
    offer_with(state, session, net_id, HOOKS)
}

fn offer_with(state: &State, session: SessionId, net_id: u32, hooks: Hooks) -> Option<Offer> {
    let s = state.trades.sessions.get(&session)?;
    let world = state.players.get(&session)?.world;
    let v = (hooks.villager)(state, s.villager).filter(|v| v.world == world)?;
    let r = v.offers.tradable(v.offers.index_of(net_id)?)?;
    Some(Offer {
        buy_a: r.buy_a.clone(),
        buy_b: r.buy_b.clone(),
        damage_a: r.damage_a,
        damage_b: r.damage_b,
        sell: r.sell.clone(),
        max_times: r.uses_left().min(MAX_REPEAT),
    })
}

/// The villager `session` trades with and the place of the offer with network ID `net_id` in its
/// list (for the trade event).
pub(super) fn trading(state: &State, session: SessionId, net_id: u32) -> Option<(Id, u32)> {
    let s = state.trades.sessions.get(&session)?;
    let v = (HOOKS.villager)(state, s.villager)?;
    let index = v.offers.index_of(net_id)?;
    Some((s.villager, u32::try_from(index).ok()?))
}

/// Whether `given` pays for the payment `want` of an offer, counts aside: the same item type and
/// damage, and the same extra data when the offer's payment has any. A payment of any damage
/// (BDS `Damage` 32767, the Bedrock any-meta value) takes the item of any damage or meta: the
/// same item name.
pub(super) fn is_payment(want: &Stack, damage: Damage, given: &Stack) -> bool {
    let item = match damage {
        Damage::Exact => given.item() == want.item() && given.damage() == want.damage(),
        Damage::Any => given.item().map(Type::name) == want.item().map(Type::name),
    };
    !want.is_empty() && item && (want.extra().is_none() || given.extra() == want.extra())
}

/// The executor completed `times` trades of `net_id` for `session` (after [`offer`] allowed
/// them and the items moved): counts the uses, gives the villager its experience (and tier)
/// and drops the reward orbs. It cannot fail after `offer` succeeded in the same request; called
/// otherwise, it counts at most what `offer` would allow and logs the executor's error.
pub(super) fn traded(state: &mut State, session: SessionId, net_id: u32, times: u32) {
    traded_with(state, session, net_id, times, HOOKS);
}

fn traded_with(state: &mut State, session: SessionId, net_id: u32, times: u32, hooks: Hooks) {
    let Some(&Session { villager, .. }) = state.trades.sessions.get(&session) else {
        error!(
            session = session.get(),
            "a trade without a villager was not counted"
        );
        return;
    };
    let Some(v) = (hooks.villager)(state, villager) else {
        error!(
            session = session.get(),
            "a trade with a vanished villager was not counted"
        );
        return;
    };
    let (world, position) = (v.world, v.position);
    let Some(index) = v.offers.index_of(net_id) else {
        error!(
            session = session.get(),
            net_id, "a trade of an unknown offer was not counted"
        );
        return;
    };
    let allowed = v
        .offers
        .tradable(index)
        .map_or(0, |r| r.uses_left().min(MAX_REPEAT));
    let uses = v.offers.recipes().get(index).map_or(0, |r| r.uses);
    let before = (v.offers.tier(), v.offers.experience());
    if times > allowed {
        error!(
            session = session.get(),
            times, allowed, "the executor traded more than the offer allows; counted what it does"
        );
    }
    let times = times.min(allowed);
    if times == 0 {
        return;
    }
    let Some(applied) = (hooks.offers_mut)(state, villager).and_then(|o| o.apply(index, times))
    else {
        return;
    };
    if applied.reward_exp && applied.times > 0 {
        let seed = state.worlds.get(world).map_or(0, crate::world::World::seed);
        let index = u64::try_from(index).unwrap_or(u64::MAX);
        let parts = [
            u64::from(villager.index()),
            u64::from(villager.generation()),
            index,
            u64::from(uses),
        ];
        let mut r = Random::new(seed, state.tick, random::key(REWARD_RANDOM, &parts));
        let amount = (0..applied.times).fold(0_u32, |sum, _| {
            sum.saturating_add(REWARD_BASE + r.below(REWARD_SPREAD))
        });
        let at = Vec3::new(position.x, position.y + REWARD_HEIGHT, position.z);
        (hooks.drop_experience)(state, world, at, amount);
    }
    // A new tier reaches the traders through the villager's entity data alone: BDS sends no
    // UpdateTrade then (P6 `tradeup`).
    (hooks.villager_traded)(state, villager, before);
}

/// `session`'s trade window closed (by the client, leaving, dying, another window, the villager
/// gone): it no longer trades. The inventory module calls it after returning the payment slots.
pub(super) fn closed(state: &mut State, session: SessionId) {
    state.trades.end(session);
}

/// Ends the trading of every player whose villager is gone or in another world: closes the
/// window (the payments go back) and forgets the session; forgets the sessions of players who
/// left. Runs after the entities' tick.
pub(super) fn tick(state: &mut State) {
    tick_with(state, HOOKS);
}

fn tick_with(state: &mut State, hooks: Hooks) {
    let mut ended = Vec::new();
    for (&s, t) in &state.trades.sessions {
        let here = state.players.get(&s).map(|p| p.world);
        let there = (hooks.villager)(state, t.villager).map(|v| v.world);
        if here.is_none() || here != there {
            ended.push(s);
        }
    }
    for s in ended {
        if state.players.contains_key(&s) {
            (hooks.close_window)(state, s);
        }
        closed(state, s);
    }
}

/// Changes the offers of the villager NPC `villager` with `change` (one of the [`Offers`]
/// methods: `set`, `add`, `remove`, `replace`, `set_tier`, `set_tier_experience`, `reset_uses`)
/// and shows the result to every player trading with it. The trade API and the P6 console wrap
/// it. Nothing changes if `change` fails.
pub(super) fn change_offers<T>(
    state: &mut State,
    villager: Id,
    change: impl FnOnce(&mut Offers) -> Result<T>,
) -> Result<T> {
    change_offers_with(state, villager, change, HOOKS)
}

fn change_offers_with<T>(
    state: &mut State,
    villager: Id,
    change: impl FnOnce(&mut Offers) -> Result<T>,
    hooks: Hooks,
) -> Result<T> {
    let offers = (hooks.offers_mut)(state, villager).ok_or(Error::NotVillager)?;
    let before = (offers.tier(), offers.experience());
    let out = change(offers)?;
    (hooks.villager_traded)(state, villager, before);
    refresh(state, villager, hooks);
    Ok(out)
}

/// Sends the villager's offers again to everyone trading with it (a new tier, a changed list);
/// a list too large to send closes their windows instead.
fn refresh(state: &mut State, villager: Id, hooks: Hooks) {
    let traders = state.trades.traders(villager);
    if traders.is_empty() {
        return;
    }
    allocate_net_ids(state, villager, hooks);
    let Some(view) = View::of(state, villager, hooks) else {
        for (s, _) in traders {
            (hooks.close_window)(state, s);
            closed(state, s);
        }
        return;
    };
    for (s, window) in traders {
        if let Some(p) = state.players.get_mut(&s) {
            p.send_packet(view.packet(window));
        }
    }
}

/// Gives the villager's offers network IDs from the server counter unless they have them.
fn allocate_net_ids(state: &mut State, villager: Id, hooks: Hooks) {
    let mut next = state.trades.next_net_id;
    let first = first_net_id(state);
    if let Some(offers) = (hooks.offers_mut)(state, villager) {
        offers.allocate_net_ids(&mut next, first);
    }
    state.trades.next_net_id = next;
}

/// The first network ID after the last `CraftingData` recipe, the server's own recipes included
/// (BDS numbers trade offers after it).
fn first_net_id(state: &State) -> u32 {
    state.custom.first_special_id()
}

/// The trade screen of a villager: everything `UpdateTrade` sends but the window.
struct View {
    tier: u8,
    unique_id: u64,
    profession: u8,
    /// The offers in network NBT.
    offers: Vec<u8>,
}

impl View {
    /// The villager's screen as it is now; `None` if it is gone, its offers have no network IDs
    /// or are too large to send.
    fn of(state: &State, villager: Id, hooks: Hooks) -> Option<Self> {
        let v = (hooks.villager)(state, villager)?;
        let nbt = v.offers.wire_nbt()?;
        let mut w = Writer::new(Vec::new(), 0);
        if let Err(e) = w.nbt(&nbt, Encoding::NetworkLittleEndian) {
            warn!(error = %e, "villager offers cannot be sent");
            return None;
        }
        Some(Self {
            tier: v.offers.tier(),
            unique_id: v.unique_id,
            profession: v.profession,
            offers: w.into_inner(),
        })
    }

    /// `UpdateTrade` for trade window `window`, as BDS sends it (P6).
    fn packet(&self, window: u8) -> Packet {
        Packet::UpdateTrade(UpdateTrade {
            window_id: window,
            window_type: ContainerType::Trade.into(),
            size: 0,
            trade_tier: i32::from(self.tier),
            villager_unique_id: self.unique_id.cast_signed(),
            entity_unique_id: ANY_ENTITY,
            display_name: display_name(self.profession).to_owned(),
            new_trade_ui: true,
            demand_based_prices: true,
            serialised_offers: self.offers.clone(),
        })
    }
}

/// The trade screen title of a profession.
fn display_name(profession: u8) -> &'static str {
    DISPLAY_NAMES
        .get(usize::from(profession))
        .copied()
        .unwrap_or(DEFAULT_DISPLAY_NAME)
}
