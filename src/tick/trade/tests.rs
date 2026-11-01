//! Tests of trading: the screen BDS sends, the trade rules, the end of trading, and that nothing
//! stays behind. Villagers, windows and orbs are stand-ins here (the entity and inventory modules
//! own them).

use std::collections::BTreeMap;

use tokio::sync::mpsc::Receiver;

use super::offers::tests::{from_network_bytes, sample, shape, stack};
use super::*;
use crate::event::Listeners;
use crate::item::{Extra, Kind};
use crate::nbt::{Compound, List, Value};
use crate::tick::queue::Outbound;
use crate::tick::tests::{join, new_state, radius, settings, tick};
use crate::tick::{Output, SessionId, State};

const TEST_HOOKS: Hooks = Hooks {
    villager: villager_of,
    offers_mut: offers_of,
    open_window,
    close_window,
    close_window_without_event,
    drop_experience,
    villager_traded,
};

/// A stand-in villager NPC.
#[derive(Debug)]
struct Fixture {
    world: WorldIndex,
    position: Vec3,
    unique_id: u64,
    profession: u8,
    offers: Offers,
}

/// The stand-ins for the entity and inventory modules, kept in [`Trades`].
#[derive(Debug, Default)]
pub(super) struct Fixtures {
    villagers: BTreeMap<Id, Fixture>,
    /// The last window ID the stand-in inventory gave.
    window: u8,
    /// Whether the stand-in inventory refuses to open a window.
    refuse: bool,
    /// The sessions whose window the stand-in inventory closed for a game rule.
    closed: Vec<SessionId>,
    /// The sessions whose window the stand-in inventory closed for a direct API call.
    closed_without_event: Vec<SessionId>,
    /// The orbs dropped: world, position, amount.
    drops: Vec<(WorldIndex, Vec3, u32)>,
    /// The villagers told of a trade, with their tier and experience before it.
    traded: Vec<(Id, (u8, u32))>,
}

impl Fixtures {
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            villagers,
            // The inventory module's window counter, outside the trade state.
            window: _,
            // A test switch.
            refuse: _,
            // Records of closes, whose effects the inventory module keeps.
            closed: _,
            closed_without_event: _,
            drops,
            // Records of calls that only send packets.
            traded: _,
        } = self;
        h.usize(villagers.len());
        for (id, v) in villagers {
            h.u64(u64::from(id.index()));
            v.offers.hash_into(h);
        }
        // Dropped orbs are entities in the game.
        h.usize(drops.len());
    }
}

/// A test server with joined players 1 to `players`.
struct Server {
    state: State,
    /// The players' connection ends; dropping one would drop its player.
    _receivers: Vec<Receiver<Outbound>>,
    /// The request ID of the next trade, a new one each as clients send them.
    next_request: i32,
}

impl Server {
    fn new(players: u64) -> Self {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let mut receivers = Vec::new();
        let mut batch = Vec::new();
        for n in 1..=players {
            let (j, rx) = join(n, 0);
            receivers.push(rx);
            batch.push(j);
            batch.push(radius(n, 1, 4));
        }
        tick(&mut state, &mut batch, &mut output);
        tick(&mut state, &mut Vec::new(), &mut output);
        for n in 1..=players {
            let p = state.players.get_mut(&SessionId::new(n)).unwrap();
            p.outbox.clear();
        }
        Self {
            state,
            _receivers: receivers,
            next_request: -1,
        }
    }

    fn offers(&self, villager: Id) -> &Offers {
        &self.state.trades.fixtures.villagers[&villager].offers
    }

    fn offer(&self, s: u64, net_id: u32) -> Option<Offer> {
        offer_with(&self.state, SessionId::new(s), net_id, TEST_HOOKS)
    }

    fn is_trading(&self, s: u64) -> bool {
        self.state.trades.sessions.contains_key(&SessionId::new(s))
    }

    /// Whether the session table and its index agree.
    fn index_matches(&self) -> bool {
        let t = &self.state.trades;
        t.by_villager.len() == t.sessions.len()
            && t.sessions
                .iter()
                .all(|(&s, x)| t.by_villager.contains(&(x.villager, s)))
    }

