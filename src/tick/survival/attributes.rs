//! What the client is told about a player's survival state: attributes (`UpdateAttributes`),
//! entity data (`SetActorData`), abilities (`UpdateAbilities`), adventure settings and game
//! rules, with the values BDS sends (P6 facts 7) and the changes sent at the end of each tick.

use super::{
    MAX_AIR, Survival, broadcast_to_viewers, effects, f32_of, first_session, game_mode_of,
    next_session,
};
use crate::config::GameMode;
use crate::effect;
use crate::protocol::consts::{
    ABILITY_BASE_FLY_SPEED, ABILITY_BASE_VERTICAL_FLY_SPEED, ABILITY_BASE_WALK_SPEED, Ability,
    AbilityLayerType, EntityDataFlag, EntityDataKey,
};
use crate::protocol::packet::{
    GameRulesChanged, Packet, SetActorData, UpdateAbilities, UpdateAdventureSettings,
    UpdateAttributes,
};
use crate::protocol::{
    AbilityData, AbilityLayer, Attribute, AttributeValue, EntityMetadata, EntityProperties,
    FlagSet, GameRule, GameRuleValue, MetadataValue,
};
use crate::tick::{Phase, Player, Settings, State};

/// Health changed.
pub(super) const HEALTH: u16 = 1;
/// Food level changed.
pub(super) const HUNGER: u16 = 1 << 1;
/// Saturation changed.
pub(super) const SATURATION: u16 = 1 << 2;
/// Exhaustion changed.
pub(super) const EXHAUSTION: u16 = 1 << 3;
/// Experience level changed.
pub(super) const LEVEL: u16 = 1 << 4;
/// Experience progress changed.
pub(super) const EXPERIENCE: u16 = 1 << 5;
/// Absorption changed.
pub(super) const ABSORPTION: u16 = 1 << 6;
/// Movement speed changed (sprinting, speed and slowness).
pub(super) const MOVEMENT: u16 = 1 << 7;
/// Every attribute (respawn).
pub(super) const ALL: u16 = 0xff;
/// Abilities changed (game mode, flying).
pub(super) const ABILITIES: u16 = 1 << 8;

/// The base movement speed (BDS and PMMP `Attribute::MOVEMENT_SPEED` default).
const BASE_SPEED: f64 = 0.1;
/// The absorption maximum BDS sends; more absorption raises it.
const ABSORPTION_MAX: f32 = 16.0;

/// What changes an attribute: its dirty bit, or 0 for one that never changes.
#[derive(Debug, Clone, Copy)]
struct Def {
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    bit: u16,
}

impl Def {
    const fn new(name: &'static str, min: f32, max: f32, default: f32, bit: u16) -> Self {
        Self {
            name,
            min,
            max,
            default,
            bit,
        }
    }
}

/// The attributes of a player in the order BDS sends them, with BDS's ranges and defaults
/// (P6 facts 7: 17 attributes, no modifiers).
const DEFS: [Def; 17] = [
    Def::new("minecraft:player.hunger", 0.0, 20.0, 20.0, HUNGER),
    Def::new("minecraft:player.saturation", 0.0, 20.0, 5.0, SATURATION),
    Def::new("minecraft:player.exhaustion", 0.0, 20.0, 0.0, EXHAUSTION),
    Def::new("minecraft:player.level", 0.0, 24_791.0, 0.0, LEVEL),
    Def::new("minecraft:player.experience", 0.0, 1.0, 0.0, EXPERIENCE),
    Def::new("minecraft:health", 0.0, 20.0, 20.0, HEALTH),
    Def::new("minecraft:follow_range", 0.0, 2048.0, 16.0, 0),
    Def::new("minecraft:knockback_resistance", -2.0, 1.0, 0.0, 0),
    Def::new("minecraft:movement", 0.0, f32::MAX, 0.1, MOVEMENT),
    Def::new("minecraft:underwater_movement", 0.0, f32::MAX, 0.02, 0),
    Def::new("minecraft:lava_movement", 0.0, f32::MAX, 0.02, 0),
    Def::new("minecraft:attack_damage", 1.0, 1.0, 1.0, 0),
    Def::new("minecraft:absorption", 0.0, ABSORPTION_MAX, 0.0, ABSORPTION),
    Def::new("minecraft:luck", -1024.0, 1024.0, 0.0, 0),
    Def::new("minecraft:friction_modifier", 0.0, 256.0, 1.0, 0),
    Def::new("minecraft:bounciness", 0.0, 1.0, 0.0, 0),
    Def::new("minecraft:air_drag_modifier", 0.0, 256.0, 1.0, 0),
];

