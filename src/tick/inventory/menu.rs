//! Menu windows: a chest, double chest or hopper screen whose items are the server's, opened on
//! fake container blocks behind the player.
//!
//! Opening shows the container blocks to the player alone (`view`'s menu blocks, over its other
//! fake blocks) with their client data (the title as `CustomName`, a double chest's pair), and
//! the window opens [`OPEN_DELAY_TICKS`] ticks later, once the client has the blocks. Any item stack
//! request that names a menu slot that is not editable is refused whole and the player's screen
//! is sent again; the request's first take, place, swap or drop of such a slot becomes one
//! [`Click`] for the handler. A request that names only editable slots and only takes, places,
//! swaps, drops and destroys (creative mode) runs as in a chest window ([`super::request`]), with
//! the handler as its last listener: it hears the changes ([`crate::menu::Change`]) after the
//! request's events, and the request is checked again and made (a drop thrown) only if it keeps
//! them. The handler runs in the input phase with the handler taken out of the state
//! ([`Handlers`]). A repeated request ID is refused before, so it clicks or changes once. A menu
//! that lends has the request executor count the items its slots move onto the cursor as lent
//! (`Inventory::lent`), as [`crate::menu::Menu::lending`] says; while the cursor holds some, every
//! request runs so, and its handler hears those that change them or the cursor.
//!
//! The menu closes as any window does (the client closing it, a server close, another window,
//! leaving, dying, a teleport or another world) and when the player is more than 8 blocks from
//! it; closing drops the menu's items (a copy of what its handler kept), shows the blocks it
//! stood on again, hands the handler the cursor's stack ([`crate::menu::Close`], but for a direct
//! call's close) and gives it back but its lent items unless the handler took it. A menu's close
//! is heard by its handler alone, never as `InventoryClose`; a change raises the request's events,
//! the menu's slots as the window's.

use std::collections::BTreeMap;
use std::fmt;

use super::{
    Notice, StackId, Window, WindowKind, announce, close_now, id_for, own_part, slot_packet,
};
use crate::block;
use crate::item::Stack;
use crate::math::Vec3;
use crate::menu::{Click, Error, Id, Input, Kind, Menu, Response, Result, SlotAction};
use crate::nbt::{Compound, Value};
use crate::player;
use crate::protocol::consts::ContainerId;
use crate::protocol::packet::{BlockActorData, Packet};
use crate::protocol::{
    ItemStackRequest, PlaceStackRequestAction, StackRequestAction, StackRequestSlotInfo,
    TakeStackRequestAction,
};
use crate::tick::checkpoint;
use crate::tick::entity::Raise;
use crate::tick::{Context, Phase, Player, SessionId, State, survival, view};
use crate::world::{self, BlockPos};

/// A menu's place behind the player: the look direction times -(1 + width) horizontally and
/// -(1 + height) vertically, from the feet (player size 0.6 × 1.8).
const BEHIND_HORIZONTAL: f64 = 1.6;
/// The vertical offset factor of the same place (see [`BEHIND_HORIZONTAL`]).
const BEHIND_VERTICAL: f64 = 2.8;
/// A menu closes when its player's eyes are farther than this from its block's centre (8 blocks).
const MAX_DISTANCE_SQUARED: f64 = 64.0;
/// Ticks between showing a menu's blocks and opening its window, so the client has the container
/// blocks and their data when the window opens. The delay the official client needs is not
/// verified.
const OPEN_DELAY_TICKS: u64 = 2;

/// What runs when the player clicks a menu slot or changes its editable slots, and when the menu
/// closes.
type Handler = Box<dyn FnMut(&mut Context<'_>, player::Id, Input) -> Response + Send>;

/// The handlers of the open menus, by session: one per player with a menu open, kept in
/// the state beside the players (the output phase shares the players between threads, and a
/// handler need not be `Sync`). A handler is out of the table while it runs.
#[derive(Default)]
pub(in crate::tick) struct Handlers(BTreeMap<SessionId, (Id, Handler)>);

impl Handlers {
    /// How many handlers wait.
    pub(in crate::tick) fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Handlers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(s, (id, _))| (s.get(), id)))
            .finish()
    }
}

