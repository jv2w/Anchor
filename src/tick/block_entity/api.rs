//! The public block entity API: [`Context::block_entity`] lends a [`Tile`] to read and
//! [`Context::block_entity_mut`] a [`Mut`] to change.
//!
//! A read looks the block entity up by position (two ordered-map steps) each time, so a [`Tile`]
//! holds no borrow of the store's insides. A write checks its value, changes the block entity (its
//! chunk is saved at the next commit) and marks its client data, which the players who see the
//! chunk get once at the end of the tick (PMMP sends a tile's spawn compound when it changes).
//! Writes raise no events: a direct API call never re-enters the listeners.

use super::{
    BOOKSHELF_SLOTS, BlockEntity, FRAME_ROTATIONS, Kind, Plant, beacon, beacon_effect_allowed,
    block_at, campfire_kind, chests_pair, decor, mark_spawn, set_beacon,
};
use crate::block::{self, Face, TypeTag};
use crate::effect;
use crate::item::{self, Stack};
use crate::nbt::Compound;
use crate::protocol::Rgba;
use crate::tick::interact::book::page_count;
use crate::tick::{Context, State, inventory};
use crate::tile::{
    self, BannerPattern, BannerType, DyeColour, Error, MAX_NAME_BYTES, Mut, Result, Side, SignText,
    Tile,
};
use crate::world::{self, BlockPos, ChunkPos};

/// The potion item names a cauldron's potion container type stands for (0, 1, 2).
const POTIONS: [&str; 3] = [
    "minecraft:potion",
    "minecraft:splash_potion",
    "minecraft:lingering_potion",
];

impl Context<'_> {
    /// The block entity at `pos` in `world` to read; `None` if its chunk is not loaded or the
    /// block has none (a block [`World::set_block`](crate::world::World::set_block) placed gets
    /// its block entity in the tick's next block update phase).
    #[doc(alias = "World::getTile")]
    #[doc(alias = "World::getTileAt")]
    #[doc(alias = "Chunk::getTile")]
    pub fn block_entity(&self, world: world::Id, pos: BlockPos) -> Option<Tile<'_>> {
        let world = world.index();
        self.state.rules.block_entities.get(world, pos)?;
        Some(Tile {
            state: self.state,
            world,
            pos,
        })
    }

    /// The block entity at `pos` in `world` to change; `None` where
    /// [`block_entity`](Self::block_entity) is.
    pub fn block_entity_mut(&mut self, world: world::Id, pos: BlockPos) -> Option<Mut<'_>> {
        let world = world.index();
        self.state.rules.block_entities.get(world, pos)?;
        Some(Mut {
            state: self.state,
            world,
            pos,
        })
    }

    /// The block entities of a loaded chunk of `world`, by position; none if it is not loaded.
    #[doc(alias = "Chunk::getTiles")]
    pub fn chunk_block_entities(
        &self,
        world: world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = Tile<'_>> {
        let world = world.index();
        let state: &State = self.state;
        state
            .rules
            .block_entities
            .positions(world, chunk)
            .map(move |pos| Tile { state, world, pos })
    }
}

impl<'a> Tile<'a> {
    /// The world it is in.
    pub fn world(&self) -> world::Id {
        world::Id::of(self.world)
    }

    /// Its position.
    #[doc(alias = "Tile::getPosition")]
    pub fn position(&self) -> BlockPos {
        self.pos
    }

    /// Its block; `None` for a block Anchor does not know (an imported block it keeps without
    /// rules, whose block entity it keeps as saved).
    #[doc(alias = "Tile::getBlock")]
    pub fn block(&self) -> Option<block::State> {
        block_at(self.state, self.world, self.pos)
    }

