//! Using and hitting blocks whose block entity holds what they show (PMMP `onInteract` and
//! `onAttack` of `BaseSign`, `FlowerPot`, `ItemFrame`, `Jukebox`, `Lectern`, `ChiseledBookshelf`,
//! `Campfire`'s cooking and `WaterCauldron`'s dye): the block entity changes and the players who
//! see it get its new client data.
//!
//! An action that would drop an item first checks that the world can take one more entity
//! (PLAN.md 10.7: at the entity limit the action is refused and nothing changes).

use super::super::block_entity::{self, BlockEntity, Kind};
use super::super::{SessionId, State, dispatch, entity};
use super::using::{self, Use};
use super::{Actor, center, level_event, open_sign_editor, props, sound};
use crate::block::{self, Class, Face, TypeTag};
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::protocol::consts::{LevelEvent as Event, SoundEvent};
use crate::protocol::packet::LecternUpdate;
use crate::save::WorldIndex;
use crate::world::BlockPos;

/// The block entity at `pos` to change.
fn entity_mut(state: &mut State, world: WorldIndex, pos: BlockPos) -> Option<&mut BlockEntity> {
    state.rules.block_entities.get_mut(world, pos)
}

/// The block entity at `pos`.
fn entity_at(state: &State, world: WorldIndex, pos: BlockPos) -> Option<&BlockEntity> {
    state.rules.block_entities.get(world, pos)
}

/// The colour a dye-like item gives a sign (PMMP `BaseSign::onInteract`: dyes, bone meal white,
/// lapis blue, cocoa brown; black is pure black), as signed ARGB.
fn sign_colour(stack: &Stack) -> Option<i32> {
    let t = stack.item()?;
    let colour = match t.kind() {
        Some(item::Kind::BoneMeal) => 0,
        Some(item::Kind::LapisLazuli) => 11,
        Some(item::Kind::CocoaBeans) => 12,
        _ => item::dye_colour(t)?,
    };
    if colour == 15 {
        return Some(block_entity::BLACK_TEXT);
    }
    item::dye_argb(colour).map(u32::cast_signed)
}

/// What a held item does to one side of a sign (PMMP `BaseSign::onInteract`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignUse {
    /// A dye gives the text this colour (signed ARGB).
    Colour(i32),
    /// An ink sac dims the text, a glow ink sac lights it.
    Glow(bool),
    /// Honeycomb waxes the sign.
    Wax,
}

/// What the held item does to the side (`front` or back) of the sign at `u.pos`: `None` when it
/// changes nothing (the editor opens instead) or there is no sign block entity.
fn sign_use(state: &State, u: &Use, front: bool) -> Option<SignUse> {
    let BlockEntity {
        kind: Kind::Sign(sign),
        ..
    } = entity_at(state, u.actor.world, u.pos)?
    else {
        return None;
    };
    let side = if front { &sign.front } else { &sign.back };
    if let Some(colour) = sign_colour(&u.item) {
        (side.colour != colour).then_some(SignUse::Colour(colour))
    } else if u.item_kind(item::Kind::InkSac) || u.item_kind(item::Kind::GlowInkSac) {
        let glow = u.item_kind(item::Kind::GlowInkSac);
        (side.glowing != glow).then_some(SignUse::Glow(glow))
    } else if u.item_kind(item::Kind::Honeycomb) {
        Some(SignUse::Wax)
    } else {
        None
    }
}

