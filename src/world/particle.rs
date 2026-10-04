//! Particles a world shows at a position (PMMP `world\particle\*`), for
//! [`Context::add_particle`](crate::Context::add_particle): each is the `LevelEvent` PMMP sends for
//! its class.
//!
//! Particle type numbers are gophertunnel's (`protocol::consts::ParticleType`, PLAN.md 4.9), not
//! PMMP's: PMMP numbers `CANDLE_FLAME` 9 and `BREEZE_WIND_EXPLOSION` 18, which gophertunnel puts
//! at 81 and 90, so PMMP's numbers from 9 on are one or two higher. The numbers used are the
//! variants' docs; comparing them with BDS (`/particle`) is pending (PLAN.md 10.8 검증 필요, P6).
//!
//! Left out: `FloatingTextParticle` (an invisible entity per viewer, not an event; a human NPC
//! with a name tag does the same) and the base class `Particle` (this enum).

use crate::block::{self, Face};
use crate::item;
use crate::math::Vec3;
use crate::protocol::Rgba;
use crate::protocol::consts::{LevelEvent, ParticleType};
use crate::protocol::packet::{LevelEvent as LevelEventPacket, Packet};
use crate::tile;

/// A particle effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// pmmp-class: Particle
pub enum Particle {
    /// An angry villager's cloud (`VillagerAngry`, 38).
    #[doc(alias = "AngryVillagerParticle")]
    AngryVillager,
    /// The pieces of a block of this state breaking, with its sound (level event
    /// `ParticlesDestroyBlock`).
    #[doc(alias = "BlockBreakParticle")]
    BlockBreak(block::State),
    /// A block force field (`BlockForceField`, 4) with its data (PMMP's default 0).
    #[doc(alias = "BlockForceFieldParticle")]
    BlockForceField(i32),
    /// The cracks of a block of this state being hit on a face (level event
    /// `ParticlesCrackBlock`).
    #[doc(alias = "BlockPunchParticle")]
    BlockPunch(block::State, Face),
    /// A bubble (`Bubble`, 1).
    #[doc(alias = "BubbleParticle")]
    Bubble,
    /// A critical hit's stars (`Crit`, 3) of a scale (PMMP's default 2).
    #[doc(alias = "CriticalParticle")]
    Critical(i32),
    /// A dragon egg's trail to a block this many blocks away on each axis (level event
    /// `ParticlesDragonEgg`; each axis -255 to 255, larger ones are cut).
    #[doc(alias = "DragonEggTeleportParticle")]
    DragonEggTeleport {
        /// Blocks along x.
        x: i16,
        /// Blocks along y.
        y: i16,
        /// Blocks along z.
        z: i16,
    },
    /// Dust of a colour (`FallingDust`, 31).
    #[doc(alias = "DustParticle")]
    Dust(Rgba),
    /// Potion swirls of a colour (`MobSpell`, 32).
    #[doc(alias = "EnchantParticle")]
    Enchant(Rgba),
    /// Enchanting table glyphs (`EnchantingTable`, 40).
    #[doc(alias = "EnchantmentTableParticle")]
    EnchantmentTable,
    /// An enderman's teleport (level event `ParticlesTeleport`).
    #[doc(alias = "EndermanTeleportParticle")]
    EndermanTeleport,
    /// A burning entity's flame (`MobFlame`, 17).
    #[doc(alias = "EntityFlameParticle")]
    EntityFlame,
    /// An explosion puff (`Explode`, 6).
    #[doc(alias = "ExplodeParticle")]
    Explode,
    /// A flame (`Flame`, 8).
    #[doc(alias = "FlameParticle")]
    Flame,
    /// A happy villager's sparkle (`VillagerHappy`, 39).
    #[doc(alias = "HappyVillagerParticle")]
    HappyVillager,
    /// Hearts of a scale (`Heart`, 18; PMMP's default 0).
    #[doc(alias = "HeartParticle")]
    Heart(i32),
    /// A large explosion (`LargeExplode`, 15; PMMP's `HUGE_EXPLODE` 16).
    #[doc(alias = "HugeExplodeParticle")]
    HugeExplode,
    /// An explosion emitter (`HugeExplosion`, 16; PMMP's `HUGE_EXPLODE_SEED` 17).
    #[doc(alias = "HugeExplodeSeedParticle")]
    HugeExplodeSeed,
    /// Ink of a scale (`Ink`, 35; PMMP's default 0).
    #[doc(alias = "InkParticle")]
    Ink(i32),
    /// Instant potion swirls of a colour (`MobSpellInstantaneous`, 34).
    #[doc(alias = "InstantEnchantParticle")]
    InstantEnchant(Rgba),
    /// The pieces of an item of this type breaking (`IconCrack`, 13).
    #[doc(alias = "ItemBreakParticle")]
    ItemBreak(item::Type),
    /// A lava drip (`DripLava`, 27).
    #[doc(alias = "LavaDripParticle")]
    LavaDrip,
    /// A lava pop (`Lava`, 9).
    #[doc(alias = "LavaParticle")]
    Lava,
    /// A spawner's flames around a box of this width and height in blocks (level event
    /// `ParticlesMobBlockSpawn`).
    #[doc(alias = "MobSpawnParticle")]
    MobSpawn {
        /// The width.
        width: u8,
        /// The height.
        height: u8,
    },
    /// A portal's swirl (`Portal`, 21).
    #[doc(alias = "PortalParticle")]
    Portal,
    /// A splash potion's burst of a colour (level event `ParticlesPotionSplash`).
    #[doc(alias = "PotionSplashParticle")]
    PotionSplash(Rgba),
    /// A raindrop's splash (`RainSplash`, 37).
    #[doc(alias = "RainSplashParticle")]
    RainSplash,
    /// Redstone dust lasting this long (`RedDust`, 11; PMMP's default 1).
    #[doc(alias = "RedstoneParticle")]
    Redstone(i32),
    /// Smoke of a scale (`Smoke`, 5; PMMP's default 0).
    #[doc(alias = "SmokeParticle")]
    Smoke(i32),
    /// A snowball's puff (`SnowballPoof`, 14).
    #[doc(alias = "SnowballPoofParticle")]
    SnowballPoof,
    /// A sonic boom (`SonicExplosion`, 84).
    #[doc(alias = "SonicExplosionParticle")]
    SonicExplosion,
    /// A water splash (`WaterSplash`, 23).
    #[doc(alias = "SplashParticle")]
    Splash,
    /// Spores in the air (`TownAura`, 20).
    #[doc(alias = "SporeParticle")]
    Spore,
    /// Pieces of a block of this state (`Terrain`, 19).
    #[doc(alias = "TerrainParticle")]
    Terrain(block::State),
    /// A water drip (`DripWater`, 26).
    #[doc(alias = "WaterDripParticle")]
    WaterDrip,
    /// A wake in water (`WaterWake`, 25).
    #[doc(alias = "WaterParticle")]
    Water,
}