    /// Adds stand-in villager `n` (a farmer at the spawn) with `offers`.
    fn villager(&mut self, n: u32, offers: Offers) -> Id {
        let id = Id::new(n, 0);
        let fixture = Fixture {
            world: 0,
            position: Vec3::new(0.5, -60.0, 0.5),
            unique_id: 1000 + u64::from(n),
            profession: 1,
            offers,
        };
        self.state.trades.fixtures.villagers.insert(id, fixture);
        id
    }

    fn open(&mut self, s: u64, villager: Id) -> bool {
        open_with(&mut self.state, SessionId::new(s), villager, TEST_HOOKS)
    }

    fn traded(&mut self, s: u64, net_id: u32, times: u32) {
        let request = self.next_request;
        self.next_request -= 2;
        traded_with(
            &mut self.state,
            SessionId::new(s),
            request,
            net_id,
            times,
            TEST_HOOKS,
        );
    }

    fn tick(&mut self) {
        tick_with(&mut self.state, TEST_HOOKS);
    }

    /// Takes the `UpdateTrade`s queued for player `s` since the last call.
    fn screens(&mut self, s: u64) -> Vec<UpdateTrade> {
        let p = self.state.players.get_mut(&SessionId::new(s)).unwrap();
        std::mem::take(&mut p.outbox)
            .into_iter()
            .filter_map(|p| match p {
                Packet::UpdateTrade(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    /// Takes the last screen of player `s` and gives the network IDs of its offers.
    fn net_ids(&mut self, s: u64) -> Vec<u32> {
        let screen = self.screens(s).pop().expect("a screen was queued");
        net_ids(&screen)
    }
}

fn villager_of(state: &State, id: Id) -> Option<Villager<'_>> {
    state.trades.fixtures.villagers.get(&id).map(|v| Villager {
        world: v.world,
        position: v.position,
        unique_id: v.unique_id,
        profession: v.profession,
        offers: &v.offers,
    })
}

fn offers_of(state: &mut State, id: Id) -> Option<&mut Offers> {
    state
        .trades
        .fixtures
        .villagers
        .get_mut(&id)
        .map(|v| &mut v.offers)
}

/// Opens a window as `inventory::open_trade` does: an open trade window closes first (reporting
/// `closed`). Window IDs count from 2 to 99 and start over at 2, on one counter for every player.
fn open_window(state: &mut State, session: SessionId, _: Id) -> Option<u8> {
    if state.trades.fixtures.refuse {
        return None;
    }
    if state.trades.sessions.contains_key(&session) {
        closed(state, session);
    }
    let f = &mut state.trades.fixtures;
    f.window = if f.window >= 99 {
        2
    } else {
        f.window.max(1) + 1
    };
    Some(f.window)
}

/// Closes the window as `inventory::close_window` does: returns the payments and reports
/// `closed`.
fn close_window(state: &mut State, session: SessionId) {
    state.trades.fixtures.closed.push(session);
    closed(state, session);
}

/// Closes the window as `inventory::close_window_without_event` does: returns the payments and
/// reports `closed`.
fn close_window_without_event(state: &mut State, session: SessionId) {
    state.trades.fixtures.closed_without_event.push(session);
    closed(state, session);
}

fn drop_experience(state: &mut State, world: WorldIndex, at: Vec3, amount: u32) {
    state.trades.fixtures.drops.push((world, at, amount));
}

fn villager_traded(state: &mut State, villager: Id, before: (u8, u32)) {
    state.trades.fixtures.traded.push((villager, before));
}

fn recipes(offers: &Compound) -> Vec<Compound> {
    match offers.get("Recipes") {
        Some(Value::List(List::Compound(l))) => l.clone(),
        other => panic!("no recipes: {other:?}"),
    }
}

fn net_ids(screen: &UpdateTrade) -> Vec<u32> {
    recipes(&from_network_bytes(&screen.serialised_offers))
        .iter()
        .map(|r| match r.get("netId") {
            Some(Value::Int(id)) => u32::try_from(*id).unwrap(),
            other => panic!("no netId: {other:?}"),
        })
        .collect()
}

#[test]
fn the_trade_screen_is_the_one_bds_sends() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    let screens = server.screens(1);
    assert_eq!(screens.len(), 1, "one UpdateTrade and no ContainerOpen");
    let u = &screens[0];
    assert_eq!(u.window_id, 2);
    assert_eq!(u.window_type, 15);
    assert_eq!(u.size, 0);
    assert_eq!(u.trade_tier, 0);
    assert_eq!(u.villager_unique_id, 1001);
    assert_eq!(u.entity_unique_id, -1);
    assert_eq!(u.display_name, "entity.villager.farmer");
    assert!(u.new_trade_ui);
    assert!(u.demand_based_prices);
    // The offers: BDS's root keys, every offer, IDs after the last recipe.
    let offers = from_network_bytes(&u.serialised_offers);
    assert_eq!(
        offers.keys().collect::<Vec<_>>(),
        ["Recipes", "TierExpRequirements"]
    );
    let first = first_net_id(&server.state);
    assert!(crate::crafting::recipe(first - 1).is_some());
    assert!(crate::crafting::recipe(first).is_none());
    assert_eq!(net_ids(u), [first, first + 1, first + 2]);
    let mut keys = Vec::new();
    shape(&recipes(&offers)[0], "", &mut keys);
    for key in [
        "buyA:compound",
        "buyCountA:int",
        "buyCountB:int",
        "demand:int",
        "maxUses:int",
        "netId:int",
        "priceMultiplierA:float",
        "priceMultiplierB:float",
        "rewardExp:byte",
        "sell:compound",
        "tier:int",
        "traderExp:int",
        "uses:int",
    ] {
        assert!(keys.contains(&key.to_owned()), "{key}");
    }
    assert!(server.is_trading(1));
}

#[test]
fn display_names_follow_the_profession() {
    assert_eq!(display_name(0), "entity.villager.unskilled");
    assert_eq!(display_name(8), "entity.villager.armor");
    assert_eq!(display_name(13), "entity.villager.mason");
    assert_eq!(display_name(14), "entity.villager.name");
    assert_eq!(display_name(255), "entity.villager.name");
}

#[test]
fn only_a_villager_with_offers_in_the_same_world_opens() {
    let mut server = Server::new(1);
    let before = server.state.hash();
    assert!(!server.open(1, Id::new(9, 0)), "not a villager");
    let empty = server.villager(2, Offers::default());
    assert!(!server.open(1, empty), "no offers");
    let far = server.villager(3, sample());
    server
        .state
        .trades
        .fixtures
        .villagers
        .get_mut(&far)
        .unwrap()
        .world = 1;
    assert!(!server.open(1, far), "another world");
    let v = server.villager(4, sample());
    assert!(!server.open(2, v), "no such player");
    server.state.trades.fixtures.refuse = true;
    assert!(!server.open(1, v), "no window");
    assert!(!server.is_trading(1));
    assert!(server.screens(1).is_empty());
    // Network IDs a refused opening allocated stay with the unchanged list (the next opening
    // shows them); nothing else is left.
    server.state.trades.fixtures = Fixtures::default();
    server.state.trades.next_net_id = 0;
    assert_eq!(
        server.state.hash(),
        before,
        "refused openings leave no trace"
    );
}

/// Builds an offer list whose only offer has `max_uses` uses and the given rewards.
fn single(max_uses: u32, trader_exp: u32, reward_exp: bool) -> Offers {
    let mut r = Recipe::new(
        stack(Kind::Wheat, 20),
        Stack::EMPTY,
        stack(Kind::Emerald, 1),
        max_uses,
    );
    r.trader_exp = trader_exp;
    r.reward_exp = reward_exp;
    let mut offers = Offers::default();
    offers.add(r).unwrap();
    offers
}

#[test]
fn offers_need_a_current_id_an_unlocked_tier_and_uses_left() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    let other = server.villager(2, sample());
    assert_eq!(server.offer(1, 0), None, "not trading");
    assert!(server.open(1, other));
    let foreign = server.net_ids(1);
    assert!(server.open(1, v));
    let ids = server.net_ids(1);
    assert!(
        foreign.iter().all(|id| !ids.contains(id)),
        "villagers get their own IDs"
    );

    let offer = server.offer(1, ids[0]).unwrap();
    assert_eq!(offer.buy_a, stack(Kind::Wheat, 20));
    assert!(offer.buy_b.is_empty());
    assert_eq!(offer.sell, stack(Kind::Emerald, 1));
    assert_eq!(offer.max_times, 16);
    assert_eq!(server.offer(1, ids[1]), None, "tier 1 is locked");
    for id in foreign {
        assert_eq!(server.offer(1, id), None, "another villager's offer");
    }
    assert_eq!(server.offer(1, 1), None, "a recipe's ID");
    assert_eq!(server.offer(1, ids[2] + 1), None, "past the list");
    assert_eq!(server.offer(1, 0), None);
    assert_eq!(server.offer(1, u32::MAX), None);

    // A changed list has new IDs: the old screen's IDs fail.
    change_offers_with(
        &mut server.state,
        v,
        |o| {
            o.reset_uses();
            Ok(())
        },
        TEST_HOOKS,
    )
    .unwrap();
    assert_eq!(server.net_ids(1), ids, "uses and tier keep the IDs");
    change_offers_with(
        &mut server.state,
        v,
        |o| o.replace(2, single(1, 0, false).recipes()[0].clone()),
        TEST_HOOKS,
    )
    .unwrap();
    let new = server.net_ids(1);
    assert!(new.iter().all(|id| !ids.contains(id)));
    assert_eq!(server.offer(1, ids[0]), None, "a stale screen's ID");
    assert!(server.offer(1, new[0]).is_some());
}

#[test]
fn trades_count_uses_up_to_the_repeat_cap() {
    let mut server = Server::new(1);
    let v = server.villager(1, single(1000, 0, false));
    assert!(server.open(1, v));
    let id = server.net_ids(1)[0];
    assert_eq!(server.offer(1, id).unwrap().max_times, MAX_REPEAT);
    server.traded(1, id, 256);
    assert_eq!(server.offers(v).recipes()[0].uses, 256);
    // An executor that ignores `max_times` gets at most the cap counted.
    server.traded(1, id, 300);
    assert_eq!(server.offers(v).recipes()[0].uses, 512);
    server.traded(1, id, 256);
    server.traded(1, id, 256);
    assert_eq!(
        server.offers(v).recipes()[0].uses,
        1000,
        "never past the maximum"
    );
    assert_eq!(server.offer(1, id), None, "no uses left");
    let before = server.state.hash();
    server.traded(1, id, 1);
    assert_eq!(
        server.state.hash(),
        before,
        "a sold-out offer counts nothing"
    );
    assert!(
        server.screens(1).is_empty(),
        "no UpdateTrade after trades (BDS)"
    );
    assert!(server.state.trades.fixtures.drops.is_empty(), "no reward");
}

#[test]
fn trades_reward_orbs_and_raise_the_tier() {
    let mut server = Server::new(2);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    assert!(server.open(2, v));
    let ids = server.net_ids(1);
    server.screens(2);
    // Wheat gives 2 experience per trade: five trades reach tier 1 (10).
    server.traded(1, ids[0], 4);
    assert!(server.screens(1).is_empty());
    let drops = std::mem::take(&mut server.state.trades.fixtures.drops);
    assert_eq!(drops.len(), 1);
    let (world, at, amount) = drops[0];
    assert_eq!((world, at), (0, Vec3::new(0.5, -59.5, 0.5)));
    assert!((12..=24).contains(&amount), "3 to 6 per trade: {amount}");
    server.traded(1, ids[0], 1);
    assert_eq!(server.offers(v).tier(), 1);
    assert_eq!(
        server.state.trades.fixtures.traded,
        [(v, (0, 0)), (v, (0, 8))],
        "each trade tells the villager's viewers of its tier and experience"
    );
    for s in [1, 2] {
        // BDS sends no UpdateTrade at a new tier (P6 `tradeup`): the traders learn it from the
        // villager's entity data (`traded` above).
        assert!(server.screens(s).is_empty(), "no new screen at a new tier");
    }
    assert!(server.offer(1, ids[1]).is_some(), "tier 1 unlocked");
    // Glass (tier 2) gives no reward.
    server.state.trades.fixtures.drops.clear();
    change_offers_with(&mut server.state, v, |o| o.set_tier(2), TEST_HOOKS).unwrap();
    server.traded(2, ids[2], 3);
    assert!(server.state.trades.fixtures.drops.is_empty());
    assert_eq!(server.offers(v).recipes()[2].uses, 3);
}

#[test]
fn the_reward_depends_only_on_the_state() {
    let amount = |tick: u64| {
        let mut server = Server::new(1);
        server.state.tick = tick;
        let v = server.villager(1, single(16, 0, true));
        assert!(server.open(1, v));
        let id = server.net_ids(1)[0];
        server.traded(1, id, 10);
        server.state.trades.fixtures.drops[0].2
    };
    assert_eq!(amount(7), amount(7));
    let amounts: Vec<u32> = (0..16).map(amount).collect();
    assert!(amounts.iter().all(|a| (30..=60).contains(a)));
    assert!(amounts.iter().any(|&a| a != amounts[0]), "{amounts:?}");
}

/// Two trades of one player in one tick draw their rewards apart, also when the offer's uses were
/// set back between them (a plugin that keeps a trade unlimited).
#[test]
fn trades_in_one_tick_draw_their_rewards_apart() {
    let mut apart = false;
    for tick in 0..16 {
        let mut server = Server::new(1);
        server.state.tick = tick;
        let v = server.villager(1, single(16, 0, true));
        assert!(server.open(1, v));
        let id = server.net_ids(1)[0];
        server.traded(1, id, 1);
        change_offers_with(
            &mut server.state,
            v,
            |o| {
                o.reset_uses();
                Ok(())
            },
            TEST_HOOKS,
        )
        .unwrap();
        server.traded(1, id, 1);
        let drops = &server.state.trades.fixtures.drops;
        apart |= drops[0].2 != drops[1].2;
    }
    assert!(apart, "the two trades of a tick always got the same reward");
}

#[test]
fn reopening_keeps_the_ids_and_the_counter() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    let ids = server.net_ids(1);
    let next = server.state.trades.next_net_id;
    closed(&mut server.state, SessionId::new(1));
    assert!(server.open(1, v));
    let screen = server.screens(1).pop().unwrap();
    assert_eq!(net_ids(&screen), ids);
    assert_eq!(screen.window_id, 3, "a new window");
    assert_eq!(server.state.trades.next_net_id, next);
}

