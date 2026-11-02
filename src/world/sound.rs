//! Sounds a world plays at a position, for
//! [`Context::add_sound`](crate::Context::add_sound): each is the packet the official server
//! sends for it, with gophertunnel's names and numbers.
//!
//! Left out: sounds of an entity (`EntityLandSound`, `EntityLongFallSound`,
//! `EntityShortFallSound`: the entity rules play them) and pressure plates' (`PressurePlate*`:
//! redstone).
//!
//! A sound outside [`Sound`], such as a resource pack's named sound, cannot be played.

use crate::block;
use crate::item;
use crate::math::Vec3;
use crate::protocol::consts::{LevelEvent, SoundEvent};
use crate::protocol::packet::{
    LevelEvent as LevelEventPacket, LevelSoundEvent as LevelSoundEventPacket, Packet,
};

/// The calls of the goat horn types, in `GoatHornType` order.
const GOAT_CALLS: [SoundEvent; 8] = [
    SoundEvent::GoatCall0,
    SoundEvent::GoatCall1,
    SoundEvent::GoatCall2,
    SoundEvent::GoatCall3,
    SoundEvent::GoatCall4,
    SoundEvent::GoatCall5,
    SoundEvent::GoatCall6,
    SoundEvent::GoatCall7,
];
/// The entity type of a sound with no entity.
const NO_ENTITY: &str = ":";
/// The entity type of a player: the one hit and throw sounds carry, and the one the sounds a
/// player makes name.
pub(crate) const PLAYER_ENTITY_TYPE: &str = "minecraft:player";

