//! Tests of the public block entity API: each write changes the block entity, reaches the players
//! who see it as `BlockActorData` and is in the chunk's saved form.

use tokio::sync::mpsc::Receiver;

use super::super::super::queue::{Input, Outbound};
use super::super::super::tests::{answer, join, new_state, radius, settings};
use super::super::super::{InputLog, Output, SessionId, boundary, deliver, step};
use super::super::{beside, cardinal, placed};
use super::*;
use crate::effect::Type as Effect;
use crate::event::Listeners;
use crate::nbt::Value;
use crate::pool::Pool;
use crate::protocol::packet::Packet;
use crate::save::Response;

const WORLD: world::Id = world::Id::DEFAULT;

/// A flat world with one player who sees the spawn chunks.
struct Harness {
    state: State,
    pool: Pool,
    answers: Vec<Response>,
    /// Kept so the session stays.
    receivers: Vec<Receiver<Outbound>>,
}

impl Harness {
    fn new() -> Self {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let loaded: Vec<_> = state.worlds[0].loaded().collect();
        for pos in loaded {
            let extras = state
                .chunks
                .extras
                .get((0, pos))
                .cloned()
                .unwrap_or_default();
            state.rules.load((0, pos), &extras, &state.worlds[0]);
        }
        let mut h = Self {
            state,
            pool: crate::runtime::cpu_pool(1).unwrap(),
            answers: Vec::new(),
            receivers: Vec::new(),
        };
        let (j, rx) = join(1, 0);
        h.receivers.push(rx);
        h.tick(vec![j, radius(1, 1, 4)]);
        h.tick(Vec::new());
        h.sent();
        h
    }

    fn tick(&mut self, mut inputs: Vec<Input>) {
        let mut output = Output::default();
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut self.answers);
        step(
            &mut self.state,
            &mut inputs,
            &mut answers,
            &self.pool,
            &mut output,
            &mut log,
        );
        deliver(&mut self.state, &mut output, &mut log);
        boundary(&mut self.state, &mut output, &mut log);
        self.answers = answer(&self.state, std::mem::take(&mut output.requests));
    }

    /// Takes the packets queued for the player.
    fn sent(&mut self) -> Vec<Packet> {
        let player = self.state.players.get_mut(&SessionId::new(1)).unwrap();
        std::mem::take(&mut player.outbox)
    }

    /// The block entity data the player was sent for `pos`, the last first.
    fn actor_data(&mut self, pos: BlockPos) -> Vec<Compound> {
        let mut out: Vec<Compound> = self
            .sent()
            .into_iter()
            .filter_map(|p| match p {
                Packet::BlockActorData(d) if d.position == [pos.x, pos.y, pos.z] => {
                    Some(d.nbt_data)
                }
                _ => None,
            })
            .collect();
        out.reverse();
        out
    }

    /// A free spot on the ground near the spawn, `dx` blocks east.
    fn spot(&self, dx: i32) -> BlockPos {
        let y = self.state.worlds[0].highest_block(dx, 2).unwrap() + 1;
        BlockPos::new(dx, y, 2)
    }

    /// Sets `block` at `pos` and makes its block entity as a placement does.
    fn place(&mut self, pos: BlockPos, block: block::State) {
        self.state.worlds[0].set_block(pos, block).unwrap();
        placed(&mut self.state, 0, pos, None, &Stack::EMPTY, Face::Up);
        self.sent();
    }

    fn cx(&mut self) -> Context<'_> {
        Context {
            state: &mut self.state,
        }
    }

    /// Takes the saved form of every changed chunk and loads the chunks of `at` back from it.
    fn reload(&mut self, at: &[BlockPos]) {
        let saved = self.state.rules.block_entities.take_changed();
        let entities = &mut self.state.rules.block_entities;
        for pos in at {
            let key = (0, pos.chunk());
            let (_, list) = saved.iter().find(|(k, _)| *k == key).unwrap();
            assert!(
                entities.unload(key).is_none(),
                "the chunk changed again since the save"
            );
            entities.load(key, list, &self.state.worlds[0], &self.state.custom);
        }
    }
}

