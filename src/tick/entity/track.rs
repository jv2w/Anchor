//! What players are sent of entities and of each other (PLAN.md 10.7 "추적"): the spawn packets
//! (`AddItemActor`, `AddActor`, `AddPlayer` with their metadata, `AddPainting`; a human NPC also
//! goes through the player list, as PMMP `Human::sendSpawnPacket` does), movement (`MoveActorDelta` as BDS
//! sends it, `SetActorMotion`; `MoveActorAbsolute` for players, as PMMP sends it), `SetActorData`
//! when the flags or a cloud's radius change, equipment, and `RemoveActor`.

use super::kind::{Character, Cloud, Form, Kind, Painting, Projectile, Shot};
use super::object;
use super::{Entity, PLAYER_EYE_HEIGHT, PLAYER_HEIGHT, PLAYER_WIDTH};
use crate::config::GameMode;
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::{EntityDataFlag, EntityDataKey, MoveFlag};
use crate::protocol::packet::{
    AddActor, AddItemActor, AddPainting, AddPlayer, LocatorBar, MobArmourEquipment, MobEquipment,
    MoveActorAbsolute, MoveActorDelta, Packet, PlayerList, PlayerLocation, RemoveActor,
    SetActorData, SetActorMotion, UpdateAttributes,
};
use crate::protocol::{
    AbilityData, AbilityLayer, Attribute, AttributeValue, EntityMetadata, FlagSet, ItemInstance,
    LocatorBarWaypoint, MetadataValue, PlayerListEntry, Rgba, Skin, Uuid, Waypoint,
    WaypointWorldPosition,
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
/// The health attribute (PMMP `Attribute::HEALTH`).
const HEALTH: &str = "minecraft:health";

/// The unique ID of a runtime ID (they are equal in Anchor, PLAN.md 10.7).
fn unique(runtime_id: u64) -> i64 {
    runtime_id.cast_signed()
}

/// The flags of an entity (PMMP `Entity::syncNetworkData` and the kinds' additions): gravity,
/// collision, the name tag shown, invisible, burning, no client prediction while it rests
/// (`still`); a critical arrow, a lingering potion, lit TNT, an end crystal's base, a trident's
/// enchantment glint.
pub(super) fn flags(entity: &Entity, still: bool) -> u64 {
    let mut set = FlagSet::<EntityDataFlag>::new();
    if entity.gravity {
        set.insert(EntityDataFlag::HasGravity);
    }
    set.insert(EntityDataFlag::HasCollision);
    if entity.name_tag_visible {
        set.insert(EntityDataFlag::ShowName);
    }
    if entity.invisible {
        set.insert(EntityDataFlag::Invisible);
    }
    if entity.fire_ticks > 0 {
        set.insert(EntityDataFlag::OnFire);
    }
    if still {
        set.insert(EntityDataFlag::NoAi);
    }
    match &entity.kind {
        Kind::Projectile(Projectile {
            shot: Shot::Arrow(a),
            ..
        }) if a.critical => set.insert(EntityDataFlag::Critical),
        Kind::Projectile(Projectile {
            shot: Shot::Potion { linger: true, .. },
            ..
        }) => set.insert(EntityDataFlag::Lingering),
        Kind::Projectile(Projectile {
            shot: Shot::Trident(t),
            ..
        }) if t.item.extra().is_some_and(|x| !x.enchantments.is_empty()) => {
            set.insert(EntityDataFlag::Enchanted);
        }
        Kind::Tnt(_) => set.insert(EntityDataFlag::Ignited),
        Kind::EndCrystal(c) if c.show_base => set.insert(EntityDataFlag::ShowBottom),
        kind if kind.is_baby() => set.insert(EntityDataFlag::Baby),
        _ => {}
    }
    set.words()[0]
}

/// The ARGB int of an RGBA colour (PMMP `Color::toARGB`, [`crate::tile::to_argb`]).
pub(super) fn argb([r, g, b, a]: [u8; 4]) -> i32 {
    crate::tile::to_argb(crate::protocol::Rgba { r, g, b, a })
}

/// The bubble colour of effects (PMMP `EffectCollection::getBubbleColor` and `Color::mix`: the
/// mean of each visible effect's colour, once per level); the default colour without one.
pub(super) fn effect_colour(effects: &[crate::effect::Instance]) -> [u8; 4] {
    crate::effect::mix(effects.iter().filter(|e| e.is_visible())).unwrap_or(DEFAULT_POTION_COLOUR)
}

/// The metadata of an entity (PMMP `syncNetworkData` and the kinds' additions), with the flags
/// last sent.
pub(super) fn metadata(entity: &Entity) -> EntityMetadata {
    let (width, height) = entity.size();
    let mut m = EntityMetadata::new();
    let mut put = |k: EntityDataKey, v: MetadataValue| {
        m.insert(k as u32, v);
    };
    put(
        EntityDataKey::Flags,
        MetadataValue::Int64(entity.sent.flags.cast_signed()),
    );
    put(
        EntityDataKey::AlwaysShowNameTag,
        MetadataValue::Byte(u8::from(entity.name_tag_always_visible)),
    );
    put(
        EntityDataKey::Height,
        MetadataValue::Float32(to_f32(height)),
    );
    put(EntityDataKey::Width, MetadataValue::Float32(to_f32(width)));
    put(
        EntityDataKey::Scale,
        MetadataValue::Float32(to_f32(entity.scale)),
    );
    put(EntityDataKey::LeashHolder, MetadataValue::Int64(-1));
    // A firework rocket shows no owner (BDS, P6 `entities`, m9-facts.md).
    let owner = match &entity.kind {
        Kind::Projectile(p) if p.owner_runtime_id != 0 => unique(p.owner_runtime_id),
        _ => -1,
    };
    put(EntityDataKey::Owner, MetadataValue::Int64(owner));
    put(EntityDataKey::Target, MetadataValue::Int64(0));
    put(
        EntityDataKey::Name,
        MetadataValue::String(entity.name_tag.clone()),
    );
    put(EntityDataKey::Score, MetadataValue::String(String::new()));
    put(EntityDataKey::ColorIndex, MetadataValue::Byte(0));
    match &entity.kind {
        Kind::Item(_)
        | Kind::Projectile(_)
        | Kind::Painting(_)
        | Kind::Npc(Character {
            form: Form::Human { .. } | Form::Custom(_) | Form::Zombie { .. },
            ..
        }) => {}
        Kind::EndCrystal(_) | Kind::Firework(_) => object_metadata(&entity.kind, &mut put),
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
            form: Form::Villager {
                profession, offers, ..
            },
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

/// An end crystal's and a firework rocket's own metadata (PMMP `EndCrystal::syncNetworkData`:
/// the beam target, (0, 0, 0) without one; a rocket as BDS shows it (P6 `entities`,
/// m9-facts.md): the item's NBT with the explosions and the flight, the direction (0, 1, 0) and
/// no attached entity (-1)).
fn object_metadata(kind: &Kind, put: &mut impl FnMut(EntityDataKey, MetadataValue)) {
    match kind {
        Kind::EndCrystal(c) => {
            let t = c.beam_target.unwrap_or_default();
            put(
                EntityDataKey::BlockTarget,
                MetadataValue::BlockPos([t.x, t.y, t.z]),
            );
        }
        Kind::Firework(f) => {
            let explosions = List::Compound(f.explosions.clone());
            let fireworks = Compound::from([(
                object::TAG_FIREWORKS.to_owned(),
                Value::Compound(Compound::from([
                    (object::TAG_EXPLOSIONS.to_owned(), Value::List(explosions)),
                    (object::TAG_FLIGHT.to_owned(), Value::Byte(f.flight)),
                ])),
            )]);
            put(
                EntityDataKey::DISPLAY_FIREWORK,
                MetadataValue::CompoundTag(fireworks),
            );
            put(
                EntityDataKey::DisplayOffset,
                MetadataValue::Vec3([0.0, 1.0, 0.0]),
            );
            put(EntityDataKey::CustomDisplay, MetadataValue::Int64(-1));
        }
        _ => {}
    }
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

/// The particle a potion's cloud shows (BDS, P6 `potions`, m9-facts.md).
const CLOUD_PARTICLE: i32 = 35;

/// A cloud's metadata as BDS shows a lingering potion's (P6 `potions`, m9-facts.md; PMMP
/// `AreaEffectCloud::syncNetworkData` names the keys): the radius, colour, particle, duration,
/// radius change on pickup and pickup count. The radius change per tick stays 0: the server
/// shrinks the cloud and sends its radius.
fn cloud_metadata(cloud: &Cloud, put: &mut impl FnMut(EntityDataKey, MetadataValue)) {
    put(
        EntityDataKey::DataRadius,
        MetadataValue::Float32(to_f32(cloud.radius)),
    );
    put(
        EntityDataKey::EffectColor,
        MetadataValue::Int32(argb(effect_colour(&cloud.effects))),
    );
    put(
        EntityDataKey::DataParticle,
        MetadataValue::Int32(CLOUD_PARTICLE),
    );
    put(
        EntityDataKey::DataDuration,
        MetadataValue::Int32(cloud.max_age),
    );
    put(
        EntityDataKey::DataChangeOnPickup,
        MetadataValue::Float32(to_f32(cloud.radius_on_pickup)),
    );
    put(EntityDataKey::DataChangeRate, MetadataValue::Float32(0.0));
    put(
        EntityDataKey::DataPickupCount,
        MetadataValue::Int32(cloud.pickup_count),
    );
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
            form: Form::Human { uuid, skin },
            ..
        }) => {
            let name = &entity.name_tag;
            let entry = PlayerListEntry {
                action_type: PLAYER_LIST_ADD,
                uuid: *uuid,
                entity_unique_id: unique(entity.runtime_id),
                username: name.clone(),
                build_platform: -1,
                skin: skin.as_ref().map_or_else(npc_skin, |s| s.to_protocol()),
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
        Kind::Painting(p) => vec![add_painting(entity, *p)],
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
            attributes: health(entity).into_iter().collect(),
            entity_metadata: metadata,
            ..AddActor::default()
        }))],
    }
}

