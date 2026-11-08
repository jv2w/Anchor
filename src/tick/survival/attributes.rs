//! What the client is told about a player's survival state: attributes (`UpdateAttributes`),
//! entity data (`SetActorData`), abilities (`UpdateAbilities`), adventure settings and game
//! rules, with the values the official server sends (P6) and the changes sent at the end of each
//! tick.

use super::{
    DEFAULT_SATURATION, MAX_EXHAUSTION, MAX_FOOD, MAX_HEALTH, MAX_SATURATION, PLAYER_HEIGHT,
    PLAYER_WIDTH, Survival, broadcast_to_viewers, effects, first_session, next_session,
};
use crate::config::GameMode;
use crate::effect;
use crate::math::{self, to_f32};
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
use crate::tick::update::RANDOM_TICKS_PER_SUB_CHUNK;
use crate::tick::{Phase, Player, SessionId, Settings, State, checkpoint, entity};

/// Abilities changed (game mode, flying).
pub(super) const ABILITIES: u16 = 1 << 8;
/// Absorption changed.
pub(super) const ABSORPTION: u16 = 1 << 6;
/// The adventure settings changed (auto jump).
pub(super) const ADVENTURE: u16 = 1 << 11;
/// Every attribute.
pub(super) const ALL: u16 =
    HEALTH | FOOD | SATURATION | EXHAUSTION | LEVEL | XP | ABSORPTION | MOVEMENT;
/// The base movement speed.
pub(super) const BASE_SPEED: f64 = 0.1;
/// Entity data the API set changed (flags, fire): the end of the tick compares it.
pub(super) const DATA: u16 = 1 << 10;
/// The attributes of a player in the order the official server sends them, with its ranges and
/// defaults (P6: 17 attributes, no modifiers).
const DEFS: [Def; 17] = [
    Def::new(
        "minecraft:player.hunger",
        0.0,
        to_f32(MAX_FOOD),
        to_f32(MAX_FOOD),
        FOOD,
    ),
    Def::new(
        "minecraft:player.saturation",
        0.0,
        to_f32(MAX_SATURATION),
        to_f32(DEFAULT_SATURATION),
        SATURATION,
    ),
    Def::new(
        "minecraft:player.exhaustion",
        0.0,
        to_f32(MAX_EXHAUSTION),
        0.0,
        EXHAUSTION,
    ),
    Def::new(
        "minecraft:player.level",
        0.0,
        to_f32(MAX_LEVEL as f64),
        0.0,
        LEVEL,
    ),
    Def::new("minecraft:player.experience", 0.0, 1.0, 0.0, XP),
    Def::new(
        HEALTH_ATTRIBUTE,
        0.0,
        to_f32(MAX_HEALTH),
        to_f32(MAX_HEALTH),
        HEALTH,
    ),
    Def::new("minecraft:follow_range", 0.0, 2048.0, 16.0, 0),
    Def::new("minecraft:knockback_resistance", -2.0, 1.0, 0.0, 0),
    Def::new(
        "minecraft:movement",
        0.0,
        f32::MAX,
        to_f32(BASE_SPEED),
        MOVEMENT,
    ),
    Def::new("minecraft:underwater_movement", 0.0, f32::MAX, 0.02, 0),
    Def::new("minecraft:lava_movement", 0.0, f32::MAX, 0.02, 0),
    Def::new("minecraft:attack_damage", 1.0, 1.0, 1.0, 0),
    Def::new("minecraft:absorption", 0.0, MAX_ABSORPTION, 0.0, ABSORPTION),
    Def::new("minecraft:luck", -1024.0, 1024.0, 0.0, 0),
    Def::new("minecraft:friction_modifier", 0.0, 256.0, 1.0, 0),
    Def::new("minecraft:bounciness", 0.0, 1.0, 0.0, 0),
    Def::new("minecraft:air_drag_modifier", 0.0, 256.0, 1.0, 0),
];
/// The box width and height of a dying player, as the official server sends them at the death (P6
/// `others`).
pub(super) const DYING_SIZE: [f32; 2] = [0.2, 0.2];
/// Exhaustion changed.
pub(super) const EXHAUSTION: u16 = 1 << 3;
/// Food level changed.
pub(super) const FOOD: u16 = 1 << 1;
/// Health changed.
pub(super) const HEALTH: u16 = 1;
/// The name of the health attribute.
pub(in crate::tick) const HEALTH_ATTRIBUTE: &str = "minecraft:health";
/// Xp level changed.
pub(super) const LEVEL: u16 = 1 << 4;
/// The absorption maximum the official server sends; more absorption raises it.
const MAX_ABSORPTION: f32 = 16.0;
/// Movement speed changed (sprinting, speed and slowness).
pub(super) const MOVEMENT: u16 = 1 << 7;
/// Saturation changed.
pub(super) const SATURATION: u16 = 1 << 2;
/// The box height of a sneaking player.
const SNEAKING_HEIGHT: f32 = 1.49;
/// The name tag or the score tag changed.
pub(super) const TAGS: u16 = 1 << 9;
/// Xp progress changed.
pub(super) const XP: u16 = 1 << 5;