/// A sound.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
// api-class: Sound
pub enum Sound {
    /// An amethyst block chiming.
    #[doc(alias = "AmethystBlockChimeSound")]
    AmethystBlockChime,
    /// Chainmail armor being put on.
    #[doc(alias = "ArmorEquipChainSound")]
    ArmorEquipChain,
    /// Copper armor being put on.
    #[doc(alias = "ArmorEquipCopperSound")]
    ArmorEquipCopper,
    /// Diamond armor being put on.
    #[doc(alias = "ArmorEquipDiamondSound")]
    ArmorEquipDiamond,
    /// Other armor being put on.
    #[doc(alias = "ArmorEquipGenericSound")]
    ArmorEquipGeneric,
    /// Golden armor being put on.
    #[doc(alias = "ArmorEquipGoldSound")]
    ArmorEquipGold,
    /// Iron armor being put on.
    #[doc(alias = "ArmorEquipIronSound")]
    ArmorEquipIron,
    /// Leather armor being put on.
    #[doc(alias = "ArmorEquipLeatherSound")]
    ArmorEquipLeather,
    /// Netherite armor being put on.
    #[doc(alias = "ArmorEquipNetheriteSound")]
    ArmorEquipNetherite,
    /// An arrow hitting.
    #[doc(alias = "ArrowHitSound")]
    ArrowHit,
    /// A barrel closing.
    #[doc(alias = "BarrelCloseSound")]
    BarrelClose,
    /// A barrel opening.
    #[doc(alias = "BarrelOpenSound")]
    BarrelOpen,
    /// A bell ringing.
    #[doc(alias = "BellRingSound")]
    BellRing,
    /// A blaze shooting.
    #[doc(alias = "BlazeShootSound")]
    BlazeShoot,
    /// A block of this state breaking.
    #[doc(alias = "BlockBreakSound")]
    BlockBreak(block::State),
    /// A block of this state being placed.
    #[doc(alias = "BlockPlaceSound")]
    BlockPlace(block::State),
    /// A block of this state being hit while it is broken.
    #[doc(alias = "BlockPunchSound")]
    BlockPunch(block::State),
    /// A bottle being emptied.
    #[doc(alias = "BottleEmptySound")]
    BottleEmpty,
    /// A bow shooting.
    #[doc(alias = "BowShootSound")]
    BowShoot,
    /// Lava being poured from a bucket.
    #[doc(alias = "BucketEmptyLavaSound")]
    BucketEmptyLava,
    /// Water being poured from a bucket.
    #[doc(alias = "BucketEmptyWaterSound")]
    BucketEmptyWater,
    /// A bucket being filled with lava.
    #[doc(alias = "BucketFillLavaSound")]
    BucketFillLava,
    /// A bucket being filled with water.
    #[doc(alias = "BucketFillWaterSound")]
    BucketFillWater,
    /// A burp after eating.
    #[doc(alias = "BurpSound")]
    Burp,
    /// Dye going into a cauldron.
    #[doc(alias = "CauldronAddDyeSound")]
    CauldronAddDye,
    /// A cauldron washing an item.
    #[doc(alias = "CauldronCleanItemSound")]
    CauldronCleanItem,
    /// A cauldron dyeing an item.
    #[doc(alias = "CauldronDyeItemSound")]
    CauldronDyeItem,
    /// Lava being taken from a cauldron.
    #[doc(alias = "CauldronEmptyLavaSound")]
    CauldronEmptyLava,
    /// A potion being taken from a cauldron.
    #[doc(alias = "CauldronEmptyPotionSound")]
    CauldronEmptyPotion,
    /// Powder snow being taken from a cauldron.
    #[doc(alias = "CauldronEmptyPowderSnowSound")]
    CauldronEmptyPowderSnow,
    /// Water being taken from a cauldron.
    #[doc(alias = "CauldronEmptyWaterSound")]
    CauldronEmptyWater,
    /// Lava going into a cauldron.
    #[doc(alias = "CauldronFillLavaSound")]
    CauldronFillLava,
    /// A potion going into a cauldron.
    #[doc(alias = "CauldronFillPotionSound")]
    CauldronFillPotion,
    /// Powder snow going into a cauldron.
    #[doc(alias = "CauldronFillPowderSnowSound")]
    CauldronFillPowderSnow,
    /// Water going into a cauldron.
    #[doc(alias = "CauldronFillWaterSound")]
    CauldronFillWater,
    /// A chest closing.
    #[doc(alias = "ChestCloseSound")]
    ChestClose,
    /// A chest opening.
    #[doc(alias = "ChestOpenSound")]
    ChestOpen,
    /// A chorus flower dying.
    #[doc(alias = "ChorusFlowerDieSound")]
    ChorusFlowerDie,
    /// A chorus flower growing.
    #[doc(alias = "ChorusFlowerGrowSound")]
    ChorusFlowerGrow,
    /// A click at `pitch` (0 by default).
    #[doc(alias = "ClickSound")]
    Click {
        /// The pitch.
        pitch: f32,
    },
    /// Copper being waxed.
    #[doc(alias = "CopperWaxApplySound")]
    CopperWaxApply,
    /// Wax coming off copper.
    #[doc(alias = "CopperWaxRemoveSound")]
    CopperWaxRemove,
    /// A door being bumped.
    #[doc(alias = "DoorBumpSound")]
    DoorBump,
    /// A door being broken in.
    #[doc(alias = "DoorCrashSound")]
    DoorCrash,
    /// A door opening or closing, at `pitch` (0 by default).
    #[doc(alias = "DoorSound")]
    Door {
        /// The pitch.
        pitch: f32,
    },
    /// A big dripleaf tilting down.
    #[doc(alias = "DripleafTiltDownSound")]
    DripleafTiltDown,
    /// A big dripleaf tilting up.
    #[doc(alias = "DripleafTiltUpSound")]
    DripleafTiltUp,
    /// Dye being used on a sign.
    #[doc(alias = "DyeUseSound")]
    DyeUse,
    /// An ender chest closing.
    #[doc(alias = "EnderChestCloseSound")]
    EnderChestClose,
    /// An ender chest opening.
    #[doc(alias = "EnderChestOpenSound")]
    EnderChestOpen,
    /// Something teleporting as an enderman does.
    #[doc(alias = "EndermanTeleportSound")]
    EndermanTeleport,
    /// A hit landing.
    #[doc(alias = "EntityAttackSound")]
    EntityAttack,
    /// A hit doing nothing.
    #[doc(alias = "EntityAttackNoDamageSound")]
    EntityAttackNoDamage,
    /// An explosion.
    #[doc(alias = "ExplodeSound")]
    Explode,
    /// A fire being put out.
    #[doc(alias = "FireExtinguishSound")]
    FireExtinguish,
    /// A firework crackling.
    #[doc(alias = "FireworkCrackleSound")]
    FireworkCrackle,
    /// A firework exploding.
    #[doc(alias = "FireworkExplosionSound")]
    FireworkExplosion,
    /// A large firework exploding.
    #[doc(alias = "FireworkLargeExplosionSound")]
    FireworkLargeExplosion,
    /// A firework launching.
    #[doc(alias = "FireworkLaunchSound")]
    FireworkLaunch,
    /// A fizz at `pitch` (0 by default).
    #[doc(alias = "FizzSound")]
    Fizz {
        /// The pitch.
        pitch: f32,
    },
    /// Flint and steel striking.
    #[doc(alias = "FlintSteelSound")]
    FlintSteel,
    /// A ghast shooting.
    #[doc(alias = "GhastShootSound")]
    GhastShoot,
    /// A ghast warning.
    #[doc(alias = "GhastSound")]
    Ghast,
    /// Glow berries being picked.
    #[doc(alias = "GlowBerriesPickSound")]
    GlowBerriesPick,
    /// A goat horn of this item type sounding: the call of its meta, 0 to 7 (in
    /// `GoatHornType` order); any other item plays the first call, as Anchor's horn rule does.
    #[doc(alias = "GoatHornSound")]
    GoatHorn(item::Type),
    /// An ice bomb hitting.
    #[doc(alias = "IceBombHitSound")]
    IceBombHit,
    /// TNT being lit.
    #[doc(alias = "IgniteSound")]
    Ignite,
    /// An ink sac being used on a sign.
    #[doc(alias = "InkSacUseSound")]
    InkSacUse,
    /// A tool breaking.
    #[doc(alias = "ItemBreakSound")]
    ItemBreak,
    /// An item going into an item frame.
    #[doc(alias = "ItemFrameAddItemSound")]
    ItemFrameAddItem,
    /// An item coming out of an item frame.
    #[doc(alias = "ItemFrameRemoveItemSound")]
    ItemFrameRemoveItem,
    /// An item frame's item turning.
    #[doc(alias = "ItemFrameRotateItemSound")]
    ItemFrameRotateItem,
    /// An item being used on a block of this state.
    #[doc(alias = "ItemUseOnBlockSound")]
    ItemUseOnBlock(block::State),
    /// A launch at `pitch` (0 by default).
    #[doc(alias = "LaunchSound")]
    Launch {
        /// The pitch.
        pitch: f32,
    },
    /// A book being put on a lectern.
    #[doc(alias = "LecternPlaceBookSound")]
    LecternPlaceBook,
    /// A note block playing `note` on `instrument`.
    #[doc(alias = "NoteSound")]
    Note {
        /// The instrument.
        instrument: NoteInstrument,
        /// The note: 0 to 24 are the note block's pitches.
        note: u8,
    },
    /// A painting being placed.
    #[doc(alias = "PaintingPlaceSound")]
    PaintingPlace,
    /// A pop at `pitch` (0 by default).
    #[doc(alias = "PopSound")]
    Pop {
        /// The pitch.
        pitch: f32,
    },
    /// A potion bottle shattering.
    #[doc(alias = "PotionSplashSound")]
    PotionSplash,
    /// A jukebox playing the music disc of this item type; any other item
    /// stops the music, as [`Sound::RecordStop`].
    #[doc(alias = "RecordSound")]
    Record(item::Type),
    /// A jukebox stopping.
    #[doc(alias = "RecordStopSound")]
    RecordStop,
    /// A lever or button switching on.
    #[doc(alias = "RedstonePowerOnSound")]
    RedstonePowerOn,
    /// A lever or button switching off.
    #[doc(alias = "RedstonePowerOffSound")]
    RedstonePowerOff,
    /// A respawn anchor being charged.
    #[doc(alias = "RespawnAnchorChargeSound")]
    RespawnAnchorCharge,
    /// A respawn anchor running out.
    #[doc(alias = "RespawnAnchorDepleteSound")]
    RespawnAnchorDeplete,
    /// A respawn anchor setting the spawn.
    #[doc(alias = "RespawnAnchorSetSpawnSound")]
    RespawnAnchorSetSpawn,
    /// An axe scraping copper.
    #[doc(alias = "ScrapeSound")]
    Scrape,
    /// A shulker box closing.
    #[doc(alias = "ShulkerBoxCloseSound")]
    ShulkerBoxClose,
    /// A shulker box opening.
    #[doc(alias = "ShulkerBoxOpenSound")]
    ShulkerBoxOpen,
    /// Sweet berries being picked.
    #[doc(alias = "SweetBerriesPickSound")]
    SweetBerriesPick,
    /// Something being thrown.
    #[doc(alias = "ThrowSound")]
    Throw,
    /// A totem saving its holder.
    #[doc(alias = "TotemUseSound")]
    TotemUse,
    /// A trident hitting a block.
    #[doc(alias = "TridentHitBlockSound")]
    TridentHitBlock,
    /// A trident hitting an entity.
    #[doc(alias = "TridentHitEntitySound")]
    TridentHitEntity,
    /// A trident being thrown.
    #[doc(alias = "TridentThrowSound")]
    TridentThrow,
    /// A splash in water at `volume` (0 to 1).
    #[doc(alias = "WaterSplashSound")]
    WaterSplash {
        /// The volume, 0 to 1.
        volume: f32,
    },
    /// An XP orb being collected.
    #[doc(alias = "XpCollectSound")]
    XpCollect,
    /// A player reaching XP level `level`.
    #[doc(alias = "XpLevelUpSound")]
    XpLevelUp {
        /// The level reached.
        level: u32,
    },
}

