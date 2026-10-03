//! What players are sent of entities and of each other (PLAN.md 10.7 "추적"): the spawn packets
//! (`AddItemActor`, `AddActor`, `AddPlayer` with their metadata; a human NPC also goes through
//! the player list, as PMMP `Human::sendSpawnPacket` does), movement (`MoveActorDelta` as BDS
//! sends it, `SetActorMotion`, `MovePlayer`), `SetActorData` when the flags or a cloud's radius
//! change, equipment, and `RemoveActor`.

use super::kind::{Character, Cloud, Form, Kind, Projectile, Shot};
use super::{Entity, PLAYER_EYE_HEIGHT, PLAYER_HEIGHT, PLAYER_WIDTH};
use crate::config::GameMode;
use crate::item::Stack;
use crate::math::Vec3;
use crate::protocol::consts::{EntityDataFlag, EntityDataKey};
use crate::protocol::packet::{
    AddActor, AddItemActor, AddPlayer, MobArmourEquipment, MobEquipment, MoveActorDelta,
    MovePlayer, Packet, PlayerList, RemoveActor, SetActorData, SetActorMotion,
};
use crate::protocol::{
    AbilityData, AbilityLayer, EntityMetadata, FlagSet, ItemInstance, MetadataValue,
    PlayerListEntry, Rgba, Skin, Uuid,
};
use crate::tick::trade::{MAX_TIER, Offers};

/// Ticks over which the client spreads a `MoveActorDelta`: BDS sends 0 for items, orbs and
/// villagers (P6 game captures; 3 only for its own mobs, which Anchor does not have).
const MOVE_TICKS: u64 = 0;
/// `MobEquipment` window of the main inventory and of the off hand (gophertunnel
/// `ContainerIDInventory`, `ContainerIDOffHand`).
const WINDOW_INVENTORY: u8 = 0;
const WINDOW_OFF_HAND: u8 = 119;
/// `AddPlayer` game type BDS sends for a player in the world's default mode, the spectator type
/// that hides a player from viewers, and the survival type PMMP sends for a `Human`
/// (gophertunnel `GameType*`).
const GAME_TYPE_DEFAULT: i32 = 5;
const GAME_TYPE_SPECTATOR: i32 = 6;
const GAME_TYPE_SURVIVAL: i32 = 0;
/// `MovePlayer` mode of ordinary movement (gophertunnel `MoveModeNormal`).
const MOVE_MODE_NORMAL: u8 = 0;
/// The base ability layer BDS sends with `AddPlayer` (P6 `duplicate`): every ability, the walk
/// and fly speeds, member permissions.
const ABILITY_LAYER_BASE: u16 = 1;
const ABILITIES_ALL: u32 = 1_048_575;
const ABILITY_VALUES: u32 = 63;
const PLAYER_PERMISSIONS_MEMBER: u8 = 1;
/// PMMP's permissions of a `Human`: a visitor with no ability.
const PLAYER_PERMISSIONS_VISITOR: u8 = 0;
/// `PlayerList` actions (gophertunnel `PlayerListAction*`).
const PLAYER_LIST_ADD: u8 = 0;
const PLAYER_LIST_REMOVE: u8 = 1;
/// The potion splash colour of an effect-less potion or cloud (PMMP
/// `PotionSplashParticle::DEFAULT_COLOR`, ARGB 0x385dc6).
pub(super) const DEFAULT_POTION_COLOUR: [u8; 4] = [0x38, 0x5d, 0xc6, 0xff];
/// The skin of human NPCs: one opaque colour on the standard 64 × 32 model (a default skin;
/// skins of NPCs are an M8 API).
const NPC_SKIN_COLOUR: [u8; 4] = [0x9a, 0x6e, 0x52, 0xff];

/// The unique ID of a runtime ID (they are equal in Anchor, PLAN.md 10.7).
fn unique(runtime_id: u64) -> i64 {
    runtime_id.cast_signed()
}