fn text(lines: [&str; 4], argb: u32) -> SignText {
    let colour = crate::tile::from_argb(argb.cast_signed());
    SignText::new(lines.map(String::from), colour, false).unwrap()
}

fn compound<'a>(nbt: &'a Compound, key: &str) -> &'a Compound {
    match nbt.get(key) {
        Some(Value::Compound(c)) => c,
        other => panic!("{key}: {other:?}"),
    }
}

fn item(name: &str, meta: u16) -> Stack {
    Stack::new(
        item::Type::from_name(name, meta, None, &Compound::new()).unwrap(),
        1,
    )
}

// pmmp: World::getTile, World::getTileAt, Chunk::getTile, Tile, Tile::getPosition, Tile::getBlock, BaseSign::getText, BaseSign::getFaceText, Sign::getText, Sign::getBackText, BaseSign::setText, BaseSign::setFaceText, Sign::setText, Sign::setBackText, BaseSign::isWaxed, Sign::isWaxed, BaseSign::setWaxed, Sign::setWaxed
#[test]
fn a_sign_takes_text_on_each_side_tells_the_clients_and_keeps_it_saved() {
    let mut h = Harness::new();
    let pos = h.spot(1);
    let sign = block::Kind::OakSign.default_state();
    h.place(pos, sign);
    let above = BlockPos::new(pos.x, pos.y + 3, pos.z);
    let mut cx = h.cx();
    assert!(
        cx.block_entity(WORLD, above).is_none(),
        "no block entity there"
    );
    let tile = cx.block_entity(WORLD, pos).unwrap();
    assert_eq!(
        (tile.world(), tile.position(), tile.block()),
        (WORLD, pos, sign)
    );
    assert_eq!(tile.sign_text(Side::Front), Some(&SignText::default()));
    assert!(!tile.is_sign_waxed());
    let mut t = cx.block_entity_mut(WORLD, pos).unwrap();
    t.set_sign_text(Side::Front, text(["Hello", "", "", ""], 0xff00_ff00))
        .unwrap();
    t.set_sign_text(Side::Back, text(["", "Back", "", ""], 0xff00_0000))
        .unwrap();
    t.set_sign_waxed(true).unwrap();
    assert_eq!(t.set_cooking_time_ticks(0, 1), Err(Error::WrongKind));
    let view = t.view();
    assert_eq!(
        view.sign_text(Side::Front).map(SignText::colour),
        Some(Rgba {
            r: 0,
            g: 0xff,
            b: 0,
            a: 0xff
        })
    );
    assert_eq!(
        view.sign_text(Side::Back).and_then(|s| s.line(1)),
        Some("Back")
    );
    assert!(view.is_sign_waxed());
    let sent = h.actor_data(pos);
    assert_eq!(
        sent.len(),
        3,
        "one update per write, none for the refused one"
    );
    assert_eq!(
        compound(&sent[0], "FrontText").get("Text"),
        Some(&Value::String("Hello".to_owned()))
    );
    assert_eq!(sent[0].get("IsWaxed"), Some(&Value::Byte(1)));
    h.reload(&[pos]);
    let cx = h.cx();
    let tile = cx.block_entity(WORLD, pos).unwrap();
    assert_eq!(
        tile.sign_text(Side::Back).map(SignText::lines),
        Some(&["", "Back", "", ""].map(String::from))
    );
    assert!(tile.is_sign_waxed());
}