/// An attribute's name, range and default, and the dirty bit that marks it changed (0 for one
/// that never changes).
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

/// The entity data of a player that changes (keys 0, 7, 42, 53, 54, 92, 127, 129, 131).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Shown {
    /// Entity flags, keys 0 and 92.
    flags: [u64; 2],
    /// Air supply, key 7.
    air: i16,
    /// The full air supply, key 42.
    max_air: i16,
    /// Whether the player has died, key 129, and where, key 127.
    died: bool,
    death: [i32; 3],
    /// The box width and height, keys 53 and 54.
    pub(super) size: [f32; 2],
    /// The packed visible effects, key 131.
    effects: i64,
}

impl Shown {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            flags,
            air,
            max_air,
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
        h.u64(u64::from(*died));
        for c in death {
            h.u64(u64::from(c.cast_unsigned()));
        }
        for c in size {
            h.u64(u64::from(c.to_bits()));
        }
        h.u64(effects.cast_unsigned());
    }
}

checkpoint::fields!(Shown {
    flags,
    air,
    max_air,
    died,
    death,
    size,
    effects,
});

/// Sends every spawned player the attributes, abilities, adventure settings and entity data that
/// changed this tick: these to the player, the entity data and the health also to the players
/// that see it.
pub(super) fn flush(state: &mut State) {
    let mut next = first_session(state);
    while let Some(session) = next {
        next = next_session(state, session);
        flush_one(state, session);
    }
}

/// Runs [`flush`] for one player, if it is spawned.
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
    let abilities = (dirty & ABILITIES != 0).then(|| update_abilities(player));
    let adventure =
        (dirty & ADVENTURE != 0).then(|| adventure_packet(player.survival.custom.auto_jump));
    let attributes = (dirty & ALL != 0).then(|| attributes_packet(player, dirty & ALL));
    let health = (dirty & HEALTH != 0).then(|| health_packet(player));
    let tags = dirty & TAGS != 0;
    let metadata = (now != old || tags).then(|| {
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
        m
    });
    let runtime_id = player.runtime_id;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.dirty = 0;
    player.survival.shown = now;
    let answer_tick = player.survival.motion.answer_tick.take();
    for packet in [attributes, abilities, adventure].into_iter().flatten() {
        player.send_packet(packet);
    }
    if let Some(entity_metadata) = metadata {
        // The answer tick is the player's own input's: the viewers get none.
        let own = answer_tick.map(|tick| actor_data(runtime_id, entity_metadata.clone(), tick));
        let viewed = actor_data(runtime_id, entity_metadata, 0);
        broadcast_to_viewers(state, session, &viewed);
        if let Some(player) = state.players.get_mut(&session) {
            player.send_packet(own.unwrap_or(viewed));
        }
    }
    if let Some(health) = health {
        broadcast_to_viewers(state, session, &health);
    }
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

/// Every attribute of the player (spawn and respawn) but those of a system the settings turn off
/// (`food`, `xp`): the client shows their defaults, and the server never changes them.
pub(in crate::tick) fn all_attributes(player: &Player) -> Packet {
    let on = player.survival.switches;
    let food = if on.food {
        0
    } else {
        FOOD | SATURATION | EXHAUSTION
    };
    let xp = if on.xp { 0 } else { LEVEL | XP };
    attributes_of(player, |d| d.bit & (food | xp) == 0)
}

