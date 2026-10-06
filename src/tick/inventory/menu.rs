//! Menu windows (`InvMenu`'s way): a chest, double chest or hopper screen whose items are the
//! server's, opened on fake container blocks behind the player.
//!
//! Opening shows the container blocks to the player alone (`view`'s menu blocks, over its other
//! fake blocks) with their client data (the title as `CustomName`, a double chest's pair), and
//! the window opens [`OPEN_DELAY`] ticks later, once the client has the blocks. Any item stack
//! request that names the menu's container is refused whole and the player's screen is sent
//! again, so no item moves into or out of a menu; the request's first take, place, swap or drop
//! of a menu slot becomes one [`Click`] for the handler, which runs in the input phase with the
//! handler taken out of the state ([`Handlers`]). A repeated request ID is refused before, so it
//! clicks once.
//!
//! The menu closes as any window does (the client closing it, a server close, another window,
//! leaving, dying, a teleport or another world) and when the player is more than 8 blocks from
//! it; closing shows the blocks it stood on again and calls nothing. Menus raise no events.

use std::collections::BTreeMap;
use std::fmt;

use super::super::{Context, Phase, Player, SessionId, State, survival, view};
use super::{
    MAX_GIVE_BACK, MAX_PENDING, Notice, StackId, Window, WindowKind, close_now, slot_packet,
};
use crate::block;
use crate::item::Stack;
use crate::math::Vec3;
use crate::menu::{Action, Click, Error, Id, Kind, Menu, Response, Result};
use crate::nbt::{Compound, Value};
use crate::player;
use crate::protocol::consts::ContainerId;
use crate::protocol::packet::{BlockActorData, Packet};
use crate::protocol::{
    ItemStackRequest, PlaceStackRequestAction, StackRequestAction, StackRequestSlotInfo,
    TakeStackRequestAction,
};
use crate::world::{self, BlockPos};

/// Ticks between showing a menu's blocks and opening its window, so the client has the container
/// blocks and their data when the window opens (`InvMenu` waits for the client the same way).
/// The delay the official client needs is not verified.
const OPEN_DELAY: u64 = 2;
/// A menu closes when its player's eyes are farther than this from its block's centre (8 blocks).
const MAX_DISTANCE_SQUARED: f64 = 64.0;
/// The eye height of a player above its feet (the tick keeps the eye position).
const EYE_HEIGHT: f64 = 1.62;
/// `InvMenu`'s place behind the player (`getBehindPositionOffset`): the look direction times
/// -(1 + width) horizontally and -(1 + height) vertically, from the feet (PMMP player size
/// 0.6 × 1.8).
const BEHIND_HORIZONTAL: f64 = 1.6;
const BEHIND_VERTICAL: f64 = 2.8;

/// What runs when the player clicks a menu slot.
type Handler = Box<dyn FnMut(&mut Context<'_>, player::Id, Click) -> Response + Send>;

/// The click handlers of the open menus, by session: one per player with a menu open, kept in
/// the state beside the players (the output phase shares the players between threads, and a
/// handler need not be `Sync`). A handler is out of the table while it runs.
#[derive(Default)]
pub(in crate::tick) struct Handlers(BTreeMap<SessionId, (Id, Handler)>);

impl fmt::Debug for Handlers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(s, (id, _))| (s.get(), id)))
            .finish()
    }
}

impl Handlers {
    /// How many handlers wait.
    pub(in crate::tick) fn len(&self) -> usize {
        self.0.len()
    }
}

/// An open menu of a player; its window is the player's window ([`WindowKind::Menu`]) and its
/// handler is in [`Handlers`].
#[derive(Debug)]
pub(in crate::tick) struct Open {
    id: Id,
    /// The items, one per slot.
    pub(super) stacks: Vec<Stack>,
    /// The tick from which the window may open.
    ready: u64,
}

impl Open {
    /// The menu's ID.
    pub(super) fn id(&self) -> Id {
        self.id
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::super::Fnv) {
        let Self { id, stacks, ready } = self;
        h.u64(u64::from(id.get()));
        h.usize(stacks.len());
        for s in stacks {
            super::hash_stack(h, s);
        }
        h.u64(*ready);
    }
}

crate::tick::checkpoint::fields!(Open { id, stacks, ready });

/// The player of `id` if it is in the game (its saved data has loaded) and its client has a view.
fn in_game(state: &mut State, id: player::Id) -> Option<&mut Player> {
    state
        .players
        .get_mut(&id.session())
        .filter(|p| p.phase != Phase::Loading && p.view.is_open())
}

