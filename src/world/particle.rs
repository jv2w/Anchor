//! Particles a world shows at a position, for
//! [`Context::add_particle`](crate::Context::add_particle): each is the `LevelEvent` the official
//! server sends for it.
//!
//! Particle type numbers are the official server's: its snowball and egg hits send 15 and 14 and
//! its potion clouds 35 (P6). gophertunnel's `protocol::consts::ParticleType` puts `CANDLE_FLAME`
//! (9) and `BREEZE_WIND_EXPLOSION` (18) at its end, so its numbers from 9 on are one or two
//! lower. The numbers used are the variants' docs.
//!
//! Left out: floating text (an invisible entity per viewer, not an event; a human NPC with a name
//! tag does the same), and particles by name with `MoLang` variables (the `SpawnParticleEffect`
//! packet).

/// The official server's particle numbers that the particles use.
pub(crate) mod id {
    /// The number of the bubble particle.
    pub(crate) const BUBBLE: i32 = 1;
    /// The number of the critical hit particle.
    pub(crate) const CRITICAL: i32 = 3;
    /// The number of the block force field particle.
    pub(crate) const BLOCK_FORCE_FIELD: i32 = 4;
    /// The number of the smoke particle.
    pub(crate) const SMOKE: i32 = 5;
    /// The number of the explosion particle.
    pub(crate) const EXPLODE: i32 = 6;
    /// The number of the flame particle.
    pub(crate) const FLAME: i32 = 8;
    /// The number of the lava particle.
    pub(crate) const LAVA: i32 = 10;
    /// The number of the redstone particle.
    pub(crate) const REDSTONE: i32 = 12;
    /// The number of the item break particle.
    pub(crate) const ITEM_BREAK: i32 = 14;
    /// The number of the snowball poof particle.
    pub(crate) const SNOWBALL_POOF: i32 = 15;
    /// The number of the huge explosion particle.
    pub(crate) const HUGE_EXPLODE: i32 = 16;
    /// The number of the huge explosion seed particle.
    pub(crate) const HUGE_EXPLODE_SEED: i32 = 17;
    /// The number of the mob flame particle.
    pub(crate) const MOB_FLAME: i32 = 19;
    /// The number of the heart particle.
    pub(crate) const HEART: i32 = 20;
    /// The number of the terrain particle.
    pub(crate) const TERRAIN: i32 = 21;
    /// The number of the town aura particle.
    pub(crate) const TOWN_AURA: i32 = 22;
    /// The number of the portal particle.
    pub(crate) const PORTAL: i32 = 23;
    /// The number of the splash particle.
    pub(crate) const SPLASH: i32 = 25;
    /// The number of the water wake particle.
    pub(crate) const WATER_WAKE: i32 = 27;
    /// The number of the dripping water particle.
    pub(crate) const DRIP_WATER: i32 = 28;
    /// The number of the dripping lava particle.
    pub(crate) const DRIP_LAVA: i32 = 29;
    /// The number of the dust particle.
    pub(crate) const DUST: i32 = 33;
    /// The number of the mob spell particle.
    pub(crate) const MOB_SPELL: i32 = 34;
    /// The number of the instant mob spell particle.
    pub(crate) const MOB_SPELL_INSTANTANEOUS: i32 = 36;
    /// The number of the ink particle.
    pub(crate) const INK: i32 = 37;
    /// The number of the rain splash particle.
    pub(crate) const RAIN_SPLASH: i32 = 39;
    /// The number of the angry villager particle.
    pub(crate) const VILLAGER_ANGRY: i32 = 40;
    /// The number of the happy villager particle.
    pub(crate) const VILLAGER_HAPPY: i32 = 41;
    /// The number of the enchantment table particle.
    pub(crate) const ENCHANTMENT_TABLE: i32 = 42;
    /// The number of the sonic explosion particle.
    pub(crate) const SONIC_EXPLOSION: i32 = 85;
}

use crate::block::{self, Face};
use crate::item;
use crate::math::Vec3;
use crate::protocol::Rgba;
use crate::protocol::consts::LevelEvent;
use crate::protocol::packet::{LevelEvent as LevelEventPacket, Packet};
use crate::tile;

