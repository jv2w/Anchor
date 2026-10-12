//! Packet hooks: user code that sees, changes or drops the packets players send, in the input
//! phase, and the packets the server sends, in the output phase; and the start-game hook, which
//! edits each player's `StartGame`.
//!
//! Receive hooks run in the tick's serial input phase with a [`Context`], in registration order,
//! before Anchor applies the packet; the input log keeps the packet as it arrived, so a replay
//! runs the hooks again the same way. A packet a hook defers waits with its player for the
//! player's next `PlayerAuthInput`. Send hooks run in the parallel output phase and reach no
//! game state: they get the player and the packet only.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use tracing::debug;

use super::queue::{Message, SessionId};
use super::{Context, Settings, State, dispatch};
use crate::ServerBuilder;
use crate::player;
use crate::protocol::packet::{Packet, StartGame};

/// A receive hook.
type ReceiveHook = Box<dyn FnMut(&mut Context<'_>, player::Id, &mut Packet) -> Verdict + Send>;
/// A send hook: `false` drops the packet.
pub(super) type SendHook = Box<dyn Fn(player::Id, &mut Packet) -> bool + Send + Sync>;
/// A start-game hook.
pub(super) type StartHook = Box<dyn Fn(&mut StartGame) + Send + Sync>;

/// The packets a player's receive hooks may hold for its next input ([`Verdict::Defer`]); one
/// more is dropped.
pub(crate) const MAX_DEFERRED_PACKETS: usize = 1024;

/// What a receive hook ([`ServerBuilder::on_receive`]) decides for a packet. A hook that returns
/// `bool` decides [`Apply`](Self::Apply) with `true` and [`Drop`](Self::Drop) with `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Later hooks see the packet, then Anchor applies it.
    Apply,
    /// The packet is dropped; later hooks do not see it.
    Drop,
    /// The packet waits, without later hooks seeing it, with its player: Anchor applies it right
    /// before it applies the player's next `PlayerAuthInput` (before that input's movement and
    /// flags), after the packets deferred before it, as BDS applies an attack when the
    /// attacker's next input takes effect. Only `InventoryTransaction`, `MobEquipment`,
    /// `Interact`, `PlayerAction`, `Animate` and `ItemStackRequest` can wait; another packet, or
    /// one past the 1,024 a player may have waiting, is dropped. A player's waiting packets go
    /// when it leaves.
    Defer,
}

impl From<bool> for Verdict {
    fn from(apply: bool) -> Self {
        if apply { Self::Apply } else { Self::Drop }
    }
}

/// What the receive hooks made of a packet ([`receive`]).
#[derive(Debug)]
pub(super) enum Received<'a> {
    /// Apply it now, changed or not.
    Apply(Cow<'a, Packet>),
    /// Hold it for the player's next input ([`defer`]).
    Defer(Packet),
    Drop,
}

/// The registered packet hooks, fixed when the server is built.
#[derive(Default)]
pub(crate) struct Hooks {
    receive: Vec<ReceiveHook>,
    send: Vec<SendHook>,
    start_game: Vec<StartHook>,
}

impl Hooks {
    /// The send hooks, in registration order.
    pub(super) fn send(&self) -> &[SendHook] {
        &self.send
    }

    /// The start-game hooks, in registration order.
    pub(super) fn start_game(&self) -> &[StartHook] {
        &self.start_game
    }
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks")
            .field("receive", &self.receive.len())
            .field("send", &self.send.len())
            .field("start_game", &self.start_game.len())
            .finish()
    }
}

