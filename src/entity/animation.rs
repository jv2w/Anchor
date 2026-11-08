//! Animations a player or an entity plays for the players who see it, sent with
//! [`Context::play_animation`](crate::Context::play_animation).

use crate::item;
use crate::protocol::consts::{ActorEvent, AnimateAction, HandSlot};
use crate::protocol::packet::{self, Packet};

/// An animation. The client plays it on the actor it is sent for; one that does not fit the actor
/// (a totem on a dropped item) shows nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
// api-class: Animation
pub enum Animation {
    /// The arm swings. A player's own swing at nothing raises
    /// [`PlayerMissSwing`](crate::event::PlayerMissSwing); a receive hook
    /// ([`ServerBuilder::on_receive`](crate::ServerBuilder::on_receive)) sees the `Animate` packet
    /// of any swing.
    #[doc(alias = "ArmSwingAnimation")]
    #[doc(alias = "ArmSwingAnimation::__construct")]
    ArmSwing,
    /// A stuck arrow shakes for `ticks` (a hit arrow shakes for 7).
    #[doc(alias = "ArrowShakeAnimation")]
    #[doc(alias = "ArrowShakeAnimation::__construct")]
    ArrowShake {
        /// How long it shakes, in ticks.
        ticks: u16,
    },
    /// Eating or drinking `item` (crumbs or drops of it).
    #[doc(alias = "ConsumingItemAnimation")]
    #[doc(alias = "ConsumingItemAnimation::__construct")]
    ConsumingItem(item::Type),
    /// The critical hit stars, `particles` of them.
    #[doc(alias = "CriticalHitAnimation")]
    #[doc(alias = "CriticalHitAnimation::__construct")]
    CriticalHit {
        /// How many stars.
        particles: u16,
    },
    /// The death animation.
    #[doc(alias = "DeathAnimation")]
    #[doc(alias = "DeathAnimation::__construct")]
    Death,
    /// A firework rocket bursts.
    #[doc(alias = "FireworkParticlesAnimation")]
    #[doc(alias = "FireworkParticlesAnimation::__construct")]
    FireworkParticles,
    /// The red hurt flash.
    #[doc(alias = "HurtAnimation")]
    #[doc(alias = "HurtAnimation::__construct")]
    Hurt,
    /// A dropped item shows `count` items.
    #[doc(alias = "ItemEntityStackSizeChangeAnimation")]
    #[doc(alias = "ItemEntityStackSizeChangeAnimation::__construct")]
    ItemStackSize {
        /// The items it shows.
        count: u8,
    },
    /// The enchanted hit stars, `particles` of them.
    #[doc(alias = "MagicHitAnimation")]
    #[doc(alias = "MagicHitAnimation::__construct")]
    MagicHit {
        /// How many stars.
        particles: u16,
    },
    /// Coming back to life.
    #[doc(alias = "RespawnAnimation")]
    #[doc(alias = "RespawnAnimation::__construct")]
    Respawn,
    /// A totem of undying saves a player.
    #[doc(alias = "TotemUseAnimation")]
    #[doc(alias = "TotemUseAnimation::__construct")]
    TotemUse,
}

impl Animation {
    /// The stars of a melee critical hit; the client draws as many as the packet asks, so 0
    /// shows none.
    pub(crate) const CRITICAL_HIT_PARTICLES: u16 = 55;
    /// The stars of a melee hit that a weapon enchantment added damage to.
    pub(crate) const MAGIC_HIT_PARTICLES: u16 = 15;

    /// The packet that plays it on the actor `runtime_id`.
    pub(crate) fn packet(self, runtime_id: u64) -> Packet {
        let event = |event: ActorEvent, data: i32| {
            Packet::ActorEvent(packet::ActorEvent {
                entity_runtime_id: runtime_id,
                event_type: u8::from(event),
                event_data: data,
                fire_at_position: None,
            })
        };
        let animate = |action: AnimateAction, particles: u16| {
            Packet::Animate(packet::Animate {
                action_type: u8::from(action),
                entity_runtime_id: runtime_id,
                data: f32::from(particles),
                swing_source: 0,
                hand: u8::from(HandSlot::MainHand),
            })
        };
        match self {
            Self::ArmSwing => event(ActorEvent::StartAttacking, 0),
            Self::ArrowShake { ticks } => event(ActorEvent::Shake, i32::from(ticks)),
            Self::ConsumingItem(item) => event(
                ActorEvent::Feed,
                (i32::from(item.runtime_id()) << 16) | i32::from(item.meta()),
            ),
            Self::CriticalHit { particles } => animate(AnimateAction::CriticalHit, particles),
            Self::Death => event(ActorEvent::Death, 0),
            Self::FireworkParticles => event(ActorEvent::FireworksExplode, 0),
            Self::Hurt => event(ActorEvent::Hurt, 0),
            Self::ItemStackSize { count } => event(ActorEvent::UpdateStackSize, i32::from(count)),
            Self::MagicHit { particles } => animate(AnimateAction::MagicCriticalHit, particles),
            Self::Respawn => event(ActorEvent::SpawnAlive, 0),
            Self::TotemUse => event(ActorEvent::TalismanActivate, 0),
        }
    }
}