/// The movement speed: the base, times 1.3 sprinting, times 1 + 0.2 per speed level, times
/// 1 - 0.15 per slowness level (at least 0). PMMP multiplies and divides the attribute as each
/// changes, which drifts and stays at 0 once slowness reached it; Anchor works it out again.
pub(super) fn movement_speed(s: &Survival) -> f64 {
    let sprint = if s.motion.sprinting { 1.3 } else { 1.0 };
    let speed = 1.0 + 0.2 * f64::from(s.effect_level(effect::Type::Speed));
    let slow = 1.0 - 0.15 * f64::from(s.effect_level(effect::Type::Slowness));
    let slow = if slow > 0.0 { slow } else { 0.0 };
    BASE_SPEED * sprint * speed * slow
}

/// One attribute with its current value.
fn attribute(def: &Def, s: &Survival) -> Attribute {
    let (value, max) = match def.bit {
        HUNGER => (f32_of(s.food), def.max),
        SATURATION => (f32_of(s.saturation), def.max),
        EXHAUSTION => (f32_of(s.exhaustion), def.max),
        LEVEL => (f32_of(f64::from(s.xp_level)), def.max),
        EXPERIENCE => (f32_of(s.xp_progress), def.max),
        HEALTH => (f32_of(s.health.ceil()), f32_of(s.max_health())),
        MOVEMENT => (f32_of(movement_speed(s)), def.max),
        ABSORPTION => {
            let value = f32_of(s.absorption);
            (value, if value > def.max { value } else { def.max })
        }
        _ => (def.default, def.max),
    };
    Attribute {
        attribute_value: AttributeValue {
            name: def.name.to_owned(),
            value,
            max,
            min: def.min,
        },
        default_min: def.min,
        default_max: def.max,
        default: def.default,
        modifiers: Vec::new(),
    }
}

/// Every attribute of the player (spawn and respawn).
pub(in crate::tick) fn all_attributes(player: &Player) -> Packet {
    attributes_packet(player, u16::MAX)
}

/// The attributes whose bits are set in `bits`, in BDS order (the fixed ones only for
/// [`all_attributes`]).
fn attributes_packet(player: &Player, bits: u16) -> Packet {
    let s = &player.survival;
    Packet::UpdateAttributes(UpdateAttributes {
        entity_runtime_id: player.runtime_id,
        attributes: DEFS
            .iter()
            .filter(|d| bits == u16::MAX || d.bit & bits != 0)
            .map(|d| attribute(d, s))
            .collect(),
        tick: 0,
    })
}

/// The entity data of a player that changes (keys 0, 7, 26, 28, 92, 127, 129, 130, 131).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Shown {
    /// Entity flags, keys 0 and 92.
    flags: [u64; 2],
    /// Air supply, key 7.
    air: i16,
    /// Player flags (bit 1: sleeping), key 26.
    player_flags: u8,
    /// The bed position, key 28.
    bed: [i32; 3],
    /// Whether the player has died, key 129, and where, key 127.
    died: bool,
    death: [i32; 3],
    /// The collision box width and height, key 130.
    size: [f32; 2],
    /// The packed visible effects, key 131.
    effects: i64,
}

impl Shown {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            flags,
            air,
            player_flags,
            bed,
            died,
            death,
            size,
            effects,
        } = self;
        for w in flags {
            h.u64(*w);
        }
        h.u64(u64::from(air.cast_unsigned()));
        h.u64(u64::from(*player_flags));
        for c in bed.iter().chain(death) {
            h.u64(u64::from(c.cast_unsigned()));
        }
        h.u64(u64::from(*died));
        for c in size {
            h.u64(u64::from(c.to_bits()));
        }
        h.u64(effects.cast_unsigned());
    }
}

/// `PlayerFlagSleep`: bit 1 of the player flags (PMMP `PlayerMetadataFlags::SLEEP`).
const PLAYER_FLAG_SLEEP: u8 = 1 << 1;