/// An open menu of a player; its window is the player's window ([`WindowKind::Menu`]) and its
/// handler is in [`Handlers`].
#[derive(Debug)]
pub(in crate::tick) struct Open {
    id: Id,
    /// The items, one per slot.
    pub(super) stacks: Vec<Stack>,
    /// The editable slots, bit `i` for slot `i`.
    editable: u64,
    /// Whether it lends the items taken onto the cursor ([`Menu::lending`]).
    lends: bool,
    /// The tick from which the window may open.
    ready: u64,
}

impl Open {
    /// Feeds the menu into the state hash of a test.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            id,
            stacks,
            editable,
            lends,
            ready,
        } = self;
        h.u64(u64::from(id.get()));
        h.usize(stacks.len());
        for s in stacks {
            super::hash_stack(h, s);
        }
        h.u64(*editable);
        h.u64(u64::from(*lends));
        h.u64(*ready);
    }

    /// The menu's ID.
    pub(super) fn id(&self) -> Id {
        self.id
    }

    /// Whether requests move the items of slot `slot`.
    pub(super) fn is_editable(&self, slot: usize) -> bool {
        crate::menu::slot_bit(slot).is_some_and(|bit| self.editable & bit != 0)
    }

    /// Whether it lends the items taken onto the cursor ([`Menu::lending`]).
    pub(super) fn is_lending(&self) -> bool {
        self.lends
    }
}

checkpoint::fields!(Open {
    id,
    stacks,
    editable,
    lends,
    ready
});

/// What an item stack request does to an open menu.
#[derive(Debug)]
pub(super) enum Touch {
    /// It names no slot of a shown menu.
    Nothing,
    /// It only takes, places, swaps, drops and destroys, and names editable slots of the menu but
    /// no other, or it names no menu slot while the cursor holds lent items: it runs, and the
    /// handler hears of its changes.
    Edit(Id),
    /// It names a menu slot but clicks none, or does more than take, place, swap, drop and
    /// destroy with an editable slot: it is refused.
    Refused,
    /// It clicks a slot that is not editable: it is refused, and the handler hears of the click.
    Click(Click),
}