/// The network ID of a block state as particles carry it.
fn block_id(b: block::State) -> i32 {
    b.network_id().cast_signed()
}

/// The dragon egg trail data of one axis (PMMP `DragonEggTeleportParticle::encode`): the size in
/// a byte at `shift` and the sign in bit `sign`.
fn axis(v: i16, shift: u32, sign: u32) -> i32 {
    let size = i32::from(v.unsigned_abs().min(255));
    (size << shift) | if v < 0 { 1 << sign } else { 0 }
}

impl Particle {
    /// The packet that shows the particle at `at` (PMMP `Particle::encode`).
    pub(crate) fn packet(self, at: Vec3) -> Packet {
        let (event_type, event_data) = self.event();
        Packet::LevelEvent(LevelEventPacket {
            event_type,
            position: at.to_f32(),
            event_data,
        })
    }

    /// The level event type and data (a standard particle is the legacy event mask with the type,
    /// PMMP `LevelEventPacket::standardParticle`).
    fn event(self) -> (i32, i32) {
        use ParticleType as T;
        let standard = |t: T, data: i32| {
            (
                i32::from(LevelEvent::ParticleLegacyEvent) | i32::from(t),
                data,
            )
        };
        let level = |e: LevelEvent, data: i32| (i32::from(e), data);
        match self {
            Self::AngryVillager => standard(T::VillagerAngry, 0),
            Self::BlockBreak(b) => level(LevelEvent::ParticlesDestroyBlock, block_id(b)),
            Self::BlockForceField(data) => standard(T::BlockForceField, data),
            Self::BlockPunch(b, face) => level(
                LevelEvent::ParticlesCrackBlock,
                block_id(b) | (i32::from(face.index()) << 24),
            ),
            Self::Bubble => standard(T::Bubble, 0),
            Self::Critical(scale) => standard(T::Crit, scale),
            Self::DragonEggTeleport { x, y, z } => level(
                LevelEvent::ParticlesDragonEgg,
                axis(x, 16, 24) | axis(y, 8, 25) | axis(z, 0, 26),
            ),
            Self::Dust(c) => standard(T::FallingDust, tile::to_argb(c)),
            Self::Enchant(c) => standard(T::MobSpell, tile::to_argb(c)),
            Self::EnchantmentTable => standard(T::EnchantingTable, 0),
            Self::EndermanTeleport => level(LevelEvent::ParticlesTeleport, 0),
            Self::EntityFlame => standard(T::MobFlame, 0),
            Self::Explode => standard(T::Explode, 0),
            Self::Flame => standard(T::Flame, 0),
            Self::HappyVillager => standard(T::VillagerHappy, 0),
            Self::Heart(scale) => standard(T::Heart, scale),
            Self::HugeExplode => standard(T::LargeExplode, 0),
            Self::HugeExplodeSeed => standard(T::HugeExplosion, 0),
            Self::Ink(scale) => standard(T::Ink, scale),
            Self::InstantEnchant(c) => standard(T::MobSpellInstantaneous, tile::to_argb(c)),
            Self::ItemBreak(t) => {
                let s = item::Stack::new(t, 1).to_network().item_type;
                let meta = u16::try_from(s.metadata_value).unwrap_or(u16::MAX);
                let data = s.network_id.wrapping_shl(16) | i32::from(meta);
                standard(T::IconCrack, data)
            }
            Self::LavaDrip => standard(T::DripLava, 0),
            Self::Lava => standard(T::Lava, 0),
            Self::MobSpawn { width, height } => level(
                LevelEvent::ParticlesMobBlockSpawn,
                i32::from(width) | (i32::from(height) << 8),
            ),
            Self::Portal => standard(T::Portal, 0),
            Self::PotionSplash(c) => level(LevelEvent::ParticlesPotionSplash, tile::to_argb(c)),
            Self::RainSplash => standard(T::RainSplash, 0),
            Self::Redstone(lifetime) => standard(T::RedDust, lifetime),
            Self::Smoke(scale) => standard(T::Smoke, scale),
            Self::SnowballPoof => standard(T::SnowballPoof, 0),
            Self::SonicExplosion => standard(T::SonicExplosion, 0),
            Self::Splash => standard(T::WaterSplash, 0),
            Self::Spore => standard(T::TownAura, 0),
            Self::Terrain(b) => standard(T::Terrain, block_id(b)),
            Self::WaterDrip => standard(T::DripWater, 0),
            Self::Water => standard(T::WaterWake, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(p: Particle) -> (i32, i32) {
        match p.packet(Vec3::new(0.5, 1.0, 2.0)) {
            Packet::LevelEvent(e) => {
                assert_eq!(e.position, [0.5, 1.0, 2.0]);
                (e.event_type, e.event_data)
            }
            other => panic!("{other:?}"),
        }
    }

    // pmmp: AngryVillagerParticle, BlockBreakParticle, BlockForceFieldParticle, BlockPunchParticle, BubbleParticle, CriticalParticle, DragonEggTeleportParticle, DustParticle, EnchantParticle, EnchantmentTableParticle, EndermanTeleportParticle, EntityFlameParticle, ExplodeParticle, FlameParticle, HappyVillagerParticle, HeartParticle, HugeExplodeParticle, HugeExplodeSeedParticle, InkParticle, InstantEnchantParticle, ItemBreakParticle, LavaDripParticle, LavaParticle, MobSpawnParticle, PortalParticle, PotionSplashParticle, RainSplashParticle, RedstoneParticle, SmokeParticle, SnowballPoofParticle, SonicExplosionParticle, SplashParticle, SporeParticle, TerrainParticle, WaterDripParticle, WaterParticle, Particle
    #[test]
    fn particles_send_pmmps_events_with_gophertunnel_numbers() {
        const MASK: i32 = 0x4000;
        let stone = block::State::find("minecraft:stone", &[]).unwrap();
        let id = stone.network_id().cast_signed();
        let red = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let argb_red = 0xffff_0000_u32.cast_signed();
        let stick = item::Type::from_name("minecraft:stick", 0, None, &crate::nbt::Compound::new())
            .unwrap();
        let network = item::Stack::new(stick, 1).to_network().item_type.network_id;
        let cases = [
            (Particle::AngryVillager, (MASK + 38, 0)),
            (Particle::BlockBreak(stone), (2001, id)),
            (Particle::BlockForceField(3), (MASK + 4, 3)),
            (
                Particle::BlockPunch(stone, Face::Up),
                (2014, id + (1 << 24)),
            ),
            (Particle::Bubble, (MASK + 1, 0)),
            (Particle::Critical(2), (MASK + 3, 2)),
            (
                Particle::DragonEggTeleport {
                    x: -3,
                    y: 300,
                    z: 4,
                },
                (2010, 0x0103_ff04),
            ),
            (Particle::Dust(red), (MASK + 31, argb_red)),
            (Particle::Enchant(red), (MASK + 32, argb_red)),
            (Particle::EnchantmentTable, (MASK + 40, 0)),
            (Particle::EndermanTeleport, (2013, 0)),
            (Particle::EntityFlame, (MASK + 17, 0)),
            (Particle::Explode, (MASK + 6, 0)),
            (Particle::Flame, (MASK + 8, 0)),
            (Particle::HappyVillager, (MASK + 39, 0)),
            (Particle::Heart(1), (MASK + 18, 1)),
            (Particle::HugeExplode, (MASK + 15, 0)),
            (Particle::HugeExplodeSeed, (MASK + 16, 0)),
            (Particle::Ink(2), (MASK + 35, 2)),
            (Particle::InstantEnchant(red), (MASK + 34, argb_red)),
            (Particle::ItemBreak(stick), (MASK + 13, network << 16)),
            (Particle::LavaDrip, (MASK + 27, 0)),
            (Particle::Lava, (MASK + 9, 0)),
            (
                Particle::MobSpawn {
                    width: 2,
                    height: 3,
                },
                (2004, 0x0302),
            ),
            (Particle::Portal, (MASK + 21, 0)),
            (Particle::PotionSplash(red), (2002, argb_red)),
            (Particle::RainSplash, (MASK + 37, 0)),
            (Particle::Redstone(1), (MASK + 11, 1)),
            (Particle::Smoke(0), (MASK + 5, 0)),
            (Particle::SnowballPoof, (MASK + 14, 0)),
            (Particle::SonicExplosion, (MASK + 84, 0)),
            (Particle::Splash, (MASK + 23, 0)),
            (Particle::Spore, (MASK + 20, 0)),
            (Particle::Terrain(stone), (MASK + 19, id)),
            (Particle::WaterDrip, (MASK + 26, 0)),
            (Particle::Water, (MASK + 25, 0)),
        ];
        for (particle, want) in cases {
            assert_eq!(event(particle), want, "{particle:?}");
        }
    }
}
