//! What the client is told about a player's survival state: attributes (`UpdateAttributes`),
//! entity data (`SetActorData`), abilities (`UpdateAbilities`), adventure settings and game
//! rules, with the values BDS sends (P6) and the changes sent at the end of each tick.

use super::{
    DEFAULT_SATURATION, MAX_EXHAUSTION, MAX_FOOD, MAX_HEALTH, MAX_SATURATION, PLAYER_HEIGHT,
    PLAYER_WIDTH, Survival, broadcast_to_viewers, effects, f32_of, first_session, next_session,
};
use crate::config::GameMode;
use crate::effect;
use crate::player::MAX_LEVEL;
use crate::protocol::consts::{
    ABILITY_BASE_VERTICAL_FLY_SPEED, ABILITY_BASE_WALK_SPEED, Ability, AbilityLayerType,
    CommandPermissionLevel, EntityDataFlag, EntityDataKey, PermissionLevel,
};
use crate::protocol::packet::{
    GameRulesChanged, Packet, SetActorData, UpdateAbilities, UpdateAdventureSettings,
    UpdateAttributes,
};
use crate::protocol::{
    AbilityData, AbilityLayer, Attribute, AttributeValue, EntityMetadata, EntityProperties,
    FlagSet, GameRule, GameRuleValue, MetadataValue,
};
use crate::tick::{Phase, Player, SessionId, Settings, State};

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
/// The name tag or the score tag changed.
pub(super) const TAGS: u16 = 1 << 9;
/// Entity data the API set changed (flags, fire): the end of the tick compares it.
pub(super) const DATA: u16 = 1 << 10;
/// The adventure settings changed (auto jump).
pub(super) const ADVENTURE: u16 = 1 << 11;

