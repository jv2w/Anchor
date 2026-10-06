//! Sounds a world plays at a position (PMMP `world\sound\*`), for
//! [`Context::add_sound`](crate::Context::add_sound): each is the packet PMMP sends for its
//! class, with gophertunnel's names and numbers.
//!
//! Left out: sounds of an entity (`EntityLandSound`, `EntityLongFallSound`,
//! `EntityShortFallSound`: the entity rules play them) and pressure plates' (`PressurePlate*`:
//! redstone). Levers and buttons switch their own state, with their
//! `RedstonePower*` sounds.

use crate::block;
use crate::item;
use crate::math::Vec3;
use crate::protocol::consts::{LevelEvent, SoundEvent};
use crate::protocol::packet::{
    LevelEvent as LevelEventPacket, LevelSoundEvent as LevelSoundEventPacket, Packet,
};

/// A sound.
#[derive(Debug, Clone, Copy, PartialEq)]
// pmmp-class: Sound
pub enum Sound {
    /// An amethyst block chimes.
    #[doc(alias = "AmethystBlockChimeSound")]
    AmethystBlockChime,
    /// An anvil breaks.
    #[doc(alias = "AnvilBreakSound")]
    AnvilBreak,
    /// An anvil lands.
    #[doc(alias = "AnvilFallSound")]
    AnvilFall,
    /// An anvil is used.
    #[doc(alias = "AnvilUseSound")]
    AnvilUse,
    /// Chainmail armour is put on.
    #[doc(alias = "ArmorEquipChainSound")]
    ArmorEquipChain,
    /// Copper armour is put on.
    #[doc(alias = "ArmorEquipCopperSound")]
    ArmorEquipCopper,
    /// Diamond armour is put on.
    #[doc(alias = "ArmorEquipDiamondSound")]
    ArmorEquipDiamond,
    /// Other armour is put on.
    #[doc(alias = "ArmorEquipGenericSound")]
    ArmorEquipGeneric,
    /// Golden armour is put on.
    #[doc(alias = "ArmorEquipGoldSound")]
    ArmorEquipGold,
    /// Iron armour is put on.
    #[doc(alias = "ArmorEquipIronSound")]
    ArmorEquipIron,
    /// Leather armour is put on.
    #[doc(alias = "ArmorEquipLeatherSound")]
    ArmorEquipLeather,
    /// Netherite armour is put on.
    #[doc(alias = "ArmorEquipNetheriteSound")]
    ArmorEquipNetherite,
    /// An arrow hits.
    #[doc(alias = "ArrowHitSound")]
    ArrowHit,
    /// A barrel closes.
    #[doc(alias = "BarrelCloseSound")]
    BarrelClose,
    /// A barrel opens.
    #[doc(alias = "BarrelOpenSound")]
    BarrelOpen,
    /// A bell rings.
    #[doc(alias = "BellRingSound")]
    BellRing,
    /// A blast furnace crackles.
    #[doc(alias = "BlastFurnaceSound")]
    BlastFurnace,
    /// A blaze shoots.
    #[doc(alias = "BlazeShootSound")]
    BlazeShoot,
    /// A block of this state breaks.
    #[doc(alias = "BlockBreakSound")]
    BlockBreak(block::State),
    /// A block of this state is placed.
    #[doc(alias = "BlockPlaceSound")]
    BlockPlace(block::State),
    /// A block of this state is hit while being broken.
    #[doc(alias = "BlockPunchSound")]
    BlockPunch(block::State),
    /// A bottle is emptied.
    #[doc(alias = "BottleEmptySound")]
    BottleEmpty,
    /// A bow shoots.
    #[doc(alias = "BowShootSound")]
    BowShoot,
    /// Lava is poured from a bucket.
    #[doc(alias = "BucketEmptyLavaSound")]
    BucketEmptyLava,
    /// Water is poured from a bucket.
    #[doc(alias = "BucketEmptyWaterSound")]
    BucketEmptyWater,
    /// A bucket is filled with lava.
    #[doc(alias = "BucketFillLavaSound")]
    BucketFillLava,
    /// A bucket is filled with water.
    #[doc(alias = "BucketFillWaterSound")]
    BucketFillWater,
    /// A burp after eating.
    #[doc(alias = "BurpSound")]
    Burp,
    /// A campfire crackles.
    #[doc(alias = "CampfireSound")]
    Campfire,
    /// Dye goes into a cauldron.
    #[doc(alias = "CauldronAddDyeSound")]
    CauldronAddDye,
    /// A cauldron washes an item.
    #[doc(alias = "CauldronCleanItemSound")]
    CauldronCleanItem,
    /// A cauldron dyes an item.
    #[doc(alias = "CauldronDyeItemSound")]
    CauldronDyeItem,
    /// Lava is taken from a cauldron.
    #[doc(alias = "CauldronEmptyLavaSound")]
    CauldronEmptyLava,
    /// A potion is taken from a cauldron.
    #[doc(alias = "CauldronEmptyPotionSound")]
    CauldronEmptyPotion,
    /// Powder snow is taken from a cauldron.
    #[doc(alias = "CauldronEmptyPowderSnowSound")]
    CauldronEmptyPowderSnow,
    /// Water is taken from a cauldron.
    #[doc(alias = "CauldronEmptyWaterSound")]
    CauldronEmptyWater,
    /// Lava goes into a cauldron.
    #[doc(alias = "CauldronFillLavaSound")]
    CauldronFillLava,
    /// A potion goes into a cauldron.
    #[doc(alias = "CauldronFillPotionSound")]
    CauldronFillPotion,
    /// Powder snow goes into a cauldron.
    #[doc(alias = "CauldronFillPowderSnowSound")]
    CauldronFillPowderSnow,
    /// Water goes into a cauldron.
    #[doc(alias = "CauldronFillWaterSound")]
    CauldronFillWater,
    /// A chest closes.
    #[doc(alias = "ChestCloseSound")]
    ChestClose,
    /// A chest opens.
    #[doc(alias = "ChestOpenSound")]
    ChestOpen,
    /// A chorus flower dies.
    #[doc(alias = "ChorusFlowerDieSound")]
    ChorusFlowerDie,
    /// A chorus flower grows.
    #[doc(alias = "ChorusFlowerGrowSound")]
    ChorusFlowerGrow,
    /// A click at `pitch` (PMMP's default 0).
    #[doc(alias = "ClickSound")]
    Click {
        /// The pitch.
        pitch: f32,
    },
    /// Copper is waxed.
    #[doc(alias = "CopperWaxApplySound")]
    CopperWaxApply,
    /// Wax comes off copper.
    #[doc(alias = "CopperWaxRemoveSound")]
    CopperWaxRemove,
    /// A door is bumped.
    #[doc(alias = "DoorBumpSound")]
    DoorBump,
    /// A door is broken in.
    #[doc(alias = "DoorCrashSound")]
    DoorCrash,
    /// A door opens or closes, at `pitch` (PMMP's default 0).
    #[doc(alias = "DoorSound")]
    Door {
        /// The pitch.
        pitch: f32,
    },
    /// A big dripleaf tilts down.
    #[doc(alias = "DripleafTiltDownSound")]
    DripleafTiltDown,
    /// A big dripleaf tilts up.
    #[doc(alias = "DripleafTiltUpSound")]
    DripleafTiltUp,
    /// Dye is used on a sign.
    #[doc(alias = "DyeUseSound")]
    DyeUse,
    /// An ender chest closes.
    #[doc(alias = "EnderChestCloseSound")]
    EnderChestClose,
    /// An ender chest opens.
    #[doc(alias = "EnderChestOpenSound")]
    EnderChestOpen,
    /// Something teleports as an enderman does.
    #[doc(alias = "EndermanTeleportSound")]
    EndermanTeleport,
    /// A hit lands.
    #[doc(alias = "EntityAttackSound")]
    EntityAttack,
    /// A hit does nothing.
    #[doc(alias = "EntityAttackNoDamageSound")]
    EntityAttackNoDamage,
    /// An explosion.
    #[doc(alias = "ExplodeSound")]
    Explode,
    /// A fire is put out.
    #[doc(alias = "FireExtinguishSound")]
    FireExtinguish,
    /// A firework crackles.
    #[doc(alias = "FireworkCrackleSound")]
    FireworkCrackle,
    /// A firework explodes.
    #[doc(alias = "FireworkExplosionSound")]
    FireworkExplosion,
    /// A large firework explodes.
    #[doc(alias = "FireworkLargeExplosionSound")]
    FireworkLargeExplosion,
    /// A firework launches.
    #[doc(alias = "FireworkLaunchSound")]
    FireworkLaunch,
    /// A fizz at `pitch` (PMMP's default 0).
    #[doc(alias = "FizzSound")]
    Fizz {
        /// The pitch.
        pitch: f32,
    },
    /// Flint and steel strikes.
    #[doc(alias = "FlintSteelSound")]
    FlintSteel,
    /// A furnace burns.
    #[doc(alias = "FurnaceSound")]
    Furnace,
    /// A ghast shoots.
    #[doc(alias = "GhastShootSound")]
    GhastShoot,
    /// A ghast warns.
    #[doc(alias = "GhastSound")]
    Ghast,
    /// Glow berries are picked.
    #[doc(alias = "GlowBerriesPickSound")]
    GlowBerriesPick,
    /// A goat horn of this item type sounds: the call of its meta, 0 to 7 (PMMP `GoatHornType`
    /// in its order); any other item plays the first call, as Anchor's horn rule does.
    #[doc(alias = "GoatHornSound")]
    GoatHorn(item::Type),
    /// An ice bomb hits.
    #[doc(alias = "IceBombHitSound")]
    IceBombHit,
    /// TNT is lit.
    #[doc(alias = "IgniteSound")]
    Ignite,
    /// An ink sac is used on a sign.
    #[doc(alias = "InkSacUseSound")]
    InkSacUse,
    /// A tool breaks.
    #[doc(alias = "ItemBreakSound")]
    ItemBreak,
    /// An item goes into an item frame.
    #[doc(alias = "ItemFrameAddItemSound")]
    ItemFrameAddItem,
    /// An item comes out of an item frame.
    #[doc(alias = "ItemFrameRemoveItemSound")]
    ItemFrameRemoveItem,
    /// An item frame's item turns.
    #[doc(alias = "ItemFrameRotateItemSound")]
    ItemFrameRotateItem,
    /// An item is used on a block of this state.
    #[doc(alias = "ItemUseOnBlockSound")]
    ItemUseOnBlock(block::State),
    /// A launch at `pitch` (PMMP's default 0).
    #[doc(alias = "LaunchSound")]
    Launch {
        /// The pitch.
        pitch: f32,
    },
    /// A book is put on a lectern.
    #[doc(alias = "LecternPlaceBookSound")]
    LecternPlaceBook,
    /// A note block plays `note` on `instrument`.
    #[doc(alias = "NoteSound")]
    Note {
        /// The instrument.
        instrument: NoteInstrument,
        /// The note: 0 to 24 are the note block's pitches (PMMP takes 0 to 255).
        note: u8,
    },
    /// A painting is placed.
    #[doc(alias = "PaintingPlaceSound")]
    PaintingPlace,
    /// A pop at `pitch` (PMMP's default 0).
    #[doc(alias = "PopSound")]
    Pop {
        /// The pitch.
        pitch: f32,
    },
    /// A brewing stand finishes.
    #[doc(alias = "PotionFinishBrewingSound")]
    PotionFinishBrewing,
    /// A potion bottle shatters.
    #[doc(alias = "PotionSplashSound")]
    PotionSplash,
    /// A jukebox plays the music disc of this item type (PMMP `RecordType`); any other item
    /// stops the music, as [`Sound::RecordStop`].
    #[doc(alias = "RecordSound")]
    Record(item::Type),
    /// A jukebox stops.
    #[doc(alias = "RecordStopSound")]
    RecordStop,
    /// A lever or button switches on.
    #[doc(alias = "RedstonePowerOnSound")]
    RedstonePowerOn,
    /// A lever or button switches off.
    #[doc(alias = "RedstonePowerOffSound")]
    RedstonePowerOff,
    /// A respawn anchor is charged.
    #[doc(alias = "RespawnAnchorChargeSound")]
    RespawnAnchorCharge,
    /// A respawn anchor runs out.
    #[doc(alias = "RespawnAnchorDepleteSound")]
    RespawnAnchorDeplete,
    /// A respawn anchor sets the spawn.
    #[doc(alias = "RespawnAnchorSetSpawnSound")]
    RespawnAnchorSetSpawn,
    /// An axe scrapes copper.
    #[doc(alias = "ScrapeSound")]
    Scrape,
    /// A shulker box closes.
    #[doc(alias = "ShulkerBoxCloseSound")]
    ShulkerBoxClose,
    /// A shulker box opens.
    #[doc(alias = "ShulkerBoxOpenSound")]
    ShulkerBoxOpen,
    /// A smoker smokes.
    #[doc(alias = "SmokerSound")]
    Smoker,
    /// Sweet berries are picked.
    #[doc(alias = "SweetBerriesPickSound")]
    SweetBerriesPick,
    /// Something is thrown.
    #[doc(alias = "ThrowSound")]
    Throw,
    /// A totem saves its holder.
    #[doc(alias = "TotemUseSound")]
    TotemUse,
    /// A trident hits a block.
    #[doc(alias = "TridentHitBlockSound")]
    TridentHitBlock,
    /// A trident hits an entity.
    #[doc(alias = "TridentHitEntitySound")]
    TridentHitEntity,
    /// A trident is thrown.
    #[doc(alias = "TridentThrowSound")]
    TridentThrow,
    /// A splash in water at `volume` (0 to 1).
    #[doc(alias = "WaterSplashSound")]
    WaterSplash {
        /// The volume, 0 to 1.
        volume: f32,
    },
    /// An experience orb is collected.
    #[doc(alias = "XpCollectSound")]
    XpCollect,
    /// A player reaches experience level `level`.
    #[doc(alias = "XpLevelUpSound")]
    XpLevelUp {
        /// The level reached.
        level: u32,
    },
}