impl Context<'_> {
    /// Opens `menu` for `player`: a chest, double chest or hopper screen whose items the player
    /// cannot take, place or drop. Each click on a menu slot is refused (the player's screen is
    /// put back) and calls `handler` once, in the tick, with what the player tried; the handler's
    /// [`Response`] keeps the menu open or closes it. A window the player had open closes first.
    ///
    /// The screen stands on container blocks shown to this player alone, behind it (as `InvMenu`
    /// places them), and opens two ticks later, once the client has them. It closes as any
    /// window does ([`close_window`](Self::close_window) closes it from the server) and when the
    /// player gets more than 8 blocks away; the blocks it stood on show again then. Neither
    /// opening nor closing raises an event or calls the handler.
    ///
    /// Fails, opening nothing, if the player is not in the game, waits with stacks for room in its
    /// inventory, has 4,096 fake block changes waiting this tick or was given `u32::MAX` menus
    /// already (menu IDs do not wrap), or if the menu is invalid (a slot past its end, a title
    /// over 1,024 bytes).
    #[doc(alias = "Player::setCurrentWindow")]
    pub fn open_menu(
        &mut self,
        player: player::Id,
        menu: Menu,
        handler: impl FnMut(&mut Context<'_>, player::Id, Click) -> Response + Send + 'static,
    ) -> Result<Id> {
        let (kind, title, stacks) = menu.prepare()?;
        let session = player.session();
        let tick = self.state.tick;
        let p = in_game(self.state, player).ok_or(Error::Offline)?;
        // The old menu's two positions and the new menu's two.
        if p.inventory.pending.len() + MAX_GIVE_BACK > MAX_PENDING || !p.view.has_room_to_show(4) {
            return Err(Error::Busy);
        }
        let n = u32::try_from(p.inventory.next_menu).map_err(|_| Error::Exhausted)?;
        let blocks = place(p, kind);
        if p.inventory.window.is_some() {
            close_now(self.state, session, Notice::Send);
        }
        let p = in_game(self.state, player).ok_or(Error::Offline)?;
        p.inventory.next_menu = p.inventory.next_menu.saturating_add(1);
        let id = Id::new(n);
        // The old menu, if any, closed above: no menu blocks were shown.
        let _none = p.view.set_menu_blocks(blocks);
        for packet in block_data(kind, &title, blocks) {
            p.send_packet(packet);
        }
        let pos = blocks[0].map(|(pos, _)| pos).unwrap_or_default();
        p.inventory.window = Some(Window {
            id: 0,
            kind: WindowKind::Menu { pos, kind },
            ui: Vec::new(),
            ids: Vec::new(),
            station: super::station::Shown::default(),
        });
        p.inventory.menu = Some(Open {
            id,
            stacks,
            ready: tick.saturating_add(OPEN_DELAY),
        });
        self.state.menus.0.insert(session, (id, Box::new(handler)));
        Ok(id)
    }

    /// The menu `player` has open, if any.
    pub fn menu(&self, player: player::Id) -> Option<Id> {
        let p = self.state.players.get(&player.session())?;
        p.inventory.menu.as_ref().map(|m| m.id)
    }

    /// Puts `stack` into `slot` of the menu `player` has open (an empty stack clears it) and
    /// shows it to the player.
    ///
    /// Fails, changing nothing, if the player has no menu open or the menu has no such slot.
    pub fn set_menu_item(&mut self, player: player::Id, slot: usize, stack: Stack) -> Result<()> {
        let p = self
            .state
            .players
            .get_mut(&player.session())
            .ok_or(Error::Offline)?;
        let inv = &mut p.inventory;
        let there = inv
            .menu
            .as_ref()
            .ok_or(Error::NotOpen)?
            .stacks
            .get(slot)
            .ok_or(Error::NoSlot)?;
        if *there == stack {
            return Ok(());
        }
        // `None` while the window waits to open: it opens with the new stack.
        let shown = inv.window.as_ref().filter(|w| w.id != 0).map(|w| w.id);
        let net = match shown {
            Some(_) if !stack.is_empty() => inv.new_id(),
            _ => 0,
        };
        let item = stack.to_instance(net);
        if let Some(there) = inv.menu.as_mut().and_then(|m| m.stacks.get_mut(slot)) {
            *there = stack;
        }
        let Some(window_id) = shown else {
            return Ok(());
        };
        if let Some(id) = inv.window.as_mut().and_then(|w| w.ids.get_mut(slot)) {
            *id = StackId { net, request: None };
        }
        let at = u32::try_from(slot).unwrap_or(u32::MAX);
        p.send_packet(slot_packet(u32::from(window_id), at, item));
        Ok(())
    }
}