impl Context<'_> {
    /// The menu `player` has open, if any.
    pub fn menu(&self, player: player::Id) -> Option<Id> {
        let p = self.state.players.get(&player.session())?;
        p.inventory.menu.as_ref().map(|m| m.id)
    }

    /// The items of the menu `player` has open, one per slot, as the player's changes and
    /// [`set_menu_item`](Self::set_menu_item) left them.
    pub fn menu_items(&self, player: player::Id) -> Option<&[Stack]> {
        let p = self.state.players.get(&player.session())?;
        p.inventory.menu.as_ref().map(|m| m.stacks.as_slice())
    }

    /// How many of the items on the cursor of `player` the menu it has open lends
    /// ([`Menu::lending`]): the handler's, though the cursor's stack holds them; 0 for a menu that
    /// does not lend.
    pub fn menu_lent(&self, player: player::Id) -> Option<u8> {
        let p = self.state.players.get(&player.session())?;
        p.inventory.menu.as_ref().map(|_| p.inventory.lent)
    }

    /// Opens `menu` for `player`: a chest, double chest or hopper screen whose items the player
    /// cannot take, place or drop but in its editable slots. Each click on another slot is
    /// refused (the player's screen is put back) and calls `handler` once, in the tick, with what
    /// the player tried ([`Input::Click`]). Each request that moves items into, out of (a drop
    /// throws them, a creative destroy deletes them) or between editable slots is checked as in a
    /// chest window, then calls `handler` once with its changes ([`Input::Change`]) before they
    /// are made. The handler's [`Response`] makes a change and keeps the menu open or closes it,
    /// or refuses the change. A window the player had open closes first, as
    /// [`close_window`](Self::close_window) closes it.
    ///
    /// The screen stands on container blocks shown to this player alone, behind it, and opens a
    /// few ticks later, once the client has them. It closes as any
    /// window does ([`close_window`](Self::close_window) closes it from the server) and when the
    /// player gets more than 8 blocks away; the blocks it stood on show again then, and the
    /// handler hears of the close ([`Input::Close`]) unless a direct call closed it. Neither
    /// opening nor closing raises an event.
    ///
    /// # Errors
    ///
    /// Opens nothing with:
    /// [`Error::NoSlot`] for a menu with a slot past its end.
    /// [`Error::TitleTooLong`] for a title over 1,024 bytes.
    /// [`Error::Offline`] if the player is not in the game or its client has no view yet.
    /// [`Error::Busy`] if it waits with so many stacks for room that a window's items could not
    /// go back, or this tick's changes of what it is shown leave no room for the menu's blocks.
    /// [`Error::Exhausted`] if it was given 2^32 menus already, as menu IDs do not wrap.
    /// [`Error::OutsideWorld`] if the menu's blocks cannot stand within 8 blocks of its eyes,
    /// the world's height being in the way.
    #[doc(alias = "Player::setCurrentWindow")]
    pub fn open_menu(
        &mut self,
        player: player::Id,
        menu: Menu,
        handler: impl FnMut(&mut Context<'_>, player::Id, Input) -> Response + Send + 'static,
    ) -> Result<Id> {
        let (kind, title, stacks, editable, lends) = menu.prepare()?;
        let session = player.session();
        let tick = self.state.tick;
        let p = viewing_player_mut(self.state, player).ok_or(Error::Offline)?;
        // The old menu's two positions and the new menu's two.
        if !p.inventory.has_room_to_open() || !p.view.has_room_to_show(4) {
            return Err(Error::Busy);
        }
        let n = u32::try_from(p.inventory.next_menu).map_err(|_| Error::Exhausted)?;
        let (first, second) = place(p, kind);
        if is_far(p.position, first.0) {
            return Err(Error::OutsideWorld);
        }
        if p.inventory.window.is_some() {
            close_now(self.state, session, Notice::Send, Raise::Quiet);
        }
        let p = viewing_player_mut(self.state, player).ok_or(Error::Offline)?;
        p.inventory.next_menu = p.inventory.next_menu.saturating_add(1);
        let id = Id::new(n);
        // The old menu, if any, closed above: no menu blocks were shown.
        let _none = p.view.set_menu_blocks([Some(first), second]);
        let pos = first.0;
        for packet in block_data(kind, &title, pos, second.map(|(pos, _)| pos)) {
            p.send_packet(packet);
        }
        p.inventory.window = Some(Window::new(WindowKind::Menu { pos, kind }, 0));
        p.inventory.menu = Some(Open {
            id,
            stacks,
            editable,
            lends,
            ready: tick.saturating_add(OPEN_DELAY_TICKS),
        });
        self.state.menus.0.insert(session, (id, Box::new(handler)));
        Ok(id)
    }

    /// Puts `stack` into `slot` of the menu `player` has open (an empty stack clears it) and
    /// shows it to the player.
    ///
    /// # Errors
    ///
    /// Changes nothing with:
    /// [`Error::Offline`] if the player has left.
    /// [`Error::NotOpen`] if the player has no menu open.
    /// [`Error::NoSlot`] if the menu has no such slot.
    pub fn set_menu_item(&mut self, player: player::Id, slot: usize, stack: Stack) -> Result<()> {
        let p = self
            .state
            .players
            .get_mut(&player.session())
            .ok_or(Error::Offline)?;
        let inv = &mut p.inventory;
        // Numbered as it goes: the slot's packet index without a conversion that could fail.
        let (there, at) = inv
            .menu
            .as_mut()
            .ok_or(Error::NotOpen)?
            .stacks
            .iter_mut()
            .zip(0_u32..)
            .nth(slot)
            .ok_or(Error::NoSlot)?;
        if *there == stack {
            return Ok(());
        }
        // `None` while the window waits to open: it opens with the new stack.
        let Some(window_id) = inv.window.as_ref().filter(|w| w.id != 0).map(|w| w.id) else {
            *there = stack;
            return Ok(());
        };
        let net = id_for(&mut inv.next_id, &stack);
        let item = stack.to_instance(net);
        *there = stack;
        if let Some(id) = inv.window.as_mut().and_then(|w| w.ids.get_mut(slot)) {
            *id = StackId { net, request: None };
        }
        p.send_packet(slot_packet(u32::from(window_id), at, item));
        Ok(())
    }
}

/// The player of `id` if it is in the game (its saved data has loaded) and its client has a view.
fn viewing_player_mut(state: &mut State, id: player::Id) -> Option<&mut Player> {
    state
        .players
        .get_mut(&id.session())
        .filter(|p| p.phase != Phase::Loading && p.view.is_open())
}