#[test]
fn closing_ends_trading_once() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    let id = server.net_ids(1)[0];
    closed(&mut server.state, SessionId::new(1));
    let once = server.state.hash();
    closed(&mut server.state, SessionId::new(1));
    assert_eq!(server.state.hash(), once, "closing twice is closing once");
    assert!(!server.is_trading(1));
    assert_eq!(server.offer(1, id), None);
    let before = server.state.hash();
    server.traded(1, id, 1);
    assert_eq!(
        server.state.hash(),
        before,
        "a trade after closing counts nothing"
    );
}

#[test]
fn a_vanished_or_moved_villager_closes_its_traders_windows() {
    let mut server = Server::new(3);
    let a = server.villager(1, sample());
    let b = server.villager(2, sample());
    assert!(server.open(1, a));
    assert!(server.open(2, a));
    assert!(server.open(3, b));
    server.tick();
    assert!(
        server.state.trades.fixtures.closed.is_empty(),
        "nothing changed"
    );
    server.state.trades.fixtures.villagers.remove(&a);
    server.tick();
    assert_eq!(
        server.state.trades.fixtures.closed,
        [SessionId::new(1), SessionId::new(2)]
    );
    assert!(!server.is_trading(1) && !server.is_trading(2) && server.is_trading(3));
    server
        .state
        .trades
        .fixtures
        .villagers
        .get_mut(&b)
        .unwrap()
        .world = 1;
    server.tick();
    assert!(!server.is_trading(3));
    assert!(server.index_matches());
}

