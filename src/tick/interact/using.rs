//! Using the held item on blocks, in the air and on entities.
//!
//! The clicks on blocks that stay are the storage blocks' windows, doors, trapdoors, fence gates
//! and signs; every other block ignores a click. The items used on a block are spawn eggs and
//! firework rockets; thrown items, bows, tridents, firework flights and entity attacks go to the
//! entity rules.

use tracing::debug;

use super::{
    Actor, Source, USE, adventure_allows, block_vec, center, place, props, return_items, stream,
};
use crate::block::{self, Class, Face, Kind, State as BlockState};
use crate::config::GameMode;
use crate::event::{
    BlockPlace, InteractAction, PlayerEntityInteract, PlayerInteract, PlayerItemUse,
};
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::player;
use crate::protocol::consts::UseItemOnEntityAction;
use crate::random::Random;
use crate::save::WorldIndex;
use crate::tick::{
    SessionId, State, block_entity, dispatch, emit, entity, inventory, survival, trade,
};
use crate::world::{BlockPos, MAX_Y, MIN_Y, Sound};

/// The result of an item's own use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Not the item's kind of use: placement comes next.
    None,
    /// Used.
    Success,
    /// Refused.
    Fail,
}

/// A use of the held item on a block. The rules change the item and the caller applies it
/// afterwards, then plays the use's sound.
pub(super) struct Use {
    pub(super) actor: Actor,
    pub(super) pos: BlockPos,
    pub(super) face: Face,
    /// Where on the face the click was, 0 to 1 on each axis.
    pub(super) click: Vec3,
    /// The clicked block.
    pub(super) block: BlockState,
    /// The held item as the rules change it.
    pub(super) item: Stack,
    /// The sound the use plays once it is applied with the held item, so that a
    /// [`WorldSound`](crate::event::WorldSound) listener never sees it half done.
    sound: Option<(Vec3, Sound)>,
    pub(super) rng: Random,
    /// Whether user code (a custom block's hook) ran in the block's own use: a use that goes on to
    /// the item and placement checks the click again.
    ran_user_code: bool,
    /// Where the click comes from.
    pub(super) source: Source,
}

impl Use {
    /// The block next to the clicked face.
    fn replace(&self) -> Option<BlockPos> {
        self.pos.step(self.face)
    }

    /// Plays `sound` at `at` once the use is applied ([`finish`]).
    fn play(&mut self, at: Vec3, sound: Sound) {
        self.sound = Some((at, sound));
    }

    /// Takes one item.
    fn pop(&mut self) {
        self.item = self.item.with_count(self.item.count().saturating_sub(1));
    }
}

/// Whether, after listeners ran, the player is still in the world of `actor`, not a
/// spectator, and holds `held`, so that a use planned before them can go on (a listener that took
/// the item or moved the player away cancels it).
pub(super) fn still_holds(state: &State, actor: &Actor, held: &Stack) -> bool {
    super::actor(state, actor.session).is_some_and(|a| {
        a.world == actor.world
            && a.mode != GameMode::Spectator
            && state
                .players
                .get(&actor.session)
                .is_some_and(|p| p.inventory.held().same(held))
    })
}