/// The flags of an entity (PMMP `Entity::syncNetworkData` and the kinds' additions): gravity,
/// collision, the name tag shown, burning, no client prediction while it rests; a critical
/// arrow, a lingering potion, lit TNT.
pub(super) fn flags(kind: &Kind, on_fire: bool, still: bool) -> u64 {
    let mut set = FlagSet::<EntityDataFlag>::new();
    set.insert(EntityDataFlag::HasGravity);
    set.insert(EntityDataFlag::HasCollision);
    set.insert(EntityDataFlag::ShowName);
    if on_fire {
        set.insert(EntityDataFlag::OnFire);
    }
    if still {
        set.insert(EntityDataFlag::NoAi);
    }
    match kind {
        Kind::Projectile(Projectile {
            shot: Shot::Arrow(a),
            ..
        }) if a.critical => set.insert(EntityDataFlag::Critical),
        Kind::Projectile(Projectile {
            shot: Shot::Potion { linger: true, .. },
            ..
        }) => set.insert(EntityDataFlag::Lingering),
        Kind::Tnt(_) => set.insert(EntityDataFlag::Ignited),
        _ => {}
    }
    set.words()[0]
}

/// The ARGB int of a colour (PMMP `Color::toARGB`).
pub(super) fn argb([r, g, b, a]: [u8; 4]) -> i32 {
    i32::from_be_bytes([a, r, g, b])
}

/// The bubble colour of effects (PMMP `EffectCollection::getBubbleColor` and `Color::mix`: the
/// mean of each visible effect's colour, once per level); the default colour without one.
pub(super) fn effect_colour(effects: &[crate::effect::Instance]) -> [u8; 4] {
    let mut sum = [0u32; 4];
    let mut n = 0u32;
    for e in effects.iter().filter(|e| e.is_visible()) {
        let level = u32::from(e.amplifier()) + 1;
        for (s, c) in sum.iter_mut().zip(e.effect().colour()) {
            *s = s.saturating_add(u32::from(c).saturating_mul(level));
        }
        n = n.saturating_add(level);
    }
    if n == 0 {
        return DEFAULT_POTION_COLOUR;
    }
    sum.map(|s| u8::try_from(s / n).unwrap_or(u8::MAX))
}

/// The metadata of an entity (PMMP `syncNetworkData` and the kinds' additions).
fn metadata(entity: &Entity) -> EntityMetadata {
    let (width, height) = entity.kind.size();
    let mut m = EntityMetadata::new();
    let mut put = |k: EntityDataKey, v: MetadataValue| {
        m.insert(k as u32, v);
    };
    put(
        EntityDataKey::Flags,
        MetadataValue::Int64(entity.sent.flags.cast_signed()),
    );
    put(EntityDataKey::AlwaysShowNameTag, MetadataValue::Byte(0));
    put(
        EntityDataKey::Height,
        MetadataValue::Float32(to_f32(height)),
    );
    put(EntityDataKey::Width, MetadataValue::Float32(to_f32(width)));
    put(EntityDataKey::Scale, MetadataValue::Float32(1.0));
    put(EntityDataKey::LeashHolder, MetadataValue::Int64(-1));
    let owner = match &entity.kind {
        Kind::Projectile(p) if p.owner_runtime_id != 0 => unique(p.owner_runtime_id),
        _ => -1,
    };
    put(EntityDataKey::Owner, MetadataValue::Int64(owner));
    put(EntityDataKey::Target, MetadataValue::Int64(0));
    put(EntityDataKey::Name, MetadataValue::String(String::new()));
    put(EntityDataKey::Score, MetadataValue::String(String::new()));
    put(EntityDataKey::ColorIndex, MetadataValue::Byte(0));
    match &entity.kind {
        Kind::Item(_) | Kind::Projectile(_) => {}
        Kind::ExperienceOrb(orb) => {
            put(
                EntityDataKey::Value,
                MetadataValue::Int32(i32::from(orb.value)),
            );
        }
        Kind::FallingBlock(falling) => {
            let id = falling.block.network_id().cast_signed();
            put(EntityDataKey::Variant, MetadataValue::Int32(id));
        }
        Kind::Tnt(tnt) => {
            put(
                EntityDataKey::Variant,
                MetadataValue::Int32(i32::from(tnt.underwater)),
            );
            put(
                EntityDataKey::FuseTime,
                MetadataValue::Int32(i32::from(tnt.fuse)),
            );
        }
        Kind::Cloud(cloud) => cloud_metadata(cloud, &mut put),
        Kind::Npc(Character {
            form: Form::Villager { profession, offers },
            ..
        }) => {
            put(
                EntityDataKey::Variant,
                MetadataValue::Int32(i32::from(*profession)),
            );
            // BDS `villager_v2` (P6 `trade`): the tier, the top tier, the trade experience and a
            // skin (BDS picks one of a few at random; Anchor shows the first).
            put(EntityDataKey::TradeTier, trade_tier(offers));
            put(
                EntityDataKey::MaxTradeTier,
                MetadataValue::Int32(i32::from(MAX_TIER)),
            );
            put(EntityDataKey::TradeExperience, trade_experience(offers));
            put(EntityDataKey::SkinId, MetadataValue::Int32(0));
        }
        Kind::Npc(Character {
            form: Form::Human { name, .. },
            ..
        }) => {
            put(EntityDataKey::Name, MetadataValue::String(name.clone()));
            put(EntityDataKey::AlwaysShowNameTag, MetadataValue::Byte(1));
        }
    }
    if let Kind::Projectile(Projectile {
        shot: Shot::Potion { potion, .. },
        ..
    }) = &entity.kind
    {
        put(
            EntityDataKey::AuxValueData,
            MetadataValue::Int16(i16::from(potion.id())),
        );
    }
    m
}

