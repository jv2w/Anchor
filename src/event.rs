//! Events: listener priorities, the events Anchor raises, and the listener lists the tick calls.
//!
//! Listeners are registered with [`ServerBuilder::on`](crate::ServerBuilder::on) and monitors
//! with [`ServerBuilder::monitor`](crate::ServerBuilder::monitor), sorted once when the server is
//! built, and called only in the tick's serial step (PLAN.md 5, 8.2, 10.8). Every listener sees
//! the event, cancelled or not; one that should not act on a cancelled event checks
//! `is_cancelled` (PMMP calls a listener on a cancelled event only with `@handleCancelled`).
//!
//! How Anchor raises an event: it checks that the action may happen, builds the event, calls the
//! listeners, checks again, applies the outcome and then calls the monitors. A listener gets a
//! [`Context`] and may change the game, but its calls raise no events (there is no re-entry); if
//! it changes the game so that the action no longer passes the check, the action is cancelled.
//! Monitors see the applied outcome through a shared [`Context`] and cannot change anything.
//! Without listeners and monitors the event is never built.
//!
//! The events mirror PMMP's namespaces (`block`, `entity`, `inventory`, `player`, `server`,
//! `world`) and are all reachable here, without the `Event` suffix of their PMMP names.

use std::fmt;

use crate::tick::Context;