impl ServerBuilder {
    /// Calls `hook` on every packet a joined player sends, before Anchor handles it, in the
    /// tick's serial input phase and in registration order. The hook may change the packet, and
    /// its [`Verdict`] (or `bool`: `true` applies, `false` drops) says whether the packet goes
    /// on to the later hooks and Anchor, is dropped, or waits for the player's next input.
    ///
    /// Hooks see the packets from the join on: the login and resource pack sequence is the
    /// session's and never reaches them. With `hook-player-input = false`, `PlayerAuthInput`
    /// (sent every client tick) skips them. Like listeners, hooks run only in the tick with a
    /// [`Context`], so a replay of the input log runs them the same way. PMMP's
    /// `DataPacketDecodeEvent` (raw bytes before decoding) has no counterpart: Anchor decodes
    /// and checks every packet first.
    #[doc(alias = "DataPacketReceiveEvent")]
    #[doc(alias = "DataPacketDecodeEvent")]
    #[must_use]
    pub fn on_receive<V: Into<Verdict>>(
        mut self,
        mut hook: impl FnMut(&mut Context<'_>, player::Id, &mut Packet) -> V + Send + 'static,
    ) -> Self {
        self.setup
            .hooks
            .receive
            .push(Box::new(move |cx, player, packet| {
                hook(cx, player, packet).into()
            }));
        self
    }

    /// Calls `hook` on every packet the tick sends a player, after Anchor built it, in the
    /// output phase and in registration order. The hook may change the packet; when it returns
    /// `false` the packet is not sent and later hooks do not see it.
    ///
    /// The output phase runs on the CPU pool, several players at once, so the hook reaches no
    /// game state; for one player the hooks run in the packets' order. Three kinds of packet
    /// skip the hooks: the start-game data every player gets the same (item registry, recipes,
    /// creative inventory, biomes, entity types, block shapes, armour trims, jigsaw structure
    /// data, entity property definitions, camera presets), which is encoded once at start and
    /// shared; the last packet of a session the server closes (its
    /// `Disconnect`); and the login and resource pack sequence, which the session sends. With
    /// `hook-movement-packets = false` the movement packets skip them too: `MovePlayer`,
    /// `MoveActorDelta`, `MoveActorAbsolute` and `SetActorMotion`.
    #[doc(alias = "DataPacketSendEvent")]
    #[must_use]
    pub fn on_send(
        mut self,
        hook: impl Fn(player::Id, &mut Packet) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.setup.hooks.send.push(Box::new(hook));
        self
    }

    /// Calls `edit` on each player's `StartGame` packet when it is built, in registration order,
    /// so a server can set what Anchor leaves at BDS's values: the world name, the seed shown,
    /// experiments, education features, chat and emote settings and the like. Send hooks see
    /// the edited packet.
    ///
    /// Anchor then sets back the fields its game state decides, so the client and the server
    /// keep agreeing: the entity IDs, the game modes, the position, the dimension, the spawn,
    /// the difficulty and hardcore, the time and its day cycle lock, the rain and lightning levels
    /// (the weather), the chunk tick radius, the block and item data (`blocks`,
    /// `use_block_network_id_hashes`, `server_block_state_checksum`, `property_data`), the
    /// server-authoritative settings (`server_authoritative_inventory`,
    /// `server_authoritative_sound`, `player_movement_settings`) and the versions
    /// (`game_version`, `base_game_version`). Like send hooks, `edit` runs on the CPU pool and
    /// reaches no game state.
    #[must_use]
    pub fn start_game(mut self, edit: impl Fn(&mut StartGame) + Send + Sync + 'static) -> Self {
        self.setup.hooks.start_game.push(Box::new(edit));
        self
    }
}

/// Runs the receive hooks on a packet of `session` until one does not apply it. Without hooks,
/// for a session that is not a player, and for `PlayerAuthInput` with
/// `hook-player-input = false`, the packet passes as it is, without a copy.
pub(super) fn receive<'a>(
    state: &mut State,
    session: SessionId,
    packet: &'a Packet,
) -> Received<'a> {
    if state.hooks.receive.is_empty()
        || !state.players.contains_key(&session)
        || (!state.settings.hook_player_input && matches!(packet, Packet::PlayerAuthInput(_)))
    {
        return Received::Apply(Cow::Borrowed(packet));
    }
    // Taken out while they run, so each hook's context can lend the whole state.
    let mut hooks = std::mem::take(&mut state.hooks.receive);
    let mut packet = packet.clone();
    let id = player::Id::new(session);
    let verdict = hooks
        .iter_mut()
        .map(|hook| hook(&mut Context { state }, id, &mut packet))
        .find(|&v| v != Verdict::Apply)
        .unwrap_or(Verdict::Apply);
    state.hooks.receive = hooks;
    match verdict {
        Verdict::Apply => Received::Apply(Cow::Owned(packet)),
        Verdict::Drop => Received::Drop,
        Verdict::Defer => Received::Defer(packet),
    }
}

