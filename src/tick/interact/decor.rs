//! Using and hitting blocks whose block entity holds what they show (PMMP `onInteract` and
//! `onAttack` of `BaseSign`, `FlowerPot`, `ItemFrame`, `Jukebox`, `Lectern`, `ChiseledBookshelf`,
//! `Campfire`'s cooking and `WaterCauldron`'s dye): the block entity changes and the players who
//! see it get its new client data.
//!
//! An action that would drop an item first checks that the world can take one more entity
//! (PLAN.md 10.7: at the entity limit the action is refused and nothing changes).

use super::super::block_entity::{self, BlockEntity, Kind};
use super::super::{SessionId, State, entity};
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

/// PMMP `BaseSign::onInteract`: a waxed sign does nothing; a dye colours the side the player
/// faces, ink sacs light or dim it, honeycomb waxes it; anything else opens the editor.
pub(super) fn sign(state: &mut State, u: &mut Use) -> bool {
    let front = using::sign_front(u.block, u.actor.feet, u.pos);
    let world = u.actor.world;
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = entity_mut(state, world, u.pos)
    else {
        open_sign_editor(state, u.actor.session, world, u.pos, front);
        return true;
    };
    if sign.waxed {
        return true;
    }
    let side = sign.side_mut(front);
    let changed = if let Some(colour) = sign_colour(&u.item) {
        (side.colour != colour).then(|| {
            side.colour = colour;
            Event::SoundDyeUsed
        })
    } else if u.item_kind(item::Kind::InkSac) || u.item_kind(item::Kind::GlowInkSac) {
        let glow = u.item_kind(item::Kind::GlowInkSac);
        (side.glowing != glow).then(|| {
            side.glowing = glow;
            Event::SoundInkSacUsed
        })
    } else if u.item_kind(item::Kind::Honeycomb) {
        sign.waxed = true;
        Some(Event::WaxOn)
    } else {
        None
    };
    match changed {
        Some(event) => {
            u.pop();
            level_event(state, world, super::block_vec(u.pos), event, 0);
            block_entity::broadcast_spawn(state, world, u.pos);
        }
        None => open_sign_editor(state, u.actor.session, world, u.pos, front),
    }
    true
}

/// Stores a sign edit (PMMP `BaseSign::updateFaceText` for both sides: the lines change, the
/// colour and glow stay): false for a waxed sign or one without its block entity.
pub(super) fn write_sign(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    edit: &super::sign::Edit,
) -> bool {
    let Some(BlockEntity {
        kind: Kind::Sign(sign),
        ..
    }) = entity_mut(state, world, pos)
    else {
        return false;
    };
    if sign.waxed {
        return false;
    }
    sign.front.lines.clone_from(&edit.front);
    sign.back.lines.clone_from(&edit.back);
    block_entity::broadcast_spawn(state, world, pos);
    true
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
    if u.item.is_empty() || !block_entity::campfire_cooks(kind, &u.item) {
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