impl Sound {
    /// A [`packet`](Self::packet) heard at full volume however far `at` is (the `LevelSoundEvent`
    /// flag Mojang names Is Global); `None` for a sound sent as a `LevelEvent`, which has no such
    /// flag.
    pub(crate) fn full_volume_packet(self, at: Vec3) -> Option<Packet> {
        match self.packet(at) {
            Packet::LevelSoundEvent(sound) => {
                Some(Packet::LevelSoundEvent(LevelSoundEventPacket {
                    disable_relative_volume: true,
                    ..sound
                }))
            }
            _ => None,
        }
    }

    /// The packet that plays the sound at `at`.
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

    /// What the sound sends.
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one arm per sound, in one place"
    )]
    fn sent(self) -> Sent {
        use LevelEvent as L;
        use SoundEvent as S;
        let plain = |s| Sent::Sound(s, -1, NO_ENTITY);
        let level = |l| Sent::Level(l, 0);
        let block = |s, b: block::State| Sent::Sound(s, b.network_id().cast_signed(), NO_ENTITY);
        match self {
            Self::AmethystBlockChime => plain(S::AmethystBlockChime),
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
            // ATTACK seems dysfunctional, so a landed hit is ATTACK_STRONG.
            Self::EntityAttack => Sent::Sound(S::AttackStrong, -1, PLAYER_ENTITY_TYPE),
            Self::EntityAttackNoDamage => Sent::Sound(S::AttackNoDamage, -1, PLAYER_ENTITY_TYPE),
            Self::Explode => plain(S::Explode),
            Self::FireExtinguish => plain(S::ExtinguishFire),
            Self::FireworkCrackle => plain(S::Twinkle),
            Self::FireworkExplosion => plain(S::Blast),
            Self::FireworkLargeExplosion => plain(S::LargeBlast),
            Self::FireworkLaunch => plain(S::Launch),
            Self::Fizz { pitch } => Sent::Level(L::SoundFizz, permille(pitch)),
            Self::FlintSteel => plain(S::Ignite),
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
            // Item frames and paintings share the sound.
            Self::PaintingPlace => level(L::SoundItemFramePlace),
            Self::Pop { pitch } => Sent::Level(L::SoundInfinityArrowPickup, permille(pitch)),
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
            Self::SweetBerriesPick => plain(S::SweetBerryBushPick),
            Self::Throw => Sent::Sound(S::Throw, -1, PLAYER_ENTITY_TYPE),
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
                // the values the official server sends.
                let volume = 0x1000_0000_u32 * (level.min(30) / 5);
                Sent::Sound(S::LevelUp, volume.cast_signed(), NO_ENTITY)
            }
        }
    }
}