/// A villager's `TradeTier`.
fn trade_tier(offers: &Offers) -> MetadataValue {
    MetadataValue::Int32(i32::from(offers.tier()))
}

/// A villager's `TradeExperience` (kept at most `i32::MAX`).
fn trade_experience(offers: &Offers) -> MetadataValue {
    MetadataValue::Int32(i32::try_from(offers.experience()).unwrap_or(i32::MAX))
}

/// The `SetActorData` of a villager whose tier or trade experience changed from `before`: only
/// the changed keys, as BDS sends after a trade (P6 `trade`); `None` if neither changed.
pub(super) fn trade_data(runtime_id: u64, offers: &Offers, before: (u8, u32)) -> Option<Packet> {
    let mut m = EntityMetadata::new();
    if offers.tier() != before.0 {
        m.insert(EntityDataKey::TradeTier as u32, trade_tier(offers));
    }
    if offers.experience() != before.1 {
        m.insert(
            EntityDataKey::TradeExperience as u32,
            trade_experience(offers),
        );
    }
    (!m.is_empty()).then(|| {
        Packet::SetActorData(SetActorData {
            entity_runtime_id: runtime_id,
            entity_metadata: m,
            ..SetActorData::default()
        })
    })
}

/// A cloud's metadata (PMMP `AreaEffectCloud::syncNetworkData`: the radius and colour, and the
/// properties that keep the client from changing the cloud by itself).
fn cloud_metadata(cloud: &Cloud, put: &mut impl FnMut(EntityDataKey, MetadataValue)) {
    put(
        EntityDataKey::DataRadius,
        MetadataValue::Float32(to_f32(cloud.radius)),
    );
    put(
        EntityDataKey::EffectColor,
        MetadataValue::Int32(argb(effect_colour(&cloud.effects))),
    );
    put(EntityDataKey::EffectAmbience, MetadataValue::Byte(0));
    put(EntityDataKey::DataDuration, MetadataValue::Int32(-1));
    put(
        EntityDataKey::DataChangeOnPickup,
        MetadataValue::Float32(0.0),
    );
    put(EntityDataKey::DataChangeRate, MetadataValue::Float32(0.0));
    put(EntityDataKey::DataSpawnTime, MetadataValue::Int32(0));
    put(EntityDataKey::DataPickupCount, MetadataValue::Float32(0.0));
    put(EntityDataKey::DataWaiting, MetadataValue::Int32(0));
}

/// An `f32` for a packet (rounded to nearest).
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: packets carry f32, and rounding f64 to f32 is exact IEEE behaviour"
)]
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// Where the client draws the entity.
pub(super) fn shown_at(entity: &Entity) -> [f32; 3] {
    let offset = Vec3::new(0.0, entity.kind.network_offset(), 0.0);
    (entity.body.position + offset).to_f32()
}

