//! The public block entity API: [`Context::block_entity`] lends a [`Tile`] to read and
//! [`Context::block_entity_mut`] a [`Mut`] to change.
//!
//! A read looks the block entity up by position (two ordered-map steps) each time, so a [`Tile`]
//! holds no borrow of the store's insides. A write checks its value, changes the block entity (its
//! chunk is saved at the next commit) and marks its client data, which the players who see the
//! chunk get once at the end of the tick. Writes raise no events: a direct API call never
//! re-enters the listeners.

use super::{BlockEntity, Kind, Plant, block_at, chests_pair, decor, mark_spawn};
use crate::block::{self, Face, TypeTag};
use crate::tick::{Context, State, inventory};
use crate::tile::{
    BannerLayer, BannerType, DyeColour, Error, MAX_NAME_BYTES, Mut, Result, Side, SignText, Tile,
};
use crate::world::{self, BlockPos, ChunkPos};

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

    /// Whether a held item named `key` opens it: true without a `Lock`.
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
    pub fn banner_patterns(&self) -> Option<impl Iterator<Item = BannerLayer> + 'a> {
        match self.kind()? {
            Kind::Banner(b) => Some(b.patterns.iter().filter_map(|(id, colour)| {
                Some(BannerLayer::new(
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

    /// A mob head's rotation on the floor, in sixteenths of a turn (0 to 15).
    #[doc(alias = "block\\MobHead::getRotation")]
    #[doc(alias = "block\\tile\\MobHead::getRotation")]
    pub fn skull_rotation(&self) -> Option<u8> {
        match self.kind()? {
            Kind::Skull { rotation, .. } => Some(*rotation),
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

    /// Names a container (`None` or an empty name clears it): a chest, a barrel or a shulker box.
    /// A window open on it keeps its old title.
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

    /// Writes one side of a sign (a waxed sign takes it too: only players are kept out).
    #[doc(alias = "BaseSign::setText")]
    #[doc(alias = "BaseSign::setFaceText")]
    #[doc(alias = "Sign::setText")]
    #[doc(alias = "Sign::setBackText")]
    pub fn set_sign_text(&mut self, side: Side, text: SignText) -> Result<()> {
        self.edit(|e| match &mut e.kind {
            Kind::Sign(s) => {
                match side {
                    Side::Front => s.front = text,
                    Side::Back => s.back = text,
                }
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
    pub fn set_banner_patterns(&mut self, patterns: &[BannerLayer]) -> Result<()> {
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

    /// Puts a plant into a flower pot (`None` empties it): a block the pot takes (the pottable
    /// type tag). The plant before, even one Anchor does not know, is replaced and not dropped.
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

    use super::*;

    use tokio::sync::mpsc::Receiver;

    use crate::event::Listeners;
    use crate::item::{self, Stack};
    use crate::nbt::{Compound, Value};
    use crate::pool::Pool;
    use crate::protocol::Rgba;
    use crate::protocol::packet::Packet;
    use crate::save::Response;
    use crate::tick::block_entity::{beside, cardinal, placed, send_marked};
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{answer, join, new_state, radius, settings};
    use crate::tick::{InputLog, Output, SessionId, boundary, deliver, step};

    /// The world the tests use.
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
        SignText::new(lines.map(str::to_owned), colour, false).unwrap()
    }

    fn compound<'a>(nbt: &'a Compound, key: &str) -> &'a Compound {
        match nbt.get(key) {
            Some(Value::Compound(c)) => c,
            other => panic!("{key}: {other:?}"),
        }
    }

    // api: World::setBlock
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
        let barrel = block::Kind::Barrel.default_state();
        h.state.worlds[0].set_block(sign_at, barrel).unwrap();
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

    // api: World::getTile, World::getTileAt, Chunk::getTile, Tile, Tile::getPosition,
    // api: Tile::getBlock, BaseSign::getText, BaseSign::getFaceText, Sign::getText,
    // api: Sign::getBackText, BaseSign::setText, BaseSign::setFaceText, Sign::setText,
    // api: Sign::setBackText, BaseSign::isWaxed, Sign::isWaxed, BaseSign::setWaxed, Sign::setWaxed
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
        assert_eq!(t.set_bed_colour(DyeColour::Red), Err(Error::WrongKind));
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
            Some(&["", "Back", "", ""].map(str::to_owned))
        );
        assert!(tile.is_sign_waxed());
    }

    // api: Chunk::getTiles, Nameable::getName, NameableTrait::getName, Nameable::setName,
    // api: NameableTrait::setName, Container::canOpenWith, ContainerTrait::canOpenWith,
    // api: block\tile\EnderChest::getViewerCount, block\tile\Chest::getPair,
    // api: block\tile\Chest::pairWith, block\tile\Chest::unpair, block\tile\ShulkerBox::getFacing,
    // api: block\tile\ShulkerBox::setFacing
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

    // api: block\Bed::getColor, block\tile\Bed::getColor, block\Bed::setColor,
    // api: block\tile\Bed::setColor, BaseBanner::getColor, block\tile\Banner::getBaseColor,
    // api: BaseBanner::setColor, block\tile\Banner::setBaseColor, BaseBanner::getPatterns,
    // api: block\tile\Banner::getPatterns, BaseBanner::setPatterns,
    // api: block\tile\Banner::setPatterns, block\tile\Banner::getType, block\tile\Banner::setType,
    // api: block\MobHead::getRotation, block\tile\MobHead::getRotation,
    // api: block\MobHead::setRotation, block\tile\MobHead::setRotation
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
            BannerLayer::new(block::BannerPattern::Border, DyeColour::White),
            BannerLayer::new(block::BannerPattern::Creeper, DyeColour::Green),
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

    // api: block\FlowerPot::getPlant, block\tile\FlowerPot::getPlant, block\FlowerPot::setPlant,
    // api: block\tile\FlowerPot::setPlant
    #[test]
    fn a_flower_pot_holds_the_plant_it_is_given() {
        let mut h = Harness::new();
        let pot = h.spot(1);
        h.place(pot, block::Kind::FlowerPot.default_state());
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
        h.reload(&[pot]);
        let mut cx = h.cx();
        assert_eq!(
            cx.block_entity(WORLD, pot).unwrap().potted_plant(),
            Some(dandelion)
        );
        let mut t = cx.block_entity_mut(WORLD, pot).unwrap();
        t.set_potted_plant(None).unwrap();
        assert_eq!(t.view().potted_plant(), None);
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

    /// A renamed item's name goes to the block entity only for the kinds that have one, the kinds
    /// `set_name` takes.
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