/// The instrument of a [`Sound::Note`], by the block under a note block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
// api-class: NoteInstrument
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
    /// The instrument's number in the `note` sound.
    const fn id(self) -> i32 {
        match self {
            Self::Piano => 0,
            Self::BassDrum => 1,
            Self::Snare => 2,
            Self::ClicksAndSticks => 3,
            Self::DoubleBass => 4,
            Self::Bell => 6,
            Self::Flute => 5,
            Self::Chime => 8,
            Self::Guitar => 7,
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

/// The call of a goat horn type: its meta in `GoatHornType` order; the
/// first for another item.
fn horn_call(horn: item::Type) -> SoundEvent {
    if horn.kind() != Some(item::Kind::GoatHorn) {
        return SoundEvent::GoatCall0;
    }
    GOAT_CALLS
        .get(usize::from(horn.meta()))
        .copied()
        .unwrap_or(SoundEvent::GoatCall0)
}

/// The music of a disc; the jukebox stop for another item.
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

/// A pitch as level events carry it: thousandths.
fn permille(pitch: f32) -> i32 {
    scaled(pitch, 1000.0)
}

/// `value × factor` as an `i32`, saturating (0 for a value that is not a number).
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: Rust's `as` saturates and maps NaN to 0, the same on every platform"
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

    // api: AmethystBlockChimeSound, ArmorEquipChainSound, ArmorEquipCopperSound,
    // api: ArmorEquipDiamondSound, ArmorEquipGenericSound, ArmorEquipGoldSound,
    // api: ArmorEquipIronSound, ArmorEquipLeatherSound, ArmorEquipNetheriteSound, ArrowHitSound,
    // api: BarrelCloseSound, BarrelOpenSound, BellRingSound, BlazeShootSound,
    // api: BlockBreakSound, BlockPlaceSound, BlockPunchSound, BottleEmptySound, BowShootSound,
    // api: BucketEmptyLavaSound, BucketEmptyWaterSound, BucketFillLavaSound,
    // api: BucketFillWaterSound, BurpSound, CauldronAddDyeSound,
    // api: CauldronCleanItemSound, CauldronDyeItemSound, CauldronEmptyLavaSound,
    // api: CauldronEmptyPotionSound, CauldronEmptyPowderSnowSound, CauldronEmptyWaterSound,
    // api: CauldronFillLavaSound, CauldronFillPotionSound, CauldronFillPowderSnowSound,
    // api: CauldronFillWaterSound, ChestCloseSound, ChestOpenSound, ChorusFlowerDieSound,
    // api: ChorusFlowerGrowSound, ClickSound, CopperWaxApplySound, CopperWaxRemoveSound,
    // api: DoorBumpSound, DoorCrashSound, DoorSound, DripleafTiltDownSound,
    // api: DripleafTiltUpSound, DyeUseSound, EnderChestCloseSound, EnderChestOpenSound,
    // api: EndermanTeleportSound, EntityAttackSound, EntityAttackNoDamageSound, ExplodeSound,
    // api: FireExtinguishSound, FireworkCrackleSound, FireworkExplosionSound,
    // api: FireworkLargeExplosionSound, FireworkLaunchSound, FizzSound, FlintSteelSound,
    // api: GhastShootSound, GhastSound, GlowBerriesPickSound, IceBombHitSound,
    // api: IgniteSound, InkSacUseSound, ItemBreakSound, ItemFrameAddItemSound,
    // api: ItemFrameRemoveItemSound, ItemFrameRotateItemSound, ItemUseOnBlockSound,
    // api: LaunchSound, LecternPlaceBookSound, PaintingPlaceSound, PopSound,
    // api: PotionSplashSound, RecordStopSound,
    // api: RespawnAnchorChargeSound, RespawnAnchorDepleteSound, RespawnAnchorSetSpawnSound,
    // api: ScrapeSound, ShulkerBoxCloseSound, ShulkerBoxOpenSound,
    // api: SweetBerriesPickSound, ThrowSound, TotemUseSound, TridentHitBlockSound,
    // api: TridentHitEntitySound, TridentThrowSound, WaterSplashSound, XpCollectSound,
    // api: XpLevelUpSound, Sound
    #[test]
    fn sounds_send_the_official_packets_with_gophertunnel_names() {
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
                (
                    "attack.strong".to_owned(),
                    -1,
                    PLAYER_ENTITY_TYPE.to_owned(),
                ),
            ),
            (
                Sound::ArmorEquipNetherite,
                ("armor.equip_netherite".to_owned(), -1, ":".to_owned()),
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

    // api: RedstonePowerOnSound, RedstonePowerOffSound
    #[test]
    fn levers_and_buttons_sound_as_redstone_power() {
        assert_eq!(
            sent(Sound::RedstonePowerOn),
            ("power.on".to_owned(), -1, ":".to_owned())
        );
        assert_eq!(
            sent(Sound::RedstonePowerOff),
            ("power.off".to_owned(), -1, ":".to_owned())
        );
    }

    // api: NoteSound, NoteInstrument, RecordSound, GoatHornSound
    #[test]
    fn notes_discs_and_horns_send_the_official_sound_events() {
        let none = crate::nbt::Compound::new();
        let ty = |name: &str, meta: u16| item::Type::from_name(name, meta, None, &none).unwrap();
        let play = |instrument, note| Sound::Note { instrument, note };
        assert_eq!(
            sent(play(NoteInstrument::Piano, 0)),
            ("note".to_owned(), 0, ":".to_owned())
        );
        // Flute is 5 and bell 6 on the wire, unlike their enum order.
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
            Sound::BlazeShoot,
            Sound::BlockPlace(stone),
            Sound::BottleEmpty,
            Sound::BowShoot,
            Sound::BucketEmptyLava,
            Sound::BucketEmptyWater,
            Sound::BucketFillLava,
            Sound::BucketFillWater,
            Sound::Burp,
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
            Sound::PotionSplash,
            Sound::RecordStop,
            Sound::RespawnAnchorCharge,
            Sound::RespawnAnchorDeplete,
            Sound::RespawnAnchorSetSpawn,
            Sound::Scrape,
            Sound::ShulkerBoxClose,
            Sound::ShulkerBoxOpen,
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