/// PMMP `BaseSign::onInteract`: a waxed sign does nothing; a dye colours the side the player
/// faces, ink sacs light or dim it (each a [`SignChange`](crate::event::SignChange), PMMP
/// `doSignChange`; cancelled, the editor opens), honeycomb waxes it; anything else opens the
/// editor.
pub(super) fn sign(state: &mut State, u: &mut Use) -> bool {
    let front = using::sign_front(u.block, u.actor.feet, u.pos);
    let world = u.actor.world;
    let Some(Kind::Sign(sign)) = entity_at(state, world, u.pos).map(|e| &e.kind) else {
        open_sign_editor(state, u.actor.session, world, u.pos, front);
        return true;
    };
    if sign.waxed {
        return true;
    }
    let Some(change) = sign_use(state, u, front) else {
        open_sign_editor(state, u.actor.session, world, u.pos, front);
        return true;
    };
    let mut lines = None;
    if change != SignUse::Wax && dispatch::heard::<crate::event::SignChange>(state) {
        let (Some(world_id), Some(old)) =
            (crate::world::Id::new(world), sign_text(state, world, u.pos))
        else {
            return true;
        };
        let old_text = if front {
            old.front.clone()
        } else {
            old.back.clone()
        };
        let mut e = crate::event::SignChange {
            player: crate::player::Id::new(u.actor.session),
            world: world_id,
            pos: u.pos,
            block: u.block,
            old_text: old_text.clone(),
            new_text: old_text,
            front_face: front,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            open_sign_editor(state, u.actor.session, world, u.pos, front);
            return true;
        }
        // A listener may have changed the sign or taken the item: then nothing happens.
        if dispatch::listened::<crate::event::SignChange>(state)
            && !(using::still_holds(state, &u.actor, &u.item)
                && state.worlds.get(world).and_then(|w| w.block(u.pos)) == Some(u.block)
                && sign_text(state, world, u.pos).as_ref() == Some(&old)
                && sign_use(state, u, front) == Some(change))
        {
            return true;
        }
        let Some(checked) = super::sign::check(&e.new_text) else {
            open_sign_editor(state, u.actor.session, world, u.pos, front);
            return true;
        };
        lines = Some(checked);
        u.watched = Some(using::Watched::Sign(e));
    }
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return true;
    };
    let event = match change {
        SignUse::Colour(colour) => {
            sign.side_mut(front).colour = colour;
            Event::SoundDyeUsed
        }
        SignUse::Glow(glow) => {
            sign.side_mut(front).glowing = glow;
            Event::SoundInkSacUsed
        }
        SignUse::Wax => {
            sign.waxed = true;
            Event::WaxOn
        }
    };
    if let Some(lines) = lines {
        sign.side_mut(front).lines = lines;
    }
    u.pop();
    level_event(state, world, super::block_vec(u.pos), event, 0);
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// What a sign's block entity holds of its text: the lines of each side and whether it is waxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignText {
    /// The front side's lines.
    pub(super) front: [String; crate::tile::SIGN_LINES],
    /// The back side's lines.
    pub(super) back: [String; crate::tile::SIGN_LINES],
    /// A waxed sign takes no edits.
    pub(super) waxed: bool,
}

/// The text of the sign at `pos`; `None` without a sign block entity.
pub(super) fn sign_text(state: &State, world: WorldIndex, pos: BlockPos) -> Option<SignText> {
    let BlockEntity {
        kind: Kind::Sign(sign),
        ..
    } = entity_at(state, world, pos)?
    else {
        return None;
    };
    Some(SignText {
        front: sign.front.lines.clone(),
        back: sign.back.lines.clone(),
        waxed: sign.waxed,
    })
}

/// Stores the lines of one side of the sign at `pos` (PMMP `BaseSign::updateFaceText`: the lines
/// change, the colour and glow stay); the caller checked that the sign takes the edit.
pub(super) fn write_sign(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    front: bool,
    lines: [String; crate::tile::SIGN_LINES],
) {
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = entity_mut(state, world, pos)
    else {
        return;
    };
    sign.side_mut(front).lines = lines;
    block_entity::broadcast_spawn(state, world, pos);
}

/// PMMP `FlowerPot::onInteract`: a pottable plant goes in an empty pot; a full pot gives its plant
/// to the player (what does not fit drops), unless the held item could be potted too (vanilla
/// then places it).
pub(super) fn flower_pot(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let plant = u
        .item
        .item()
        .and_then(|t| t.placed_block(None))
        .filter(|b| b.has_type_tag(TypeTag::Pottable));
    let Some(BlockEntity {
        kind: Kind::FlowerPot { plant: potted },
        ..
    }) = entity_at(state, world, u.pos)
    else {
        return false;
    };
    match (*potted, plant) {
        (Some(old), None) => {
            if !entity::has_room(state, world, 1) {
                return false;
            }
            if let Some(BlockEntity {
                kind: Kind::FlowerPot { plant },
                ..
            }) = entity_mut(state, world, u.pos)
            {
                *plant = None;
            }
            if let block::ItemForm::Item(t) = old.item() {
                u.returned.push(Stack::new(t, 1));
            }
            block_entity::broadcast_spawn(state, world, u.pos);
            true
        }
        (None, Some(new)) => {
            if let Some(BlockEntity {
                kind: Kind::FlowerPot { plant },
                ..
            }) = entity_mut(state, world, u.pos)
            {
                *plant = Some(new);
            }
            u.pop();
            block_entity::broadcast_spawn(state, world, u.pos);
            true
        }
        (Some(_), Some(_)) | (None, None) => false,
    }
}