/// The player's health for those who see it, as the official server tells them at a hit, a death
/// and a respawn (P6 `others`). It sends viewers the player's food and exhaustion too, which the
/// client shows nobody but the player: left out, they would cost a packet per viewer each time
/// they move.
pub(super) fn health_packet(player: &Player) -> Packet {
    attributes_packet(player, HEALTH)
}

/// `UpdateAbilities` for the player, with the official server's values: a member's permissions,
/// or an operator's (P6: `op` sends command permissions 1 and player permissions 2).
pub(in crate::tick) fn update_abilities(player: &Player) -> Packet {
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
            entity_unique_id: entity::unique_id(player.runtime_id),
            player_permissions: player_permissions.into(),
            command_permissions: command_permissions.into(),
            layers: layers(&player.survival),
        },
    })
}

/// `UpdateAdventureSettings` as the official server sends it (P6).
pub(in crate::tick) fn adventure_settings() -> Packet {
    adventure_packet(true)
}

/// The 39 game rules the official server sends, in its order (P6), with the values Anchor's rules
/// have: the rules of what Anchor does not simulate that the client acts on (mob spawning,
/// weather, insomnia, freezing, command blocks, respawn anchors exploding, projectiles breaking
/// blocks) are off, and no further rule is turned off for that reason (`doFireTick` stays on, as
/// on the official server); `pvp`, the day cycle, drowning, fall damage, the coordinates, the
/// waypoints, the immediate respawn and the death messages come from the settings, and players
/// respawn on the spawn block (`spawnRadius` 0).
/// The values are the client's display only; no API changes them, and a send hook that rewrites
/// `playerWaypoints` does not change the locator bar packets, which follow the `locator-bar`
/// setting.
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
        ("showCoordinates", Bool(settings.show_coordinates)),
        // The official server saves 0 in a world without the others' waypoints, 1 by default; it
        // saves no `locatorbar`.
        ("playerWaypoints", Uint32(u32::from(settings.locator_bar))),
        ("locatorbar", Bool(true)),
        ("showDaysPlayed", Bool(false)),
        ("naturalRegeneration", Bool(true)),
        ("tntExplodes", Bool(true)),
        ("sendCommandFeedback", Bool(true)),
        ("maxCommandChainLength", Uint32(65_535)),
        ("doInsomnia", Bool(false)),
        ("commandBlocksEnabled", Bool(false)),
        ("randomTickSpeed", Uint32(RANDOM_TICKS_PER_SUB_CHUNK)),
        ("doImmediateRespawn", Bool(settings.immediate_respawn)),
        ("showDeathMessages", Bool(settings.show_death_messages)),
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
            .map(|(name, value)| rule(name, value))
            .collect(),
    })
}

/// The whole entity data of a player as the official server sends it at spawn (P6: 73 keys; the
/// types match its packet size).
pub(in crate::tick) fn full_metadata(player: &Player) -> EntityMetadata {
    use EntityDataKey as K;
    use MetadataValue as V;
    let mut m = EntityMetadata::new();
    let mut put = |key: EntityDataKey, value: MetadataValue| {
        m.insert(key.into(), value);
    };
    put(K::StructuralIntegrity, V::Int32(1));
    put(K::Variant, V::Int32(0));
    put(K::ColorIndex, V::Byte(0));
    put(K::PlayerFlags, V::Byte(0));
    put(K::BedPosition, V::BlockPos([0; 3]));
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
    // The official server sends this key as three floats (P6 `others` data types).
    put(K::EnterBedPosition, V::Vec3([0.0; 3]));
    put(K::SeatThirdPersonCameraRadius, V::Float32(0.0));
    put(K::SeatCameraRelaxDistanceSmoothing, V::Float32(0.0));
    put(K::AimAssistPriorityPresetId, V::Int32(-1));
    put(K::AimAssistPriorityCategoryId, V::Int32(-1));
    // The official server sends this key as an int32 (P6 `others` data types).
    put(K::AimAssistPriorityActorId, V::Int32(-1));
    put(K::NameplateRenderDistanceMax, V::Float32(64.0));
    // The official server sends players this byte, 0 (P6 `spawn` and `others`).
    put(K::Reserved141, V::Byte(0));
    changed_metadata(&shown(player), None, &mut m);
    m
}