/// Where a menu's container blocks stand for `player` and what they are: behind the player,
/// inside the world height; a double chest's second half beside the first, along the side its
/// facing leaves free.
fn place(
    player: &Player,
    kind: Kind,
) -> ((BlockPos, block::State), Option<(BlockPos, block::State)>) {
    let eye = player.position;
    let look = survival::look_direction(player);
    let feet_y = survival::feet(player).y;
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
        Kind::Hopper => ((pos, block::Kind::Hopper.default_state()), None),
        Kind::Chest => ((pos, block::Kind::Chest.default_state()), None),
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
            ((pos, chest), Some((pair, chest)))
        }
    }
}

/// The client data of a menu's container blocks: their kind, the title as `CustomName` and a
/// double chest's pair.
fn block_data(kind: Kind, title: &str, first: BlockPos, second: Option<BlockPos>) -> Vec<Packet> {
    let id = match kind {
        Kind::Hopper => "Hopper",
        Kind::Chest | Kind::DoubleChest => "Chest",
    };
    let mut out = Vec::with_capacity(2);
    let pairs = std::iter::once((first, second)).chain(second.map(|pos| (pos, Some(first))));
    for (pos, pair) in pairs {
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
    menus.0.retain(|&session, _| {
        let Some(p) = players.get(&session) else {
            return false;
        };
        let Some(open) = &p.inventory.menu else {
            return false;
        };
        let Some(window) = &p.inventory.window else {
            return true;
        };
        let WindowKind::Menu { pos, .. } = window.kind else {
            return true;
        };
        if is_far(p.position, pos) {
            far.push((session, open.id));
        } else if window.id == 0 && p.inventory.pending_close.is_none() && open.ready <= tick {
            ready.push(session);
        }
        true
    });
    for (session, menu) in far {
        // An earlier close's handler may have closed or replaced this menu (with one beside the
        // player); a menu still open is still far, as a handler's teleport waits for the marks.
        if is_open(state, session, menu) {
            close_now(state, session, Notice::Send, Raise::Events);
        }
    }
    for session in ready {
        announce(state, session);
    }
}

/// Whether a menu whose first block stands at `pos` is out of reach of a player whose eyes are
/// at `eye`: farther than [`MAX_DISTANCE_SQUARED`] from the block's centre.
fn is_far(eye: Vec3, pos: BlockPos) -> bool {
    eye.distance_squared(pos.to_center()) > MAX_DISTANCE_SQUARED
}

/// A closed menu window: the blocks the menu stood on show again, with their block entity's data,
/// and its handler goes (one that is running, whose own call closed it, goes when it returns).
/// Unless a direct call closed it (`raise`), the handler hears the close first with `cursor`, the
/// stack taken off the player's cursor, of which `lent` items are lent, and whether the player
/// closed it (`notice`). Gives
/// what goes back to the player: `cursor` but its lent items, or nothing if the handler took it.
pub(super) fn closed(
    state: &mut State,
    session: SessionId,
    cursor: Stack,
    lent: u8,
    notice: Notice,
    raise: Raise,
) -> Stack {
    let own = own_part(&cursor, lent);
    let handler = state.menus.0.remove(&session);
    #[expect(
        clippy::expect_used,
        reason = "invariant: close_now found the player, and players leave only at the tick's own places"
    )]
    let p = state
        .players
        .get_mut(&session)
        .expect("a closing menu's player is in the game");
    p.inventory.menu = None;
    let old = p.view.set_menu_blocks([None, None]);
    for (pos, _) in old.into_iter().flatten() {
        view::restore_block_data(state, session, pos);
    }
    let (Raise::Events, Some((menu, mut handler))) = (raise, handler) else {
        return own;
    };
    let close = Input::Close(crate::menu::Close {
        menu,
        cursor,
        lent,
        // Only the client closes a menu without a notice.
        by_player: notice == Notice::Skip,
    });
    match handler(&mut Context { state }, player::Id::new(session), close) {
        Response::Refuse => Stack::EMPTY,
        Response::Keep | Response::Close => own,
    }
}