/// The base movement speed (BDS and PMMP `Attribute::MOVEMENT_SPEED` default).
pub(super) const BASE_SPEED: f64 = 0.1;
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
/// (P6: 17 attributes, no modifiers).
const DEFS: [Def; 17] = [
    Def::new(
        "minecraft:player.hunger",
        0.0,
        f32_of(MAX_FOOD),
        f32_of(MAX_FOOD),
        HUNGER,
    ),
    Def::new(
        "minecraft:player.saturation",
        0.0,
        f32_of(MAX_SATURATION),
        f32_of(DEFAULT_SATURATION),
        SATURATION,
    ),
    Def::new(
        "minecraft:player.exhaustion",
        0.0,
        f32_of(MAX_EXHAUSTION),
        0.0,
        EXHAUSTION,
    ),
    Def::new(
        "minecraft:player.level",
        0.0,
        f32_of(MAX_LEVEL as f64),
        0.0,
        LEVEL,
    ),
    Def::new("minecraft:player.experience", 0.0, 1.0, 0.0, EXPERIENCE),
    Def::new(
        "minecraft:health",
        0.0,
        f32_of(MAX_HEALTH),
        f32_of(MAX_HEALTH),
        HEALTH,
    ),
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

/// The movement speed: the base (0.1 unless the API set another), times 1.3 sprinting, times 1 + 0.2 per speed level, times
/// 1 - 0.15 per slowness level (at least 0). PMMP multiplies and divides the attribute as each
/// changes, which drifts and stays at 0 once slowness reached it; Anchor works it out again.
pub(super) fn movement_speed(s: &Survival) -> f64 {
    let sprint = if s.motion.sprinting { 1.3 } else { 1.0 };
    let speed = 1.0 + 0.2 * f64::from(s.effect_level(effect::Type::Speed));
    let slow = 1.0 - 0.15 * f64::from(s.effect_level(effect::Type::Slowness));
    let slow = if slow > 0.0 { slow } else { 0.0 };
    s.custom.movement_speed * sprint * speed * slow
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

/// Every attribute of the player (spawn and respawn) but those of a system the settings turn off
/// (`hunger`, `experience`): the client shows their defaults, and the server never changes them.
pub(in crate::tick) fn all_attributes(player: &Player) -> Packet {
    let on = player.survival.switches;
    let hunger = if on.hunger {
        0
    } else {
        HUNGER | SATURATION | EXHAUSTION
    };
    let experience = if on.experience { 0 } else { LEVEL | EXPERIENCE };
    attributes_of(player, |d| d.bit & (hunger | experience) == 0)
}

/// The attributes whose bits are set in `bits`, in BDS order.
fn attributes_packet(player: &Player, bits: u16) -> Packet {
    attributes_of(player, |d| d.bit & bits != 0)
}

/// The attributes `keep` keeps, in BDS order.
fn attributes_of(player: &Player, keep: impl Fn(&Def) -> bool) -> Packet {
    let s = &player.survival;
    Packet::UpdateAttributes(UpdateAttributes {
        entity_runtime_id: player.runtime_id,
        attributes: DEFS
            .iter()
            .filter(|d| keep(d))
            .map(|d| attribute(d, s))
            .collect(),
        tick: 0,
    })
}

/// The entity data of a player that changes (keys 0, 7, 26, 28, 42, 92, 127, 129, 130, 131).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Shown {
    /// Entity flags, keys 0 and 92.
    flags: [u64; 2],
    /// Air supply, key 7.
    air: i16,
    /// The full air supply, key 42.
    max_air: i16,
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

crate::tick::checkpoint::fields!(Shown {
    flags,
    air,
    max_air,
    player_flags,
    bed,
    died,
    death,
    size,
    effects,
});

impl Shown {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            flags,
            air,
            max_air,
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
        h.u64(u64::from(max_air.cast_unsigned()));
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
pub(super) fn shown(player: &Player) -> Shown {
    let s = &player.survival;
    let mode = player.survival.mode();
    let mut flags: FlagSet<EntityDataFlag> = FlagSet::new();
    // BDS sends these for every player (P6); PMMP sets `CAN_CLIMB` for players too.
    flags.insert(EntityDataFlag::HiddenWhenInvisible);
    flags.insert(EntityDataFlag::PushTowardsClosestSpace);
    if mode != GameMode::Spectator {
        flags.insert(EntityDataFlag::HasCollision);
    }
    if !s.effects.contains_key(&effect::Type::Levitation) {
        flags.insert(EntityDataFlag::HasGravity);
    }
    for (on, flag) in [
        (s.custom.can_climb, EntityDataFlag::Climb),
        (s.custom.can_climb_walls, EntityDataFlag::WallClimbing),
        (s.fire_ticks > 0, EntityDataFlag::OnFire),
        (s.breathing, EntityDataFlag::Breathing),
        (s.custom.silent, EntityDataFlag::Silent),
        (s.custom.no_client_predictions, EntityDataFlag::NoAi),
        (s.motion.sneaking, EntityDataFlag::Sneaking),
        (s.motion.sprinting, EntityDataFlag::Sprinting),
        // The client predicts the use animation and stops it unless the server agrees with this
        // flag (bedrock-protocol-docs changelog 776 `StartUsingItem`): eating, drinking and
        // using a bow, trident, goat horn or spyglass (PMMP `ACTION`, set from `startAction`).
        (
            s.using.is_some() || player.breaking.is_using(),
            EntityDataFlag::UsingItem,
        ),
        (
            s.custom.invisible || s.effects.contains_key(&effect::Type::Invisibility),
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
        max_air: s.custom.max_air,
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
/// sneaking, as high as wide swimming or gliding.
pub(super) fn size(s: &Survival) -> [f32; 2] {
    let width = f32_of(PLAYER_WIDTH);
    if s.motion.swimming || s.motion.gliding {
        [width, width]
    } else if s.motion.sneaking {
        [width, 1.49]
    } else {
        [width, f32_of(PLAYER_HEIGHT)]
    }
}

/// The box width and height of a dying player, as BDS sends them at the death (P6 `others`).
pub(super) const DYING_SIZE: [f32; 2] = [0.2, 0.2];

/// `SetActorData` with a player's box width and height (keys 53, 54). The client keeps a dead
/// player's small box (it hits and draws the name tag by it) until these keys come, so BDS sends
/// them at the death and after the respawn (P6 `others`).
pub(super) fn size_data(runtime_id: u64, [width, height]: [f32; 2]) -> Packet {
    let mut m = EntityMetadata::new();
    m.insert(EntityDataKey::Width.into(), MetadataValue::Float32(width));
    m.insert(EntityDataKey::Height.into(), MetadataValue::Float32(height));
    Packet::SetActorData(SetActorData {
        entity_runtime_id: runtime_id,
        entity_metadata: m,
        entity_properties: EntityProperties::default(),
        tick: 0,
    })
}

/// The values of the changing keys whose value differs from `old` (all of them when `old` is
/// `None`).
fn changed_metadata(new: &Shown, old: Option<&Shown>, out: &mut EntityMetadata) {
    // Each flag word only when it changed, as BDS (P6: `{0: ...}` alone when Breathing changes).
    if old.is_none_or(|o| o.flags[0] != new.flags[0]) {
        out.insert(
            EntityDataKey::Flags.into(),
            MetadataValue::Int64(new.flags[0].cast_signed()),
        );
    }
    if old.is_none_or(|o| o.flags[1] != new.flags[1]) {
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
    if old.is_none_or(|o| o.max_air != new.max_air) {
        out.insert(
            EntityDataKey::AirSupplyMax.into(),
            MetadataValue::Int16(new.max_air),
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
        // BDS sends the death position as a block position (P6 `others` data types and its
        // death).
        out.insert(
            EntityDataKey::PlayerLastDeathPosition.into(),
            MetadataValue::BlockPos(new.death),
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

/// The whole entity data of a player as BDS sends it at spawn (P6: 72 keys; the types match BDS's
/// packet size). The settings argument is the spawn module's calling form.
pub(in crate::tick) fn full_metadata(_settings: &Settings, player: &Player) -> EntityMetadata {
    use EntityDataKey as K;
    use MetadataValue as V;
    let mut m = EntityMetadata::new();
    let mut put = |key: EntityDataKey, value: MetadataValue| {
        m.insert(key.into(), value);
    };
    put(K::StructuralIntegrity, V::Int32(1));
    put(K::Variant, V::Int32(0));
    put(K::ColorIndex, V::Byte(0));
    put(K::Name, V::String(player.name_tag().to_owned()));
    put(K::Owner, V::Int64(-1));
    put(K::Target, V::Int64(0));
    put(K::ChargeAmount, V::Byte(0));
    put(K::PlayerIndex, V::Int32(1));
    put(K::LeashHolder, V::Int64(0));
    put(K::HasNpc, V::Byte(0));
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
    put(K::Score, V::String(player.score_tag().to_owned()));
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
    // BDS sends this key as three floats (P6 `others` data types).
    put(K::EnterBedPosition, V::Vec3([0.0; 3]));
    put(K::SeatThirdPersonCameraRadius, V::Float32(0.0));
    put(K::SeatCameraRelaxDistanceSmoothing, V::Float32(0.0));
    put(K::AimAssistPriorityPresetId, V::Int32(-1));
    put(K::AimAssistPriorityCategoryId, V::Int32(-1));
    // BDS sends this key as an int32 (P6 `others` data types).
    put(K::AimAssistPriorityActorId, V::Int32(-1));
    put(K::FireworkDirection, V::Float32(64.0));
    changed_metadata(&shown(player), None, &mut m);
    m
}

/// The base abilities of a game mode, as BDS sends them for a member (P6: survival `Values` 63).
fn mode_abilities(mode: GameMode, flying: bool) -> FlagSet<Ability> {
    let survival = [
        Ability::Build,
        Ability::Mine,
        Ability::DoorsAndSwitches,
        Ability::OpenContainers,
        Ability::AttackPlayers,
        Ability::AttackMobs,
    ];
    match mode {
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
    }
}

/// The base ability layer with `values`, every ability defined (BDS).
fn layer_of(values: FlagSet<Ability>, fly_speed: f32) -> AbilityLayer {
    let all: FlagSet<Ability> = Ability::ALL.into_iter().collect();
    let [values, ..] = values.words();
    let [abilities, ..] = all.words();
    AbilityLayer {
        r#type: AbilityLayerType::Base.into(),
        abilities: u32::try_from(abilities).unwrap_or(u32::MAX),
        values: u32::try_from(values).unwrap_or(0),
        fly_speed,
        vertical_fly_speed: ABILITY_BASE_VERTICAL_FLY_SPEED,
        walk_speed: ABILITY_BASE_WALK_SPEED,
    }
}

/// The player's ability layers: its game mode's base layer with what the API set (PMMP
/// `NetworkSession::syncAbilities`): operator commands and teleporting for an operator, flight
/// allowed, flying in any mode, block collision and the flight speed. Without block collision outside spectator mode, a spectator layer makes the
/// client fly, as PMMP does: the client falls through the blocks otherwise.
fn layers(s: &Survival) -> Vec<AbilityLayer> {
    let mode = s.mode();
    let mut values = mode_abilities(mode, s.motion.flying);
    let collides = s.collides();
    for (on, ability) in [
        (s.custom.operator, Ability::OperatorCommands),
        (s.custom.operator, Ability::Teleport),
        (s.may_fly(), Ability::MayFly),
        (
            mode == GameMode::Spectator || s.motion.flying,
            Ability::Flying,
        ),
        (!collides, Ability::NoClip),
    ] {
        if on {
            values.insert(ability);
        } else {
            values.remove(ability);
        }
    }
    let mut layers = vec![layer_of(values, s.custom.flight_speed)];
    if !collides && mode != GameMode::Spectator {
        let flying: FlagSet<Ability> = [Ability::Flying].into_iter().collect();
        let [bits, ..] = flying.words();
        let bits = u32::try_from(bits).unwrap_or(0);
        layers.push(AbilityLayer {
            r#type: AbilityLayerType::Spectator.into(),
            abilities: bits,
            values: bits,
            fly_speed: 0.0,
            vertical_fly_speed: 0.0,
            walk_speed: 0.0,
        });
    }
    layers
}

/// `UpdateAbilities` for the player, with BDS's values: a member's permissions, or an operator's
/// (P6: `op` sends command permissions 1 and player permissions 2). The settings argument is the
/// spawn module's calling form.
pub(in crate::tick) fn update_abilities(_settings: &Settings, player: &Player) -> Packet {
    let (player_permissions, command_permissions) = if player.survival.custom.operator {
        (
            PermissionLevel::Operator,
            CommandPermissionLevel::GameDirectors,
        )
    } else {
        (PermissionLevel::Member, CommandPermissionLevel::Any)
    };
    Packet::UpdateAbilities(UpdateAbilities {
        ability_data: AbilityData {
            entity_unique_id: i64::try_from(player.runtime_id).unwrap_or(i64::MAX),
            player_permissions: player_permissions.into(),
            command_permissions: command_permissions.into(),
            layers: layers(&player.survival),
        },
    })
}

/// `UpdateAdventureSettings` as BDS sends it (P6).
pub(in crate::tick) fn adventure_settings() -> Packet {
    adventure_packet(true)
}

/// `UpdateAdventureSettings` with BDS's values and `auto_jump` (PMMP
/// `NetworkSession::syncAdventureSettings`).
fn adventure_packet(auto_jump: bool) -> Packet {
    Packet::UpdateAdventureSettings(UpdateAdventureSettings {
        no_pv_m: false,
        no_mv_p: false,
        immutable_world: false,
        show_name_tags: false,
        auto_jump,
    })
}

/// The 39 game rules BDS sends, in its order (P6), with the values Anchor's rules have: what Anchor
/// does not simulate (mob spawning, weather, insomnia, freezing, command blocks, respawn anchors
/// exploding, projectiles breaking blocks) is off, `pvp`, `randomTickSpeed`, the day cycle,
/// drowning and fall damage come from the settings, players respawn on the spawn block
/// (`spawnRadius` 0) and death messages are not broadcast.
pub(in crate::tick) fn game_rules(settings: &Settings) -> Packet {
    use GameRuleValue::{Bool, Uint32};
    let rules = [
        ("commandBlockOutput", Bool(true)),
        ("doDayLightCycle", Bool(settings.daylight_cycle)),
        ("doEntityDrops", Bool(true)),
        ("doFireTick", Bool(true)),
        ("recipesUnlock", Bool(true)),
        ("doLimitedCrafting", Bool(false)),
        ("doMobLoot", Bool(true)),
        ("doMobSpawning", Bool(false)),
        ("doTileDrops", Bool(true)),
        ("doWeatherCycle", Bool(false)),
        ("drowningDamage", Bool(settings.drowning)),
        ("fallDamage", Bool(settings.fall_damage)),
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
        flush_one(state, session);
    }
}

/// [`flush`] for one player, if it is spawned.
pub(super) fn flush_one(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase != Phase::Spawned {
        return;
    }
    let dirty = player.survival.dirty;
    let now = shown(player);
    let old = player.survival.shown;
    let abilities = (dirty & ABILITIES != 0).then(|| update_abilities(&state.settings, player));
    let adventure =
        (dirty & ADVENTURE != 0).then(|| adventure_packet(player.survival.custom.auto_jump));
    let attributes = (dirty & ALL != 0).then(|| attributes_packet(player, dirty & ALL));
    let health = (dirty & HEALTH != 0).then(|| health_packet(player));
    let tags = dirty & TAGS != 0;
    let data = (now != old || tags).then(|| {
        let mut m = EntityMetadata::new();
        changed_metadata(&now, Some(&old), &mut m);
        if tags {
            m.insert(
                EntityDataKey::Name.into(),
                MetadataValue::String(player.name_tag().to_owned()),
            );
            m.insert(
                EntityDataKey::Score.into(),
                MetadataValue::String(player.score_tag().to_owned()),
            );
        }
        Packet::SetActorData(SetActorData {
            entity_runtime_id: player.runtime_id,
            entity_metadata: m,
            entity_properties: EntityProperties::default(),
            tick: 0,
        })
    });
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.dirty = 0;
    player.survival.shown = now;
    for packet in [attributes, abilities, adventure].into_iter().flatten() {
        player.send_packet(packet);
    }
    if let Some(data) = data {
        player.send_packet(data.clone());
        broadcast_to_viewers(state, session, &data);
    }
    if let Some(health) = health {
        broadcast_to_viewers(state, session, &health);
    }
}

/// The player's health for those who see it, as BDS tells them at a hit, a death and a respawn
/// (P6 `others`). BDS sends viewers the player's food and exhaustion too, which the client shows
/// nobody but the player: left out, they would cost a packet per viewer each time they move.
pub(super) fn health_packet(player: &Player) -> Packet {
    attributes_packet(player, HEALTH)
}

/// Marks the player's current entity data as known to the client (its spawn window held it).
/// The spawn window's adventure settings jump by themselves: a player the API keeps from it gets
/// its own at the next flush.
pub(super) fn spawned(player: &mut Player) {
    let now = shown(player);
    player.survival.shown = now;
    player.survival.dirty = if player.survival.custom.auto_jump {
        0
    } else {
        ADVENTURE
    };
    player.survival.custom.touched = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::consts::ABILITY_BASE_FLY_SPEED;

    #[test]
    fn the_ability_layers_have_bds_values() {
        let mode_layer =
            |mode, flying| layer_of(mode_abilities(mode, flying), ABILITY_BASE_FLY_SPEED);
        let survival = mode_layer(GameMode::Survival, false);
        assert_eq!(survival.abilities, 1_048_575);
        assert_eq!(survival.values, 63);
        assert_eq!(survival.fly_speed, 0.05);
        assert_eq!(survival.vertical_fly_speed, 1.0);
        assert_eq!(survival.walk_speed, 0.1);
        assert_eq!(survival.r#type, 1);
        assert_eq!(mode_layer(GameMode::Adventure, false).values, 60);
        let creative = mode_layer(GameMode::Creative, true).values;
        assert_ne!(creative & 512, 0, "flying");
        assert_ne!(creative & 1024, 0, "may fly");
        assert_eq!(
            mode_layer(GameMode::Spectator, false).values & 1,
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