/// Where a menu's container blocks stand for `player` and what they are: behind the player
/// (`InvMenu`), inside the world height; a double chest's second half beside the first, along
/// the side its facing leaves free.
fn place(player: &Player, kind: Kind) -> [Option<(BlockPos, block::State)>; 2] {
    let eye = player.position;
    let look = survival::look_direction(player);
    let feet_y = eye.y - EYE_HEIGHT;
    let behind = BlockPos::containing(Vec3::new(
        eye.x - look.x * BEHIND_HORIZONTAL,
        feet_y - look.y * BEHIND_VERTICAL,
        eye.z - look.z * BEHIND_HORIZONTAL,
    ));
    let pos = BlockPos::new(
        behind.x,
        behind.y.clamp(world::MIN_Y, world::MAX_Y),
        behind.z,
    );
    match kind {
        Kind::Hopper => [Some((pos, block::Kind::Hopper.default_state())), None],
        Kind::Chest => [Some((pos, block::Kind::Chest.default_state())), None],
        Kind::DoubleChest => {
            let chest = block::Kind::Chest.default_state();
            let along_z = matches!(
                chest.value("minecraft:cardinal_direction"),
                Some(block::PropertyValue::String("east" | "west"))
            );
            let pair = if along_z {
                BlockPos::new(pos.x, pos.y, pos.z.saturating_add(1))
            } else {
                BlockPos::new(pos.x.saturating_add(1), pos.y, pos.z)
            };
            [Some((pos, chest)), Some((pair, chest))]
        }
    }
}

/// The client data of a menu's container blocks: their kind, the title as `CustomName` and a
/// double chest's pair (PMMP `Chest` and `Nameable` spawn data).
fn block_data(
    kind: Kind,
    title: &str,
    blocks: [Option<(BlockPos, block::State)>; 2],
) -> Vec<Packet> {
    let id = match kind {
        Kind::Hopper => "Hopper",
        Kind::Chest | Kind::DoubleChest => "Chest",
    };
    let [first, second] = blocks.map(|b| b.map(|(pos, _)| pos));
    let mut out = Vec::with_capacity(2);
    for (pos, pair) in [(first, second), (second, first)] {
        let Some(pos) = pos else {
            continue;
        };
        let mut nbt = Compound::from([
            ("id".to_owned(), Value::String(id.to_owned())),
            ("x".to_owned(), Value::Int(pos.x)),
            ("y".to_owned(), Value::Int(pos.y)),
            ("z".to_owned(), Value::Int(pos.z)),
        ]);
        if let Some(pair) = pair {
            nbt.insert("pairx".to_owned(), Value::Int(pair.x));
            nbt.insert("pairz".to_owned(), Value::Int(pair.z));
        }
        if !title.is_empty() {
            nbt.insert("CustomName".to_owned(), Value::String(title.to_owned()));
        }
        out.push(Packet::BlockActorData(BlockActorData {
            position: [pos.x, pos.y, pos.z],
            nbt_data: nbt,
        }));
    }
    out
}

/// Whether the menu of a window that waits to open may open now.
pub(super) fn is_ready(player: &Player, tick: u64) -> bool {
    player
        .inventory
        .menu
        .as_ref()
        .is_none_or(|m| m.ready <= tick)
}

/// The menu phase, at the start of the view phase: menus whose blocks the client has had long
/// enough open, and menus their player left more than 8 blocks behind close. It looks only at
/// the players with a handler in [`Handlers`] (every open menu has one), and drops a handler
/// whose player left with its menu open (one an `InventoryClose` listener opened as the player
/// left).
pub(in crate::tick) fn tick(state: &mut State) {
    let tick = state.tick;
    let (mut ready, mut far) = (Vec::new(), Vec::new());
    let State { players, menus, .. } = state;
    menus.0.retain(|session, _| {
        players
            .get(session)
            .is_some_and(|p| p.inventory.menu.is_some())
    });
    for &session in menus.0.keys() {
        let Some(p) = players.get(&session) else {
            continue;
        };
        let (Some(open), Some(window)) = (&p.inventory.menu, &p.inventory.window) else {
            continue;
        };
        let WindowKind::Menu { pos, .. } = window.kind else {
            continue;
        };
        let centre = Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + 0.5,
            f64::from(pos.z) + 0.5,
        );
        if p.position.distance_squared(centre) > MAX_DISTANCE_SQUARED {
            far.push(session);
        } else if window.id == 0 && p.inventory.pending_close.is_none() && open.ready <= tick {
            ready.push(session);
        }
    }
    for session in far {
        close_now(state, session, Notice::Send);
    }
    for session in ready {
        super::announce(state, session);
    }
}