/// The network form of a stack (PMMP `ItemStackWrapper::legacy`: no stack network ID).
pub(super) fn instance(stack: &Stack) -> ItemInstance {
    stack.to_instance(0)
}

/// The packets that show an entity to a player, at the drawn position ([`shown_at`]; an item
/// 0.125 above its feet as BDS sends it, P6 `crops`). A human NPC is added to the player list for
/// its skin, shown, and taken off the list again.
pub(super) fn spawn(entity: &Entity) -> Vec<Packet> {
    let velocity = entity.body.motion.to_f32();
    let metadata = metadata(entity);
    let (yaw, pitch) = (to_f32(entity.yaw), to_f32(entity.pitch));
    match &entity.kind {
        Kind::Item(item) => vec![Packet::AddItemActor(Box::new(AddItemActor {
            entity_unique_id: unique(entity.runtime_id),
            entity_runtime_id: entity.runtime_id,
            item: instance(&item.stack),
            position: shown_at(entity),
            velocity,
            entity_metadata: metadata,
            from_fishing: false,
        }))],
        Kind::Npc(Character {
            form: Form::Human { name, uuid },
            ..
        }) => {
            let entry = PlayerListEntry {
                action_type: PLAYER_LIST_ADD,
                uuid: *uuid,
                entity_unique_id: unique(entity.runtime_id),
                username: name.clone(),
                build_platform: -1,
                skin: npc_skin(),
                player_colour: Rgba {
                    r: 237,
                    g: 237,
                    b: 237,
                    a: 255,
                },
                ..PlayerListEntry::default()
            };
            let add = Packet::AddPlayer(Box::new(AddPlayer {
                uuid: *uuid,
                username: name.clone(),
                entity_runtime_id: entity.runtime_id,
                platform_chat_id: String::new(),
                position: entity.body.position.to_f32(),
                velocity,
                pitch,
                yaw,
                head_yaw: yaw,
                held_item: ItemInstance::default(),
                game_type: GAME_TYPE_SURVIVAL,
                entity_metadata: metadata,
                ability_data: AbilityData {
                    entity_unique_id: unique(entity.runtime_id),
                    player_permissions: PLAYER_PERMISSIONS_VISITOR,
                    command_permissions: 0,
                    layers: vec![AbilityLayer {
                        r#type: ABILITY_LAYER_BASE,
                        abilities: 0,
                        values: 0,
                        fly_speed: 0.0,
                        vertical_fly_speed: 0.0,
                        walk_speed: 0.0,
                    }],
                },
                build_platform: -1,
                ..AddPlayer::default()
            }));
            let remove = PlayerListEntry {
                action_type: PLAYER_LIST_REMOVE,
                uuid: *uuid,
                ..PlayerListEntry::default()
            };
            vec![
                Packet::PlayerList(PlayerList {
                    entries: vec![entry],
                }),
                add,
                Packet::PlayerList(PlayerList {
                    entries: vec![remove],
                }),
            ]
        }
        _ => vec![Packet::AddActor(Box::new(AddActor {
            entity_unique_id: unique(entity.runtime_id),
            entity_runtime_id: entity.runtime_id,
            entity_type: entity.kind.identifier().to_owned(),
            position: shown_at(entity),
            velocity,
            pitch,
            yaw,
            head_yaw: yaw,
            body_yaw: yaw,
            entity_metadata: metadata,
            ..AddActor::default()
        }))],
    }
}

/// The skin of human NPCs ([`NPC_SKIN_COLOUR`] on the standard model).
fn npc_skin() -> Skin {
    Skin {
        skin_id: "Standard_Custom".to_owned(),
        full_id: "Standard_Custom".to_owned(),
        skin_resource_patch: br#"{"geometry":{"default":"geometry.humanoid.custom"}}"#.to_vec(),
        skin_image_width: 64,
        skin_image_height: 32,
        skin_data: NPC_SKIN_COLOUR.repeat(64 * 32),
        geometry_data_engine_version: b"0.0.0".to_vec(),
        arm_size: 1,
        trusted: true,
        ..Skin::default()
    }
}