#[test]
fn a_villager_moved_to_another_world_trades_nothing_before_the_window_closes() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    let id = server.net_ids(1)[0];
    assert!(server.offer(1, id).is_some());
    server
        .state
        .trades
        .fixtures
        .villagers
        .get_mut(&v)
        .unwrap()
        .world = 1;
    assert_eq!(server.offer(1, id), None);
}

#[test]
fn a_player_who_left_is_forgotten() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    server.state.leave(SessionId::new(1), "client disconnect");
    server.tick();
    assert!(server.state.trades.sessions.is_empty());
    assert!(server.state.trades.by_villager.is_empty());
    assert!(
        server.state.trades.fixtures.closed.is_empty(),
        "no window to close"
    );
}

#[test]
fn opening_and_closing_a_hundred_times_leaves_nothing_behind() {
    let mut server = Server::new(2);
    let a = server.villager(1, sample());
    let b = server.villager(2, sample());
    for i in 0..100 {
        assert!(server.open(1, if i % 2 == 0 { a } else { b }));
        assert!(server.open(2, a));
        assert!(server.open(2, b), "a new window replaces the old one");
        assert_eq!(server.state.trades.sessions.len(), 2);
        assert!(server.index_matches());
        closed(&mut server.state, SessionId::new(1));
        if i % 2 == 0 {
            closed(&mut server.state, SessionId::new(2));
        } else {
            server
                .state
                .trades
                .fixtures
                .villagers
                .get_mut(&b)
                .unwrap()
                .world = 1;
            server.tick();
            server
                .state
                .trades
                .fixtures
                .villagers
                .get_mut(&b)
                .unwrap()
                .world = 0;
        }
        server.screens(1);
        server.screens(2);
    }
    assert!(server.state.trades.sessions.is_empty());
    assert!(server.state.trades.by_villager.is_empty());
    // Reopening unchanged lists allocates nothing new.
    let first = first_net_id(&server.state);
    assert_eq!(server.state.trades.next_net_id, first + 6);
}