/// Sets a block in a world (a missing world or chunk changes nothing).
pub(super) fn set(state: &mut State, world: WorldIndex, pos: BlockPos, block: BlockState) {
    if let Some(w) = state.worlds.get_mut(world)
        && let Err(e) = w.set_block(pos, block)
    {
        debug!(
            ?pos,
            error = &e as &(dyn std::error::Error + 'static),
            "block not set"
        );
    }
}

/// The block at `pos` (air where nothing is loaded).
fn get(state: &State, world: WorldIndex, pos: Option<BlockPos>) -> BlockState {
    state
        .worlds
        .get(world)
        .map_or(block::AIR, |w| place::block_at(w, pos))
}

/// Uses the held item on a block: the clicked block's interaction, the item's own use, then
/// placement. False when nothing happened (the caller resyncs).
pub(super) fn use_item_on(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
    click: Vec3,
    source: Source,
) -> bool {
    let Some((clicked, held)) = right_click(state, actor, pos, face) else {
        return false;
    };
    // Marked whether or not a listener hears the event, so that listeners change no state.
    let first = source == Source::Action
        || state
            .players
            .get_mut(&actor.session)
            .is_some_and(|p| p.breaking.first_of_press());
    // Sneaking uses neither the block (unless the hand is empty) nor the item.
    let mut use_block = !actor.sneak_pressed || held.is_empty();
    let mut use_item = !actor.sneak_pressed;
    let mut actor = *actor;
    let mut event = None;
    if dispatch::heard::<PlayerInteract>(state) {
        let world = crate::world::Id::of(actor.world);
        let mut e = PlayerInteract {
            player: player::Id::new(actor.session),
            world,
            pos,
            block: clicked,
            face,
            item: held.clone(),
            touch_vector: click,
            action: InteractAction::RightClickBlock,
            first,
            use_item,
            use_block,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        if dispatch::listened::<PlayerInteract>(state) {
            // A listener may have moved the player, changed the block or the held item.
            let Some(again) = click_again(state, &actor, pos, face, clicked, &held) else {
                return false;
            };
            actor = again;
        }
        use_block = e.use_block;
        use_item = e.use_item;
        event = Some(e);
    }
    let mut u = Use {
        actor,
        pos,
        face,
        click,
        block: clicked,
        item: held.clone(),
        sound: None,
        rng: stream(state, actor.world, USE, pos),
        ran_user_code: false,
        source,
    };
    let done = if use_block && interact(state, &mut u) {
        finish(state, u, &held);
        true
    } else {
        if u.ran_user_code {
            // A custom block's hook may have moved the player, changed the block or the held
            // item: the item's use and placement go on only from the same click.
            let Some(again) = click_again(state, &u.actor, pos, face, clicked, &held) else {
                return false;
            };
            u.actor = again;
        }
        match use_item.then(|| item_use(state, &mut u)) {
            Some(Outcome::Success) => {
                finish(state, u, &held);
                true
            }
            Some(Outcome::Fail) => false,
            Some(Outcome::None) | None => place_held(state, &u.actor, &held, pos, face, click),
        }
    };
    if let Some(e) = event.filter(|_| done) {
        dispatch::watch(state, &e);
    }
    done
}

/// Checks a right click: the clicked block (not air) and the one next to its face are loaded and
/// in the world, and the player is not a spectator. The clicked block and the held item if so.
fn right_click(
    state: &State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
) -> Option<(BlockState, Stack)> {
    let w = state.worlds.get(actor.world)?;
    let clicked = w.block(pos)?;
    let replace = pos.step(face).filter(|p| (MIN_Y..=MAX_Y).contains(&p.y))?;
    if w.block(replace).is_none() || clicked == block::AIR || actor.mode == GameMode::Spectator {
        return None;
    }
    let held = state.players.get(&actor.session)?.inventory.held().clone();
    Some((clicked, held))
}

/// Finds the player of `actor` again after user code ran in its click at `pos`: in the same
/// world, within reach, with the block it clicked (`clicked`) and the item it held (`held`) as
/// they were.
fn click_again(
    state: &State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
    clicked: BlockState,
    held: &Stack,
) -> Option<Actor> {
    super::actor(state, actor.session).filter(|a| {
        a.world == actor.world
            && a.can_interact(center(pos), a.reach())
            && right_click(state, a, pos, face).is_some_and(|(c, h)| c == clicked && h.same(held))
    })
}

/// Applies a use that worked: the held item, then its sound.
fn finish(state: &mut State, u: Use, held: &Stack) {
    let Use {
        actor, item, sound, ..
    } = u;
    return_items(state, actor.session, held, item, Vec::new());
    if let Some((at, sound)) = sound {
        emit::sound(state, actor.world, at, sound);
    }
}

/// Places the held block: the blocks and adventure checks ([`placement`]), [`BlockPlace`], the
/// change, block entities, the sign editor, rail joining, one item used, then the sound and the
/// custom blocks' `on_place`.
fn place_held(
    state: &mut State,
    actor: &Actor,
    held: &Stack,
    pos: BlockPos,
    face: Face,
    click: Vec3,
) -> bool {
    let world = actor.world;
    let Some(blocks) = placement(state, actor, held, pos, face, click) else {
        return false;
    };
    let mut event = None;
    if dispatch::heard::<BlockPlace>(state) {
        let mut e = BlockPlace {
            player: player::Id::new(actor.session),
            world: crate::world::Id::of(world),
            blocks: blocks.to_vec(),
            block_against: pos,
            item: held.clone(),
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        // A listener may have changed the world, moved the player or taken the item: the
        // placement must come out the same.
        if dispatch::listened::<BlockPlace>(state)
            && !super::actor(state, actor.session).is_some_and(|a| {
                still_holds(state, actor, held)
                    && a.can_interact(center(pos), a.reach())
                    && placement(state, &a, held, pos, face, click).as_ref() == Some(&blocks)
            })
        {
            return false;
        }
        event = Some(e);
    }
    for (p, s) in &blocks {
        set(state, world, *p, *s);
    }
    for (p, s) in &blocks {
        block_entity::placed(state, world, *p, Some(actor.session), held, face);
        // The placer writes the new sign.
        if s.is(Class::BaseSign) {
            super::open_sign_editor(state, actor.session, world, *p, true);
        }
        // A rail joins the rails next to it.
        if s.is(Class::BaseRail) {
            let joined = state
                .worlds
                .get(world)
                .map(|w| super::rail::reconnect(w, *p))
                .unwrap_or_default();
            for (q, r) in joined {
                set(state, world, q, r);
            }
        }
    }
    let used = held.with_count(held.count().saturating_sub(1));
    return_items(state, actor.session, held, used, Vec::new());
    if let Some((first, s)) = blocks.first() {
        emit::sound(state, world, center(*first), Sound::BlockPlace(*s));
    }
    for (p, s) in &blocks {
        if s.custom_block().is_some() {
            super::custom::placed(state, actor.session, world, *p, *s);
        }
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Works out the blocks placing `held` against the face `face` of the block at `pos` sets, if
/// adventure mode allows it and at least one block changes. The place is not checked: the game
/// chose it.
fn placement(
    state: &State,
    actor: &Actor,
    held: &Stack,
    pos: BlockPos,
    face: Face,
    click: Vec3,
) -> Option<place::Blocks> {
    let world = actor.world;
    let item = held.item()?;
    let w = state.worlds.get(world)?;
    let clicked = w.block(pos)?;
    if actor.mode == GameMode::Adventure && !adventure_allows(held, "CanPlaceOn", clicked.name()) {
        return None;
    }
    let blocks = place::placement(w, item, pos, face, click, Some(&actor.placer()))?;
    let changes = blocks.iter().any(|(p, s)| w.block(*p) != Some(*s));
    changes.then_some(blocks)
}

/// Opens the window of the clicked block ([`inventory::open_block`]); a client tries once a tick
/// at most ([`inventory::first_client_open_this_tick`]).
fn open_window(state: &mut State, u: &Use) {
    if u.source == Source::Client && !inventory::first_client_open_this_tick(state, u.actor.session)
    {
        return;
    }
    let _ = inventory::open_block(state, u.actor.session, u.actor.world, u.pos);
}

/// Uses the clicked block: true when the block did something. A custom block has its own use;
/// of the other blocks only the storage blocks, doors, trapdoors, fence gates and signs answer.
fn interact(state: &mut State, u: &mut Use) -> bool {
    let block = u.block;
    if block.custom_block().is_some() {
        u.ran_user_code = true;
        return super::custom::used(state, u.actor.session, u.actor.world, u.pos, block);
    }
    // Only code opens these two; the client is sent the real blocks again, which undoes any
    // opening it drew itself.
    if matches!(block.kind(), Kind::IronDoor | Kind::IronTrapdoor) {
        return false;
    }
    for class in block.kind().classes() {
        match class {
            Class::Barrel | Class::Chest | Class::EnderChest | Class::ShulkerBox => {
                open_window(state, u);
                return true;
            }
            Class::Door => return door(state, u),
            Class::FenceGate => return fence_gate(state, u),
            Class::BaseSign => return super::decor::sign(state, u),
            Class::Trapdoor => {
                let open = props::flag(block, "open_bit") != Some(true);
                if let Some(s) = props::with_flag(block, "open_bit", open) {
                    set(state, u.actor.world, u.pos, s);
                }
                u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Opens or closes both halves of a door.
fn door(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let open = props::flag(u.block, "open_bit") != Some(true);
    let top = props::flag(u.block, "upper_block_bit") == Some(true);
    let other_pos = u.pos.step(if top { Face::Down } else { Face::Up });
    let other = get(state, world, other_pos);
    if let Some(p) = other_pos
        && other.kind() == u.block.kind()
        && let Some(s) = props::with_flag(other, "open_bit", open)
    {
        set(state, world, p, s);
    }
    if let Some(s) = props::with_flag(u.block, "open_bit", open) {
        set(state, world, u.pos, s);
    }
    u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
    true
}

/// Opens a fence gate away from the player, or closes it.
fn fence_gate(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let open = props::flag(u.block, "open_bit") != Some(true);
    let mut next = props::with_flag(u.block, "open_bit", open).unwrap_or(u.block);
    if open {
        let player = props::horizontal_facing(u.actor.yaw);
        if props::facing(u.block) == Some(player.opposite()) {
            next = props::with_facing(next, player).unwrap_or(next);
        }
    }
    set(state, world, u.pos, next);
    u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
    true
}

/// Which side of a sign the player at `feet` faces, by the sign's facing direction: true for the
/// front.
pub(super) fn sign_front(block: BlockState, feet: Vec3, pos: BlockPos) -> bool {
    let facing = if let Some(rotation) = props::int(block, props::ROTATION)
        .filter(|_| props::facing(block).is_none() || block.is(Class::CeilingCenterHangingSign))
    {
        // Sixteenths of a turn from south, clockwise seen from above.
        let angle = crate::math::radians(f64::from(rotation) * 22.5);
        Vec3::new(-crate::math::sin(angle), 0.0, crate::math::cos(angle))
    } else {
        let (dx, _, dz) = props::facing(block).unwrap_or(Face::South).offset();
        Vec3::new(f64::from(dx), 0.0, f64::from(dz))
    };
    let to_player = feet - center(pos);
    facing.x * to_player.x + facing.z * to_player.z >= 0.0
}

/// Makes the entity of an item that makes one, and uses one up (the entity rules). A spawn egg's
/// entity stands in the middle of the block next to the clicked face, turned at random.
fn item_use(state: &mut State, u: &mut Use) -> Outcome {
    let world = u.actor.world;
    let Some(item) = u.item.item() else {
        return Outcome::None;
    };
    let placed = if item.is(item::Class::SpawnEgg) {
        let Some(at) = u.replace().map(|p| block_vec(p) + Vec3::new(0.5, 0.0, 0.5)) else {
            return Outcome::None;
        };
        let yaw = u.rng.unit() * 360.0;
        entity::hatch(state, world, at, yaw, &u.item)
    } else if item.is(item::Class::FireworkRocket) {
        // The clicked point, 0.15 out of the clicked face.
        let (dx, dy, dz) = u.face.offset();
        let out = Vec3::new(f64::from(dx), f64::from(dy), f64::from(dz)) * 0.15;
        let at = block_vec(u.pos) + u.click + out;
        if entity::launch_firework(state, u.actor.session, world, at, &u.item, &mut u.rng) {
            entity::Placed::Made
        } else {
            entity::Placed::Full
        }
    } else {
        return Outcome::None;
    };
    match placed {
        entity::Placed::Nothing => Outcome::None,
        entity::Placed::Made => {
            u.pop();
            Outcome::Success
        }
        entity::Placed::Full => Outcome::Fail,
    }
}

/// Uses the held item in the air: refused while its cooldown runs; a consumable starts being
/// eaten or drunk (the survival rules'); any other use that works starts the cooldown again
/// (ender pearls, goat horns).
pub(super) fn use_held_item(state: &mut State, actor: &Actor) -> bool {
    let Some(held) = usable(state, actor) else {
        return false;
    };
    let mut actor = *actor;
    let mut event = None;
    if dispatch::heard::<PlayerItemUse>(state) {
        let mut e = PlayerItemUse {
            player: player::Id::new(actor.session),
            item: held.clone(),
            direction_vector: actor.look,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        if dispatch::listened::<PlayerItemUse>(state) {
            // A listener may have taken the item, moved the player or started its cooldown.
            let again = super::actor(state, actor.session)
                .filter(|a| a.world == actor.world && usable(state, a).as_ref() == Some(&held));
            let Some(again) = again else {
                return false;
            };
            actor = again;
        }
        event = Some(e);
    }
    let Some(item) = held.item() else {
        return false;
    };
    // A consumable has no air use or cooldown of its own: eating starts it.
    if !survival::start_eating(state, actor.session) {
        if !click_air(state, &actor, &held, item) {
            return false;
        }
        survival::reset_item_cooldown(state, actor.session, item);
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// The held item, if the player may use it in the air now: not a spectator, the item's cooldown
/// over.
fn usable(state: &State, actor: &Actor) -> Option<Stack> {
    if actor.mode == GameMode::Spectator {
        return None;
    }
    let player = state.players.get(&actor.session)?;
    let held = player.inventory.held();
    let item = held.item()?;
    (!survival::has_item_cooldown(state, player, item)).then(|| held.clone())
}

/// Applies the held item's own use in the air: armor goes on, a goat horn sounds, a projectile is
/// thrown (one used up with finite resources), a bow starts drawing.
fn click_air(state: &mut State, actor: &Actor, held: &Stack, item: item::Type) -> bool {
    let Some(player) = state.players.get(&actor.session) else {
        return false;
    };
    if let Some(armor) = item.armor() {
        // Swap with what is worn there.
        let slot = armor.slot().index();
        let worn = player
            .inventory
            .armor()
            .get(slot)
            .cloned()
            .unwrap_or_default();
        let one = held.with_count(1);
        let rest = held.with_count(held.count().saturating_sub(1));
        if let Some(player) = state.players.get_mut(&actor.session) {
            inventory::set_armor(player, slot, one);
            inventory::set_held(player, worn);
        }
        if !rest.is_empty() {
            return_items(state, actor.session, held, held.clone(), vec![rest]);
        }
        emit::sound(state, actor.world, actor.eye, Sound::ArmorEquipGeneric);
        return true;
    }
    if item.kind() == Some(item::Kind::GoatHorn) {
        // The call of the horn's type; the horn stays raised.
        start_using(state, actor.session);
        emit::sound(state, actor.world, actor.eye, Sound::GoatHorn(item));
        return true;
    }
    if item.kind() == Some(item::Kind::Spyglass) {
        // Looking through it is a use.
        start_using(state, actor.session);
        return true;
    }
    // The entity rules throw a projectile; one is used up.
    if item.throw_force().is_some() {
        if !entity::throw(state, actor.session, held) {
            return false;
        }
        let used = held.with_count(held.count().saturating_sub(1));
        return_items(state, actor.session, held, used, Vec::new());
        return true;
    }
    // Drawing a trident starts while one more throw does not break it; the release throws.
    if item.is(item::Class::Trident) {
        let whole = item
            .max_durability()
            .is_none_or(|max| held.damage() < max.saturating_sub(1));
        if whole {
            start_using(state, actor.session);
        }
        return true;
    }
    // Drawing a bow starts with an arrow to shoot (or infinite resources); the release shoots.
    if item.is(item::Class::Bow) {
        if !actor.has_finite_resources() || inventory::has_arrow(player) {
            start_using(state, actor.session);
        }
        return true;
    }
    false
}

/// Starts the use of the held item now.
fn start_using(state: &mut State, session: SessionId) {
    let tick = state.tick;
    if let Some(player) = state.players.get_mut(&session) {
        player.breaking.using_since = Some(tick);
    }
}

/// Releases the held item: a drawn bow shoots and a drawn trident is thrown (the entity rules)
/// with the ticks it was drawn; a thrown trident leaves the hand of a player with finite items.
pub(super) fn release(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let Some(since) = player.breaking.using_since.take() else {
        return;
    };
    let held = player.inventory.held().clone();
    let used = u32::try_from(tick.saturating_sub(since)).unwrap_or(u32::MAX);
    if !held.item().is_some_and(|t| t.is(item::Class::Trident)) {
        let _shot = entity::release_bow(state, session, used);
        return;
    }
    if entity::release_trident(state, session, used)
        && super::actor(state, session).is_some_and(|a| a.has_finite_resources())
        && let Some(player) = state.players.get_mut(&session)
        && player.inventory.held().same(&held)
    {
        inventory::set_held(player, held.with_count(held.count().saturating_sub(1)));
    }
}

/// Applies `UseItemOnEntity`: attacking another player is the survival rules' melee; attacking or
/// using another entity (trading with a villager) is the entity rules'. Using another player
/// raises no event and does nothing.
pub(super) fn on_entity(
    state: &mut State,
    session: SessionId,
    runtime_id: u64,
    action: UseItemOnEntityAction,
    click: Vec3,
) {
    if let Some(&target) = state.runtime_ids.get(&runtime_id) {
        if target != session && action == UseItemOnEntityAction::Attack {
            survival::attack_player(state, session, target);
        }
        return;
    }
    let Some(target) = entity::find(state, runtime_id) else {
        return;
    };
    match action {
        UseItemOnEntityAction::Attack => entity::attack(state, session, target),
        UseItemOnEntityAction::Interact => {
            let _used = interact_entity(state, session, target, click, Source::Client);
        }
    }
}

/// Uses an entity: when it is within reach, [`PlayerEntityInteract`], then the entity rules'
/// interaction (which check the reach again; a client tries to open trading once a tick at most,
/// [`trade::open_from_client`]). False if nothing happened. Each part of `click` that is not a
/// finite number counts as 0.
pub(super) fn interact_entity(
    state: &mut State,
    session: SessionId,
    target: crate::entity::Id,
    click: Vec3,
    source: Source,
) -> bool {
    let click = super::finite_vec(click);
    let mut event = None;
    if dispatch::heard::<PlayerEntityInteract>(state) {
        // Out of reach or gone: nothing happens, so the event is not raised.
        if !entity::reaches(state, session, target) {
            return false;
        }
        let mut e = PlayerEntityInteract {
            player: player::Id::new(session),
            entity: target,
            click_position: click,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        event = Some(e);
    }
    // The entity rules check again whether the entity is there and within reach.
    let open_trade = match source {
        Source::Client => trade::open_from_client,
        Source::Action => trade::open,
    };
    let used = entity::interact(state, session, target, open_trade);
    if let Some(e) = event.filter(|_| used) {
        dispatch::watch(state, &e);
    }
    used
}