/// The instrument of a [`Sound::Note`], by the block under a note block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// pmmp-class: NoteInstrument
pub enum NoteInstrument {
    /// The default: harp and piano.
    Piano,
    /// Stone: bass drum.
    BassDrum,
    /// Sand and gravel: snare drum.
    Snare,
    /// Glass: clicks and sticks.
    ClicksAndSticks,
    /// Wood: double bass.
    DoubleBass,
    /// Gold: bell.
    Bell,
    /// Clay: flute.
    Flute,
    /// Packed ice: chime.
    Chime,
    /// Wool: guitar.
    Guitar,
    /// Bone block: xylophone.
    Xylophone,
    /// Iron: iron xylophone.
    IronXylophone,
    /// Soul sand: cow bell.
    CowBell,
    /// Pumpkin: didgeridoo.
    Didgeridoo,
    /// Emerald: bit.
    Bit,
    /// Hay: banjo.
    Banjo,
    /// Glowstone: pling.
    Pling,
}

impl NoteInstrument {
    /// The instrument's number in the `note` sound (PMMP `NoteInstrumentIdMap`).
    const fn id(self) -> i32 {
        match self {
            Self::Piano => 0,
            Self::BassDrum => 1,
            Self::Snare => 2,
            Self::ClicksAndSticks => 3,
            Self::DoubleBass => 4,
            Self::Flute => 5,
            Self::Bell => 6,
            Self::Guitar => 7,
            Self::Chime => 8,
            Self::Xylophone => 9,
            Self::IronXylophone => 10,
            Self::CowBell => 11,
            Self::Didgeridoo => 12,
            Self::Bit => 13,
            Self::Banjo => 14,
            Self::Pling => 15,
        }
    }
}