/// Whether `packet` may wait for its player's next input ([`Verdict::Defer`]): what a client
/// sends between two inputs to act, which BDS applies with the next one.
fn may_defer(packet: &Packet) -> bool {
    matches!(
        packet,
        Packet::InventoryTransaction(_)
            | Packet::MobEquipment(_)
            | Packet::Interact(_)
            | Packet::PlayerAction(_)
            | Packet::Animate(_)
            | Packet::ItemStackRequest(_)
    )
}

/// Holds `packet`, which a receive hook of `session` deferred, for the player's next
/// `PlayerAuthInput` ([`apply_deferred`]); a packet that may not wait, or one past
/// [`MAX_DEFERRED_PACKETS`], is dropped.
pub(super) fn defer(state: &mut State, session: SessionId, packet: Packet) {
    if !may_defer(&packet) {
        debug!(
            session = session.get(),
            id = packet.id(),
            "a receive hook deferred a packet that cannot wait; dropped"
        );
        return;
    }
    // A hook may have made the player leave.
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.deferred_packets.len() >= MAX_DEFERRED_PACKETS {
        debug!(
            session = session.get(),
            id = packet.id(),
            "too many deferred packets; dropped"
        );
        return;
    }
    player.deferred_packets.push_back(Arc::new(packet));
}

/// Applies the packets the receive hooks of `session` deferred, in the order they arrived, each
/// followed by the callbacks it deferred, as the input phase applies a packet: the player's
/// `PlayerAuthInput` comes next. `runs` counts the deferred callbacks of the tick.
pub(super) fn apply_deferred(state: &mut State, session: SessionId, runs: &mut u32) {
    while let Some(packet) = state
        .players
        .get_mut(&session)
        .and_then(|p| p.deferred_packets.pop_front())
    {
        super::apply_packet(state, session, &packet);
        dispatch::run_deferred(state, runs);
    }
}

/// Runs the send hooks on the packets of `messages` for `session`, removing those a hook drops.
/// Shared batches pass untouched, and so do the movement packets with
/// `hook-movement-packets = false` ([`is_movement`]); a shared packet is copied for the session
/// only here, where a hook may change it.
pub(super) fn send(
    hooks: &[SendHook],
    settings: &Settings,
    session: SessionId,
    messages: &mut Vec<Message>,
) {
    if hooks.is_empty() {
        return;
    }
    let id = player::Id::new(session);
    let skip = |packet: &Packet| !settings.hook_movement_packets && is_movement(packet);
    messages.retain_mut(|message| match message {
        Message::Packet(packet) if skip(packet) => true,
        Message::SharedPacket(packet) if skip(packet) => true,
        Message::Packet(packet) => hooks.iter().all(|hook| hook(id, packet)),
        Message::SharedPacket(packet) => {
            let packet = std::sync::Arc::make_mut(packet);
            hooks.iter().all(|hook| hook(id, packet))
        }
        Message::Shared(_) => true,
    });
}

/// Whether the server sends `packet` to move an entity or a player: what
/// `hook-movement-packets = false` keeps from the send hooks.
fn is_movement(packet: &Packet) -> bool {
    matches!(
        packet,
        Packet::MovePlayer(_)
            | Packet::MoveActorDelta(_)
            | Packet::MoveActorAbsolute(_)
            | Packet::SetActorMotion(_)
    )
}