// pmmp: Chunk::getTiles, Nameable::getName, NameableTrait::getName, Nameable::setName, NameableTrait::setName, Container::canOpenWith, ContainerTrait::canOpenWith, block\tile\EnderChest::getViewerCount, block\tile\Chest::getPair, block\tile\Chest::pairWith, block\tile\Chest::unpair, block\tile\ShulkerBox::getFacing, block\tile\ShulkerBox::setFacing
#[test]
fn containers_take_names_and_chests_pair_side_by_side() {
    let mut h = Harness::new();
    let a = h.spot(1);
    let chest = block::Kind::Chest.default_state();
    let facing = cardinal(chest).unwrap();
    let (left, right) = (
        beside(a, facing.clockwise()).unwrap(),
        beside(a, facing.clockwise().opposite()).unwrap(),
    );
    let behind = beside(a, facing.opposite()).unwrap();
    let far = beside(left, facing.clockwise()).unwrap();
    for p in [a, left, right, behind, far] {
        h.state.worlds[0].set_block(p, chest).unwrap();
        h.state
            .rules
            .block_entities
            .insert(0, p, BlockEntity::new(Kind::Chest { pair: None }, 27));
    }
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, a).unwrap();
    assert_eq!(t.view().name(), None);
    t.set_name(Some("Loot")).unwrap();
    assert_eq!(
        t.set_name(Some(&"x".repeat(MAX_NAME_BYTES + 1))),
        Err(Error::TooLong)
    );
    assert_eq!(t.view().name(), Some("Loot"));
    assert!(t.view().can_open_with("anything"));
    assert_eq!(t.view().viewer_count(), 0);
    assert_eq!(t.unpair(), Err(Error::Invalid), "a single chest");
    assert_eq!(t.pair_with(far), Err(Error::Invalid), "not beside it");
    assert_eq!(t.pair_with(behind), Err(Error::Invalid), "behind it");
    assert_eq!(t.pair_with(a), Err(Error::Invalid), "itself");
    t.pair_with(left).unwrap();
    assert_eq!(t.view().chest_pair(), Some(left));
    assert_eq!(t.pair_with(right), Err(Error::Invalid), "paired already");
    let sent = h.actor_data(a);
    assert_eq!(sent[0].get("pairx"), Some(&Value::Int(left.x)));
    assert_eq!(sent[0].get("pairz"), Some(&Value::Int(left.z)));
    assert_eq!(
        sent[0].get("CustomName"),
        Some(&Value::String("Loot".to_owned()))
    );
    let mut cx = h.cx();
    assert_eq!(cx.block_entity(WORLD, left).unwrap().chest_pair(), Some(a));
    let in_chunk: Vec<BlockPos> = cx
        .chunk_block_entities(WORLD, a.chunk())
        .map(|t| t.position())
        .collect();
    assert!(in_chunk.contains(&a) && in_chunk.contains(&left));
    assert!(in_chunk.windows(2).all(|w| w[0] < w[1]), "in order");
    let mut t = cx.block_entity_mut(WORLD, left).unwrap();
    t.unpair().unwrap();
    assert_eq!(t.view().chest_pair(), None);
    assert_eq!(cx.block_entity(WORLD, a).unwrap().chest_pair(), None);
    if let Some(e) = h.state.rules.block_entities.get_mut(0, a) {
        e.rest
            .insert("Lock".to_owned(), Value::String("key".to_owned()));
    }
    let cx = h.cx();
    let tile = cx.block_entity(WORLD, a).unwrap();
    assert!(tile.can_open_with("key") && !tile.can_open_with("other"));

    let box_at = h.spot(6);
    h.place(box_at, block::Kind::ShulkerBox.default_state());
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, box_at).unwrap();
    assert_eq!(t.view().shulker_facing(), Some(Face::Up));
    t.set_shulker_facing(Face::West).unwrap();
    t.set_name(None).unwrap();
    assert_eq!(t.view().shulker_facing(), Some(Face::West));
    assert_eq!(h.actor_data(box_at)[0].get("facing"), Some(&Value::Byte(4)));
    let sign_at = h.spot(7);
    h.place(sign_at, block::Kind::OakSign.default_state());
    let mut cx = h.cx();
    assert_eq!(
        cx.block_entity_mut(WORLD, sign_at)
            .unwrap()
            .set_name(Some("x")),
        Err(Error::WrongKind),
        "a sign has no name"
    );
}