/// What a sound sends.
enum Sent {
    /// A `LevelSoundEvent` with its extra data and entity type.
    Sound(SoundEvent, i32, &'static str),
    /// A `LevelEvent` with its data.
    Level(LevelEvent, i32),
}

/// The entity type of a sound with no entity (PMMP `LevelSoundEventPacket::nonActorSound`).
const NO_ENTITY: &str = ":";
/// The entity type PMMP gives hit and throw sounds.
const PLAYER: &str = "minecraft:player";

impl Sound {
    /// The packet that plays the sound at `at` (PMMP `Sound::encode`).
    pub(crate) fn packet(self, at: Vec3) -> Packet {
        let position = at.to_f32();
        match self.sent() {
            Sent::Sound(sound, extra_data, entity_type) => {
                Packet::LevelSoundEvent(LevelSoundEventPacket {
                    sound_type: sound.as_str().to_owned(),
                    position,
                    extra_data,
                    entity_type: entity_type.to_owned(),
                    baby_mob: false,
                    disable_relative_volume: false,
                    entity_unique_id: -1,
                    fire_at_position: None,
                })
            }
            Sent::Level(event, event_data) => Packet::LevelEvent(LevelEventPacket {
                event_type: event.into(),
                position,
                event_data,
            }),
        }
    }