/// A closed menu window: its handler goes (one that is running goes when it returns), and the
/// blocks the menu stood on show again, with their block entity's data.
pub(super) fn closed(state: &mut State, session: SessionId) {
    state.menus.0.remove(&session);
    let Some(p) = state.players.get_mut(&session) else {
        return;
    };
    p.inventory.menu = None;
    let old = p.view.set_menu_blocks([None, None]);
    for (pos, _) in old.into_iter().flatten() {
        view::restore_block_data(state, session, pos);
    }
}

/// What an item stack request does to an open menu.
#[derive(Debug)]
pub(super) enum Touch {
    /// It names no slot of a shown menu.
    Nothing,
    /// It names a menu slot but clicks none: it is refused.
    Refused,
    /// It clicks a menu slot: it is refused, and the handler hears of the click.
    Click(Click),
}

/// What an item stack request of `session` does to its open menu ([`Touch`]). A request that
/// names a menu slot is refused whole.
///
/// The click is the first take (a menu slot as the source), place (as the destination), swap
/// or drop of a menu slot whose stack network ID is the one the client was shown; a destroy or
/// consume of a menu slot clicks nothing.
pub(super) fn touch(state: &State, session: SessionId, request: &ItemStackRequest) -> Touch {
    let Some(inv) = state.players.get(&session).map(|p| &p.inventory) else {
        return Touch::Nothing;
    };
    let (Some(open), Some(window)) = (inv.menu.as_ref(), inv.window.as_ref().filter(|w| w.id != 0))
    else {
        return Touch::Nothing;
    };
    let is_menu =
        |info: &StackRequestSlotInfo| info.container.container_id == ContainerId::LevelEntity as u8;
    let mut touch = Touch::Nothing;
    for action in &request.actions {
        let (info, action) = match action {
            StackRequestAction::Take(TakeStackRequestAction {
                transfer_stack_request_action: t,
            })
            | StackRequestAction::Place(PlaceStackRequestAction {
                transfer_stack_request_action: t,
            }) => {
                if is_menu(&t.source) {
                    (&t.source, Some(Action::Take))
                } else if is_menu(&t.destination) {
                    (&t.destination, Some(Action::Place))
                } else {
                    continue;
                }
            }
            StackRequestAction::Swap(a) if is_menu(&a.source) => (&a.source, Some(Action::Swap)),
            StackRequestAction::Swap(a) if is_menu(&a.destination) => {
                (&a.destination, Some(Action::Swap))
            }
            StackRequestAction::Drop(a) if is_menu(&a.source) => (&a.source, Some(Action::Drop)),
            StackRequestAction::Destroy(a) if is_menu(&a.source) => (&a.source, None),
            StackRequestAction::Consume(a) if is_menu(&a.destroy_stack_request_action.source) => {
                (&a.destroy_stack_request_action.source, None)
            }
            _ => continue,
        };
        if matches!(touch, Touch::Click(_)) {
            continue;
        }
        touch = Touch::Refused;
        let slot = usize::from(info.slot);
        let shown = window
            .ids
            .get(slot)
            .is_some_and(|id| id.net == info.stack_network_id);
        if let (Some(action), Some(item), true, None) = (
            action,
            open.stacks.get(slot),
            shown,
            info.container.dynamic_container_id,
        ) {
            touch = Touch::Click(Click {
                menu: open.id,
                slot,
                item: item.clone(),
                cursor: inv.cursor.stack(0).clone(),
                action,
            });
        }
    }
    touch
}

/// Runs the click handler of the menu `click` names, if it is still open, with the handler out
/// of the state; closes the menu if the handler says so. A handler whose menu closed while it
/// ran (or that opened another) is dropped.
pub(super) fn clicked(state: &mut State, session: SessionId, click: Click) {
    let menu = click.menu;
    let Some((_, mut handler)) = state.menus.0.remove(&session).filter(|(id, _)| *id == menu)
    else {
        return;
    };
    let response = handler(&mut Context { state }, player::Id::new(session), click);
    let open = state
        .players
        .get(&session)
        .and_then(|p| p.inventory.menu.as_ref())
        .is_some_and(|m| m.id == menu);
    if !open {
        return;
    }
    state.menus.0.insert(session, (menu, handler));
    if response == Response::Close {
        close_now(state, session, Notice::Send);
    }
}