#[test]
fn changes_are_all_or_nothing_and_reach_the_traders() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    server.screens(1);
    let before = server.state.hash();
    let full: Vec<Recipe> =
        std::iter::repeat_n(sample().recipes()[0].clone(), crate::custom::MAX_OFFERS + 1).collect();
    assert_eq!(
        change_offers_with(&mut server.state, v, |o| o.set(full), TEST_HOOKS),
        Err(Error::Full)
    );
    assert_eq!(
        change_offers_with(
            &mut server.state,
            Id::new(7, 0),
            |o| o.set_tier(1),
            TEST_HOOKS
        ),
        Err(Error::NotVillager)
    );
    assert_eq!(
        change_offers(&mut server.state, v, |o| o.set_tier(1)),
        Err(Error::NotVillager),
        "the game's entities are not the stand-ins"
    );
    assert_eq!(server.state.hash(), before);
    assert!(server.screens(1).is_empty());
    let removed = change_offers_with(&mut server.state, v, |o| o.remove(0), TEST_HOOKS).unwrap();
    assert_eq!(removed.buy_a, stack(Kind::Wheat, 20));
    let screens = server.screens(1);
    assert_eq!(screens.len(), 1);
    assert_eq!(net_ids(&screens[0]).len(), 2);
    assert_ne!(server.state.hash(), before);
}