// pmmp: block\Bed::getColor, block\tile\Bed::getColor, block\Bed::setColor, block\tile\Bed::setColor, BaseBanner::getColor, block\tile\Banner::getBaseColor, BaseBanner::setColor, block\tile\Banner::setBaseColor, BaseBanner::getPatterns, block\tile\Banner::getPatterns, BaseBanner::setPatterns, block\tile\Banner::setPatterns, block\tile\Banner::getType, block\tile\Banner::setType, block\MobHead::getRotation, block\tile\MobHead::getRotation, block\MobHead::setRotation, block\tile\MobHead::setRotation
#[test]
fn beds_banners_and_heads_keep_their_look() {
    let mut h = Harness::new();
    let (bed, banner, head) = (h.spot(1), h.spot(3), h.spot(5));
    h.place(bed, block::Kind::Bed.default_state());
    h.place(banner, block::Kind::Banner.default_state());
    h.place(head, block::Kind::MobHead.default_state());
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, bed).unwrap();
    assert_eq!(
        t.view().bed_colour(),
        Some(DyeColour::White),
        "the item's meta 0"
    );
    t.set_bed_colour(DyeColour::Blue).unwrap();
    assert_eq!(t.view().bed_colour(), Some(DyeColour::Blue));
    assert_eq!(t.view().banner_colour(), None, "a bed is no banner");
    assert_eq!(h.actor_data(bed)[0].get("color"), Some(&Value::Byte(11)));

    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, banner).unwrap();
    assert_eq!(t.view().banner_colour(), Some(DyeColour::Black));
    assert_eq!(t.view().banner_type(), Some(BannerType::Normal));
    let layers = [
        BannerPattern::new(block::BannerPattern::Border, DyeColour::White),
        BannerPattern::new(block::BannerPattern::Creeper, DyeColour::Green),
    ];
    t.set_banner_colour(DyeColour::Yellow).unwrap();
    t.set_banner_patterns(&layers).unwrap();
    t.set_banner_type(BannerType::Ominous).unwrap();
    assert_eq!(t.set_banner_patterns(&[layers[0]; 17]), Err(Error::TooLong));
    assert_eq!(t.set_bed_colour(DyeColour::Red), Err(Error::WrongKind));
    let view = t.view();
    assert_eq!(view.banner_colour(), Some(DyeColour::Yellow));
    assert_eq!(
        view.banner_patterns().unwrap().collect::<Vec<_>>(),
        layers.to_vec()
    );
    assert_eq!(view.banner_type(), Some(BannerType::Ominous));
    let sent = h.actor_data(banner);
    assert_eq!(
        sent[0].get("Base"),
        Some(&Value::Int(11)),
        "yellow, counted down"
    );
    assert_eq!(sent[0].get("Type"), Some(&Value::Int(1)));

    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, head).unwrap();
    assert_eq!(t.view().skull_rotation(), Some(0));
    t.set_skull_rotation(12).unwrap();
    assert_eq!(t.set_skull_rotation(16), Err(Error::Invalid));
    assert_eq!(t.view().skull_rotation(), Some(12));
    assert_eq!(
        h.actor_data(head)[0].get("Rotation"),
        Some(&Value::Float(270.0))
    );
    h.reload(&[bed, banner, head]);
    let cx = h.cx();
    assert_eq!(
        cx.block_entity(WORLD, bed).unwrap().bed_colour(),
        Some(DyeColour::Blue)
    );
    assert_eq!(
        cx.block_entity(WORLD, banner)
            .unwrap()
            .banner_patterns()
            .unwrap()
            .count(),
        2
    );
    assert_eq!(
        cx.block_entity(WORLD, head).unwrap().skull_rotation(),
        Some(12)
    );
}