/// A particle effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
// api-class: Particle
pub enum Particle {
    /// An angry villager's cloud (`VILLAGER_ANGRY`, 40).
    #[doc(alias = "AngryVillagerParticle")]
    AngryVillager,
    /// The pieces of a block of this state breaking, with its sound (level event
    /// `ParticlesDestroyBlock`).
    #[doc(alias = "BlockBreakParticle")]
    BlockBreak(block::State),
    /// A block force field (`BLOCK_FORCE_FIELD`, 4) with its data (0 by default).
    #[doc(alias = "BlockForceFieldParticle")]
    BlockForceField(i32),
    /// The cracks of a block of this state being hit on a face (level event
    /// `ParticlesCrackBlock`).
    #[doc(alias = "BlockPunchParticle")]
    BlockPunch(block::State, Face),
    /// A bubble (`BUBBLE`, 1).
    #[doc(alias = "BubbleParticle")]
    Bubble,
    /// A critical hit's stars (`CRITICAL`, 3) of a scale (2 by default).
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
    /// Dust of a colour (`DUST`, 33).
    #[doc(alias = "DustParticle")]
    Dust(Rgba),
    /// Potion swirls of a colour (`MOB_SPELL`, 34).
    #[doc(alias = "EnchantParticle")]
    Enchant(Rgba),
    /// Enchanting table glyphs (`ENCHANTMENT_TABLE`, 42).
    #[doc(alias = "EnchantmentTableParticle")]
    EnchantmentTable,
    /// An enderman's teleport (level event `ParticlesTeleport`).
    #[doc(alias = "EndermanTeleportParticle")]
    EndermanTeleport,
    /// A burning entity's flame (`MOB_FLAME`, 19).
    #[doc(alias = "EntityFlameParticle")]
    EntityFlame,
    /// An explosion puff (`EXPLODE`, 6).
    #[doc(alias = "ExplodeParticle")]
    Explode,
    /// A flame (`FLAME`, 8).
    #[doc(alias = "FlameParticle")]
    Flame,
    /// A happy villager's sparkle (`VILLAGER_HAPPY`, 41).
    #[doc(alias = "HappyVillagerParticle")]
    HappyVillager,
    /// Hearts of a scale (`HEART`, 20; 0 by default).
    #[doc(alias = "HeartParticle")]
    Heart(i32),
    /// A large explosion (`HUGE_EXPLODE`, 16).
    #[doc(alias = "HugeExplodeParticle")]
    HugeExplode,
    /// An explosion emitter (`HUGE_EXPLODE_SEED`, 17).
    #[doc(alias = "HugeExplodeSeedParticle")]
    HugeExplodeSeed,
    /// Ink of a scale (`INK`, 37; 0 by default).
    #[doc(alias = "InkParticle")]
    Ink(i32),
    /// Instant potion swirls of a colour (`MOB_SPELL_INSTANTANEOUS`, 36).
    #[doc(alias = "InstantEnchantParticle")]
    InstantEnchant(Rgba),
    /// The pieces of an item of this type breaking (`ITEM_BREAK`, 14).
    #[doc(alias = "ItemBreakParticle")]
    ItemBreak(item::Type),
    /// A lava drip (`DRIP_LAVA`, 29).
    #[doc(alias = "LavaDripParticle")]
    LavaDrip,
    /// A lava pop (`LAVA`, 10).
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
    /// A portal's swirl (`PORTAL`, 23).
    #[doc(alias = "PortalParticle")]
    Portal,
    /// A splash potion's burst of a colour (level event `ParticlesPotionSplash`).
    #[doc(alias = "PotionSplashParticle")]
    PotionSplash(Rgba),
    /// A raindrop's splash (`RAIN_SPLASH`, 39).
    #[doc(alias = "RainSplashParticle")]
    RainSplash,
    /// Redstone dust lasting this long (`REDSTONE`, 12; 1 by default).
    #[doc(alias = "RedstoneParticle")]
    Redstone(i32),
    /// Smoke of a scale (`SMOKE`, 5; 0 by default).
    #[doc(alias = "SmokeParticle")]
    Smoke(i32),
    /// A snowball's puff (`SNOWBALL_POOF`, 15).
    #[doc(alias = "SnowballPoofParticle")]
    SnowballPoof,
    /// A sonic boom (`SONIC_EXPLOSION`, 85).
    #[doc(alias = "SonicExplosionParticle")]
    SonicExplosion,
    /// A water splash (`SPLASH`, 25).
    #[doc(alias = "SplashParticle")]
    Splash,
    /// Spores in the air (`TOWN_AURA`, 22).
    #[doc(alias = "SporeParticle")]
    Spore,
    /// Pieces of a block of this state (`TERRAIN`, 21).
    #[doc(alias = "TerrainParticle")]
    Terrain(block::State),
    /// A water drip (`DRIP_WATER`, 28).
    #[doc(alias = "WaterDripParticle")]
    WaterDrip,
    /// A wake in water (`WATER_WAKE`, 27).
    #[doc(alias = "WaterParticle")]
    Water,
}