/// Builds offers too large for one `UpdateTrade`: [`crate::custom::MAX_OFFERS`] offers, each
/// selling a stack of 60 KB, read as a saved entity's.
fn too_large_to_send() -> Offers {
    let sell = stack(Kind::Stick, 1)
        .with_extra(Some(Extra {
            nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![3; 60_000]))]),
            ..Extra::default()
        }))
        .unwrap();
    let recipe = Compound::from([
        (
            "buyA".to_owned(),
            Value::Compound(stack(Kind::Emerald, 1).to_nbt()),
        ),
        ("maxUses".to_owned(), Value::Int(1)),
        ("sell".to_owned(), Value::Compound(sell.to_nbt())),
    ]);
    let saved = Compound::from([(
        "Recipes".to_owned(),
        Value::List(List::Compound(vec![recipe; crate::custom::MAX_OFFERS])),
    )]);
    Offers::from_nbt(&saved, &crate::custom::Registry::default())
}

/// A changed list too large to send closes its traders' windows without `InventoryClose`: the
/// change is a direct API call, which raises no event.
#[test]
fn a_list_too_large_to_send_closes_the_windows_without_an_event() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    server.screens(1);
    let big = too_large_to_send();
    assert_eq!(big.recipes().len(), crate::custom::MAX_OFFERS);
    server
        .state
        .trades
        .fixtures
        .villagers
        .get_mut(&v)
        .unwrap()
        .offers = big;
    change_offers_with(
        &mut server.state,
        v,
        |o| {
            o.reset_uses();
            Ok(())
        },
        TEST_HOOKS,
    )
    .unwrap();
    assert_eq!(
        server.state.trades.fixtures.closed_without_event,
        [SessionId::new(1)]
    );
    assert!(
        server.state.trades.fixtures.closed.is_empty(),
        "no InventoryClose"
    );
    assert!(!server.is_trading(1));
    assert!(server.screens(1).is_empty());
}