/// The changing entity data of a player now.
pub(super) fn shown(settings: &Settings, player: &Player) -> Shown {
    let s = &player.survival;
    let mode = game_mode_of(settings, player);
    let mut flags: FlagSet<EntityDataFlag> = FlagSet::new();
    // BDS sends these for every player (P6 facts 7); PMMP sets `CAN_CLIMB` for players too.
    flags.insert(EntityDataFlag::Climb);
    flags.insert(EntityDataFlag::HiddenWhenInvisible);
    flags.insert(EntityDataFlag::PushTowardsClosestSpace);
    if mode != GameMode::Spectator {
        flags.insert(EntityDataFlag::HasCollision);
    }
    if !s.effects.contains_key(&effect::Type::Levitation) {
        flags.insert(EntityDataFlag::HasGravity);
    }
    for (on, flag) in [
        (s.fire_ticks > 0, EntityDataFlag::OnFire),
        (s.motion.sneaking, EntityDataFlag::Sneaking),
        (s.motion.sprinting, EntityDataFlag::Sprinting),
        (s.using.is_some(), EntityDataFlag::UsingItem),
        (
            s.effects.contains_key(&effect::Type::Invisibility),
            EntityDataFlag::Invisible,
        ),
        (s.motion.gliding, EntityDataFlag::Gliding),
        (s.motion.swimming, EntityDataFlag::Swimming),
    ] {
        if on {
            flags.insert(flag);
        }
    }
    let [w0, w1, _] = flags.words();
    let bed = s.sleeping.map_or([0; 3], |b| [b.x, b.y, b.z]);
    Shown {
        flags: [w0, w1],
        air: s.air,
        player_flags: if s.sleeping.is_some() {
            PLAYER_FLAG_SLEEP
        } else {
            0
        },
        bed,
        died: s.death.is_some(),
        death: s
            .death
            .as_ref()
            .map_or([0; 3], |p| [p.pos.x, p.pos.y, p.pos.z]),
        size: size(s),
        effects: effects::packed(s),
    }
}

/// The collision box width and height (PMMP `Living::recalculateSize`): 0.6 by 1.8, 1.49 high
/// sneaking, 0.6 high swimming or gliding.
pub(super) fn size(s: &Survival) -> [f32; 2] {
    if s.motion.swimming || s.motion.gliding {
        [0.6, 0.6]
    } else if s.motion.sneaking {
        [0.6, 1.49]
    } else {
        [0.6, 1.8]
    }
}

/// The values of the changing keys whose value differs from `old` (all of them when `old` is
/// `None`).
fn changed_metadata(new: &Shown, old: Option<&Shown>, out: &mut EntityMetadata) {
    if old.is_none_or(|o| o.flags != new.flags) {
        out.insert(
            EntityDataKey::Flags.into(),
            MetadataValue::Int64(new.flags[0].cast_signed()),
        );
        out.insert(
            EntityDataKey::FlagsTwo.into(),
            MetadataValue::Int64(new.flags[1].cast_signed()),
        );
    }
    if old.is_none_or(|o| o.air != new.air) {
        out.insert(
            EntityDataKey::AirSupply.into(),
            MetadataValue::Int16(new.air),
        );
    }
    if old.is_none_or(|o| o.player_flags != new.player_flags) {
        out.insert(
            EntityDataKey::PlayerFlags.into(),
            MetadataValue::Byte(new.player_flags),
        );
    }
    if old.is_none_or(|o| o.bed != new.bed) {
        out.insert(
            EntityDataKey::BedPosition.into(),
            MetadataValue::BlockPos(new.bed),
        );
    }
    if old.is_none_or(|o| o.died != new.died || o.death != new.death) {
        let d = new.death;
        // BDS sends the death position as three floats (P6 SetActorData size).
        out.insert(
            EntityDataKey::PlayerLastDeathPosition.into(),
            MetadataValue::Vec3([
                f32_of(f64::from(d[0])),
                f32_of(f64::from(d[1])),
                f32_of(f64::from(d[2])),
            ]),
        );
        out.insert(
            EntityDataKey::PlayerHasDied.into(),
            MetadataValue::Byte(u8::from(new.died)),
        );
    }
    if old.is_none_or(|o| o.size != new.size) {
        out.insert(
            EntityDataKey::CollisionBox.into(),
            MetadataValue::Vec3([new.size[0], new.size[1], 0.0]),
        );
    }
    if old.is_none_or(|o| o.effects != new.effects) {
        out.insert(
            EntityDataKey::VisibleMobEffects.into(),
            MetadataValue::Int64(new.effects),
        );
    }
}