// pmmp: block\FlowerPot::getPlant, block\tile\FlowerPot::getPlant, block\FlowerPot::setPlant, block\tile\FlowerPot::setPlant, block\ItemFrame::getFramedItem, block\tile\ItemFrame::getItem, block\ItemFrame::setFramedItem, block\tile\ItemFrame::setItem, block\ItemFrame::getItemRotation, block\tile\ItemFrame::getItemRotation, block\ItemFrame::setItemRotation, block\tile\ItemFrame::setItemRotation, block\ItemFrame::getItemDropChance, block\tile\ItemFrame::getItemDropChance, block\ItemFrame::setItemDropChance, block\tile\ItemFrame::setItemDropChance, block\Jukebox::getRecord, block\tile\Jukebox::getRecord, block\tile\Jukebox::setRecord
#[test]
fn pots_frames_and_jukeboxes_hold_what_they_are_given() {
    let mut h = Harness::new();
    let (pot, frame, jukebox) = (h.spot(1), h.spot(3), h.spot(5));
    h.place(pot, block::Kind::FlowerPot.default_state());
    h.place(frame, block::Kind::ItemFrame.default_state());
    h.place(jukebox, block::Kind::Jukebox.default_state());
    let dandelion = block::Kind::Dandelion.default_state();
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, pot).unwrap();
    assert_eq!(t.view().potted_plant(), None);
    assert_eq!(
        t.set_potted_plant(Some(block::Kind::Chest.default_state())),
        Err(Error::Invalid)
    );
    t.set_potted_plant(Some(dandelion)).unwrap();
    assert_eq!(t.view().potted_plant(), Some(dandelion));
    assert!(h.actor_data(pot)[0].contains_key("PlantBlock"));

    let sticks = Stack::new(item::Kind::Stick.default_type(), 5);
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, frame).unwrap();
    assert_eq!(t.view().framed_item(), Some(&Stack::EMPTY));
    t.set_framed_item(&sticks).unwrap();
    t.set_frame_rotation(3).unwrap();
    t.set_frame_drop_chance(0.25).unwrap();
    assert_eq!(t.set_frame_rotation(8), Err(Error::Invalid));
    assert_eq!(t.set_frame_drop_chance(f32::NAN), Err(Error::Invalid));
    assert_eq!(t.set_frame_drop_chance(1.5), Err(Error::Invalid));
    let view = t.view();
    assert_eq!(view.framed_item(), Some(&sticks.with_count(1)), "one item");
    assert_eq!(
        (view.frame_rotation(), view.frame_drop_chance()),
        (Some(3), Some(0.25))
    );
    let sent = h.actor_data(frame);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0].get("ItemRotation"), Some(&Value::Float(135.0)));

    let disc = item("minecraft:music_disc_13", 0);
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, jukebox).unwrap();
    assert_eq!(t.set_record(&sticks), Err(Error::Invalid), "not a record");
    t.set_record(&disc).unwrap();
    assert_eq!(t.view().record(), Some(&disc));
    assert!(h.actor_data(jukebox)[0].contains_key("RecordItem"));
    h.reload(&[pot, frame, jukebox]);
    let cx = h.cx();
    assert_eq!(
        cx.block_entity(WORLD, pot).unwrap().potted_plant(),
        Some(dandelion)
    );
    assert_eq!(
        cx.block_entity(WORLD, frame).unwrap().frame_drop_chance(),
        Some(0.25)
    );
    assert_eq!(
        cx.block_entity(WORLD, jukebox).unwrap().record(),
        Some(&disc)
    );
}