    /// The custom name of a container (from a renamed item), shown as its window's title; `None`
    /// without one (the client shows its own name for the block).
    #[doc(alias = "Nameable::getName")]
    #[doc(alias = "NameableTrait::getName")]
    pub fn name(&self) -> Option<&'a str> {
        self.entity()?.name.as_deref()
    }

    /// Whether a held item named `key` opens it: true without a `Lock` (PMMP
    /// `Container::canOpenWith`).
    #[doc(alias = "Container::canOpenWith")]
    #[doc(alias = "ContainerTrait::canOpenWith")]
    pub fn can_open_with(&self, key: &str) -> bool {
        self.entity()
            .and_then(BlockEntity::lock)
            .is_none_or(|lock| lock == key)
    }

    /// How many players have a window open on it.
    #[doc(alias = "block\\tile\\EnderChest::getViewerCount")]
    pub fn viewer_count(&self) -> usize {
        self.state
            .rules
            .block_entities
            .viewers(self.world, self.pos)
            .len()
    }

    /// The other half of a double chest; `None` for a single chest or another kind.
    #[doc(alias = "block\\tile\\Chest::getPair")]
    pub fn chest_pair(&self) -> Option<BlockPos> {
        self.entity()?.pair()
    }

    /// The face a shulker box opens toward.
    #[doc(alias = "block\\tile\\ShulkerBox::getFacing")]
    pub fn shulker_facing(&self) -> Option<Face> {
        match self.kind()? {
            Kind::ShulkerBox { facing } => Face::ALL.get(usize::from(*facing)).copied(),
            _ => None,
        }
    }

    /// Ticks the item in campfire slot `slot` (0 to 3) has cooked.
    #[doc(alias = "block\\Campfire::getCookingTime")]
    pub fn cooking_time_ticks(&self, slot: usize) -> Option<u16> {
        match self.kind()? {
            Kind::Campfire(c) => c.times.get(slot).copied(),
            _ => None,
        }
    }

    /// A beacon's primary and secondary effects, each `None` if not chosen.
    #[doc(alias = "block\\tile\\Beacon::getPrimaryEffect")]
    #[doc(alias = "block\\tile\\Beacon::getSecondaryEffect")]
    pub fn beacon_effects(&self) -> Option<(Option<effect::Type>, Option<effect::Type>)> {
        match self.kind()? {
            Kind::Beacon { primary, secondary } => Some((
                effect::Type::from_id(*primary),
                effect::Type::from_id(*secondary),
            )),
            _ => None,
        }
    }

    /// The text of one side of a sign.
    #[doc(alias = "BaseSign::getText")]
    #[doc(alias = "BaseSign::getFaceText")]
    #[doc(alias = "Sign::getText")]
    #[doc(alias = "Sign::getBackText")]
    pub fn sign_text(&self, side: Side) -> Option<&'a SignText> {
        match self.kind()? {
            Kind::Sign(s) => Some(match side {
                Side::Front => &s.front,
                Side::Back => &s.back,
            }),
            _ => None,
        }
    }

    /// Whether a sign is waxed (players cannot edit it); false for other kinds.
    #[doc(alias = "BaseSign::isWaxed")]
    #[doc(alias = "Sign::isWaxed")]
    pub fn is_sign_waxed(&self) -> bool {
        matches!(self.kind(), Some(Kind::Sign(s)) if s.waxed)
    }

    /// A bed half's colour (each half of a bed has its own block entity).
    #[doc(alias = "block\\Bed::getColor")]
    #[doc(alias = "block\\tile\\Bed::getColor")]
    pub fn bed_colour(&self) -> Option<DyeColour> {
        match self.kind()? {
            Kind::Bed { colour } => DyeColour::from_id(*colour),
            _ => None,
        }
    }

    /// A banner's base colour.
    #[doc(alias = "BaseBanner::getColor")]
    #[doc(alias = "block\\tile\\Banner::getBaseColor")]
    pub fn banner_colour(&self) -> Option<DyeColour> {
        match self.kind()? {
            Kind::Banner(b) => DyeColour::from_banner_id(b.base),
            _ => None,
        }
    }

    /// A banner's pattern layers, bottom first, without the saved ones Anchor does not know (kept
    /// and shown as they came).
    #[doc(alias = "BaseBanner::getPatterns")]
    #[doc(alias = "block\\tile\\Banner::getPatterns")]
    pub fn banner_patterns(&self) -> Option<impl Iterator<Item = BannerPattern> + 'a> {
        match self.kind()? {
            Kind::Banner(b) => Some(b.patterns.iter().filter_map(|(id, colour)| {
                Some(BannerPattern::new(
                    block::BannerPattern::from_id(id)?,
                    DyeColour::from_banner_id(*colour)?,
                ))
            })),
            _ => None,
        }
    }

    /// Whether a banner is ominous.
    #[doc(alias = "block\\tile\\Banner::getType")]
    pub fn banner_type(&self) -> Option<BannerType> {
        match self.kind()? {
            Kind::Banner(b) if b.ty == 1 => Some(BannerType::Ominous),
            Kind::Banner(_) => Some(BannerType::Normal),
            _ => None,
        }
    }

    /// The plant in a flower pot; `None` for an empty pot, a saved plant Anchor does not know
    /// (kept and shown as it came) or another kind.
    #[doc(alias = "block\\FlowerPot::getPlant")]
    #[doc(alias = "block\\tile\\FlowerPot::getPlant")]
    pub fn potted_plant(&self) -> Option<block::State> {
        match self.kind()? {
            Kind::FlowerPot {
                plant: Some(Plant::Known(plant)),
            } => Some(*plant),
            _ => None,
        }
    }

    /// The item in an item frame (empty when none).
    #[doc(alias = "block\\ItemFrame::getFramedItem")]
    #[doc(alias = "block\\tile\\ItemFrame::getItem")]
    pub fn framed_item(&self) -> Option<&'a Stack> {
        match self.kind()? {
            Kind::ItemFrame(f) => Some(&f.item),
            _ => None,
        }
    }

    /// The rotation of an item frame's item, in eighths of a turn (0 to 7).
    #[doc(alias = "block\\ItemFrame::getItemRotation")]
    #[doc(alias = "block\\tile\\ItemFrame::getItemRotation")]
    pub fn frame_rotation(&self) -> Option<u8> {
        match self.kind()? {
            Kind::ItemFrame(f) => Some(f.rotation),
            _ => None,
        }
    }

    /// The chance (0 to 1) that an item frame's item drops when hit or broken.
    #[doc(alias = "block\\ItemFrame::getItemDropChance")]
    #[doc(alias = "block\\tile\\ItemFrame::getItemDropChance")]
    pub fn frame_drop_chance(&self) -> Option<f32> {
        match self.kind()? {
            Kind::ItemFrame(f) => Some(f.drop_chance),
            _ => None,
        }
    }

    /// The record in a jukebox (empty when none).
    #[doc(alias = "block\\Jukebox::getRecord")]
    #[doc(alias = "block\\tile\\Jukebox::getRecord")]
    pub fn record(&self) -> Option<&'a Stack> {
        match self.kind()? {
            Kind::Jukebox { record } => Some(record),
            _ => None,
        }
    }

    /// The book on a lectern (empty when none).
    #[doc(alias = "block\\Lectern::getBook")]
    #[doc(alias = "block\\tile\\Lectern::getBook")]
    pub fn lectern_book(&self) -> Option<&'a Stack> {
        match self.kind()? {
            Kind::Lectern { book, .. } => Some(book),
            _ => None,
        }
    }

    /// The page a lectern's book is open at.
    #[doc(alias = "block\\Lectern::getViewedPage")]
    #[doc(alias = "block\\tile\\Lectern::getViewedPage")]
    pub fn lectern_page(&self) -> Option<u32> {
        match self.kind()? {
            Kind::Lectern { page, .. } => u32::try_from(*page).ok(),
            _ => None,
        }
    }

    /// The chiseled bookshelf slot (0 to 5) a player used last; `None` for none or another kind.
    #[doc(alias = "block\\ChiseledBookshelf::getLastInteractedSlot")]
    #[doc(alias = "block\\tile\\ChiseledBookshelf::getLastInteractedSlot")]
    pub fn last_interacted_slot(&self) -> Option<u8> {
        match self.kind()? {
            Kind::ChiseledBookshelf { last } => u8::try_from(last.checked_sub(1)?).ok(),
            _ => None,
        }
    }

    /// A mob head's rotation on the floor, in sixteenths of a turn (0 to 15).
    #[doc(alias = "block\\MobHead::getRotation")]
    #[doc(alias = "block\\tile\\MobHead::getRotation")]
    pub fn skull_rotation(&self) -> Option<u8> {
        match self.kind()? {
            Kind::Skull { rotation, .. } => Some(*rotation),
            _ => None,
        }
    }

    /// The potion in a cauldron, as one potion, splash potion or lingering potion item (empty
    /// when none).
    #[doc(alias = "PotionCauldron::getPotionItem")]
    #[doc(alias = "block\\tile\\Cauldron::getPotionItem")]
    pub fn cauldron_potion(&self) -> Option<Stack> {
        match self.kind()? {
            Kind::Cauldron(c) => {
                let potion = usize::try_from(c.container)
                    .ok()
                    .and_then(|i| POTIONS.get(i))
                    .zip(u16::try_from(c.potion).ok())
                    .and_then(|(name, meta)| {
                        item::Type::from_name(name, meta, None, &Compound::new())
                    });
                Some(potion.map_or(Stack::EMPTY, |t| Stack::new(t, 1)))
            }
            _ => None,
        }
    }

    /// The dyed colour of a cauldron's water; `None` for none or another kind.
    #[doc(alias = "WaterCauldron::getCustomWaterColor")]
    #[doc(alias = "block\\tile\\Cauldron::getCustomWaterColor")]
    pub fn cauldron_water_colour(&self) -> Option<Rgba> {
        match self.kind()? {
            Kind::Cauldron(c) => c.colour.map(tile::from_argb),
            _ => None,
        }
    }

    /// The block entity (present: the state is borrowed since it was found).
    fn entity(&self) -> Option<&'a BlockEntity> {
        self.state.rules.block_entities.get(self.world, self.pos)
    }

    /// The kind of the block entity, with its data.
    fn kind(&self) -> Option<&'a Kind> {
        self.entity().map(|e| &e.kind)
    }
}