/// The changing entity data of a player now.
pub(super) fn shown(player: &Player) -> Shown {
    let s = &player.survival;
    let mut flags: FlagSet<EntityDataFlag> = FlagSet::new();
    for (on, flag) in [
        (s.fire_ticks > 0, EntityDataFlag::OnFire),
        (s.motion.sneaking, EntityDataFlag::Sneaking),
        (s.motion.sprinting, EntityDataFlag::Sprinting),
        // The client predicts the use animation and stops it unless the server agrees with this
        // flag (bedrock-protocol-docs changelog 776 `StartUsingItem`): eating, drinking and
        // using a bow, trident, goat horn or spyglass.
        (
            s.using.is_some() || player.breaking.is_using(),
            EntityDataFlag::UsingItem,
        ),
        (
            s.custom.invisible || s.effects.contains_key(&effect::Type::Invisibility),
            EntityDataFlag::Invisible,
        ),
        (s.custom.no_client_predictions, EntityDataFlag::NoAi),
        (s.custom.silent, EntityDataFlag::Silent),
        (s.custom.can_climb_walls, EntityDataFlag::WallClimbing),
        (s.custom.can_climb, EntityDataFlag::Climb),
        (s.motion.gliding, EntityDataFlag::Gliding),
        (s.breathing, EntityDataFlag::Breathing),
        (
            s.mode() != GameMode::Spectator,
            EntityDataFlag::HasCollision,
        ),
        (
            !s.effects.contains_key(&effect::Type::Levitation),
            EntityDataFlag::HasGravity,
        ),
        (s.motion.swimming, EntityDataFlag::Swimming),
        // The official server sends these two for every player (P6).
        (true, EntityDataFlag::HiddenWhenInvisible),
        (true, EntityDataFlag::PushTowardsClosestSpace),
    ] {
        if on {
            flags.insert(flag);
        }
    }
    let [w0, w1, _] = flags.words();
    Shown {
        flags: [w0, w1],
        air: s.air,
        max_air: s.custom.max_air,
        died: s.death.is_some(),
        death: s
            .death
            .as_ref()
            .map_or([0; 3], |p| [p.pos.x, p.pos.y, p.pos.z]),
        size: size(s),
        effects: effects::packed(s),
    }
}

/// The box width and height of the player's pose, sent as entity data 53 and 54: 0.6 by 1.8,
/// [`SNEAKING_HEIGHT`] high sneaking, as high as wide swimming or gliding, and [`DYING_SIZE`]
/// once the death is shown, whatever the pose does after.
pub(super) fn size(s: &Survival) -> [f32; 2] {
    if s.motion.death_shown {
        return DYING_SIZE;
    }
    let width = to_f32(PLAYER_WIDTH);
    if s.motion.swimming || s.motion.gliding {
        [width, width]
    } else if s.motion.sneaking {
        [width, SNEAKING_HEIGHT]
    } else {
        [width, to_f32(PLAYER_HEIGHT)]
    }
}

/// `SetActorData` with a player's box width and height (keys 53, 54). The client keeps a dead
/// player's small box (it hits and draws the name tag by it) until these keys come, so the
/// official server sends them at the death and after the respawn (P6 `others`).
pub(super) fn size_data(runtime_id: u64, [width, height]: [f32; 2]) -> Packet {
    let mut m = EntityMetadata::new();
    m.insert(EntityDataKey::Width.into(), MetadataValue::Float32(width));
    m.insert(EntityDataKey::Height.into(), MetadataValue::Float32(height));
    actor_data(runtime_id, m, 0)
}

/// `SetActorData` with `entity_metadata` of the entity with `runtime_id`, answering the input of
/// `tick` (0 for none); it carries no entity properties, and no API sets any.
pub(in crate::tick) fn actor_data(
    runtime_id: u64,
    entity_metadata: EntityMetadata,
    tick: u64,
) -> Packet {
    Packet::SetActorData(SetActorData {
        entity_runtime_id: runtime_id,
        entity_metadata,
        entity_properties: EntityProperties::default(),
        tick,
    })
}

/// The attributes whose bits are set in `bits`, in the official server's order.
fn attributes_packet(player: &Player, bits: u16) -> Packet {
    attributes_of(player, |d| d.bit & bits != 0)
}

/// The attributes `keep` keeps, in the official server's order.
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