impl Particle {
    /// The packet that shows the particle at `at`.
    pub(crate) fn packet(self, at: Vec3) -> Packet {
        let (event_type, event_data) = self.event();
        Packet::LevelEvent(LevelEventPacket {
            event_type,
            position: at.to_f32(),
            event_data,
        })
    }

    /// The level event type and data (a standard particle is the legacy event mask with the type).
    fn event(self) -> (i32, i32) {
        let standard = |t: i32, data: i32| (i32::from(LevelEvent::ParticleLegacyEvent) | t, data);
        let level = |e: LevelEvent, data: i32| (i32::from(e), data);
        match self {
            Self::AngryVillager => standard(id::VILLAGER_ANGRY, 0),
            Self::BlockBreak(b) => level(LevelEvent::ParticlesDestroyBlock, block_id(b)),
            Self::BlockForceField(data) => standard(id::BLOCK_FORCE_FIELD, data),
            Self::BlockPunch(b, face) => level(
                LevelEvent::ParticlesCrackBlock,
                punch_data(b.network_id(), face),
            ),
            Self::Bubble => standard(id::BUBBLE, 0),
            Self::Critical(scale) => standard(id::CRITICAL, scale),
            Self::DragonEggTeleport { x, y, z } => level(
                LevelEvent::ParticlesDragonEgg,
                axis(x, 16, 24) | axis(y, 8, 25) | axis(z, 0, 26),
            ),
            Self::Dust(c) => standard(id::DUST, tile::to_argb(c)),
            Self::Enchant(c) => standard(id::MOB_SPELL, tile::to_argb(c)),
            Self::EnchantmentTable => standard(id::ENCHANTMENT_TABLE, 0),
            Self::EndermanTeleport => level(LevelEvent::ParticlesTeleport, 0),
            Self::EntityFlame => standard(id::MOB_FLAME, 0),
            Self::Explode => standard(id::EXPLODE, 0),
            Self::Flame => standard(id::FLAME, 0),
            Self::HappyVillager => standard(id::VILLAGER_HAPPY, 0),
            Self::Heart(scale) => standard(id::HEART, scale),
            Self::HugeExplode => standard(id::HUGE_EXPLODE, 0),
            Self::HugeExplodeSeed => standard(id::HUGE_EXPLODE_SEED, 0),
            Self::Ink(scale) => standard(id::INK, scale),
            Self::InstantEnchant(c) => standard(id::MOB_SPELL_INSTANTANEOUS, tile::to_argb(c)),
            Self::ItemBreak(t) => {
                let s = item::Stack::new(t, 1).to_network().item_type;
                let meta = u16::try_from(s.metadata_value).unwrap_or(u16::MAX);
                let data = s.network_id.wrapping_shl(16) | i32::from(meta);
                standard(id::ITEM_BREAK, data)
            }
            Self::LavaDrip => standard(id::DRIP_LAVA, 0),
            Self::Lava => standard(id::LAVA, 0),
            Self::MobSpawn { width, height } => level(
                LevelEvent::ParticlesMobBlockSpawn,
                i32::from(width) | (i32::from(height) << 8),
            ),
            Self::Portal => standard(id::PORTAL, 0),
            Self::PotionSplash(c) => level(LevelEvent::ParticlesPotionSplash, tile::to_argb(c)),
            Self::RainSplash => standard(id::RAIN_SPLASH, 0),
            Self::Redstone(lifetime) => standard(id::REDSTONE, lifetime),
            Self::Smoke(scale) => standard(id::SMOKE, scale),
            Self::SnowballPoof => standard(id::SNOWBALL_POOF, 0),
            Self::SonicExplosion => standard(id::SONIC_EXPLOSION, 0),
            Self::Splash => standard(id::SPLASH, 0),
            Self::Spore => standard(id::TOWN_AURA, 0),
            Self::Terrain(b) => standard(id::TERRAIN, block_id(b)),
            Self::WaterDrip => standard(id::DRIP_WATER, 0),
            Self::Water => standard(id::WATER_WAKE, 0),
        }
    }
}

/// The network ID of a block state as particles carry it.
fn block_id(b: block::State) -> i32 {
    b.network_id().cast_signed()
}