/// The whole entity data of a player as BDS sends it at spawn (P6 facts 7: 72 keys; the types
/// match BDS's packet size).
pub(in crate::tick) fn full_metadata(settings: &Settings, player: &Player) -> EntityMetadata {
    use EntityDataKey as K;
    use MetadataValue as V;
    let mut m = EntityMetadata::new();
    let mut put = |key: EntityDataKey, value: MetadataValue| {
        m.insert(key.into(), value);
    };
    put(K::StructuralIntegrity, V::Int32(1));
    put(K::Variant, V::Int32(0));
    put(K::ColorIndex, V::Byte(0));
    put(K::Name, V::String(player.identity.name.clone()));
    put(K::Owner, V::Int64(-1));
    put(K::Target, V::Int64(0));
    put(K::ChargeAmount, V::Byte(0));
    put(K::PlayerIndex, V::Int32(1));
    put(K::LeashHolder, V::Int64(0));
    put(K::HasNpc, V::Byte(0));
    put(K::AirSupplyMax, V::Int16(MAX_AIR));
    put(K::MarkVariant, V::Int32(0));
    put(K::ContainerType, V::Byte(0));
    put(K::ContainerSize, V::Int32(0));
    put(K::ContainerStrengthModifier, V::Int32(0));
    put(K::FuseTime, V::Int32(-1));
    put(K::SeatLockPassengerRotation, V::Byte(0));
    put(K::SeatLockPassengerRotationDegrees, V::Float32(0.0));
    put(K::SeatRotationOffset, V::Byte(0));
    put(K::SeatRotationOffsetDegrees, V::Float32(0.0));
    put(K::HasCommandBlock, V::Byte(0));
    put(K::CommandName, V::String(String::new()));
    put(K::LastCommandOutput, V::String(String::new()));
    put(K::TrackCommandOutput, V::Byte(1));
    put(K::Strength, V::Int32(0));
    put(K::StrengthMax, V::Int32(0));
    put(K::DataSpellCastingColor, V::Int32(0));
    put(K::DataLifetimeTicks, V::Int32(-1));
    put(K::AlwaysShowNameTag, V::Byte(1));
    put(K::ColorTwoIndex, V::Byte(0));
    put(K::Score, V::String(String::new()));
    put(K::Agent, V::Int64(-1));
    put(K::EatingCounter, V::Int32(0));
    put(K::InteractText, V::String(String::new()));
    put(K::TradeTier, V::Int32(0));
    put(K::MaxTradeTier, V::Int32(0));
    put(K::TradeExperience, V::Int32(0));
    put(K::SkinId, V::Int32(0));
    put(K::CommandBlockTickDelay, V::Int32(3));
    put(K::CommandBlockExecuteOnFirstTick, V::Byte(1));
    put(K::AmbientSoundInterval, V::Float32(8.0));
    put(K::AmbientSoundIntervalRange, V::Float32(16.0));
    put(K::AmbientSoundEventName, V::String("ambient".to_owned()));
    put(K::FallDamageMultiplier, V::Float32(1.0));
    put(K::LowTierCuredTradeDiscount, V::Int32(0));
    put(K::HighTierCuredTradeDiscount, V::Int32(0));
    put(K::NearbyCuredTradeDiscount, V::Int32(0));
    put(K::NearbyCuredDiscountTimeStamp, V::Int32(0));
    put(K::HitBox, V::CompoundTag(crate::nbt::Compound::new()));
    put(K::IsBuoyant, V::Byte(0));
    put(K::FreezingEffectStrength, V::Float32(0.0));
    put(K::MovementSoundDistanceOffset, V::Float32(1.0));
    put(K::HeartbeatIntervalTicks, V::Int32(20));
    put(K::HeartbeatSoundEvent, V::Int32(614));
    put(K::PlayerLastDeathDimension, V::Int32(0));
    put(K::FilteredName, V::String(String::new()));
    put(K::EnterBedPosition, V::BlockPos([0, 0, 0]));
    put(K::SeatThirdPersonCameraRadius, V::Float32(0.0));
    put(K::SeatCameraRelaxDistanceSmoothing, V::Float32(0.0));
    put(K::AimAssistPriorityPresetId, V::Int32(-1));
    put(K::AimAssistPriorityCategoryId, V::Int32(-1));
    put(K::AimAssistPriorityActorId, V::Int64(-1));
    put(K::FireworkDirection, V::Float32(64.0));
    changed_metadata(&shown(settings, player), None, &mut m);
    m
}