/// PMMP `ItemFrame::onInteract`: a framed item turns an eighth; an empty frame takes one of the
/// held item.
pub(super) fn item_frame(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let Some(BlockEntity {
        kind: Kind::ItemFrame(frame),
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return true;
    };
    let event = if !frame.item.is_empty() {
        frame.rotation = (frame.rotation + 1) % block_entity::FRAME_ROTATIONS;
        Event::SoundItemFrameRotateItem
    } else if !u.item.is_empty() {
        frame.item = u.item.with_count(1);
        u.pop();
        Event::SoundAddItem
    } else {
        return true;
    };
    level_event(state, world, super::block_vec(u.pos), event, 0);
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// PMMP `ItemFrame::onAttack` and `Lectern::onAttack`: hitting a frame drops its item (with the
/// frame's drop chance) and a lectern its book. True when the hit was used up (a frame).
pub(super) fn attack(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    rng: &mut crate::random::Random,
) -> bool {
    let world = actor.world;
    let (dropped, at, frame) = match entity_at(state, world, pos).map(|e| &e.kind) {
        Some(Kind::ItemFrame(f)) if !f.item.is_empty() => {
            let keep = rng.unit() <= f64::from(f.drop_chance);
            (keep.then(|| f.item.clone()), center(pos), true)
        }
        Some(Kind::Lectern { book, .. }) if !book.is_empty() => (
            Some(book.clone()),
            Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y) + 1.0,
                f64::from(pos.z) + 0.5,
            ),
            false,
        ),
        _ => return false,
    };
    if let Some(stack) = dropped
        && !entity::drop_item(state, world, at, stack)
    {
        // The world is full: nothing changes.
        return frame;
    }
    match entity_mut(state, world, pos).map(|e| &mut e.kind) {
        Some(Kind::ItemFrame(f)) => {
            f.item = Stack::EMPTY;
            f.rotation = 0;
        }
        Some(Kind::Lectern { book, page }) => {
            *book = Stack::EMPTY;
            *page = 0;
        }
        _ => {}
    }
    if frame {
        level_event(
            state,
            world,
            super::block_vec(pos),
            Event::SoundItemFrameRemoveItem,
            0,
        );
    }
    block_entity::broadcast_spawn(state, world, pos);
    frame
}