/// The data of a block punch level event (`ParticlesCrackBlock`): the block's network ID with
/// the face hit in the top byte.
fn punch_data(network_id: u32, face: Face) -> i32 {
    network_id.cast_signed() | (i32::from(face.index()) << 24)
}

/// The dragon egg trail data of one axis: the size in a byte at `shift` and the sign in bit
/// `sign`.
fn axis(v: i16, shift: u32, sign: u32) -> i32 {
    let size = i32::from(v.unsigned_abs().min(255));
    (size << shift) | if v < 0 { 1 << sign } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bit a particle number gets in the packet's event number.
    const MASK: i32 = 0x4000;

    fn event(p: Particle) -> (i32, i32) {
        match p.packet(Vec3::new(0.5, 1.0, 2.0)) {
            Packet::LevelEvent(e) => {
                assert_eq!(e.position, [0.5, 1.0, 2.0]);
                (e.event_type, e.event_data)
            }
            other => panic!("{other:?}"),
        }
    }

    // The numbers are the official server's: 14 and 15 in its snowball and egg hits (P6), 35 in
    // its potion cloud (P6); gophertunnel's are smaller from 9 on.
    // api: AngryVillagerParticle, BlockBreakParticle, BlockForceFieldParticle,
    // api: BlockPunchParticle, BubbleParticle, CriticalParticle, DragonEggTeleportParticle,
    // api: DustParticle, EnchantParticle, EnchantmentTableParticle, EndermanTeleportParticle,
    // api: EntityFlameParticle, ExplodeParticle, FlameParticle, HappyVillagerParticle,
    // api: HeartParticle, HugeExplodeParticle, HugeExplodeSeedParticle, InkParticle,
    // api: InstantEnchantParticle, ItemBreakParticle, LavaDripParticle, LavaParticle,
    // api: MobSpawnParticle, PortalParticle, PotionSplashParticle, RainSplashParticle,
    // api: RedstoneParticle, SmokeParticle, SnowballPoofParticle, SonicExplosionParticle,
    // api: SplashParticle, SporeParticle, TerrainParticle, WaterDripParticle, WaterParticle,
    // api: Particle
    #[test]
    fn particles_send_the_official_events_with_the_official_numbers() {
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
            (Particle::AngryVillager, (MASK + 40, 0)),
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
                // The sign of x in bit 24, of y in 25, of z in 26.
                (2010, 0x0103_ff04),
            ),
            (Particle::Dust(red), (MASK + 33, argb_red)),
            (Particle::Enchant(red), (MASK + 34, argb_red)),
            (Particle::EnchantmentTable, (MASK + 42, 0)),
            (Particle::EndermanTeleport, (2013, 0)),
            (Particle::EntityFlame, (MASK + 19, 0)),
            (Particle::Explode, (MASK + 6, 0)),
            (Particle::Flame, (MASK + 8, 0)),
            (Particle::HappyVillager, (MASK + 41, 0)),
            (Particle::Heart(1), (MASK + 20, 1)),
            (Particle::HugeExplode, (MASK + 16, 0)),
            (Particle::HugeExplodeSeed, (MASK + 17, 0)),
            (Particle::Ink(2), (MASK + 37, 2)),
            (Particle::InstantEnchant(red), (MASK + 36, argb_red)),
            (Particle::ItemBreak(stick), (MASK + 14, network << 16)),
            (Particle::LavaDrip, (MASK + 29, 0)),
            (Particle::Lava, (MASK + 10, 0)),
            (
                Particle::MobSpawn {
                    width: 2,
                    height: 3,
                },
                (2004, 0x0302),
            ),
            (Particle::Portal, (MASK + 23, 0)),
            (Particle::PotionSplash(red), (2002, argb_red)),
            (Particle::RainSplash, (MASK + 39, 0)),
            (Particle::Redstone(1), (MASK + 12, 1)),
            (Particle::Smoke(0), (MASK + 5, 0)),
            (Particle::SnowballPoof, (MASK + 15, 0)),
            (Particle::SonicExplosion, (MASK + 85, 0)),
            (Particle::Splash, (MASK + 25, 0)),
            (Particle::Spore, (MASK + 22, 0)),
            (Particle::Terrain(stone), (MASK + 21, id)),
            (Particle::WaterDrip, (MASK + 28, 0)),
            (Particle::Water, (MASK + 27, 0)),
        ];
        for (particle, want) in cases {
            assert_eq!(event(particle), want, "{particle:?}");
        }
    }
}