/// The base ability layer for a game mode, as BDS sends it for a member (P6 facts 7: every
/// ability defined, survival `Values` 63).
fn layer(mode: GameMode, flying: bool) -> AbilityLayer {
    let survival = [
        Ability::Build,
        Ability::Mine,
        Ability::DoorsAndSwitches,
        Ability::OpenContainers,
        Ability::AttackPlayers,
        Ability::AttackMobs,
    ];
    let set: FlagSet<Ability> = match mode {
        GameMode::Survival => survival.into_iter().collect(),
        GameMode::Adventure => survival[2..].iter().copied().collect(),
        GameMode::Creative => survival
            .into_iter()
            .chain([
                Ability::Invulnerable,
                Ability::MayFly,
                Ability::InstantBuild,
            ])
            .chain(flying.then_some(Ability::Flying))
            .collect(),
        GameMode::Spectator => [
            Ability::Invulnerable,
            Ability::MayFly,
            Ability::Flying,
            Ability::NoClip,
        ]
        .into_iter()
        .collect(),
    };
    let all: FlagSet<Ability> = Ability::ALL.into_iter().collect();
    let [values, ..] = set.words();
    let [abilities, ..] = all.words();
    AbilityLayer {
        r#type: AbilityLayerType::Base.into(),
        abilities: u32::try_from(abilities).unwrap_or(u32::MAX),
        values: u32::try_from(values).unwrap_or(0),
        fly_speed: ABILITY_BASE_FLY_SPEED,
        vertical_fly_speed: ABILITY_BASE_VERTICAL_FLY_SPEED,
        walk_speed: ABILITY_BASE_WALK_SPEED,
    }
}

/// `UpdateAbilities` for the player (member permissions, BDS values).
pub(in crate::tick) fn update_abilities(settings: &Settings, player: &Player) -> Packet {
    let mode = game_mode_of(settings, player);
    Packet::UpdateAbilities(UpdateAbilities {
        ability_data: AbilityData {
            entity_unique_id: i64::try_from(player.runtime_id).unwrap_or(i64::MAX),
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![layer(mode, player.survival.motion.flying)],
        },
    })
}

/// `UpdateAdventureSettings` as BDS sends it (P6 facts 7).
pub(in crate::tick) fn adventure_settings() -> Packet {
    Packet::UpdateAdventureSettings(UpdateAdventureSettings {
        no_pv_m: false,
        no_mv_p: false,
        immutable_world: false,
        show_name_tags: false,
        auto_jump: true,
    })
}

/// The 39 game rules BDS sends, in its order (P6 facts 7), with the values Anchor's rules have:
/// what Anchor does not simulate (mob spawning, weather, insomnia, freezing, command blocks,
/// respawn anchors exploding, projectiles breaking blocks) is off, `pvp` and `randomTickSpeed`
/// come from the settings, players respawn on the spawn block (`spawnRadius` 0) and death
/// messages are not broadcast.
pub(in crate::tick) fn game_rules(settings: &Settings) -> Packet {
    use GameRuleValue::{Bool, Uint32};
    let rules = [
        ("commandBlockOutput", Bool(true)),
        ("doDayLightCycle", Bool(true)),
        ("doEntityDrops", Bool(true)),
        ("doFireTick", Bool(true)),
        ("recipesUnlock", Bool(true)),
        ("doLimitedCrafting", Bool(false)),
        ("doMobLoot", Bool(true)),
        ("doMobSpawning", Bool(false)),
        ("doTileDrops", Bool(true)),
        ("doWeatherCycle", Bool(false)),
        ("drowningDamage", Bool(true)),
        ("fallDamage", Bool(true)),
        ("fireDamage", Bool(true)),
        ("keepInventory", Bool(false)),
        ("mobGriefing", Bool(true)),
        ("pvp", Bool(settings.pvp)),
        ("showCoordinates", Bool(false)),
        ("playerWaypoints", Uint32(1)),
        ("locatorbar", Bool(true)),
        ("showDaysPlayed", Bool(false)),
        ("naturalRegeneration", Bool(true)),
        ("tntExplodes", Bool(true)),
        ("sendCommandFeedback", Bool(true)),
        ("maxCommandChainLength", Uint32(65_535)),
        ("doInsomnia", Bool(false)),
        ("commandBlocksEnabled", Bool(false)),
        ("randomTickSpeed", Uint32(settings.random_tick_speed)),
        ("doImmediateRespawn", Bool(false)),
        ("showDeathMessages", Bool(false)),
        ("functionCommandLimit", Uint32(10_000)),
        ("spawnRadius", Uint32(0)),
        ("showTags", Bool(true)),
        ("freezeDamage", Bool(false)),
        ("respawnBlocksExplode", Bool(false)),
        ("showBorderEffect", Bool(true)),
        ("showRecipeMessages", Bool(true)),
        ("playersSleepingPercentage", Uint32(100)),
        ("projectilesCanBreakBlocks", Bool(false)),
        ("tntExplosionDropDecay", Bool(false)),
    ];
    Packet::GameRulesChanged(GameRulesChanged {
        game_rules: rules
            .into_iter()
            .map(|(name, value)| GameRule {
                name: name.to_owned(),
                can_be_modified_by_player: true,
                value,
            })
            .collect(),
    })
}