/// Writes `cancel`, `uncancel` and `is_cancelled` for each listed event, which has a
/// `cancelled: bool` field. The three attributes after a name, when given, are the PMMP aliases of
/// the three methods in that order.
macro_rules! cancellable {
    ($($event:ident $(#[$cancel:meta] #[$uncancel:meta] #[$is:meta])?;)*) => {$(
        impl $event {
            /// Cancels the event: Anchor does not do what the event announces.
            $(#[$cancel])?
            pub fn cancel(&mut self) {
                self.cancelled = true;
            }

            /// Undoes [`cancel`](Self::cancel).
            $(#[$uncancel])?
            pub fn uncancel(&mut self) {
                self.cancelled = false;
            }

            /// Whether a listener cancelled the event.
            $(#[$is])?
            pub fn is_cancelled(&self) -> bool {
                self.cancelled
            }
        }
    )*};
}

/// Writes getters that return the field of the same name: a reference when the type is written
/// `&T`, a copy otherwise. The attributes (docs, PMMP aliases) go on the getter.
macro_rules! getters {
    ($event:ident { $($items:tt)* }) => {
        impl $event {
            getters!(@ $($items)*);
        }
    };
    (@) => {};
    (@ $(#[$attr:meta])* $name:ident: &$ty:ty, $($rest:tt)*) => {
        $(#[$attr])*
        pub fn $name(&self) -> &$ty {
            &self.$name
        }

        getters!(@ $($rest)*);
    };
    (@ $(#[$attr:meta])* $name:ident: $ty:ty, $($rest:tt)*) => {
        $(#[$attr])*
        pub fn $name(&self) -> $ty {
            self.$name
        }

        getters!(@ $($rest)*);
    };
}

/// Writes setters that replace a field: `set_x => x: T`. The attributes (docs, PMMP aliases) go
/// on the setter.
macro_rules! setters {
    ($event:ident { $($(#[$attr:meta])* $setter:ident => $field:ident: $ty:ty,)* }) => {
        impl $event {
            $(
                $(#[$attr])*
                pub fn $setter(&mut self, $field: $ty) {
                    self.$field = $field;
                }
            )*
        }
    };
}

mod block;
mod entity;
mod inventory;
mod player;
mod server;
mod world;

pub use block::{
    BlockBreak, BlockBurn, BlockDeath, BlockExplode, BlockForm, BlockGrow, BlockMelt, BlockPlace,
    BlockPreExplode, BlockSpread, BlockTeleport, BlockUpdate, BrewItem, BrewingFuelUse,
    CampfireCook, ChestPair, FarmlandHydrationChange, LeavesDecay, SignChange, StructureGrow,
};
pub use entity::{
    AreaEffectCloudApply, CombustSource, DamageCause, DamageModifier, DamageSource,
    EntityBlockChange, EntityCombust, EntityDamage, EntityDeath, EntityDespawn, EntityEffectAdd,
    EntityEffectRemove, EntityExplode, EntityExtinguish, EntityFrostWalker, EntityItemPickup,
    EntityMotion, EntityPreExplode, EntityRegainHealth, EntityShootBow, EntitySpawn,
    EntityTeleport, EntityTrampleFarmland, ExtinguishCause, ItemDespawn, ItemMerge, ItemSpawn,
    MAX_FROST_WALKER_RADIUS, ProjectileHitBlock, ProjectileHitEntity, ProjectileLaunch,
    RegainReason,
};
pub use inventory::{
    CraftItem, FurnaceBurn, FurnaceSmelt, Inventory, InventoryClose, InventoryOpen,
    InventoryTransaction, SlotChange, Trade, TradeOpen, Window,
};
pub(crate) use entity::DEFAULT_FIRE_CHANCE;
pub(crate) use player::DuplicateOutcome;
pub use player::{
    BookAction, EnchantOption, ExhaustCause, InteractAction, KickFlag, PlayerBedEnter,
    PlayerBedLeave, PlayerBlockPick, PlayerBucketEmpty, PlayerBucketFill, PlayerChangeSkin,
    PlayerChat, PlayerDataSave, PlayerDeath, PlayerDropItem, PlayerDuplicateLogin, PlayerEditBook,
    PlayerEmote, PlayerEnchantingOptionsRequest, PlayerEntityInteract, PlayerEntityPick,
    PlayerExhaust, PlayerExperienceChange, PlayerGameModeChange, PlayerInfo, PlayerInteract,
    PlayerItemConsume, PlayerItemEnchant, PlayerItemHeld, PlayerItemUse, PlayerJoin, PlayerJump,
    PlayerKick, PlayerLogin, PlayerMissSwing, PlayerMove, PlayerPostChunkSend, PlayerQuit,
    PlayerRespawn, PlayerRespawnAnchorUse, PlayerToggleFlight, PlayerToggleGlide,
    PlayerToggleSneak, PlayerToggleSprint, PlayerToggleSwim, PlayerTransfer,
    PlayerViewDistanceChange, RespawnAnchorAction,
};
pub(crate) use sealed::Listeners;
pub use server::Command;
pub use world::{
    ChunkLoad, ChunkUnload, SpawnChange, WorldInit, WorldLoad, WorldParticle, WorldSave,
    WorldSound, WorldUnload,
};

/// A player or another entity: what PMMP calls an entity, which includes players.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Actor {
    /// A player.
    Player(crate::player::Id),
    /// An entity that is not a player.
    Entity(crate::entity::Id),
}

/// When a listener runs relative to the other listeners of the same event. Listeners run from
/// [`Lowest`](Self::Lowest) to [`Highest`](Self::Highest), so a higher priority has the last say;
/// listeners of equal priority run in registration order.
///
/// PMMP's `MONITOR` is not a priority here: a monitor is registered with
/// [`ServerBuilder::monitor`](crate::ServerBuilder::monitor), runs after the outcome is applied
/// and gets only shared access, so it cannot change what it watches.
#[doc(alias = "EventPriority")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Runs first, so that other listeners can change the outcome.
    #[doc(alias = "EventPriority::LOWEST")]
    Lowest,
    /// Runs after [`Lowest`](Self::Lowest).
    #[doc(alias = "EventPriority::LOW")]
    Low,
    /// The usual priority.
    #[doc(alias = "EventPriority::NORMAL")]
    Normal,
    /// Runs after [`Normal`](Self::Normal).
    #[doc(alias = "EventPriority::HIGH")]
    High,
    /// Runs last and decides the outcome.
    #[doc(alias = "EventPriority::HIGHEST")]
    Highest,
}

/// An event Anchor raises. Only Anchor's own events implement it.
// pmmp-class: Event
pub trait Event: sealed::Sealed + Send + 'static {}

/// A listener: an extension point, so a boxed closure (CLAUDE.md 2.2).
type Listener<E> = Box<dyn FnMut(&mut E, &mut Context<'_>) + Send>;
/// A monitor: called after the outcome is applied, with shared access only.
type Monitor<E> = Box<dyn FnMut(&E, &Context<'_>) + Send>;

/// Declares the listener table: one [`List`] per event, the sealed impls that find it, and the
/// sort that puts every list in calling order. Every event appears here exactly once.
///
/// [`List`]: sealed::List
macro_rules! listeners {
    ($($field:ident: $event:ty),* $(,)?) => {
        /// Every event's listeners and monitors. Moves into the tick state when the server runs.
        #[derive(Default)]
        pub struct Listeners {
            $(pub(super) $field: List<$event>,)*
        }

        /// Shows only the events something listens to, so that the many empty lists stay out.
        impl fmt::Debug for Listeners {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let Self { $($field),* } = self;
                let mut s = f.debug_struct("Listeners");
                $(
                    if !$field.is_empty() {
                        s.field(stringify!($field), $field);
                    }
                )*
                s.finish()
            }
        }

        impl Listeners {
            /// Orders every list by priority, keeping registration order within a priority.
            pub(crate) fn sort(&mut self) {
                let Self { $($field),* } = self;
                $($field.sort();)*
            }

            /// Adds a listener on every event that calls `heard` with the event's field name.
            #[cfg(test)]
            pub(crate) fn listen_to_every_event(&mut self, heard: fn(&'static str)) {
                $(
                    self.push(Priority::Normal, move |_: &mut $event, _: &mut Context<'_>| {
                        heard(stringify!($field));
                    });
                )*
            }

            /// Adds a monitor on every event that calls `watched` with the event's field name.
            #[cfg(test)]
            pub(crate) fn monitor_every_event(&mut self, watched: fn(&'static str)) {
                $(
                    self.push_monitor(move |_: &$event, _: &Context<'_>| {
                        watched(stringify!($field));
                    });
                )*
            }
        }

        $(
            impl Sealed for $event {
                fn list(listeners: &mut Listeners) -> &mut List<Self> {
                    &mut listeners.$field
                }

                fn list_ref(listeners: &Listeners) -> &List<Self> {
                    &listeners.$field
                }
            }

            impl Event for $event {}
        )*
    };
}

/// Public in name only: the module is private, so other crates can neither name nor implement
/// these items, and [`Event`] stays closed to Anchor's events.
mod sealed {
    use super::{Context, Event, Listener, Monitor, Priority, fmt};

    /// Finds an event's listener list, so that registration and dispatch need no type map.
    pub trait Sealed: Sized {
        /// The list of this event in `listeners`.
        fn list(listeners: &mut Listeners) -> &mut List<Self>;

        /// The list of this event in `listeners`, to look at.
        fn list_ref(listeners: &Listeners) -> &List<Self>;
    }

    listeners! {
        block_break: super::BlockBreak,
        block_burn: super::BlockBurn,
        block_death: super::BlockDeath,
        block_explode: super::BlockExplode,
        block_form: super::BlockForm,
        block_grow: super::BlockGrow,
        block_melt: super::BlockMelt,
        block_place: super::BlockPlace,
        block_pre_explode: super::BlockPreExplode,
        block_spread: super::BlockSpread,
        block_teleport: super::BlockTeleport,
        block_update: super::BlockUpdate,
        brew_item: super::BrewItem,
        brewing_fuel_use: super::BrewingFuelUse,
        campfire_cook: super::CampfireCook,
        chest_pair: super::ChestPair,
        farmland_hydration_change: super::FarmlandHydrationChange,
        leaves_decay: super::LeavesDecay,
        sign_change: super::SignChange,
        structure_grow: super::StructureGrow,
        area_effect_cloud_apply: super::AreaEffectCloudApply,
        entity_block_change: super::EntityBlockChange,
        entity_combust: super::EntityCombust,
        entity_damage: super::EntityDamage,
        entity_death: super::EntityDeath,
        entity_despawn: super::EntityDespawn,
        entity_effect_add: super::EntityEffectAdd,
        entity_effect_remove: super::EntityEffectRemove,
        entity_explode: super::EntityExplode,
        entity_extinguish: super::EntityExtinguish,
        entity_frost_walker: super::EntityFrostWalker,
        entity_item_pickup: super::EntityItemPickup,
        entity_motion: super::EntityMotion,
        entity_pre_explode: super::EntityPreExplode,
        entity_regain_health: super::EntityRegainHealth,
        entity_shoot_bow: super::EntityShootBow,
        entity_spawn: super::EntitySpawn,
        entity_teleport: super::EntityTeleport,
        entity_trample_farmland: super::EntityTrampleFarmland,
        item_despawn: super::ItemDespawn,
        item_merge: super::ItemMerge,
        item_spawn: super::ItemSpawn,
        projectile_hit_block: super::ProjectileHitBlock,
        projectile_hit_entity: super::ProjectileHitEntity,
        projectile_launch: super::ProjectileLaunch,
        craft_item: super::CraftItem,
        furnace_burn: super::FurnaceBurn,
        furnace_smelt: super::FurnaceSmelt,
        inventory_close: super::InventoryClose,
        inventory_open: super::InventoryOpen,
        inventory_transaction: super::InventoryTransaction,
        trade_open: super::TradeOpen,
        trade: super::Trade,
        player_bed_enter: super::PlayerBedEnter,
        player_bed_leave: super::PlayerBedLeave,
        player_block_pick: super::PlayerBlockPick,
        player_bucket_empty: super::PlayerBucketEmpty,
        player_bucket_fill: super::PlayerBucketFill,
        player_change_skin: super::PlayerChangeSkin,
        player_chat: super::PlayerChat,
        player_data_save: super::PlayerDataSave,
        player_death: super::PlayerDeath,
        player_drop_item: super::PlayerDropItem,
        duplicate_login: super::PlayerDuplicateLogin,
        player_edit_book: super::PlayerEditBook,
        player_emote: super::PlayerEmote,
        player_enchanting_options_request: super::PlayerEnchantingOptionsRequest,
        player_entity_interact: super::PlayerEntityInteract,
        player_entity_pick: super::PlayerEntityPick,
        player_exhaust: super::PlayerExhaust,
        player_experience_change: super::PlayerExperienceChange,
        player_game_mode_change: super::PlayerGameModeChange,
        player_interact: super::PlayerInteract,
        player_item_consume: super::PlayerItemConsume,
        player_item_enchant: super::PlayerItemEnchant,
        player_item_held: super::PlayerItemHeld,
        player_item_use: super::PlayerItemUse,
        player_join: super::PlayerJoin,
        player_jump: super::PlayerJump,
        player_kick: super::PlayerKick,
        player_login: super::PlayerLogin,
        player_miss_swing: super::PlayerMissSwing,
        player_move: super::PlayerMove,
        player_post_chunk_send: super::PlayerPostChunkSend,
        player_quit: super::PlayerQuit,
        player_respawn: super::PlayerRespawn,
        player_respawn_anchor_use: super::PlayerRespawnAnchorUse,
        player_toggle_flight: super::PlayerToggleFlight,
        player_toggle_glide: super::PlayerToggleGlide,
        player_toggle_sneak: super::PlayerToggleSneak,
        player_toggle_sprint: super::PlayerToggleSprint,
        player_toggle_swim: super::PlayerToggleSwim,
        player_transfer: super::PlayerTransfer,
        player_view_distance_change: super::PlayerViewDistanceChange,
        command: super::Command,
        chunk_load: super::ChunkLoad,
        chunk_unload: super::ChunkUnload,
        spawn_change: super::SpawnChange,
        world_save: super::WorldSave,
        world_init: super::WorldInit,
        world_load: super::WorldLoad,
        world_unload: super::WorldUnload,
        world_sound: super::WorldSound,
        world_particle: super::WorldParticle,
    }

    /// The listeners and monitors of one event, in calling order after [`Listeners::sort`].
    ///
    /// Most events have neither, so an empty list is a null pointer: the table of every event
    /// stays small and [`is_empty`](Self::is_empty) is one comparison.
    pub struct List<E>(Option<Box<Registered<E>>>);

    /// What a [`List`] holds once something is registered.
    struct Registered<E> {
        listeners: Vec<(Priority, Listener<E>)>,
        monitors: Vec<Monitor<E>>,
    }

    impl Listeners {
        /// Adds `listener` for `E` at `priority`, after the listeners already added.
        pub(crate) fn push<E: Event>(
            &mut self,
            priority: Priority,
            listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static,
        ) {
            E::list(self)
                .registered()
                .listeners
                .push((priority, Box::new(listener)));
        }

        /// Adds `monitor` for `E`, after the monitors already added.
        pub(crate) fn push_monitor<E: Event>(
            &mut self,
            monitor: impl FnMut(&E, &Context<'_>) + Send + 'static,
        ) {
            E::list(self).registered().monitors.push(Box::new(monitor));
        }
    }

    impl<E> List<E> {
        /// Whether neither a listener nor a monitor is registered, so that the event need not be
        /// built.
        pub(crate) fn is_empty(&self) -> bool {
            self.0.is_none()
        }

        /// Whether a listener is registered (monitors aside), so that the second check after
        /// the listeners can be skipped when none can have changed the game.
        pub(crate) fn has_listeners(&self) -> bool {
            self.0.as_ref().is_some_and(|r| !r.listeners.is_empty())
        }

        /// Calls every listener on `event` in order.
        pub(crate) fn call(&mut self, event: &mut E, cx: &mut Context<'_>) {
            if let Some(r) = &mut self.0 {
                for (_, listener) in &mut r.listeners {
                    listener(event, cx);
                }
            }
        }

        /// Calls every monitor on `event` in order.
        pub(crate) fn watch(&mut self, event: &E, cx: &Context<'_>) {
            if let Some(r) = &mut self.0 {
                for monitor in &mut r.monitors {
                    monitor(event, cx);
                }
            }
        }

        /// Stable, so equal priorities keep registration order.
        fn sort(&mut self) {
            if let Some(r) = &mut self.0 {
                r.listeners.sort_by_key(|&(priority, _)| priority);
            }
        }

        /// The registered listeners and monitors, made on the first registration.
        fn registered(&mut self) -> &mut Registered<E> {
            self.0.get_or_insert_with(|| {
                Box::new(Registered {
                    listeners: Vec::new(),
                    monitors: Vec::new(),
                })
            })
        }
    }

    impl<E> Default for List<E> {
        fn default() -> Self {
            Self(None)
        }
    }

    impl<E> fmt::Debug for List<E> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let (listeners, monitors) = self
                .0
                .as_ref()
                .map_or((0, 0), |r| (r.listeners.len(), r.monitors.len()));
            f.debug_struct("List")
                .field("listeners", &listeners)
                .field("monitors", &monitors)
                .finish()
        }
    }
}

/// Values the event tests build their events from.
#[cfg(test)]
mod fixtures {
    use crate::item::{Stack, Type};
    use crate::nbt::Compound;
    use crate::tick::queue::SessionId;
    use crate::world::BlockPos;

    /// A block position.
    pub(super) const POS: BlockPos = BlockPos::new(1, 64, -2);

    /// The player of session `n`.
    pub(super) fn player(n: u64) -> crate::player::Id {
        crate::player::Id::new(SessionId::new(n))
    }

    /// The entity in slot `n`.
    pub(super) fn entity(n: u32) -> crate::entity::Id {
        crate::entity::Id::new(n, 0)
    }

    /// `count` sticks.
    pub(super) fn sticks(count: u8) -> Stack {
        let stick = Type::from_name("minecraft:stick", 0, None, &Compound::new()).expect("stick");
        Stack::new(stick, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listener_table_has_only_events_anchor_raises() {
        let mut l = Listeners::default();
        l.listen_to_every_event(|_| {});
        let table = format!("{l:?}");
        assert!(table.contains("world_save") && table.contains("player_join"));
        for never in ["world_difficulty_change", "player_resource_pack_offer"] {
            assert!(!table.contains(never), "{never} is never raised");
        }
    }
}