    /// What the sound sends (the PMMP class's `encode`).
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one arm per PMMP sound class, in one place"
    )]
    fn sent(self) -> Sent {
        use LevelEvent as L;
        use SoundEvent as S;
        let plain = |s| Sent::Sound(s, -1, NO_ENTITY);
        let level = |l| Sent::Level(l, 0);
        let block = |s, b: block::State| Sent::Sound(s, b.network_id().cast_signed(), NO_ENTITY);
        match self {
            Self::AmethystBlockChime => plain(S::AmethystBlockChime),
            Self::AnvilBreak => level(L::SoundAnvilBroken),
            Self::AnvilFall => level(L::SoundAnvilLand),
            Self::AnvilUse => level(L::SoundAnvilUsed),
            Self::ArmorEquipChain => plain(S::EquipChain),
            Self::ArmorEquipCopper => plain(S::EquipCopper),
            Self::ArmorEquipDiamond => plain(S::EquipDiamond),
            Self::ArmorEquipGeneric => plain(S::EquipGeneric),
            Self::ArmorEquipGold => plain(S::EquipGold),
            Self::ArmorEquipIron => plain(S::EquipIron),
            Self::ArmorEquipLeather => plain(S::EquipLeather),
            Self::ArmorEquipNetherite => plain(S::EquipNetherite),
            Self::ArrowHit => plain(S::BowHit),
            Self::BarrelClose => plain(S::BarrelClose),
            Self::BarrelOpen => plain(S::BarrelOpen),
            Self::BellRing => plain(S::Bell),
            Self::BlastFurnace => plain(S::BlastFurnaceUse),
            Self::BlazeShoot => level(L::SoundBlazeFireball),
            Self::BlockBreak(b) => block(S::Break, b),
            Self::BlockPlace(b) => block(S::Place, b),
            Self::BlockPunch(b) => block(S::Hit, b),
            Self::BottleEmpty => plain(S::BottleEmpty),
            Self::BowShoot => plain(S::Bow),
            Self::BucketEmptyLava => plain(S::BucketEmptyLava),
            Self::BucketEmptyWater => plain(S::BucketEmptyWater),
            Self::BucketFillLava => plain(S::BucketFillLava),
            Self::BucketFillWater => plain(S::BucketFillWater),
            Self::Burp => plain(S::Burp),
            Self::Campfire => plain(S::CampfireCrackle),
            Self::CauldronAddDye => level(L::CauldronAddDye),
            Self::CauldronCleanItem => level(L::CauldronCleanArmor),
            Self::CauldronDyeItem => level(L::CauldronDyeArmor),
            Self::CauldronEmptyLava => level(L::CauldronTakeLava),
            Self::CauldronEmptyPotion => level(L::CauldronTakePotion),
            Self::CauldronEmptyPowderSnow => level(L::CauldronTakePowderSnow),
            Self::CauldronEmptyWater => level(L::CauldronTakeWater),
            Self::CauldronFillLava => level(L::CauldronFillLava),
            Self::CauldronFillPotion => level(L::CauldronFillPotion),
            Self::CauldronFillPowderSnow => level(L::CauldronFillPowderSnow),
            Self::CauldronFillWater => level(L::CauldronFillWater),
            Self::ChestClose => plain(S::ChestClosed),
            Self::ChestOpen => plain(S::ChestOpen),
            Self::ChorusFlowerDie => plain(S::ChorusDeath),
            Self::ChorusFlowerGrow => plain(S::ChorusGrow),
            Self::Click { pitch } => Sent::Level(L::SoundClick, permille(pitch)),
            Self::CopperWaxApply => plain(S::CopperWaxOn),
            Self::CopperWaxRemove => plain(S::CopperWaxOff),
            Self::DoorBump => level(L::SoundZombieWoodenDoor),
            Self::DoorCrash => level(L::SoundZombieDoorCrash),
            Self::Door { pitch } => Sent::Level(L::SoundOpenDoor, permille(pitch)),
            Self::DripleafTiltDown => plain(S::BigDripleafTiltDown),
            Self::DripleafTiltUp => plain(S::BigDripleafTiltUp),
            Self::DyeUse => level(L::SoundDyeUsed),
            Self::EnderChestClose => plain(S::EnderChestClosed),
            Self::EnderChestOpen => plain(S::EnderChestOpen),
            Self::EndermanTeleport => level(L::SoundEndermanTeleport),
            // PMMP: ATTACK seems dysfunctional, so a landed hit is ATTACK_STRONG.
            Self::EntityAttack => Sent::Sound(S::AttackStrong, -1, PLAYER),
            Self::EntityAttackNoDamage => Sent::Sound(S::AttackNoDamage, -1, PLAYER),
            Self::Explode => plain(S::Explode),
            Self::FireExtinguish => plain(S::ExtinguishFire),
            Self::FireworkCrackle => plain(S::Twinkle),
            Self::FireworkExplosion => plain(S::Blast),
            Self::FireworkLargeExplosion => plain(S::LargeBlast),
            Self::FireworkLaunch => plain(S::Launch),
            Self::Fizz { pitch } => Sent::Level(L::SoundFizz, permille(pitch)),
            Self::FlintSteel => plain(S::Ignite),
            Self::Furnace => plain(S::FurnaceUse),
            Self::GhastShoot => level(L::SoundGhastFireball),
            Self::Ghast => level(L::SoundGhastWarning),
            Self::GlowBerriesPick => plain(S::CaveVinesPickBerries),
            Self::GoatHorn(horn) => plain(horn_call(horn)),
            Self::IceBombHit => plain(S::IceBombHit),
            Self::Ignite => level(L::SoundFuse),
            Self::InkSacUse => level(L::SoundInkSacUsed),
            Self::ItemBreak => plain(S::Break),
            Self::ItemFrameAddItem => level(L::SoundAddItem),
            Self::ItemFrameRemoveItem => level(L::SoundItemFrameRemoveItem),
            Self::ItemFrameRotateItem => level(L::SoundItemFrameRotateItem),
            Self::ItemUseOnBlock(b) => block(S::ItemUseOn, b),
            Self::Launch { pitch } => Sent::Level(L::SoundLaunch, permille(pitch)),
            Self::LecternPlaceBook => plain(S::LecternBookPlace),
            Self::Note { instrument, note } => {
                Sent::Sound(S::Note, instrument.id() << 8 | i32::from(note), NO_ENTITY)
            }
            // PMMP: item frames and paintings share the sound.
            Self::PaintingPlace => level(L::SoundItemFramePlace),
            Self::Pop { pitch } => Sent::Level(L::SoundInfinityArrowPickup, permille(pitch)),
            Self::PotionFinishBrewing => plain(S::PotionBrewed),
            Self::PotionSplash => plain(S::Glass),
            Self::Record(disc) => plain(record(disc)),
            Self::RecordStop => plain(S::RecordNull),
            Self::RedstonePowerOn => plain(S::PowerOn),
            Self::RedstonePowerOff => plain(S::PowerOff),
            Self::RespawnAnchorCharge => plain(S::RespawnAnchorCharge),
            Self::RespawnAnchorDeplete => plain(S::RespawnAnchorDeplete),
            Self::RespawnAnchorSetSpawn => plain(S::RespawnAnchorSetSpawn),
            Self::Scrape => plain(S::Scrape),
            Self::ShulkerBoxClose => plain(S::ShulkerBoxClosed),
            Self::ShulkerBoxOpen => plain(S::ShulkerBoxOpen),
            Self::Smoker => plain(S::SmokerUse),
            Self::SweetBerriesPick => plain(S::SweetBerryBushPick),
            Self::Throw => Sent::Sound(S::Throw, -1, PLAYER),
            Self::TotemUse => level(L::SoundTotemUsed),
            Self::TridentHitBlock => plain(S::TridentHitGround),
            Self::TridentHitEntity => plain(S::TridentHit),
            Self::TridentThrow => plain(S::TridentThrow),
            Self::WaterSplash { volume } => {
                Sent::Sound(S::Splash, scaled(volume, 16_777_215.0), NO_ENTITY)
            }
            Self::XpCollect => level(L::SoundExperienceOrbPickup),
            Self::XpLevelUp { level } => {
                // The volume field carries the level: 0x1000_0000 for every five levels up to 30,
                // the values PMMP sends.
                let step = i32::try_from(level.min(30) / 5).unwrap_or(0);
                Sent::Sound(S::LevelUp, 0x1000_0000_i32.saturating_mul(step), NO_ENTITY)
            }
        }
    }
}