/// PMMP `Jukebox::onInteract`: a record comes out (dropped above), or a held record goes in. The
/// music is the client's; only the state is kept.
pub(super) fn jukebox(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let Some(BlockEntity {
        kind: Kind::Jukebox { record },
        ..
    }) = entity_at(state, world, u.pos)
    else {
        return true;
    };
    let record = record.clone();
    if !record.is_empty() {
        let at = Vec3::new(
            f64::from(u.pos.x) + 0.5,
            f64::from(u.pos.y) + 1.0,
            f64::from(u.pos.z) + 0.5,
        );
        if !entity::drop_item(state, world, at, record) {
            return true;
        }
        if let Some(BlockEntity {
            kind: Kind::Jukebox { record },
            ..
        }) = entity_mut(state, world, u.pos)
        {
            *record = Stack::EMPTY;
        }
        sound(state, world, center(u.pos), SoundEvent::RecordNull, 0);
    } else if u.item.item().is_some_and(|t| t.is(item::Class::Record)) {
        let disc = u.item.with_count(1);
        u.pop();
        if let Some(BlockEntity {
            kind: Kind::Jukebox { record },
            ..
        }) = entity_mut(state, world, u.pos)
        {
            *record = disc;
        }
    } else {
        return true;
    }
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// PMMP `Lectern::onInteract`: a writable or written book goes on an empty lectern.
pub(super) fn lectern(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let is_book = u
        .item
        .item()
        .is_some_and(|t| t.is(item::Class::WritableBookBase));
    if !is_book {
        return true;
    }
    let Some(BlockEntity {
        kind: Kind::Lectern { book, page },
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return true;
    };
    if !book.is_empty() {
        return true;
    }
    *book = u.item.with_count(1);
    *page = 0;
    u.pop();
    sound(state, world, center(u.pos), SoundEvent::LecternBookPlace, 0);
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// PMMP `Lectern::onPageTurn` (`LecternUpdate`): the page must be in the book.
pub(super) fn on_lectern_update(state: &mut State, session: SessionId, packet: &LecternUpdate) {
    let Some(actor) = super::actor(state, session) else {
        return;
    };
    let pos = super::block_pos(packet.position);
    if !actor.can_interact(center(pos), actor.reach()) {
        return;
    }
    let world = actor.world;
    let Some(BlockEntity {
        kind: Kind::Lectern { book, page },
        ..
    }) = entity_mut(state, world, pos)
    else {
        return;
    };
    let new = i32::from(packet.page);
    if new == *page || book.is_empty() || usize::from(packet.page) >= block_entity::book_pages(book)
    {
        return;
    }
    *page = new;
    block_entity::broadcast_spawn(state, world, pos);
}

/// PMMP `ChiseledBookshelf::onInteract`: on its front, the shelf slot under the click gives its
/// book back or takes a book from the hand; the block shows which slots hold books.
pub(super) fn bookshelf(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    if props::facing(u.block) != Some(u.face) {
        return false;
    }
    let x = if matches!(u.face, Face::West | Face::East) {
        u.click.z
    } else {
        u.click.x
    };
    // PMMP: the column counts from the left seen from the front.
    let positive = matches!(props::rotate_y(u.face, true), Face::South | Face::East);
    let x = if positive { 1.0 - x } else { x };
    let column = if x < 6.0 / 16.0 {
        0
    } else if x < 11.0 / 16.0 {
        1
    } else {
        2
    };
    let slot = if u.click.y < 0.5 { 3 } else { 0 } + column;
    let Some(BlockEntity {
        kind: Kind::ChiseledBookshelf { last },
        items,
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return false;
    };
    let Some(there) = items.get_mut(slot) else {
        return false;
    };
    let is_book = u.item.item().is_some_and(|t| {
        t.is(item::Class::WritableBookBase)
            || t.is(item::Class::Book)
            || t.is(item::Class::EnchantedBook)
    });
    if !there.is_empty() {
        u.returned.push(std::mem::take(there));
    } else if is_book {
        *there = u.item.with_count(1);
        u.pop();
    } else {
        return true;
    }
    *last = i32::try_from(slot).unwrap_or(0) + 1;
    let stored = items
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .fold(0, |bits, (i, _)| bits | (1 << i));
    if let Some(next) = props::with_int(u.block, "books_stored", stored) {
        using::set(state, world, u.pos, next);
    }
    true
}

/// PMMP `Campfire::onInteract` (after lighting and putting out): an item with a campfire recipe
/// goes into a free slot.
pub(super) fn campfire_cook(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let kind = block_entity::campfire_kind(u.block.is(Class::SoulCampfire));
    if u.item.is_empty() || !block_entity::campfire_cooks(kind, &u.item, &state.custom) {
        return false;
    }
    let Some(BlockEntity {
        kind: Kind::Campfire(_),
        items,
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return false;
    };
    let Some(free) = items.iter_mut().find(|s| s.is_empty()) else {
        return false;
    };
    *free = u.item.with_count(1);
    u.pop();
    state.rules.block_entities.wake(world, u.pos);
    level_event(
        state,
        world,
        super::block_vec(u.pos),
        Event::SoundAddItem,
        0,
    );
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// PMMP `WaterCauldron::onInteract` with a dye: the water takes the colour, mixed with the one
/// it has (PMMP `Color::mix`: the mean of each channel).
pub(super) fn cauldron_dye(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let water = props::string(u.block, "cauldron_liquid") == Some("water")
        && props::int(u.block, "fill_level").is_some_and(|l| l > 0);
    let colour = match u.item.item().and_then(|t| match t.kind() {
        Some(item::Kind::LapisLazuli) => Some(11),
        Some(item::Kind::InkSac) => Some(15),
        Some(item::Kind::CocoaBeans) => Some(12),
        Some(item::Kind::BoneMeal) => Some(0),
        _ => item::dye_colour(t),
    }) {
        Some(c) if water => c,
        _ => return false,
    };
    let Some(new) = item::dye_argb(colour) else {
        return false;
    };
    let Some(BlockEntity {
        kind: Kind::Cauldron(cauldron),
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        return false;
    };
    let mixed = match cauldron.colour {
        None => new,
        Some(old) => mix(old.cast_unsigned(), new),
    };
    if cauldron.colour == Some(mixed.cast_signed()) {
        return false;
    }
    cauldron.colour = Some(mixed.cast_signed());
    u.pop();
    level_event(
        state,
        world,
        super::block_vec(u.pos),
        Event::CauldronAddDye,
        0,
    );
    block_entity::broadcast_spawn(state, world, u.pos);
    true
}

/// PMMP `Color::mix` of two ARGB colours: each channel's mean.
fn mix(a: u32, b: u32) -> u32 {
    (0..4).fold(0, |out, i| {
        let shift = i * 8;
        let channel = u32::midpoint(a >> shift & 0xff, b >> shift & 0xff);
        out | channel << shift
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_mix_by_channel_and_black_text_is_pure_black() {
        assert_eq!(mix(0xff00_0000, 0xff00_00ff), 0xff00_007f);
        let black = Stack::new(
            item::Type::from_name("minecraft:black_dye", 0, None, &crate::nbt::Compound::new())
                .unwrap(),
            1,
        );
        assert_eq!(sign_colour(&black), Some(block_entity::BLACK_TEXT));
        let bone = Stack::new(item::Kind::BoneMeal.default_type(), 1);
        assert_eq!(sign_colour(&bone), Some(0xfff0_f0f0_u32.cast_signed()));
    }
}