#[test]
fn offers_with_counters_survive_saving_through_the_entity() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    assert!(server.open(1, v));
    let ids = server.net_ids(1);
    server.traded(1, ids[0], 5);
    let saved = server.offers(v).to_nbt();
    let back = Offers::from_nbt(&saved, &crate::custom::Registry::default());
    assert_eq!(back.recipes(), server.offers(v).recipes());
    assert_eq!(back.recipes()[0].uses, 5);
    assert_eq!((back.tier(), back.experience()), (1, 10));
    assert_eq!(back.to_nbt(), saved);
}

#[test]
fn payments_match_type_damage_and_named_extra_data() {
    let pays = |want: &Stack, given: &Stack| is_payment(want, Damage::Exact, given);
    let emerald = stack(Kind::Emerald, 5);
    assert!(pays(&emerald, &stack(Kind::Emerald, 1)));
    assert!(!pays(&emerald, &stack(Kind::Wheat, 5)));
    assert!(!pays(&emerald, &Stack::EMPTY));
    assert!(!pays(&Stack::EMPTY, &Stack::EMPTY));
    let named = emerald
        .with_extra(Some(Extra {
            custom_name: Some("Coin".to_owned()),
            ..Extra::default()
        }))
        .unwrap();
    assert!(pays(&emerald, &named), "the offer names no extra data");
    assert!(!pays(&named, &emerald));
    assert!(pays(&named, &named));
    let sword = stack(Kind::DiamondSword, 1);
    assert!(!pays(&sword, &sword.with_damage(3).unwrap()));
}

#[test]
fn a_payment_of_any_damage_takes_every_damage_of_the_item_only() {
    let any = |want: &Stack, given: &Stack| is_payment(want, Damage::Any, given);
    let sword = stack(Kind::DiamondSword, 1);
    assert!(any(&sword, &sword.with_damage(3).unwrap()));
    assert!(any(&sword, &sword));
    assert!(!any(&sword, &stack(Kind::IronSword, 1)));
    assert!(!any(&sword, &Stack::EMPTY));
    assert!(!any(&Stack::EMPTY, &Stack::EMPTY));
    let named = sword
        .with_extra(Some(Extra {
            custom_name: Some("Old".to_owned()),
            ..Extra::default()
        }))
        .unwrap();
    assert!(!any(&named, &sword), "extra data of the offer still counts");
}

#[test]
fn trading_is_part_of_the_state_hash() {
    let mut server = Server::new(1);
    let v = server.villager(1, sample());
    let before = server.state.hash();
    assert!(server.open(1, v));
    assert_ne!(server.state.hash(), before);
}