/// Sends every spawned player the attributes, abilities and entity data that changed this tick:
/// attributes and abilities to the player, entity data to the player and its viewers.
pub(super) fn flush(state: &mut State) {
    let mut next = first_session(state);
    while let Some(session) = next {
        next = next_session(state, session);
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        if player.phase != Phase::Spawned {
            continue;
        }
        let dirty = player.survival.dirty;
        let now = shown(&state.settings, player);
        let old = player.survival.shown;
        let abilities = (dirty & ABILITIES != 0).then(|| update_abilities(&state.settings, player));
        let attributes = (dirty & ALL != 0).then(|| attributes_packet(player, dirty & ALL));
        let data = (now != old).then(|| {
            let mut m = EntityMetadata::new();
            changed_metadata(&now, Some(&old), &mut m);
            Packet::SetActorData(SetActorData {
                entity_runtime_id: player.runtime_id,
                entity_metadata: m,
                entity_properties: EntityProperties::default(),
                tick: 0,
            })
        });
        let Some(player) = state.players.get_mut(&session) else {
            continue;
        };
        player.survival.dirty = 0;
        player.survival.shown = now;
        for packet in [attributes, abilities].into_iter().flatten() {
            player.send(packet);
        }
        if let Some(data) = data {
            player.send(data.clone());
            broadcast_to_viewers(state, session, &data);
        }
    }
}

/// Marks the player's current entity data as known to the client (its spawn window held it).
pub(super) fn spawned(settings: &Settings, player: &mut Player) {
    let now = shown(settings, player);
    player.survival.shown = now;
    player.survival.dirty = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ability_layers_have_bds_values() {
        let survival = layer(GameMode::Survival, false);
        assert_eq!(survival.abilities, 1_048_575);
        assert_eq!(survival.values, 63);
        assert_eq!(survival.fly_speed, 0.05);
        assert_eq!(survival.vertical_fly_speed, 1.0);
        assert_eq!(survival.walk_speed, 0.1);
        assert_eq!(survival.r#type, 1);
        assert_eq!(layer(GameMode::Adventure, false).values, 60);
        let creative = layer(GameMode::Creative, true).values;
        assert_ne!(creative & 512, 0, "flying");
        assert_ne!(creative & 1024, 0, "may fly");
        assert_eq!(
            layer(GameMode::Spectator, false).values & 1,
            0,
            "no building"
        );
    }

    #[test]
    fn movement_speed_is_recomputed_without_drift() {
        let mut s = Survival::default();
        assert_eq!(movement_speed(&s), 0.1);
        s.motion.sprinting = true;
        assert!((movement_speed(&s) - 0.13).abs() < 1e-12);
        s.motion.sprinting = false;
        assert_eq!(movement_speed(&s), 0.1);
    }

    #[test]
    fn the_flag_words_match_bds() {
        let mut flags: FlagSet<EntityDataFlag> = FlagSet::new();
        flags.insert(EntityDataFlag::Climb);
        flags.insert(EntityDataFlag::HiddenWhenInvisible);
        flags.insert(EntityDataFlag::PushTowardsClosestSpace);
        flags.insert(EntityDataFlag::HasCollision);
        flags.insert(EntityDataFlag::HasGravity);
        let [w0, w1, _] = flags.words();
        assert_eq!(w0, 844_424_930_656_256);
        assert_eq!(w1, 35_184_405_643_264);
    }
}