/// The packet that shows a painting (PMMP `Painting::sendSpawnPacket`): the middle of its box,
/// its facing byte, its motive.
fn add_painting(entity: &Entity, painting: Painting) -> Packet {
    let b = entity.bounding_box();
    let middle = Vec3::new(
        f64::midpoint(b.min.x, b.max.x),
        f64::midpoint(b.min.y, b.max.y),
        f64::midpoint(b.min.z, b.max.z),
    );
    Packet::AddPainting(AddPainting {
        entity_unique_id: unique(entity.runtime_id),
        entity_runtime_id: entity.runtime_id,
        position: middle.to_f32(),
        direction: i32::from(object::facing_data(painting.facing)),
        title: painting.motive.name().to_owned(),
    })
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

/// The health attribute of a custom entity (PMMP `Living` sends its attributes with `AddActor`;
/// the health as `Living::setHealth` shows it, rounded up), for the resource pack's queries; `None`
/// for other kinds, whose health clients do not show.
pub(super) fn health(entity: &Entity) -> Option<AttributeValue> {
    let custom = entity.kind.custom()?;
    Some(AttributeValue {
        name: HEALTH.to_owned(),
        value: to_f32(entity.health.ceil()),
        max: to_f32(custom.spec.max_health),
        min: 0.0,
    })
}

/// A custom entity's health after it changed (PMMP `Living` sends its dirty attributes); `None`
/// for other kinds.
pub(super) fn health_update(entity: &Entity) -> Option<Packet> {
    let value = health(entity)?;
    let max = value.max;
    Some(Packet::UpdateAttributes(UpdateAttributes {
        entity_runtime_id: entity.runtime_id,
        attributes: vec![Attribute {
            attribute_value: value,
            default_min: 0.0,
            default_max: max,
            default: max,
            modifiers: Vec::new(),
        }],
        tick: 0,
    }))
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

/// All the metadata of an entity with `flags`, after the API changed some of it (PMMP sends the
/// dirty properties; a change through the API is rare, so all of them go).
pub(super) fn all_data(entity: &Entity, flags: u64) -> Packet {
    let mut m = metadata(entity);
    m.insert(
        EntityDataKey::Flags as u32,
        MetadataValue::Int64(flags.cast_signed()),
    );
    Packet::SetActorData(SetActorData {
        entity_runtime_id: entity.runtime_id,
        entity_metadata: m,
        ..SetActorData::default()
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
#[derive(Debug, Clone)]
pub(super) struct Shown<'a> {
    pub(super) runtime_id: u64,
    pub(super) uuid: Uuid,
    pub(super) name: &'a str,
    /// The eye position.
    pub(super) eye: Vec3,
    pub(super) game_mode: GameMode,
    pub(super) held: &'a Stack,
    /// The player's whole entity data, as its own client gets it (`survival::full_metadata`).
    pub(super) data: EntityMetadata,
    /// The device ID and the build platform (device OS) its client gave.
    pub(super) device_id: &'a str,
    pub(super) build_platform: i32,
}

/// The packet that shows a player to another as BDS shows it (P6 `others`, m9-facts.md): the
/// feet position, the default game type, the base ability layer, the device, and the player's own
/// entity data (its flags now, such as Breathing) with the seven keys BDS adds for the others:
/// jump duration, scale, the box's width and height, seat offset and index, arrow shooter.
pub(super) fn add_player(p: Shown<'_>) -> Packet {
    let mut m = p.data;
    let (width, height) = match m.get(&(EntityDataKey::CollisionBox as u32)) {
        Some(MetadataValue::Vec3([w, h, _])) => (*w, *h),
        _ => (to_f32(PLAYER_WIDTH), to_f32(PLAYER_HEIGHT)),
    };
    for (k, v) in [
        (EntityDataKey::JumpDuration, MetadataValue::Byte(0)),
        (EntityDataKey::Scale, MetadataValue::Float32(1.0)),
        (EntityDataKey::Width, MetadataValue::Float32(width)),
        (EntityDataKey::Height, MetadataValue::Float32(height)),
        (EntityDataKey::SeatOffset, MetadataValue::Vec3([0.0; 3])),
        (EntityDataKey::ControllingSeatIndex, MetadataValue::Byte(0)),
        (EntityDataKey::ArrowShooterId, MetadataValue::Int64(0)),
    ] {
        m.insert(k as u32, v);
    }
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
        device_id: p.device_id.to_owned(),
        build_platform: p.build_platform,
        ..AddPlayer::default()
    }))
}

/// `PlayerLocation` types and `LocatorBar` waypoint actions (gophertunnel `PlayerLocationType*`,
/// `WaypointAction*`).
const LOCATION_COORDINATES: i32 = 0;
const LOCATION_HIDE: i32 = 1;
const WAYPOINT_ADD: u8 = 1;
const WAYPOINT_REMOVE: u8 = 2;
const WAYPOINT_UPDATE: u8 = 3;
/// The update flag BDS gives a change of visibility (P6 `others`: 2, where gophertunnel's
/// `WaypointUpdateFlagVisible` is 1).
const WAYPOINT_VISIBLE: u32 = 2;
/// The update flags of BDS's waypoint add (all) and of its update right after (P6 `others`).
const WAYPOINT_ALL: u32 = 63;
const WAYPOINT_FOLLOW: u32 = 44;
/// A player's waypoint colour: its player list colour (BDS gives both the same value, P6
/// `others`), ARGB.
const WAYPOINT_COLOUR: i32 = 0xffed_eded_u32.cast_signed();

/// A waypoint of the player with runtime ID `runtime_id`; its group handle is made of the ID
/// (BDS's is a random UUID per player, P6 `others`).
fn waypoint(runtime_id: u64, action: u8, waypoint: Waypoint) -> Packet {
    let mut handle = [0; 16];
    handle[8..].copy_from_slice(&runtime_id.to_be_bytes());
    Packet::LocatorBar(LocatorBar {
        waypoints: vec![LocatorBarWaypoint {
            group_handle: Uuid::from_bytes(handle),
            waypoint,
            action,
        }],
    })
}

/// What a player sees of another on its locator bar when it starts seeing it, as BDS sends it (P6
/// `others`, m9-facts.md): its location at the eyes, its waypoint added there, then handed to the
/// client to follow the player (the client's position authority, the small dot).
pub(super) fn locator_shown(runtime_id: u64, eye: Vec3) -> [Packet; 3] {
    let (position, unique_id) = (eye.to_f32(), unique(runtime_id));
    [
        Packet::PlayerLocation(PlayerLocation {
            r#type: LOCATION_COORDINATES,
            entity_unique_id: unique_id,
            position,
        }),
        waypoint(
            runtime_id,
            WAYPOINT_ADD,
            Waypoint {
                update_flag: WAYPOINT_ALL,
                visible: Some(true),
                world_position: Some(WaypointWorldPosition {
                    position,
                    dimension_id: 0,
                }),
                texture_path: Some("textures/ui/locator_bar_dot_3".to_owned()),
                icon_size: Some([0.2, 0.2]),
                colour: Some(WAYPOINT_COLOUR),
                client_position_authority: Some(false),
                actor_unique_id: Some(unique_id),
            },
        ),
        waypoint(
            runtime_id,
            WAYPOINT_UPDATE,
            Waypoint {
                update_flag: WAYPOINT_FOLLOW,
                texture_path: Some("textures/ui/locator_bar_dot_0".to_owned()),
                icon_size: Some([1.0, 1.0]),
                client_position_authority: Some(true),
                ..Waypoint::default()
            },
        ),
    ]
}

/// A player's waypoint leaving the bar of a player who no longer sees it.
pub(super) fn locator_removed(runtime_id: u64) -> Packet {
    waypoint(runtime_id, WAYPOINT_REMOVE, Waypoint::default())
}

/// A dead player leaves the bars of those who see it (BDS: its location hidden, its waypoint
/// invisible); `alive` puts its waypoint back (BDS at the respawn).
pub(in crate::tick) fn locator_life(runtime_id: u64, alive: bool) -> Vec<Packet> {
    let shown = waypoint(
        runtime_id,
        WAYPOINT_UPDATE,
        Waypoint {
            update_flag: WAYPOINT_VISIBLE,
            visible: Some(alive),
            ..Waypoint::default()
        },
    );
    if alive {
        return vec![shown];
    }
    vec![
        Packet::PlayerLocation(PlayerLocation {
            r#type: LOCATION_HIDE,
            entity_unique_id: unique(runtime_id),
            position: [0.0; 3],
        }),
        shown,
    ]
}

/// A player's movement and look to the players who see it (PMMP `Entity::broadcastMovement`,
/// dragonfly `ViewEntityMovement`): `MoveActorAbsolute` at the eye position with the ground flag
/// and without the teleport flag, so the client interpolates it over the next frames. The client
/// puts an actor at a `MovePlayer` at once, so that packet is only for a player's own position and
/// for teleports (BDS sends `MovePlayer` here, which makes other players jump 20 times a second).
pub(super) fn player_moved(
    runtime_id: u64,
    eye: Vec3,
    yaw: f64,
    pitch: f64,
    on_ground: bool,
) -> Packet {
    let yaw = to_f32(yaw);
    Packet::MoveActorAbsolute(MoveActorAbsolute {
        entity_runtime_id: runtime_id,
        flags: if on_ground {
            u8::from(MoveFlag::OnGround)
        } else {
            0
        },
        position: eye.to_f32(),
        rotation: [to_f32(pitch), yaw, yaw],
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A lingering potion's cloud shows BDS's cloud keys and types (P6 `potions`, m9-facts.md):
    /// the particle 35, the duration and the radius change on pickup, an int pickup count, and
    /// not the keys BDS leaves out.
    #[test]
    fn a_cloud_shows_bds_cloud_data() {
        let cloud = super::super::projectile::new_cloud(&[]);
        let mut m = EntityMetadata::new();
        cloud_metadata(&cloud, &mut |k, v| {
            m.insert(k as u32, v);
        });
        let get = |k: EntityDataKey| m.get(&(k as u32)).cloned();
        assert_eq!(
            get(EntityDataKey::DataParticle),
            Some(MetadataValue::Int32(35))
        );
        assert_eq!(
            get(EntityDataKey::DataDuration),
            Some(MetadataValue::Int32(600))
        );
        assert_eq!(
            get(EntityDataKey::DataChangeOnPickup),
            Some(MetadataValue::Float32(-0.5))
        );
        assert_eq!(
            get(EntityDataKey::DataPickupCount),
            Some(MetadataValue::Int32(0))
        );
        assert_eq!(
            get(EntityDataKey::DataRadius),
            Some(MetadataValue::Float32(3.0))
        );
        for absent in [
            EntityDataKey::EffectAmbience,
            EntityDataKey::DataSpawnTime,
            EntityDataKey::DataWaiting,
        ] {
            assert_eq!(get(absent), None, "{absent:?}");
        }
    }
}