/// Runs the start-game hooks on `packet`, then sets back the fields the game state decides
/// (see [`ServerBuilder::start_game`]) from `packet` as Anchor built it.
pub(super) fn start_game(hooks: &[StartHook], packet: StartGame) -> StartGame {
    if hooks.is_empty() {
        return packet;
    }
    let mut edited = packet.clone();
    for hook in hooks {
        hook(&mut edited);
    }
    let StartGame {
        entity_unique_id,
        entity_runtime_id,
        player_game_mode,
        player_position,
        dimension,
        world_game_mode,
        hardcore,
        difficulty,
        world_spawn,
        day_cycle_lock_time,
        rain_level,
        lightning_level,
        server_chunk_tick_radius,
        base_game_version,
        player_movement_settings,
        time,
        blocks,
        server_authoritative_inventory,
        game_version,
        property_data,
        server_block_state_checksum,
        use_block_network_id_hashes,
        server_authoritative_sound,
        ..
    } = packet;
    StartGame {
        entity_unique_id,
        entity_runtime_id,
        player_game_mode,
        player_position,
        dimension,
        world_game_mode,
        hardcore,
        difficulty,
        world_spawn,
        day_cycle_lock_time,
        rain_level,
        lightning_level,
        server_chunk_tick_radius,
        base_game_version,
        player_movement_settings,
        time,
        blocks,
        server_authoritative_inventory,
        game_version,
        property_data,
        server_block_state_checksum,
        use_block_network_id_hashes,
        server_authoritative_sound,
        ..edited
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Server;
    use crate::event::Listeners;
    use crate::protocol::ExperimentData;
    use crate::protocol::packet::{
        MoveActorAbsolute, MoveActorDelta, MovePlayer, SetActorMotion, SetTime,
    };
    use crate::tick::Output;
    use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};

    fn state_with(builder: ServerBuilder) -> State {
        configured(builder, |_| {})
    }

    /// [`state_with`] with the settings `edit` makes.
    fn configured(builder: ServerBuilder, edit: impl FnOnce(&mut crate::Config)) -> State {
        let mut state = new_state(settings(edit), Listeners::default());
        state.hooks = builder.setup.hooks;
        state
    }

    /// The movement packets session 1 gets when the serial phases queue one of each kind and a
    /// send hook drops every movement packet it sees, on a server with `hook-movement-packets`
    /// set to `hooked`.
    fn movement_sent(hooked: bool) -> usize {
        let builder = Server::builder().on_send(|_, packet| !is_movement(packet));
        let mut state = configured(builder, |c| c.developer.hook_movement_packets = hooked);
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        received(&mut rx);
        let player = state.players.get_mut(&SessionId::new(1)).unwrap();
        for packet in [
            Packet::MovePlayer(MovePlayer::default()),
            Packet::MoveActorDelta(MoveActorDelta::default()),
            Packet::MoveActorAbsolute(MoveActorAbsolute::default()),
            Packet::SetActorMotion(SetActorMotion::default()),
            Packet::SetTime(SetTime { time: 7 }),
        ] {
            player.send_packet(packet);
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        let (packets, ..) = received(&mut rx);
        assert!(packets.contains(&Packet::SetTime(SetTime { time: 7 })));
        packets.iter().filter(|p| is_movement(p)).count()
    }

    /// `hook-movement-packets = false`: movement packets skip the send hooks, so a hook that
    /// drops them does not see them and they go out.
    #[test]
    fn movement_packets_skip_the_send_hooks_when_not_hooked() {
        assert_eq!(movement_sent(true), 0);
        assert_eq!(movement_sent(false), 4);
    }

    thread_local! {
        /// The `PlayerAuthInput` and `RequestChunkRadius` packets the receive hooks saw (they
        /// run on the tick's thread).
        static RECEIVED: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    }

    /// `hook-player-input = false`: `PlayerAuthInput` skips the receive hooks; other packets
    /// still pass them.
    #[test]
    fn player_input_skips_the_receive_hooks_when_not_hooked() {
        for hooked in [true, false] {
            RECEIVED.set((0, 0));
            let builder = Server::builder().on_receive(|_, _, packet| {
                let (inputs, radii) = RECEIVED.get();
                match packet {
                    Packet::PlayerAuthInput(_) => RECEIVED.set((inputs + 1, radii)),
                    Packet::RequestChunkRadius(_) => RECEIVED.set((inputs, radii + 1)),
                    _ => {}
                }
                true
            });
            let mut state = configured(builder, |c| c.developer.hook_player_input = hooked);
            let mut output = Output::default();
            let (j, _rx) = join(1, 0);
            let input = crate::tick::tests::input(
                1,
                2,
                crate::tick::tests::packet(Packet::PlayerAuthInput(Box::default())),
            );
            tick(
                &mut state,
                &mut vec![j, radius(1, 1, 4), input],
                &mut output,
            );
            assert_eq!(RECEIVED.get(), (usize::from(hooked), 1), "hooked: {hooked}");
        }
    }

    fn id(session: u64) -> player::Id {
        player::Id::new(SessionId::new(session))
    }

    /// Joins sessions 1 and 2 and asks for radius 8: the packets each was sent, and the IDs of
    /// everything sent, shared batches included.
    fn spawn_two(state: &mut State) -> [(Vec<Packet>, Vec<u32>); 2] {
        let mut output = Output::default();
        let (j1, mut rx1) = join(1, 0);
        let (j2, mut rx2) = join(2, 0);
        tick(state, &mut vec![j1, j2], &mut output);
        tick(
            state,
            &mut vec![radius(1, 1, 8), radius(2, 1, 8)],
            &mut output,
        );
        [&mut rx1, &mut rx2].map(|rx| {
            let (packets, _, ids) = received(rx);
            (packets, ids)
        })
    }

    fn radii(packets: &[Packet]) -> Vec<i32> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::ChunkRadiusUpdated(c) => Some(c.chunk_radius),
                _ => None,
            })
            .collect()
    }

    // pmmp: DataPacketReceiveEvent, DataPacketDecodeEvent
    #[test]
    fn receive_hooks_change_or_drop_a_players_packets() {
        let builder = Server::builder()
            .on_receive(|ctx, player, packet| {
                assert_eq!(ctx.tick(), 1);
                if let Packet::RequestChunkRadius(p) = packet {
                    p.chunk_radius = 2;
                }
                player != id(2)
            })
            .on_receive(|_, player, packet| {
                assert_eq!(player, id(1), "a dropped packet reaches no later hook");
                if let Packet::RequestChunkRadius(p) = packet {
                    assert_eq!(p.chunk_radius, 2, "later hooks see the change");
                }
                true
            });
        let mut state = state_with(builder);
        let [(first, _), (second, _)] = spawn_two(&mut state);
        assert_eq!(radii(&first), [2]);
        assert!(radii(&second).is_empty(), "the radius request was dropped");
        assert!(state.players[&SessionId::new(2)].chunk_radius.is_none());
        assert_eq!(state.hooks.receive.len(), 2, "the hooks are put back");
    }

    // pmmp: DataPacketSendEvent
    #[test]
    fn send_hooks_change_or_drop_what_a_player_is_sent() {
        let builder = Server::builder().on_send(|player, packet| match packet {
            Packet::ChunkRadiusUpdated(c) => {
                c.chunk_radius = 3;
                true
            }
            Packet::SetHealth(_) => false,
            Packet::SetTime(_) => player == id(1),
            _ => true,
        });
        let mut state = state_with(builder);
        let [(first, ids), (second, _)] = spawn_two(&mut state);
        assert_eq!(radii(&first), [3]);
        assert_eq!(radii(&second), [3]);
        assert!(!first.iter().any(|p| matches!(p, Packet::SetHealth(_))));
        // Shared batches pass: the item registry (162) still arrives.
        assert!(ids.contains(&162));
    }

    #[test]
    fn send_hooks_see_the_packets_of_the_serial_phases() {
        let builder = Server::builder()
            .on_send(|player, packet| !matches!(packet, Packet::SetTime(_)) || player == id(1));
        let mut state = state_with(builder);
        let mut output = Output::default();
        let (j1, mut rx1) = join(1, 0);
        let (j2, mut rx2) = join(2, 0);
        tick(&mut state, &mut vec![j1, j2], &mut output);
        received(&mut rx1);
        received(&mut rx2);
        for session in [1, 2] {
            let player = state.players.get_mut(&SessionId::new(session)).unwrap();
            player.send_packet(Packet::SetTime(SetTime { time: 7 }));
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(state.players.values().all(|p| p.outbox.is_empty()));
        assert_eq!(received(&mut rx1).0, [Packet::SetTime(SetTime { time: 7 })]);
        assert_eq!(received(&mut rx2).0, []);
    }

    #[test]
    fn start_game_hooks_edit_only_what_the_game_state_does_not_decide() {
        let builder = Server::builder().start_game(|p| {
            "Lobby".clone_into(&mut p.world_name);
            p.experiments.push(ExperimentData {
                name: "gametest".to_owned(),
                enabled: true,
            });
            p.entity_runtime_id = 999;
            p.day_cycle_lock_time = 1234;
            p.use_block_network_id_hashes = false;
            p.server_authoritative_inventory = false;
            "1.0".clone_into(&mut p.game_version);
        });
        let mut state = state_with(builder);
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        let (packets, ..) = received(&mut rx);
        let Some(Packet::StartGame(start)) =
            packets.iter().find(|p| matches!(p, Packet::StartGame(_)))
        else {
            panic!("no StartGame in {packets:?}");
        };
        assert_eq!(start.world_name, "Lobby");
        assert_eq!(start.experiments.len(), 1);
        let runtime_id = state.players[&SessionId::new(1)].runtime_id;
        assert_eq!(start.entity_runtime_id, runtime_id);
        // The daylight cycle runs by default, so no time is locked.
        assert_eq!(start.day_cycle_lock_time, 0);
        assert!(start.use_block_network_id_hashes);
        assert!(start.server_authoritative_inventory);
        assert_eq!(start.game_version, crate::protocol::packet::GAME_VERSION);
    }

    /// Spawns three players through `builder`'s hooks: the state hash and the packets sent.
    fn hooked_run(builder: ServerBuilder) -> (u64, Vec<Vec<Packet>>) {
        let mut state = state_with(builder);
        let mut output = Output::default();
        let mut receivers = Vec::new();
        let mut first = Vec::new();
        for s in 1..=3 {
            let (j, rx) = join(s, 0);
            receivers.push(rx);
            first.extend([j, radius(s, 1, 8)]);
        }
        tick(&mut state, &mut first, &mut output);
        let mut second = (1..=3).map(|s| initialised(s, 2, s)).collect();
        tick(&mut state, &mut second, &mut output);
        let sent = receivers.iter_mut().map(|rx| received(rx).0).collect();
        (state.hash(), sent)
    }

    fn hooks() -> ServerBuilder {
        Server::builder()
            .on_receive(|ctx, player, packet| {
                if let Packet::RequestChunkRadius(p) = packet {
                    p.chunk_radius = 3 + i32::try_from(ctx.tick()).unwrap();
                }
                player != id(3)
            })
            .on_send(|_, packet| !matches!(packet, Packet::SetHealth(_)))
            .start_game(|p| "Hooked".clone_into(&mut p.world_name))
    }

    #[test]
    fn hooks_change_nothing_from_run_to_run() {
        let once = hooked_run(hooks());
        assert_eq!(once, hooked_run(hooks()));
        assert_ne!(once, hooked_run(Server::builder()));
    }

    thread_local! {
        /// What the listeners of [`deferring`] heard, in order (they run on the tick's thread).
        static HEARD: std::cell::Cell<Vec<String>> = const { std::cell::Cell::new(Vec::new()) };
    }

    fn hear(what: String) {
        HEARD.with(|h| {
            let mut heard = h.take();
            heard.push(what);
            h.set(heard);
        });
    }

    /// A server whose receive hook defers every `MobEquipment` and `Text`, and whose listeners
    /// note the held slots and the jumps, with session 1 spawned: the server, the next sequence
    /// number of session 1 and its outbound queue.
    fn deferring() -> (
        State,
        Output,
        tokio::sync::mpsc::Receiver<super::super::queue::Outbound>,
    ) {
        let builder = Server::builder().on_receive(|_, _, packet| match packet {
            Packet::MobEquipment(_) | Packet::Text(_) => Verdict::Defer,
            _ => Verdict::Apply,
        });
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            |e: &mut crate::event::PlayerItemHeld, _: &mut Context<'_>| {
                hear(format!("held {}", e.slot()));
            },
        );
        listeners.push(
            crate::event::Priority::Normal,
            |_: &mut crate::event::PlayerJump, _: &mut Context<'_>| hear("jump".to_owned()),
        );
        let mut state = new_state(settings(|_| {}), listeners);
        state.hooks = builder.setup.hooks;
        let mut output = Output::default();
        let (j, rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        tick(&mut state, &mut vec![initialised(1, 2, 1)], &mut output);
        HEARD.with(std::cell::Cell::take);
        (state, output, rx)
    }

    fn equip(seq: u64, slot: u8) -> crate::tick::queue::Input {
        let p = crate::protocol::packet::MobEquipment {
            entity_runtime_id: 1,
            inventory_slot: slot,
            hot_bar_slot: slot,
            ..Default::default()
        };
        crate::tick::tests::input(
            1,
            seq,
            crate::tick::tests::packet(Packet::MobEquipment(Box::new(p))),
        )
    }

    fn jump(seq: u64) -> crate::tick::queue::Input {
        let flags: crate::protocol::FlagSet<crate::protocol::consts::InputFlag> =
            [crate::protocol::consts::InputFlag::StartJumping]
                .into_iter()
                .collect();
        let p = crate::protocol::packet::PlayerAuthInput {
            input_data: crate::protocol::InputFlags::from_set(&flags),
            ..Default::default()
        };
        let packet = Packet::PlayerAuthInput(Box::new(p));
        crate::tick::tests::input(1, seq, crate::tick::tests::packet(packet))
    }

    fn held_slot(state: &State) -> u8 {
        state.players[&SessionId::new(1)].inventory.held_slot()
    }

    /// A deferred packet waits for the player's next `PlayerAuthInput` and applies right before
    /// it, before its flags, in the order the deferred packets arrived; a packet that cannot
    /// wait is dropped.
    #[test]
    fn deferred_packets_apply_in_order_right_before_the_next_input() {
        let (mut state, mut output, _rx) = deferring();
        let text = crate::protocol::packet::Text {
            message: "hi".to_owned(),
            ..Default::default()
        };
        let text = crate::tick::tests::packet(Packet::Text(Box::new(text)));
        let mut batch = vec![
            equip(3, 3),
            equip(4, 5),
            crate::tick::tests::input(1, 5, text),
        ];
        tick(&mut state, &mut batch, &mut output);
        assert_eq!(state.players[&SessionId::new(1)].deferred_len(), 2);
        assert_eq!(held_slot(&state), 0);
        assert!(HEARD.with(std::cell::Cell::take).is_empty());
        tick(&mut state, &mut vec![jump(6)], &mut output);
        assert_eq!(state.players[&SessionId::new(1)].deferred_len(), 0);
        assert_eq!(held_slot(&state), 5);
        assert_eq!(
            HEARD.with(std::cell::Cell::take),
            ["held 3", "held 5", "jump"]
        );
    }

    /// A player holds at most [`MAX_DEFERRED_PACKETS`]; the ones past it are dropped.
    #[test]
    fn deferred_packets_are_bounded() {
        let (mut state, mut output, _rx) = deferring();
        let over = u64::try_from(MAX_DEFERRED_PACKETS).unwrap() + 1;
        let mut batch = (0..over).map(|i| equip(3 + i, 1)).collect();
        tick(&mut state, &mut batch, &mut output);
        assert_eq!(
            state.players[&SessionId::new(1)].deferred_len(),
            MAX_DEFERRED_PACKETS
        );
    }

    /// Deferred packets are in the state hash and come back from a checkpoint.
    #[test]
    fn deferred_packets_are_hashed_and_checkpointed() {
        let (mut state, mut output, _rx) = deferring();
        tick(&mut state, &mut vec![equip(3, 3)], &mut output);
        let (mut without, mut idle, _idle_rx) = deferring();
        tick(&mut without, &mut Vec::new(), &mut idle);
        assert_ne!(state.hash(), without.hash());
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let fresh = new_state(settings(|_| {}), Listeners::default());
        assert!(super::super::checkpoint::tests::assert_round_trip(
            &state, fresh, &pool
        ));
        assert!(pool.shutdown());
    }

    #[test]
    fn hooks_that_keep_everything_change_nothing() {
        let passing = Server::builder()
            .on_receive(|_, _, _| true)
            .on_send(|_, _| true)
            .start_game(|_| {});
        assert_eq!(hooked_run(passing), hooked_run(Server::builder()));
    }
}