/// What an item stack request of `session` does to its open menu ([`Touch`]). A request that
/// names a slot that is not editable is refused whole.
///
/// The click is the first take (such a slot as the source), place (as the destination), swap
/// or drop of a slot that is not editable whose stack network ID is the one the client was
/// shown; a destroy or consume of a menu slot clicks nothing.
pub(super) fn touch(state: &State, session: SessionId, request: &ItemStackRequest) -> Touch {
    let Some(inv) = state.players.get(&session).map(|p| &p.inventory) else {
        return Touch::Nothing;
    };
    let (Some(open), Some(window)) = (inv.menu.as_ref(), inv.window.as_ref().filter(|w| w.id != 0))
    else {
        return Touch::Nothing;
    };
    let mut touch = Touch::Nothing;
    // Whether it names an editable slot, and whether each action only moves items (a drop or a
    // creative destroy moves them out, as from a chest).
    let (mut edits, mut moves_only) = (false, true);
    for action in &request.actions {
        let (named, moves) = match action {
            StackRequestAction::Take(TakeStackRequestAction {
                transfer_stack_request_action: t,
            })
            | StackRequestAction::Place(PlaceStackRequestAction {
                transfer_stack_request_action: t,
            }) => (
                [
                    menu(&t.source, Some(SlotAction::Take)),
                    menu(&t.destination, Some(SlotAction::Place)),
                ],
                true,
            ),
            StackRequestAction::Swap(a) => (
                [
                    menu(&a.source, Some(SlotAction::Swap)),
                    menu(&a.destination, Some(SlotAction::Swap)),
                ],
                true,
            ),
            StackRequestAction::Drop(a) => ([menu(&a.source, Some(SlotAction::Drop)), None], true),
            StackRequestAction::Destroy(a) => ([menu(&a.source, None), None], true),
            StackRequestAction::Consume(a) => (
                [menu(&a.destroy_stack_request_action.source, None), None],
                false,
            ),
            _ => ([None, None], false),
        };
        moves_only &= moves;
        for (info, action) in named.into_iter().flatten() {
            let slot = usize::from(info.slot);
            let editable = open.is_editable(slot);
            let edit = editable && moves;
            edits |= edit;
            if edit || matches!(touch, Touch::Click(_)) {
                continue;
            }
            touch = Touch::Refused;
            if editable {
                continue;
            }
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
    }
    match touch {
        Touch::Nothing if edits && moves_only => Touch::Edit(open.id),
        Touch::Nothing if edits => Touch::Refused,
        // The handler hears it if it changes the cursor that holds the lent items.
        Touch::Nothing if inv.lent > 0 => Touch::Edit(open.id),
        touch => touch,
    }
}

/// `info` with the click `action` on it, if it names a menu slot.
fn menu(
    info: &StackRequestSlotInfo,
    action: Option<SlotAction>,
) -> Option<(&StackRequestSlotInfo, Option<SlotAction>)> {
    (info.container.container_id == ContainerId::LevelEntity as u8).then_some((info, action))
}

/// Runs the handler of the menu `click` names, if it is still open; closes the menu if the
/// handler says so.
pub(super) fn clicked(state: &mut State, session: SessionId, click: Click) {
    let menu = click.menu;
    if call(state, session, menu, Input::Click(click)) == Some(Response::Close)
        && is_open(state, session, menu)
    {
        close_now(state, session, Notice::Send, Raise::Events);
    }
}

/// Runs the handler of the menu `menu` of `session` on `input`, with the handler out of the
/// state, and gives its answer; `None` if the menu has no handler (it closed). A handler whose
/// menu closed while it ran (or that opened another) is dropped.
pub(super) fn call(
    state: &mut State,
    session: SessionId,
    menu: Id,
    input: Input,
) -> Option<Response> {
    let (_, mut handler) = state
        .menus
        .0
        .remove(&session)
        .filter(|(id, _)| *id == menu)?;
    let response = handler(&mut Context { state }, player::Id::new(session), input);
    if is_open(state, session, menu) {
        state.menus.0.insert(session, (menu, handler));
    }
    Some(response)
}

/// Whether `session` has the menu `menu` open.
pub(super) fn is_open(state: &State, session: SessionId, menu: Id) -> bool {
    state
        .players
        .get(&session)
        .and_then(|p| p.inventory.menu.as_ref())
        .is_some_and(|m| m.id == menu)
}