/// The player list UUID of a human NPC with runtime ID `runtime_id` (version 4 form; the bytes
/// are a fixed tag and the runtime ID, so no player's UUID equals it in practice).
pub(super) fn npc_uuid(runtime_id: u64) -> Uuid {
    let [r0, r1, r2, r3, r4, r5, r6, r7] = runtime_id.to_be_bytes();
    Uuid::from_bytes([
        b'a',
        b'n',
        b'c',
        b'h',
        b'o',
        b'r',
        (b'N' & 0x0f) | 0x40,
        b'P',
        (r0 & 0x3f) | 0x80,
        r1,
        r2,
        r3,
        r4,
        r5,
        r6,
        r7,
    ])
}

/// The packet that removes an entity or player from a client.
pub(super) fn remove(runtime_id: u64) -> Packet {
    Packet::RemoveActor(RemoveActor {
        entity_unique_id: unique(runtime_id),
    })
}

/// Where the client drew the entity when it was last sent.
fn shown_before(entity: &Entity) -> [f32; 3] {
    let offset = Vec3::new(0.0, entity.kind.network_offset(), 0.0);
    (entity.sent.position + offset).to_f32()
}

/// Whether an axis of the drawn position changed since it was last sent (BDS sends a
/// `MoveActorDelta` every tick an entity moves, P6 `crops`).
pub(super) fn has_moved(entity: &Entity) -> bool {
    shown_at(entity) != shown_before(entity)
}

/// The movement of an entity since `sent` as BDS sends it: only the axes and angles that
/// changed.
pub(super) fn movement(entity: &Entity) -> Packet {
    let now = shown_at(entity);
    let before = shown_before(entity);
    let axis = |i: usize| {
        let (n, b) = (now.get(i).copied(), before.get(i).copied());
        (n != b).then_some(n).flatten()
    };
    let (yaw, pitch) = (to_f32(entity.yaw), to_f32(entity.pitch));
    let turned = (yaw != to_f32(entity.sent.yaw)) || (pitch != to_f32(entity.sent.pitch));
    Packet::MoveActorDelta(MoveActorDelta {
        entity_runtime_id: entity.runtime_id,
        position_x: axis(0),
        position_y: axis(1),
        position_z: axis(2),
        rotation_x: turned.then_some(pitch),
        rotation_y: turned.then_some(yaw),
        rotation_y_head: turned.then_some(yaw),
        on_ground: entity.body.on_ground,
        force_move: false,
        force_move_local_entity: false,
        force_completion: false,
        ticks: MOVE_TICKS,
    })
}

/// The speed of an entity (PMMP `broadcastMotion`).
pub(super) fn motion(entity: &Entity) -> Packet {
    Packet::SetActorMotion(SetActorMotion {
        entity_runtime_id: entity.runtime_id,
        velocity: entity.body.motion.to_f32(),
        tick: 0,
    })
}

/// The flags of an entity that changed, and a cloud's radius (PMMP sends the dirty properties).
pub(super) fn data(runtime_id: u64, flags: u64, radius: Option<f64>) -> Packet {
    let mut m = EntityMetadata::new();
    m.insert(
        EntityDataKey::Flags as u32,
        MetadataValue::Int64(flags.cast_signed()),
    );
    if let Some(r) = radius {
        m.insert(
            EntityDataKey::DataRadius as u32,
            MetadataValue::Float32(to_f32(r)),
        );
        m.insert(
            EntityDataKey::Width as u32,
            MetadataValue::Float32(to_f32(r * 2.0)),
        );
    }
    Packet::SetActorData(SetActorData {
        entity_runtime_id: runtime_id,
        entity_metadata: m,
        ..SetActorData::default()
    })
}

/// What a viewer needs to draw another player.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shown<'a> {
    pub(super) runtime_id: u64,
    pub(super) uuid: Uuid,
    pub(super) name: &'a str,
    /// The eye position.
    pub(super) eye: Vec3,
    pub(super) game_mode: GameMode,
    pub(super) held: &'a Stack,
}