// pmmp: block\Lectern::getBook, block\tile\Lectern::getBook, block\Lectern::setBook, block\tile\Lectern::setBook, block\Lectern::getViewedPage, block\tile\Lectern::getViewedPage, block\Lectern::setViewedPage, block\tile\Lectern::setViewedPage, block\ChiseledBookshelf::getLastInteractedSlot, block\tile\ChiseledBookshelf::getLastInteractedSlot, block\ChiseledBookshelf::setLastInteractedSlot, block\tile\ChiseledBookshelf::setLastInteractedSlot
#[test]
fn lecterns_and_bookshelves_keep_their_books_place() {
    let mut h = Harness::new();
    let (lectern, shelf) = (h.spot(1), h.spot(3));
    h.place(lectern, block::Kind::Lectern.default_state());
    h.place(shelf, block::Kind::ChiseledBookshelf.default_state());
    let pages = Compound::from([(
        "pages".to_owned(),
        Value::List(crate::nbt::List::Compound(vec![Compound::new(); 3])),
    )]);
    let book = Stack::new(item::Kind::WritableBook.default_type(), 2)
        .with_extra(Some(item::Extra {
            nbt: pages,
            ..item::Extra::default()
        }))
        .unwrap();
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, lectern).unwrap();
    assert_eq!(t.set_lectern_page(0), Err(Error::Invalid), "no book");
    assert_eq!(
        t.set_lectern_book(&Stack::new(item::Kind::Stick.default_type(), 1)),
        Err(Error::Invalid)
    );
    t.set_lectern_book(&book).unwrap();
    t.set_lectern_page(2).unwrap();
    assert_eq!(
        t.set_lectern_page(3),
        Err(Error::Invalid),
        "past the last page"
    );
    assert_eq!(t.view().lectern_book(), Some(&book.with_count(1)));
    assert_eq!(t.view().lectern_page(), Some(2));
    let sent = h.actor_data(lectern);
    assert_eq!(sent[0].get("page"), Some(&Value::Int(2)));
    assert_eq!(sent[0].get("hasBook"), Some(&Value::Byte(1)));

    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, shelf).unwrap();
    assert_eq!(t.view().last_interacted_slot(), None);
    t.set_last_interacted_slot(Some(5)).unwrap();
    assert_eq!(t.set_last_interacted_slot(Some(6)), Err(Error::Invalid));
    assert_eq!(t.view().last_interacted_slot(), Some(5));
    assert!(
        h.actor_data(shelf).is_empty(),
        "clients get no bookshelf data"
    );
    h.reload(&[lectern, shelf]);
    let mut cx = h.cx();
    assert_eq!(
        cx.block_entity(WORLD, lectern).unwrap().lectern_page(),
        Some(2)
    );
    let mut t = cx.block_entity_mut(WORLD, shelf).unwrap();
    assert_eq!(t.view().last_interacted_slot(), Some(5));
    t.set_last_interacted_slot(None).unwrap();
    assert_eq!(t.view().last_interacted_slot(), None);
}

