//! The public block entity API: [`Context::block_entity`] lends a [`Tile`] to read and
//! [`Context::block_entity_mut`] a [`Mut`] to change.
//!
//! A read looks the block entity up by position (two ordered-map steps) each time, so a [`Tile`]
//! holds no borrow of the store's insides. A write checks its value, changes the block entity (its
//! chunk is saved at the next commit) and sends its client data to the players who see the chunk
//! (PMMP sends a tile's spawn compound when it changes). Writes raise no events: a direct API call
//! never re-enters the listeners.

use super::super::{Context, State, inventory};
use super::{
    BOOKSHELF_SLOTS, BlockEntity, FRAME_ROTATIONS, Kind, Plant, beacon_effect_allowed, block_at,
    book_pages, broadcast_spawn, campfire_kind, chests_pair, decor, set_beacon,
};
use crate::block::{self, Class, Face, TypeTag};
use crate::effect;
use crate::item::{self, Stack};
use crate::nbt::Compound;
use crate::protocol::Rgba;
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
    /// block has none.
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

    /// The block entity at `pos` in `world` to change; `None` if its chunk is not loaded or the
    /// block has none.
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
    /// The block entity (present: the state is borrowed since it was found).
    fn entity(&self) -> Option<&'a BlockEntity> {
        self.state.rules.block_entities.get(self.world, self.pos)
    }

    fn kind(&self) -> Option<&'a Kind> {
        self.entity().map(|e| &e.kind)
    }

    /// The world it is in.
    pub fn world(&self) -> world::Id {
        world::Id::of(self.world)
    }

    /// Its position.
    #[doc(alias = "Tile::getPosition")]
    pub fn position(&self) -> BlockPos {
        self.pos
    }

    /// Its block.
    #[doc(alias = "Tile::getBlock")]
    pub fn block(&self) -> block::State {
        block_at(self.state, self.world, self.pos).unwrap_or(block::AIR)
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

    /// Changes the block entity with `f` (`None`: not its kind, nothing changed), then marks its
    /// chunk changed and sends its client data to the players who see it.
    fn edit(&mut self, f: impl FnOnce(&mut BlockEntity) -> Option<Result<()>>) -> Result<()> {
        let (world, pos) = (self.world, self.pos);
        let mut outcome = Err(Error::WrongKind);
        self.state.rules.block_entities.update(world, pos, |e| {
            outcome = f(e)?;
            outcome.ok()
        });
        outcome?;
        broadcast_spawn(self.state, world, pos);
        Ok(())
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

    /// Splits a double chest into two single chests. The windows open on either close first.
    #[doc(alias = "block\\tile\\Chest::unpair")]
    pub fn unpair(&mut self) -> Result<()> {
        let (world, pos) = (self.world, self.pos);
        let other = match self
            .state
            .rules
            .block_entities
            .get(world, pos)
            .map(|e| &e.kind)
        {
            Some(Kind::Chest { pair: Some(other) }) => *other,
            Some(Kind::Chest { pair: None }) => return Err(Error::Invalid),
            _ => return Err(Error::WrongKind),
        };
        self.set_pairs([(pos, None), (other, None)]);
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
                broadcast_spawn(self.state, world, at);
            }
        }
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
    /// cook time (the item cooks at its next update once the time is reached).
    #[doc(alias = "block\\Campfire::setCookingTime")]
    pub fn set_cooking_time_ticks(&mut self, slot: usize, ticks: u16) -> Result<()> {
        let soul = self.view().block().is(Class::SoulCampfire);
        let most = campfire_kind(soul).cook_ticks();
        self.edit(|e| match &mut e.kind {
            Kind::Campfire(c) => Some(match c.times.get_mut(slot) {
                Some(t) if ticks <= most => {
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
        if !beacon_effect_allowed(primary, 4) || !beacon_effect_allowed(secondary, 4) {
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
    /// dropped; the clients play what the jukebox holds.
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
                let pages = book_pages(book).max(1);
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
        if rotation > 15 {
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
}

#[cfg(test)]
mod tests;