/// The call of a goat horn type (PMMP `GoatHornSound`): its meta in `GoatHornType` order; the
/// first for another item.
fn horn_call(horn: item::Type) -> SoundEvent {
    const CALLS: [SoundEvent; 8] = [
        SoundEvent::GoatCall0,
        SoundEvent::GoatCall1,
        SoundEvent::GoatCall2,
        SoundEvent::GoatCall3,
        SoundEvent::GoatCall4,
        SoundEvent::GoatCall5,
        SoundEvent::GoatCall6,
        SoundEvent::GoatCall7,
    ];
    if horn.kind() != Some(item::Kind::GoatHorn) {
        return SoundEvent::GoatCall0;
    }
    CALLS
        .get(usize::from(horn.meta()))
        .copied()
        .unwrap_or(SoundEvent::GoatCall0)
}

/// The music of a disc (PMMP `RecordSound`); the jukebox stop for another item.
fn record(disc: item::Type) -> SoundEvent {
    use item::Kind as K;
    match disc.kind() {
        Some(K::Record13) => SoundEvent::Record13,
        Some(K::Record5) => SoundEvent::Record5,
        Some(K::RecordCat) => SoundEvent::RecordCat,
        Some(K::RecordBlocks) => SoundEvent::RecordBlocks,
        Some(K::RecordChirp) => SoundEvent::RecordChirp,
        Some(K::RecordCreator) => SoundEvent::RecordCreator,
        Some(K::RecordCreatorMusicBox) => SoundEvent::RecordCreatorMusicBox,
        Some(K::RecordFar) => SoundEvent::RecordFar,
        Some(K::RecordLavaChicken) => SoundEvent::RecordLavaChicken,
        Some(K::RecordMall) => SoundEvent::RecordMall,
        Some(K::RecordMellohi) => SoundEvent::RecordMellohi,
        Some(K::RecordOtherside) => SoundEvent::RecordOtherside,
        Some(K::RecordPigstep) => SoundEvent::RecordPigstep,
        Some(K::RecordPrecipice) => SoundEvent::RecordPrecipice,
        Some(K::RecordRelic) => SoundEvent::RecordRelic,
        Some(K::RecordStal) => SoundEvent::RecordStal,
        Some(K::RecordStrad) => SoundEvent::RecordStrad,
        Some(K::RecordWard) => SoundEvent::RecordWard,
        Some(K::Record11) => SoundEvent::Record11,
        Some(K::RecordWait) => SoundEvent::RecordWait,
        _ => SoundEvent::RecordNull,
    }
}