/// The packet that shows a player to another (BDS `AddPlayer`, P6 `duplicate`; the feet
/// position, the default game type, the base ability layer).
pub(super) fn add_player(p: Shown<'_>) -> Packet {
    let mut flags = FlagSet::<EntityDataFlag>::new();
    for f in [
        EntityDataFlag::HasGravity,
        EntityDataFlag::HasCollision,
        EntityDataFlag::HiddenWhenInvisible,
        EntityDataFlag::PushTowardsClosestSpace,
    ] {
        flags.insert(f);
    }
    let [low, high, ..] = flags.words();
    let mut m = EntityMetadata::new();
    let mut put = |k: EntityDataKey, v: MetadataValue| {
        m.insert(k as u32, v);
    };
    put(
        EntityDataKey::Flags,
        MetadataValue::Int64(low.cast_signed()),
    );
    put(
        EntityDataKey::FlagsTwo,
        MetadataValue::Int64(high.cast_signed()),
    );
    put(
        EntityDataKey::Name,
        MetadataValue::String(p.name.to_owned()),
    );
    put(
        EntityDataKey::Width,
        MetadataValue::Float32(to_f32(PLAYER_WIDTH)),
    );
    put(
        EntityDataKey::Height,
        MetadataValue::Float32(to_f32(PLAYER_HEIGHT)),
    );
    put(EntityDataKey::Scale, MetadataValue::Float32(1.0));
    put(EntityDataKey::AlwaysShowNameTag, MetadataValue::Byte(1));
    put(EntityDataKey::LeashHolder, MetadataValue::Int64(0));
    put(EntityDataKey::Owner, MetadataValue::Int64(-1));
    let feet = p.eye - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    Packet::AddPlayer(Box::new(AddPlayer {
        uuid: p.uuid,
        username: p.name.to_owned(),
        entity_runtime_id: p.runtime_id,
        platform_chat_id: String::new(),
        position: feet.to_f32(),
        held_item: instance(p.held),
        game_type: if p.game_mode == GameMode::Spectator {
            GAME_TYPE_SPECTATOR
        } else {
            GAME_TYPE_DEFAULT
        },
        entity_metadata: m,
        ability_data: AbilityData {
            entity_unique_id: unique(p.runtime_id),
            player_permissions: PLAYER_PERMISSIONS_MEMBER,
            command_permissions: 0,
            layers: vec![AbilityLayer {
                r#type: ABILITY_LAYER_BASE,
                abilities: ABILITIES_ALL,
                values: ABILITY_VALUES,
                fly_speed: 0.05,
                vertical_fly_speed: 1.0,
                walk_speed: 0.1,
            }],
        },
        ..AddPlayer::default()
    }))
}

/// A player's movement to the players who see it (dragonfly and PMMP send the eye position).
pub(super) fn move_player(runtime_id: u64, eye: Vec3, tick: u64) -> Packet {
    Packet::MovePlayer(MovePlayer {
        entity_runtime_id: runtime_id,
        position: eye.to_f32(),
        mode: MOVE_MODE_NORMAL,
        tick,
        ..MovePlayer::default()
    })
}

/// The held item of a player (PMMP `onMobMainHandItemChange`).
pub(super) fn held(runtime_id: u64, stack: &Stack, slot: u8) -> Packet {
    Packet::MobEquipment(Box::new(MobEquipment {
        entity_runtime_id: runtime_id,
        new_item: instance(stack),
        inventory_slot: slot,
        hot_bar_slot: slot,
        window_id: WINDOW_INVENTORY,
    }))
}

/// The off-hand item of a player (PMMP `onMobOffHandItemChange`).
pub(super) fn off_hand(runtime_id: u64, stack: &Stack) -> Packet {
    Packet::MobEquipment(Box::new(MobEquipment {
        entity_runtime_id: runtime_id,
        new_item: instance(stack),
        inventory_slot: 0,
        hot_bar_slot: 0,
        window_id: WINDOW_OFF_HAND,
    }))
}

/// The armour of a player (PMMP `onMobArmorChange`).
pub(super) fn armour(runtime_id: u64, armour: &[Stack; 4]) -> Packet {
    let [helmet, chestplate, leggings, boots] = armour;
    Packet::MobArmourEquipment(Box::new(MobArmourEquipment {
        entity_runtime_id: runtime_id,
        helmet: instance(helmet),
        chestplate: instance(chestplate),
        leggings: instance(leggings),
        boots: instance(boots),
        body: ItemInstance::default(),
    }))
}