// pmmp: block\Campfire::getCookingTime, block\Campfire::setCookingTime, block\tile\Beacon::getPrimaryEffect, block\tile\Beacon::getSecondaryEffect, block\tile\Beacon::setPrimaryEffect, block\tile\Beacon::setSecondaryEffect, PotionCauldron::getPotionItem, block\tile\Cauldron::getPotionItem, PotionCauldron::setPotionItem, block\tile\Cauldron::setPotionItem, WaterCauldron::getCustomWaterColor, block\tile\Cauldron::getCustomWaterColor, WaterCauldron::setCustomWaterColor, block\tile\Cauldron::setCustomWaterColor
#[test]
fn campfires_beacons_and_cauldrons_take_their_values() {
    let mut h = Harness::new();
    let (fire, beacon, cauldron) = (h.spot(1), h.spot(3), h.spot(5));
    h.place(fire, block::Kind::Campfire.default_state());
    h.place(beacon, block::Kind::Beacon.default_state());
    h.place(cauldron, block::Kind::Cauldron.default_state());
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, fire).unwrap();
    assert_eq!(t.view().cooking_time_ticks(0), Some(0));
    t.set_cooking_time_ticks(2, 100).unwrap();
    assert_eq!(
        t.set_cooking_time_ticks(4, 1),
        Err(Error::Invalid),
        "no fifth slot"
    );
    assert_eq!(
        t.set_cooking_time_ticks(0, u16::MAX),
        Err(Error::Invalid),
        "past the cook time"
    );
    assert_eq!(t.view().cooking_time_ticks(2), Some(100));
    assert_eq!(t.view().cooking_time_ticks(4), None);

    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, beacon).unwrap();
    assert_eq!(t.view().beacon_effects(), Some((None, None)));
    t.set_beacon_effects(Some(Effect::Speed), Some(Effect::Regeneration))
        .unwrap();
    assert_eq!(
        t.set_beacon_effects(Some(Effect::Poison), None),
        Err(Error::Invalid),
        "not a beacon effect"
    );
    assert_eq!(
        t.view().beacon_effects(),
        Some((Some(Effect::Speed), Some(Effect::Regeneration)))
    );
    assert_eq!(h.actor_data(beacon)[0].get("primary"), Some(&Value::Int(1)));

    let splash = item("minecraft:splash_potion", 5);
    let mut cx = h.cx();
    let mut t = cx.block_entity_mut(WORLD, cauldron).unwrap();
    assert_eq!(t.view().cauldron_potion(), Some(Stack::EMPTY));
    assert_eq!(t.view().cauldron_water_colour(), None);
    assert_eq!(
        t.set_cauldron_potion(&Stack::new(item::Kind::Stick.default_type(), 1)),
        Err(Error::Invalid)
    );
    t.set_cauldron_potion(&splash).unwrap();
    let teal = Rgba {
        r: 0x12,
        g: 0x34,
        b: 0x56,
        a: 0xff,
    };
    t.set_cauldron_water_colour(Some(teal)).unwrap();
    assert_eq!(t.view().cauldron_potion(), Some(splash.clone()));
    assert_eq!(t.view().cauldron_water_colour(), Some(teal));
    let sent = h.actor_data(cauldron);
    assert_eq!(sent[0].get("PotionType"), Some(&Value::Short(1)));
    assert_eq!(sent[0].get("PotionId"), Some(&Value::Short(5)));
    h.reload(&[fire, beacon, cauldron]);
    let mut cx = h.cx();
    assert_eq!(
        cx.block_entity(WORLD, cauldron).unwrap().cauldron_potion(),
        Some(splash)
    );
    assert_eq!(
        cx.block_entity(WORLD, beacon).unwrap().beacon_effects(),
        Some((Some(Effect::Speed), Some(Effect::Regeneration)))
    );
    let mut t = cx.block_entity_mut(WORLD, cauldron).unwrap();
    t.set_cauldron_potion(&Stack::EMPTY).unwrap();
    t.set_cauldron_water_colour(None).unwrap();
    assert_eq!(t.view().cauldron_potion(), Some(Stack::EMPTY));
    assert_eq!(t.view().cauldron_water_colour(), None);
}

#[test]
fn a_write_twice_leaves_the_same_state_as_once() {
    let mut h = Harness::new();
    let pos = h.spot(1);
    h.place(pos, block::Kind::OakSign.default_state());
    let once = |h: &mut Harness| {
        let mut cx = h.cx();
        let mut t = cx.block_entity_mut(WORLD, pos).unwrap();
        t.set_sign_text(Side::Front, text(["a", "b", "", ""], 0xff00_0000))
            .unwrap();
        t.set_sign_waxed(true).unwrap();
        h.state.hash()
    };
    let first = once(&mut h);
    assert_eq!(once(&mut h), first);
}

/// `/status` counts the block entities of a world's loaded chunks, across chunks, and none for
/// another world.
#[test]
fn block_entities_are_counted_per_world_across_chunks() {
    let mut h = Harness::new();
    assert_eq!(h.state.rules.block_entities.count(0), 0);
    let chest = block::Kind::Chest.default_state();
    // Two chunks: x 1 and 3 share chunk 0, x -1 is in chunk -1.
    for dx in [1, 3, -1] {
        let pos = h.spot(dx);
        h.place(pos, chest);
    }
    assert_eq!(h.state.rules.block_entities.count(0), 3);
    assert_eq!(h.state.rules.block_entities.count(1), 0);
}