impl Mut<'_> {
    /// The block entity to read.
    pub fn view(&self) -> Tile<'_> {
        Tile {
            state: self.state,
            world: self.world,
            pos: self.pos,
        }
    }

    /// Names a container (`None` or an empty name clears it): a chest, a barrel, a shulker box,
    /// a hopper, a furnace, a brewing stand or an enchanting table. A window open on it keeps
    /// its old title.
    #[doc(alias = "Nameable::setName")]
    #[doc(alias = "NameableTrait::setName")]
    pub fn set_name(&mut self, name: Option<&str>) -> Result<()> {
        let name = name.filter(|n| !n.is_empty());
        if name.is_some_and(|n| n.len() > MAX_NAME_BYTES) {
            return Err(Error::TooLong);
        }
        self.edit(|e| {
            e.kind.is_nameable().then(|| {
                e.name = name.map(str::to_owned);
                Ok(())
            })
        })
    }

    /// Pairs a single chest with the single chest at `other` into a double chest: `other` must
    /// be the same block, facing the same way, beside it on its left or right (as a placed chest
    /// pairs). The windows open on either close first.
    #[doc(alias = "block\\tile\\Chest::pairWith")]
    pub fn pair_with(&mut self, other: BlockPos) -> Result<()> {
        let (world, pos) = (self.world, self.pos);
        let state: &State = self.state;
        let single = |at| match state.rules.block_entities.get(world, at).map(|e| &e.kind) {
            Some(Kind::Chest { pair: None }) => Ok(()),
            Some(Kind::Chest { .. }) => Err(Error::Invalid),
            _ => Err(Error::WrongKind),
        };
        single(pos)?;
        single(other)?;
        if !chests_pair(state, world, pos, other) {
            return Err(Error::Invalid);
        }
        self.set_pairs([(pos, Some(other)), (other, Some(pos))]);
        Ok(())
    }

    /// Splits a double chest into two single chests; `Invalid` for a single chest, or while the
    /// other half's chunk is not loaded (its saved half would stay paired). The windows open on
    /// either close first.
    #[doc(alias = "block\\tile\\Chest::unpair")]
    pub fn unpair(&mut self) -> Result<()> {
        let (world, pos) = (self.world, self.pos);
        let entities = &self.state.rules.block_entities;
        let other = match entities.get(world, pos).map(|e| &e.kind) {
            Some(Kind::Chest { pair: Some(other) }) => *other,
            Some(Kind::Chest { pair: None }) => return Err(Error::Invalid),
            _ => return Err(Error::WrongKind),
        };
        if !entities.is_loaded(world, other) {
            return Err(Error::Invalid);
        }
        self.set_pairs([(pos, None), (other, None)]);
        Ok(())
    }

    /// Turns a shulker box to open toward `facing`.
    #[doc(alias = "block\\tile\\ShulkerBox::setFacing")]
    pub fn set_shulker_facing(&mut self, facing: Face) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::ShulkerBox { facing: f } => {
                *f = facing.index();
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Sets how long the item in campfire slot `slot` (0 to 3) has cooked, up to the campfire's
    /// cook time (the item cooks at its next update once the time is reached). `Invalid` for an
    /// empty slot: a slot's time starts at 0 when an item goes in.
    #[doc(alias = "block\\Campfire::setCookingTime")]
    pub fn set_cooking_time_ticks(&mut self, slot: usize, ticks: u16) -> Result<()> {
        let most = self.view().block().map(|b| campfire_kind(b).cook_ticks());
        self.edit(|e| match &mut e.kind {
            Kind::Campfire(c) => Some(match (c.times.get_mut(slot), e.items.get(slot), most) {
                (Some(t), Some(item), Some(most)) if !item.is_empty() && ticks <= most => {
                    *t = ticks;
                    Ok(())
                }
                _ => Err(Error::Invalid),
            }),
            _ => None,
        })
    }

    /// Chooses a beacon's effects (each a beacon effect or `None`); the pyramid under it decides
    /// which it gives.
    #[doc(alias = "block\\tile\\Beacon::setPrimaryEffect")]
    #[doc(alias = "block\\tile\\Beacon::setSecondaryEffect")]
    pub fn set_beacon_effects(
        &mut self,
        primary: Option<effect::Type>,
        secondary: Option<effect::Type>,
    ) -> Result<()> {
        let id = |e: Option<effect::Type>| e.map_or(0, |t| i32::from(t.id()));
        let (primary, secondary) = (id(primary), id(secondary));
        if !matches!(self.view().kind(), Some(Kind::Beacon { .. })) {
            return Err(Error::WrongKind);
        }
        if !beacon_effect_allowed(primary, beacon::MAX_LEVEL)
            || !beacon_effect_allowed(secondary, beacon::MAX_LEVEL)
        {
            return Err(Error::Invalid);
        }
        set_beacon(self.state, self.world, self.pos, primary, secondary);
        Ok(())
    }

    /// Writes one side of a sign (a waxed sign takes it too: only players are kept out).
    #[doc(alias = "BaseSign::setText")]
    #[doc(alias = "BaseSign::setFaceText")]
    #[doc(alias = "Sign::setText")]
    #[doc(alias = "Sign::setBackText")]
    pub fn set_sign_text(&mut self, side: Side, text: SignText) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Sign(s) => {
                *s.side_mut(side == Side::Front) = text;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Waxes a sign so players cannot edit it, or takes the wax off.
    #[doc(alias = "BaseSign::setWaxed")]
    #[doc(alias = "Sign::setWaxed")]
    pub fn set_sign_waxed(&mut self, waxed: bool) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Sign(s) => {
                s.waxed = waxed;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Colours a bed half (each half of a bed has its own block entity; a broken half drops a
    /// bed of its colour).
    #[doc(alias = "block\\Bed::setColor")]
    #[doc(alias = "block\\tile\\Bed::setColor")]
    pub fn set_bed_colour(&mut self, colour: DyeColour) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Bed { colour: c } => {
                *c = colour as u8;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Sets a banner's base colour.
    #[doc(alias = "BaseBanner::setColor")]
    #[doc(alias = "block\\tile\\Banner::setBaseColor")]
    pub fn set_banner_colour(&mut self, colour: DyeColour) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Banner(b) => {
                b.base = colour.banner_id();
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Replaces every pattern layer of a banner, even those Anchor does not know, bottom first
    /// (at most 16).
    #[doc(alias = "BaseBanner::setPatterns")]
    #[doc(alias = "block\\tile\\Banner::setPatterns")]
    pub fn set_banner_patterns(&mut self, patterns: &[BannerPattern]) -> Result<()> {
        if patterns.len() > decor::MAX_PATTERNS {
            return Err(Error::TooLong);
        }
        self.edit(|e| match &mut e.kind {
            Kind::Banner(b) => {
                b.patterns = patterns
                    .iter()
                    .map(|p| (p.pattern().id().to_owned(), p.colour().banner_id()))
                    .collect();
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Makes a banner normal or ominous.
    #[doc(alias = "block\\tile\\Banner::setType")]
    pub fn set_banner_type(&mut self, ty: BannerType) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Banner(b) => {
                b.ty = i32::from(ty == BannerType::Ominous);
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Puts a plant into a flower pot (`None` empties it): a block the pot takes (PMMP
    /// `FlowerPot::isValidPlant`: the pottable type tag). The plant before, even one Anchor does
    /// not know, is replaced and not dropped.
    #[doc(alias = "block\\FlowerPot::setPlant")]
    #[doc(alias = "block\\tile\\FlowerPot::setPlant")]
    pub fn set_potted_plant(&mut self, plant: Option<block::State>) -> Result<()> {
        if plant.is_some_and(|p| !p.has_type_tag(TypeTag::Pottable)) {
            return Err(Error::Invalid);
        }
        self.edit(|e| match &mut e.kind {
            Kind::FlowerPot { plant: p } => {
                *p = plant.map(Plant::Known);
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Puts one of `item` into an item frame (an empty stack empties it and turns the frame back
    /// to rotation 0, as taking the item out does). The item before is not dropped.
    #[doc(alias = "block\\ItemFrame::setFramedItem")]
    #[doc(alias = "block\\tile\\ItemFrame::setItem")]
    pub fn set_framed_item(&mut self, item: &Stack) -> Result<()> {
        let one = item.with_count(1);
        self.edit(|e| match &mut e.kind {
            Kind::ItemFrame(f) => {
                if one.is_empty() {
                    f.rotation = 0;
                }
                f.item = one;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Turns an item frame's item to `rotation` eighths of a turn (0 to 7).
    #[doc(alias = "block\\ItemFrame::setItemRotation")]
    #[doc(alias = "block\\tile\\ItemFrame::setItemRotation")]
    pub fn set_frame_rotation(&mut self, rotation: u8) -> Result<()> {
        if rotation >= FRAME_ROTATIONS {
            return Err(Error::Invalid);
        }
        self.edit(|e| match &mut e.kind {
            Kind::ItemFrame(f) => {
                f.rotation = rotation;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Sets the chance (0 to 1) that an item frame's item drops when hit or broken.
    #[doc(alias = "block\\ItemFrame::setItemDropChance")]
    #[doc(alias = "block\\tile\\ItemFrame::setItemDropChance")]
    pub fn set_frame_drop_chance(&mut self, chance: f32) -> Result<()> {
        if !(0.0..=1.0).contains(&chance) {
            return Err(Error::Invalid);
        }
        self.edit(|e| match &mut e.kind {
            Kind::ItemFrame(f) => {
                f.drop_chance = chance;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Puts one of a record into a jukebox (an empty stack empties it). The record before is not
    /// dropped. Only the jukebox's data changes: Anchor sends no record sound.
    #[doc(alias = "block\\tile\\Jukebox::setRecord")]
    pub fn set_record(&mut self, record: &Stack) -> Result<()> {
        if !record.is_empty() && !record.item().is_some_and(|t| t.is(item::Class::Record)) {
            return Err(Error::Invalid);
        }
        let one = record.with_count(1);
        self.edit(|e| match &mut e.kind {
            Kind::Jukebox { record } => {
                *record = one;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Puts one of a writable or written book on a lectern, open at its first page (an empty
    /// stack takes the book off). The book before is not dropped.
    #[doc(alias = "block\\Lectern::setBook")]
    #[doc(alias = "block\\tile\\Lectern::setBook")]
    pub fn set_lectern_book(&mut self, book: &Stack) -> Result<()> {
        let is_book = book
            .item()
            .is_some_and(|t| t.is(item::Class::WritableBookBase));
        if !book.is_empty() && !is_book {
            return Err(Error::Invalid);
        }
        let one = book.with_count(1);
        self.edit(|e| match &mut e.kind {
            Kind::Lectern { book, page } => {
                *book = one;
                *page = 0;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Opens a lectern's book at `page`, a page the book has (the first page of a book without
    /// pages).
    #[doc(alias = "block\\Lectern::setViewedPage")]
    #[doc(alias = "block\\tile\\Lectern::setViewedPage")]
    pub fn set_lectern_page(&mut self, page: u32) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Lectern { book, page: p } => Some({
                let pages = page_count(book).max(1);
                match i32::try_from(page) {
                    Ok(at)
                        if !book.is_empty() && usize::try_from(page).is_ok_and(|i| i < pages) =>
                    {
                        *p = at;
                        Ok(())
                    }
                    _ => Err(Error::Invalid),
                }
            }),
            _ => None,
        })
    }

    /// Sets the chiseled bookshelf slot (0 to 5) used last, or none.
    #[doc(alias = "block\\ChiseledBookshelf::setLastInteractedSlot")]
    #[doc(alias = "block\\tile\\ChiseledBookshelf::setLastInteractedSlot")]
    pub fn set_last_interacted_slot(&mut self, slot: Option<u8>) -> Result<()> {
        let last = match slot {
            None => 0,
            Some(s) if usize::from(s) < BOOKSHELF_SLOTS => i32::from(s) + 1,
            Some(_) => return Err(Error::Invalid),
        };
        self.edit(|e| match &mut e.kind {
            Kind::ChiseledBookshelf { last: l } => {
                *l = last;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Turns a mob head on the floor to `rotation` sixteenths of a turn (0 to 15).
    #[doc(alias = "block\\MobHead::setRotation")]
    #[doc(alias = "block\\tile\\MobHead::setRotation")]
    pub fn set_skull_rotation(&mut self, rotation: u8) -> Result<()> {
        if rotation >= decor::SKULL_ROTATIONS {
            return Err(Error::Invalid);
        }
        self.edit(|e| match &mut e.kind {
            Kind::Skull { rotation: r, .. } => {
                *r = rotation;
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Fills a cauldron's potion from a potion, splash potion or lingering potion item (an empty
    /// stack empties it). The cauldron block's fill level is the block's.
    #[doc(alias = "PotionCauldron::setPotionItem")]
    #[doc(alias = "block\\tile\\Cauldron::setPotionItem")]
    pub fn set_cauldron_potion(&mut self, potion: &Stack) -> Result<()> {
        let (container, id) = match potion.item() {
            None => (-1, -1),
            Some(t) => {
                let container = POTIONS
                    .iter()
                    .position(|n| *n == t.name())
                    .and_then(|i| i16::try_from(i).ok())
                    .ok_or(Error::Invalid)?;
                (
                    container,
                    i16::try_from(t.meta()).map_err(|_| Error::Invalid)?,
                )
            }
        };
        self.edit(|e| match &mut e.kind {
            Kind::Cauldron(c) => {
                (c.container, c.potion) = (container, id);
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Dyes a cauldron's water `colour`, or clears the dye.
    #[doc(alias = "WaterCauldron::setCustomWaterColor")]
    #[doc(alias = "block\\tile\\Cauldron::setCustomWaterColor")]
    pub fn set_cauldron_water_colour(&mut self, colour: Option<Rgba>) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Cauldron(c) => {
                c.colour = colour.map(tile::to_argb);
                Some(Ok(()))
            }
            _ => None,
        })
    }

    /// Changes the block entity with `f` (`None`: not its kind, nothing changed), then marks its
    /// chunk changed and its client data for the players who see it.
    fn edit(&mut self, f: impl FnOnce(&mut BlockEntity) -> Option<Result<()>>) -> Result<()> {
        let (world, pos) = (self.world, self.pos);
        let mut outcome = Err(Error::WrongKind);
        self.state.rules.block_entities.update(world, pos, |e| {
            outcome = f(e)?;
            outcome.ok()
        });
        outcome?;
        mark_spawn(self.state, world, pos);
        Ok(())
    }

    /// Sets the pair of two chests, after closing the windows on them (a window's slots are the
    /// halves it opened with); both get their client data.
    fn set_pairs(&mut self, pairs: [(BlockPos, Option<BlockPos>); 2]) {
        let world = self.world;
        for (at, _) in pairs {
            let viewers = self.state.rules.block_entities.viewers(world, at).to_vec();
            for session in viewers {
                inventory::close_window_without_event(self.state, session);
            }
        }
        for (at, with) in pairs {
            let set = self
                .state
                .rules
                .block_entities
                .update(world, at, |e| match &mut e.kind {
                    Kind::Chest { pair } => {
                        *pair = with;
                        Some(())
                    }
                    _ => None,
                });
            if set.is_some() {
                mark_spawn(self.state, world, at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Each write changes the block entity, reaches the players who see it as `BlockActorData`
    //! and is in the chunk's saved form.

    use tokio::sync::mpsc::Receiver;

    use super::*;
    use crate::effect::Type as Effect;
    use crate::event::Listeners;
    use crate::nbt::Value;
    use crate::pool::Pool;
    use crate::protocol::packet::Packet;
    use crate::save::Response;
    use crate::tick::block_entity::{beside, cardinal, placed, send_marked};
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{answer, join, new_state, radius, settings};
    use crate::tick::{InputLog, Output, SessionId, boundary, deliver, step};

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
            let loaded: Vec<_> = state.worlds[0].loaded_chunks().collect();
            for pos in loaded {
                let extras = state
                    .chunks
                    .extras
                    .get((0, pos))
                    .cloned()
                    .unwrap_or_default();
                let world = &state.worlds[0];
                let read = state
                    .rules
                    .read((0, pos), &extras, world.chunk(pos).unwrap(), world);
                state.rules.load((0, pos), read, &extras);
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

        /// The block entity data the player was sent for `pos` by the end of the tick, the last
        /// first.
        fn actor_data(&mut self, pos: BlockPos) -> Vec<Compound> {
            send_marked(&mut self.state);
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

        /// Sets `block` at `pos` and makes its block entity as a placement does, ending the tick
        /// of the placement so that only later writes leave a mark.
        fn place(&mut self, pos: BlockPos, block: block::State) {
            self.state.worlds[0].set_block(pos, block).unwrap();
            placed(&mut self.state, 0, pos, None, &Stack::EMPTY, Face::Up);
            send_marked(&mut self.state);
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
                entities.load_list(key, list, &self.state.worlds[0], &self.state.custom);
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

    // pmmp: World::setBlock
    /// A block with a block entity that a task sets gets an empty one in the tick's block update
    /// phase, as a placed one does, without packets: its text and slots can be set; a block that
    /// replaces it with another kind's swaps it, and air takes it away.
    #[test]
    fn a_block_a_task_sets_gets_its_block_entity_in_the_block_updates() {
        let mut h = Harness::new();
        let (sign_at, chest_at) = (h.spot(1), h.spot(3));
        let sign = block::Kind::OakSign.default_state();
        let chest = block::Kind::Chest.default_state();
        h.state.worlds[0].set_block(sign_at, sign).unwrap();
        h.state.worlds[0].set_block(chest_at, chest).unwrap();
        assert!(h.cx().block_entity(WORLD, sign_at).is_none(), "not yet");
        h.tick(Vec::new());
        assert!(h.actor_data(sign_at).is_empty(), "the client makes its own");
        let mut cx = h.cx();
        let mut t = cx.block_entity_mut(WORLD, sign_at).unwrap();
        t.set_sign_text(Side::Front, text(["Set", "", "", ""], 0xff00_ff00))
            .unwrap();
        let bread = Stack::new(crate::item::Kind::Bread.default_type(), 3);
        let mut slots = cx.block_inventory_mut(WORLD, chest_at).unwrap();
        assert!(slots.add(&bread).is_empty());
        let furnace = block::Kind::Furnace.default_state();
        h.state.worlds[0].set_block(sign_at, furnace).unwrap();
        h.tick(Vec::new());
        let cx = h.cx();
        assert_eq!(
            cx.block_entity(WORLD, sign_at)
                .unwrap()
                .sign_text(Side::Front),
            None
        );
        h.state.worlds[0].set_block(sign_at, block::AIR).unwrap();
        h.tick(Vec::new());
        assert!(h.cx().block_entity(WORLD, sign_at).is_none());
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
            (WORLD, pos, Some(sign))
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
        assert_eq!(sent.len(), 1, "one update for the writes of a tick");
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
                .insert(0, p, BlockEntity::new(Kind::Chest { pair: None }));
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

    /// A double chest whose other half's chunk is not loaded stays paired: splitting it would
    /// leave that half's saved form paired, so the halves would disagree once it loads.
    #[test]
    fn a_double_chest_with_its_other_half_unloaded_does_not_split() {
        let mut h = Harness::new();
        // x 15 and 16 are in two chunks.
        let (a, b) = (h.spot(15), h.spot(16));
        assert_ne!(a.chunk(), b.chunk());
        for (at, with) in [(a, b), (b, a)] {
            h.state.worlds[0]
                .set_block(at, block::Kind::Chest.default_state())
                .unwrap();
            h.state.rules.block_entities.insert(
                0,
                at,
                BlockEntity::new(Kind::Chest { pair: Some(with) }),
            );
        }
        assert!(
            h.state
                .rules
                .block_entities
                .unload((0, b.chunk()))
                .is_some(),
            "the chunk changed"
        );
        let mut cx = h.cx();
        let mut t = cx.block_entity_mut(WORLD, a).unwrap();
        assert_eq!(t.unpair(), Err(Error::Invalid));
        assert_eq!(t.view().chest_pair(), Some(b));
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

    fn item(name: &str, meta: u16) -> Stack {
        Stack::new(
            item::Type::from_name(name, meta, None, &Compound::new()).unwrap(),
            1,
        )
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
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].get("ItemRotation"), Some(&Value::Float(135.0)));
        let mut cx = h.cx();
        let mut t = cx.block_entity_mut(WORLD, frame).unwrap();
        t.set_framed_item(&Stack::EMPTY).unwrap();
        assert_eq!(
            t.view().frame_rotation(),
            Some(0),
            "an emptied frame turns back for the next item"
        );

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

    /// Pages in a list of another type are no pages for a lectern either (PMMP
    /// `WritableBookBase::deserializeCompoundTag` reads compound or string lists only).
    #[test]
    fn a_lectern_book_with_pages_of_another_type_has_none() {
        let mut h = Harness::new();
        let lectern = h.spot(1);
        h.place(lectern, block::Kind::Lectern.default_state());
        let pages = Compound::from([(
            "pages".to_owned(),
            Value::List(crate::nbt::List::Int(vec![1, 2])),
        )]);
        let book = Stack::new(item::Kind::WritableBook.default_type(), 1)
            .with_extra(Some(item::Extra {
                nbt: pages,
                ..item::Extra::default()
            }))
            .unwrap();
        let mut cx = h.cx();
        let mut t = cx.block_entity_mut(WORLD, lectern).unwrap();
        t.set_lectern_book(&book).unwrap();
        assert_eq!(t.set_lectern_page(1), Err(Error::Invalid));
        let sent = h.actor_data(lectern);
        assert_eq!(sent[0].get("totalPages"), Some(&Value::Int(0)));
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
        cx.block_inventory_mut(WORLD, fire)
            .unwrap()
            .set(2, item("minecraft:beef", 0))
            .unwrap();
        let mut t = cx.block_entity_mut(WORLD, fire).unwrap();
        assert_eq!(t.view().cooking_time_ticks(0), Some(0));
        t.set_cooking_time_ticks(2, 100).unwrap();
        assert_eq!(
            t.set_cooking_time_ticks(0, 1),
            Err(Error::Invalid),
            "an empty slot"
        );
        assert_eq!(
            t.set_cooking_time_ticks(4, 1),
            Err(Error::Invalid),
            "no fifth slot"
        );
        assert_eq!(
            t.set_cooking_time_ticks(2, u16::MAX),
            Err(Error::Invalid),
            "past the cook time"
        );
        assert_eq!(t.view().cooking_time_ticks(2), Some(100));
        assert_eq!(t.view().cooking_time_ticks(0), Some(0));
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
            cx.block_entity(WORLD, fire).unwrap().cooking_time_ticks(2),
            Some(100)
        );
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

    /// `/status` counts the block entities of a world's loaded chunks, across chunks, and none
    /// for another world.
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

    /// A renamed item's name goes to the block entity only for the kinds that have one (PMMP
    /// `NameableTrait::copyDataFromItem`), the kinds `set_name` takes.
    #[test]
    fn only_containers_take_a_placed_items_name() {
        let mut h = Harness::new();
        let named = |stack: Stack| {
            stack
                .with_extra(Some(item::Extra {
                    custom_name: Some("x".to_owned()),
                    ..item::Extra::default()
                }))
                .unwrap()
        };
        let chest = item::Type::from_name(
            "minecraft:chest",
            0,
            Some(block::Kind::Chest.default_state()),
            &Compound::new(),
        )
        .unwrap();
        for (dx, block, stack, name) in [
            (1, block::Kind::OakSign, item("minecraft:oak_sign", 0), None),
            (3, block::Kind::Chest, Stack::new(chest, 1), Some("x")),
        ] {
            let pos = h.spot(dx);
            h.state.worlds[0]
                .set_block(pos, block.default_state())
                .unwrap();
            placed(&mut h.state, 0, pos, None, &named(stack), Face::Up);
            let sent = h.actor_data(pos);
            assert_eq!(
                sent[0].get("CustomName"),
                name.map(|n| Value::String(n.to_owned())).as_ref(),
                "{block:?}"
            );
            let cx = h.cx();
            assert_eq!(
                cx.block_entity(WORLD, pos).unwrap().name(),
                name,
                "{block:?}"
            );
        }
    }
}