/// A pitch as level events carry it (PMMP `(int) ($pitch * 1000)`).
fn permille(pitch: f32) -> i32 {
    scaled(pitch, 1000.0)
}

/// `value × factor` as an `i32`, saturating (0 for a value that is not a number).
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: PMMP casts with `(int)`; Rust's `as` saturates and maps NaN to 0, the \
              same on every platform"
)]
fn scaled(value: f32, factor: f64) -> i32 {
    (f64::from(value) * factor) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(sound: Sound) -> (String, i32, String) {
        match sound.packet(Vec3::new(1.0, 2.0, 3.0)) {
            Packet::LevelSoundEvent(p) => {
                assert_eq!(p.position, [1.0, 2.0, 3.0]);
                (p.sound_type, p.extra_data, p.entity_type)
            }
            Packet::LevelEvent(p) => (
                format!("level {}", p.event_type),
                p.event_data,
                String::new(),
            ),
            other => panic!("{other:?}"),
        }
    }

    // pmmp: AmethystBlockChimeSound, AnvilBreakSound, AnvilFallSound, AnvilUseSound, ArmorEquipChainSound, ArmorEquipCopperSound, ArmorEquipDiamondSound, ArmorEquipGenericSound, ArmorEquipGoldSound, ArmorEquipIronSound, ArmorEquipLeatherSound, ArmorEquipNetheriteSound, ArrowHitSound, BarrelCloseSound, BarrelOpenSound, BellRingSound, BlastFurnaceSound, BlazeShootSound, BlockBreakSound, BlockPlaceSound, BlockPunchSound, BottleEmptySound, BowShootSound, BucketEmptyLavaSound, BucketEmptyWaterSound, BucketFillLavaSound, BucketFillWaterSound, BurpSound, CampfireSound, CauldronAddDyeSound, CauldronCleanItemSound, CauldronDyeItemSound, CauldronEmptyLavaSound, CauldronEmptyPotionSound, CauldronEmptyPowderSnowSound, CauldronEmptyWaterSound, CauldronFillLavaSound, CauldronFillPotionSound, CauldronFillPowderSnowSound, CauldronFillWaterSound, ChestCloseSound, ChestOpenSound, ChorusFlowerDieSound, ChorusFlowerGrowSound, ClickSound, CopperWaxApplySound, CopperWaxRemoveSound, DoorBumpSound, DoorCrashSound, DoorSound, DripleafTiltDownSound, DripleafTiltUpSound, DyeUseSound, EnderChestCloseSound, EnderChestOpenSound, EndermanTeleportSound, EntityAttackSound, EntityAttackNoDamageSound, ExplodeSound, FireExtinguishSound, FireworkCrackleSound, FireworkExplosionSound, FireworkLargeExplosionSound, FireworkLaunchSound, FizzSound, FlintSteelSound, FurnaceSound, GhastShootSound, GhastSound, GlowBerriesPickSound, IceBombHitSound, IgniteSound, InkSacUseSound, ItemBreakSound, ItemFrameAddItemSound, ItemFrameRemoveItemSound, ItemFrameRotateItemSound, ItemUseOnBlockSound, LaunchSound, LecternPlaceBookSound, PaintingPlaceSound, PopSound, PotionFinishBrewingSound, PotionSplashSound, RecordStopSound, RespawnAnchorChargeSound, RespawnAnchorDepleteSound, RespawnAnchorSetSpawnSound, ScrapeSound, ShulkerBoxCloseSound, ShulkerBoxOpenSound, SmokerSound, SweetBerriesPickSound, ThrowSound, TotemUseSound, TridentHitBlockSound, TridentHitEntitySound, TridentThrowSound, WaterSplashSound, XpCollectSound, XpLevelUpSound, Sound
    #[test]
    fn sounds_send_pmmps_packets_with_gophertunnel_names() {
        let stone = block::State::find("minecraft:stone", &[]).unwrap();
        let id = stone.network_id().cast_signed();
        let cases = [
            (
                Sound::ChestOpen,
                ("chest.open".to_owned(), -1, ":".to_owned()),
            ),
            (
                Sound::BlockBreak(stone),
                ("break".to_owned(), id, ":".to_owned()),
            ),
            (
                Sound::BlockPunch(stone),
                ("hit".to_owned(), id, ":".to_owned()),
            ),
            (
                Sound::EntityAttack,
                ("attack.strong".to_owned(), -1, PLAYER.to_owned()),
            ),
            (
                Sound::ArmorEquipNetherite,
                ("armor.equip_netherite".to_owned(), -1, ":".to_owned()),
            ),
            (
                Sound::AnvilFall,
                ("level 1022".to_owned(), 0, String::new()),
            ),
            (
                Sound::Click { pitch: 1.5 },
                ("level 1000".to_owned(), 1500, String::new()),
            ),
            (
                Sound::Pop { pitch: f32::NAN },
                ("level 1030".to_owned(), 0, String::new()),
            ),
            (
                Sound::XpCollect,
                ("level 1051".to_owned(), 0, String::new()),
            ),
            (
                Sound::WaterSplash { volume: 1.0 },
                ("splash".to_owned(), 16_777_215, ":".to_owned()),
            ),
            (
                Sound::XpLevelUp { level: 100 },
                ("levelup".to_owned(), 0x6000_0000, ":".to_owned()),
            ),
            (
                Sound::XpLevelUp { level: 4 },
                ("levelup".to_owned(), 0, ":".to_owned()),
            ),
            (
                Sound::CauldronFillWater,
                ("level 3506".to_owned(), 0, String::new()),
            ),
        ];
        for (sound, want) in cases {
            assert_eq!(sent(sound), want, "{sound:?}");
        }
    }

    // pmmp: RedstonePowerOnSound, RedstonePowerOffSound
    #[test]
    fn levers_and_buttons_sound_as_pmmps_redstone_power() {
        assert_eq!(
            sent(Sound::RedstonePowerOn),
            ("power.on".to_owned(), -1, ":".to_owned())
        );
        assert_eq!(
            sent(Sound::RedstonePowerOff),
            ("power.off".to_owned(), -1, ":".to_owned())
        );
    }

    // pmmp: NoteSound, NoteInstrument, RecordSound, GoatHornSound
    #[test]
    fn notes_discs_and_horns_send_pmmps_sound_events() {
        let none = crate::nbt::Compound::new();
        let ty = |name: &str, meta: u16| item::Type::from_name(name, meta, None, &none).unwrap();
        let play = |instrument, note| Sound::Note { instrument, note };
        assert_eq!(
            sent(play(NoteInstrument::Piano, 0)),
            ("note".to_owned(), 0, ":".to_owned())
        );
        // PMMP `NoteInstrumentIdMap`: flute is 5 and bell 6, unlike their enum order.
        assert_eq!(sent(play(NoteInstrument::Flute, 12)).1, 0x050c);
        assert_eq!(sent(play(NoteInstrument::Pling, 255)).1, 0x0fff);
        assert_eq!(
            sent(Sound::Record(ty("minecraft:music_disc_pigstep", 0))),
            ("record.pigstep".to_owned(), -1, ":".to_owned())
        );
        assert_eq!(
            sent(Sound::Record(ty("minecraft:stick", 0))).0,
            "record.null"
        );
        assert_eq!(
            sent(Sound::GoatHorn(ty("minecraft:goat_horn", 7))).0,
            "horn_call7"
        );
        assert_eq!(
            sent(Sound::GoatHorn(ty("minecraft:stick", 0))).0,
            "horn_call0"
        );
    }

    #[test]
    fn every_sound_names_an_event() {
        let stone = block::State::find("minecraft:stone", &[]).unwrap();
        let every = [
            Sound::AmethystBlockChime,
            Sound::AnvilBreak,
            Sound::AnvilUse,
            Sound::ArmorEquipChain,
            Sound::ArmorEquipCopper,
            Sound::ArmorEquipDiamond,
            Sound::ArmorEquipGeneric,
            Sound::ArmorEquipGold,
            Sound::ArmorEquipIron,
            Sound::ArmorEquipLeather,
            Sound::ArrowHit,
            Sound::BarrelClose,
            Sound::BarrelOpen,
            Sound::BellRing,
            Sound::BlastFurnace,
            Sound::BlazeShoot,
            Sound::BlockPlace(stone),
            Sound::BottleEmpty,
            Sound::BowShoot,
            Sound::BucketEmptyLava,
            Sound::BucketEmptyWater,
            Sound::BucketFillLava,
            Sound::BucketFillWater,
            Sound::Burp,
            Sound::Campfire,
            Sound::CauldronAddDye,
            Sound::CauldronCleanItem,
            Sound::CauldronDyeItem,
            Sound::CauldronEmptyLava,
            Sound::CauldronEmptyPotion,
            Sound::CauldronEmptyPowderSnow,
            Sound::CauldronEmptyWater,
            Sound::CauldronFillLava,
            Sound::CauldronFillPotion,
            Sound::CauldronFillPowderSnow,
            Sound::ChestClose,
            Sound::ChorusFlowerDie,
            Sound::ChorusFlowerGrow,
            Sound::CopperWaxApply,
            Sound::CopperWaxRemove,
            Sound::DoorBump,
            Sound::DoorCrash,
            Sound::Door { pitch: 0.0 },
            Sound::DripleafTiltDown,
            Sound::DripleafTiltUp,
            Sound::DyeUse,
            Sound::EnderChestClose,
            Sound::EnderChestOpen,
            Sound::EndermanTeleport,
            Sound::EntityAttackNoDamage,
            Sound::Explode,
            Sound::FireExtinguish,
            Sound::FireworkCrackle,
            Sound::FireworkExplosion,
            Sound::FireworkLargeExplosion,
            Sound::FireworkLaunch,
            Sound::Fizz { pitch: 0.0 },
            Sound::FlintSteel,
            Sound::Furnace,
            Sound::GhastShoot,
            Sound::Ghast,
            Sound::GlowBerriesPick,
            Sound::IceBombHit,
            Sound::Ignite,
            Sound::InkSacUse,
            Sound::ItemBreak,
            Sound::ItemFrameAddItem,
            Sound::ItemFrameRemoveItem,
            Sound::ItemFrameRotateItem,
            Sound::ItemUseOnBlock(stone),
            Sound::Launch { pitch: 0.0 },
            Sound::LecternPlaceBook,
            Sound::PaintingPlace,
            Sound::PotionFinishBrewing,
            Sound::PotionSplash,
            Sound::RecordStop,
            Sound::RespawnAnchorCharge,
            Sound::RespawnAnchorDeplete,
            Sound::RespawnAnchorSetSpawn,
            Sound::Scrape,
            Sound::ShulkerBoxClose,
            Sound::ShulkerBoxOpen,
            Sound::Smoker,
            Sound::SweetBerriesPick,
            Sound::Throw,
            Sound::TotemUse,
            Sound::TridentHitBlock,
            Sound::TridentHitEntity,
            Sound::TridentThrow,
        ];
        for sound in every {
            assert!(!sent(sound).0.is_empty(), "{sound:?}");
        }
    }
}