/// One attribute with its current value.
fn attribute(def: &Def, s: &Survival) -> Attribute {
    let (value, max) = match def.bit {
        FOOD => (to_f32(s.food), def.max),
        SATURATION => (to_f32(s.saturation), def.max),
        EXHAUSTION => (to_f32(s.exhaustion), def.max),
        LEVEL => (to_f32(f64::from(s.xp_level)), def.max),
        XP => (to_f32(s.xp_progress), def.max),
        HEALTH => (to_f32(s.health.ceil()), to_f32(s.max_health())),
        MOVEMENT => (to_f32(movement_speed(s)), def.max),
        ABSORPTION => {
            let value = to_f32(s.absorption);
            (
                value,
                to_f32(math::max(f64::from(value), f64::from(def.max))),
            )
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

/// The movement speed: the base (0.1 unless the API set another), times 1.3 sprinting, times
/// 1 + 0.2 per speed level, times 1 - 0.15 per slowness level (at least 0), at most what `f32`
/// holds. It is worked out from the base each time, so it does not drift as sprinting and effects
/// change.
pub(super) fn movement_speed(s: &Survival) -> f64 {
    let sprint = if s.motion.sprinting { 1.3 } else { 1.0 };
    let speed = 1.0 + 0.2 * f64::from(s.effect_level(effect::Type::Speed));
    let slow = math::max(
        1.0 - 0.15 * f64::from(s.effect_level(effect::Type::Slowness)),
        0.0,
    );
    math::min(
        s.custom.movement_speed * sprint * speed * slow,
        f64::from(f32::MAX),
    )
}

/// The values of the changing keys whose value differs from `old`. When `old` is `None`: all of
/// them and the standing collision box (130), but width and height (53, 54) only where the pose's
/// box differs from the standing one.
fn changed_metadata(new: &Shown, old: Option<&Shown>, out: &mut EntityMetadata) {
    // Each flag word only when it changed, as the official server does (P6: `{0: ...}` alone when
    // Breathing changes).
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
    if old.is_none_or(|o| o.died != new.died || o.death != new.death) {
        // The official server sends the death position as a block position (P6 `others` data
        // types and its death).
        out.insert(
            EntityDataKey::PlayerLastDeathPosition.into(),
            MetadataValue::BlockPos(new.death),
        );
        out.insert(
            EntityDataKey::PlayerHasDied.into(),
            MetadataValue::Byte(u8::from(new.died)),
        );
    }
    // The collision box (130) is the standing box, sent once at spawn; the pose's box goes in the
    // width and height (53, 54), as the official server answers a sneak (P6 `sneak`).
    let standing = [to_f32(PLAYER_WIDTH), to_f32(PLAYER_HEIGHT)];
    if old.is_none() {
        out.insert(
            EntityDataKey::CollisionBox.into(),
            MetadataValue::Vec3([standing[0], standing[1], 0.0]),
        );
    }
    let [old_width, old_height] = old.map_or(standing, |o| o.size);
    if new.size[0] != old_width {
        out.insert(
            EntityDataKey::Width.into(),
            MetadataValue::Float32(new.size[0]),
        );
    }
    if new.size[1] != old_height {
        out.insert(
            EntityDataKey::Height.into(),
            MetadataValue::Float32(new.size[1]),
        );
    }
    if old.is_none_or(|o| o.effects != new.effects) {
        out.insert(
            EntityDataKey::VisibleMobEffects.into(),
            MetadataValue::Int64(new.effects),
        );
    }
}

/// The player's ability layers: its game mode's base layer with what the API set: operator
/// commands and teleporting for an operator, flight allowed, flying in any mode, block collision
/// and the flight speed. Without block collision outside spectator mode, a spectator layer makes
/// the client fly: the client falls through the blocks otherwise.
fn layers(s: &Survival) -> Vec<AbilityLayer> {
    let mode = s.mode();
    let mut values = mode_abilities(mode, s.motion.flying);
    let collides = s.has_block_collision();
    for (on, ability) in [
        (s.custom.operator, Ability::OperatorCommands),
        (s.custom.operator, Ability::Teleport),
        (
            mode == GameMode::Spectator || s.motion.flying,
            Ability::Flying,
        ),
        (s.can_fly(), Ability::MayFly),
        (!collides, Ability::NoClip),
    ] {
        if on {
            values.insert(ability);
        } else {
            values.remove(ability);
        }
    }
    let mut layers = vec![layer_of(values, to_f32(s.custom.flight_speed))];
    if !collides && mode != GameMode::Spectator {
        let flying: FlagSet<Ability> = [Ability::Flying].into_iter().collect();
        let bits = ability_bits(flying);
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

/// The base abilities of a game mode, as the official server sends them for a member (P6:
/// survival `Values` 63).
pub(in crate::tick) fn mode_abilities(mode: GameMode, flying: bool) -> FlagSet<Ability> {
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
        GameMode::Creative => survival
            .into_iter()
            .chain([Ability::Invulnerable])
            .chain(flying.then_some(Ability::Flying))
            .chain([Ability::MayFly, Ability::InstantBuild])
            .collect(),
        GameMode::Adventure => survival[2..].iter().copied().collect(),
        GameMode::Spectator => [
            Ability::Invulnerable,
            Ability::Flying,
            Ability::MayFly,
            Ability::NoClip,
        ]
        .into_iter()
        .collect(),
    }
}

/// The base ability layer with `values` and `fly_speed`, every ability defined, as the official
/// server sends it; the one place that builds it.
pub(in crate::tick) fn layer_of(values: FlagSet<Ability>, fly_speed: f32) -> AbilityLayer {
    let all: FlagSet<Ability> = Ability::all().iter().copied().collect();
    AbilityLayer {
        r#type: AbilityLayerType::Base.into(),
        abilities: ability_bits(all),
        values: ability_bits(values),
        fly_speed,
        vertical_fly_speed: ABILITY_BASE_VERTICAL_FLY_SPEED,
        walk_speed: ABILITY_BASE_WALK_SPEED,
    }
}

/// The bits of a set of abilities, as the layer carries them.
#[expect(
    clippy::expect_used,
    reason = "invariant: every ability has a bit below 32"
)]
fn ability_bits(abilities: FlagSet<Ability>) -> u32 {
    let [bits, ..] = abilities.words();
    u32::try_from(bits).expect("an ability bit is below 32")
}

/// `UpdateAdventureSettings` with the official server's values and `auto_jump`.
fn adventure_packet(auto_jump: bool) -> Packet {
    Packet::UpdateAdventureSettings(UpdateAdventureSettings {
        no_pv_m: false,
        no_mv_p: false,
        immutable_world: false,
        show_name_tags: false,
        auto_jump,
    })
}

/// A game rule as the client gets it.
fn rule(name: &str, value: GameRuleValue) -> GameRule {
    GameRule {
        name: name.to_owned(),
        can_be_modified_by_player: true,
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::consts::ABILITY_BASE_FLY_SPEED;

    #[test]
    fn the_ability_layers_have_the_official_values() {
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
    fn the_flag_words_match_the_official_server() {
        let mut flags: FlagSet<EntityDataFlag> = FlagSet::new();
        flags.insert(EntityDataFlag::Climb);
        flags.insert(EntityDataFlag::HasCollision);
        flags.insert(EntityDataFlag::HasGravity);
        flags.insert(EntityDataFlag::HiddenWhenInvisible);
        flags.insert(EntityDataFlag::PushTowardsClosestSpace);
        let [w0, w1, _] = flags.words();
        assert_eq!(w0, 844_424_930_656_256);
        assert_eq!(w1, 35_184_405_643_264);
    }

    /// The spawn data still carries the player flags and the bed position, as the official server
    /// sends them, although nothing changes them after the spawn.
    #[test]
    fn the_spawn_data_has_the_empty_player_flags_and_bed_position() {
        let (mut state, _rx) = crate::tick::player::tests::two();
        let player = crate::tick::player::tests::player_mut(&mut state, 1);
        let data = full_metadata(player);
        assert_eq!(
            data.get(&EntityDataKey::PlayerFlags.into()),
            Some(&MetadataValue::Byte(0))
        );
        assert_eq!(
            data.get(&EntityDataKey::BedPosition.into()),
            Some(&MetadataValue::BlockPos([0; 3]))
        );
    }

    #[test]
    fn all_is_every_attribute_bit() {
        assert_eq!(DEFS.iter().fold(0, |bits, d| bits | d.bit), ALL);
    }

    #[test]
    fn the_movement_attribute_starts_at_the_base_speed() {
        let movement = DEFS.iter().find(|d| d.bit == MOVEMENT);
        assert_eq!(movement.map(|d| d.default), Some(0.1_f32));
    }
}
