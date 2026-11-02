//! Packet hooks: user code that sees, changes or drops the packets players send, in the input
//! phase, and the packets the server sends, in the output phase; and the start-game hook, which
//! edits each player's `StartGame`.
//!
//! Receive hooks run in the tick's serial input phase with a [`Context`], in registration order,
//! before Anchor applies the packet; the input log keeps the packet as it arrived, so a replay
//! runs the hooks again the same way. Send hooks run in the parallel output phase and reach no
//! game state: they get the player and the packet only.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use super::queue::{Message, SessionId, SharedPacket};
use super::{Context, Settings, State};
use crate::ServerBuilder;
use crate::player;
use crate::protocol::packet::{Packet, StartGame};

/// A receive hook: `false` drops the packet.
type ReceiveHook = Box<dyn FnMut(&mut Context<'_>, player::Id, &mut SentPacket<'_>) -> bool + Send>;
/// A send hook: `false` drops the packet.
pub(super) type SendHook = Box<dyn Fn(player::Id, &mut SentPacket<'_>) -> bool + Send + Sync>;
/// A start-game hook.
pub(super) type StartHook = Box<dyn Fn(&mut StartGame) + Send + Sync>;

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

/// A packet as a packet hook sees it: one a player sent ([`ServerBuilder::on_receive`]) or one
/// the tick sends a player ([`ServerBuilder::on_send`]).
///
/// The input log keeps a received packet as it arrived, and the tick builds some packets once for
/// many players (a skin change, the player list). Such a packet stays as it is while hooks only
/// read it: [`get`](Self::get) never copies, and the first [`get_mut`](Self::get_mut) copies it
/// for this player alone. A hook that changes a packet only when it must therefore checks it with
/// `get` first.
#[derive(Debug)]
pub struct SentPacket<'a>(Slot<'a>);

/// Where the packet a hook sees is held.
#[derive(Debug)]
enum Slot<'a> {
    /// A packet a player sent, as the input log holds it, and its copy once a hook changes it.
    Received {
        logged: &'a Packet,
        copy: &'a mut Option<Packet>,
    },
    /// This player's own packet.
    Own(&'a mut Packet),
    /// A packet shared with other players.
    Shared(&'a mut Arc<SharedPacket>),
}

impl SentPacket<'_> {
    /// The packet.
    pub fn get(&self) -> &Packet {
        match &self.0 {
            Slot::Received { logged, copy } => match copy {
                Some(changed) => changed,
                None => logged,
            },
            Slot::Own(packet) => packet,
            Slot::Shared(packet) => packet.get(),
        }
    }

    /// The packet, to change it. A packet the input log holds or one shared with other
    /// players is copied for this player first, once: later calls and later hooks change the
    /// copy.
    pub fn get_mut(&mut self) -> &mut Packet {
        match &mut self.0 {
            Slot::Received { logged, copy } => copy.get_or_insert_with(|| Packet::clone(logged)),
            Slot::Own(packet) => packet,
            Slot::Shared(packet) => Arc::make_mut(packet).get_mut(),
        }
    }
}

impl ServerBuilder {
    /// Calls `hook` on every packet a joined player sends, before Anchor handles it, in the
    /// tick's serial input phase and in registration order. The hook may change the packet
    /// ([`SentPacket::get_mut`]); when it returns `false` the packet is dropped and later hooks
    /// do not see it. The input log keeps the packet as it arrived, so a packet is copied only
    /// when a hook changes it.
    ///
    /// Hooks see the packets from the join on: the login and resource pack sequence is the
    /// session's and never reaches them. Until the player's saved data has loaded,
    /// [`Context::player`] of the hook's ID is `None`. With `hook-player-input = false`,
    /// `PlayerAuthInput` (sent every client tick) skips them. Like listeners, hooks run only in
    /// the tick with a [`Context`], so a replay of the input log runs them the same way. Raw bytes
    /// before decoding have no hook: Anchor decodes and checks every packet first.
    #[doc(alias = "DataPacketReceiveEvent")]
    #[doc(alias = "DataPacketDecodeEvent")]
    #[must_use]
    pub fn on_receive(
        mut self,
        hook: impl FnMut(&mut Context<'_>, player::Id, &mut SentPacket<'_>) -> bool + Send + 'static,
    ) -> Self {
        self.setup.hooks.receive.push(Box::new(hook));
        self
    }

    /// Calls `hook` on every packet the tick sends a player, after Anchor built it, in the
    /// output phase and in registration order. The hook may change the packet
    /// ([`SentPacket::get_mut`]); when it returns `false` the packet is not sent and later hooks
    /// do not see it.
    ///
    /// The output phase runs on the CPU pool, several players at once, so the hook reaches no
    /// game state; for one player the hooks run in the packets' order. Three kinds of packet
    /// skip the hooks: the start-game data every player gets the same (item registry, recipes,
    /// creative inventory, biomes, entity types, block shapes, armour trims, jigsaw structure
    /// data, entity property definitions, camera presets), which is encoded once at start and
    /// shared; the last packet of a session the server closes (a `Disconnect`, a transfer's
    /// `Transfer` or a refused join's `PlayStatus`); and the login and resource pack sequence,
    /// which the session sends. With `hook-movement-packets = false` the movement packets skip
    /// them too: `MovePlayer`, `MoveActorDelta`, `MoveActorAbsolute` and `SetActorMotion`.
    #[doc(alias = "DataPacketSendEvent")]
    #[must_use]
    pub fn on_send(
        mut self,
        hook: impl Fn(player::Id, &mut SentPacket<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.setup.hooks.send.push(Box::new(hook));
        self
    }

    /// Calls `edit` on each player's `StartGame` packet when it is built, in registration order,
    /// so a server can set what Anchor leaves at the official server's values: the world name, the seed shown,
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

/// Runs the receive hooks on a packet of `session`: the packet to apply, changed or not, or
/// `None` when a hook dropped it. The packet is copied only when a hook changes it. Without
/// hooks, for a session that is not a player, and for `PlayerAuthInput` with
/// `hook-player-input = false`, the packet passes as it is.
pub(super) fn receive<'a>(
    state: &mut State,
    session: SessionId,
    packet: &'a Packet,
) -> Option<Cow<'a, Packet>> {
    if state.hooks.receive.is_empty()
        || !state.players.contains_key(&session)
        || (!state.settings.hook_player_input && matches!(packet, Packet::PlayerAuthInput(_)))
    {
        return Some(Cow::Borrowed(packet));
    }
    // Taken out while they run, so each hook's context can lend the whole state.
    let mut hooks = std::mem::take(&mut state.hooks.receive);
    let mut copy = None;
    let id = player::Id::new(session);
    let keep = hooks.iter_mut().all(|hook| {
        let mut received = SentPacket(Slot::Received {
            logged: packet,
            copy: &mut copy,
        });
        hook(&mut Context { state }, id, &mut received)
    });
    state.hooks.receive = hooks;
    keep.then(|| copy.map_or(Cow::Borrowed(packet), Cow::Owned))
}

/// Runs the send hooks on the packets of `messages` for `session`, removing those a hook drops.
/// Shared batches pass untouched, and so do the movement packets with
/// `hook-movement-packets = false` ([`is_movement`]); a shared packet is copied for the session
/// only when a hook changes it ([`SentPacket::get_mut`]).
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
        Message::SharedPacket(packet) | Message::SpawnList(packet) if skip(packet.get()) => true,
        Message::Packet(packet) => {
            let mut sent = SentPacket(Slot::Own(packet));
            hooks.iter().all(|hook| hook(id, &mut sent))
        }
        Message::SharedPacket(packet) | Message::SpawnList(packet) => {
            let mut sent = SentPacket(Slot::Shared(packet));
            hooks.iter().all(|hook| hook(id, &mut sent))
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
        MoveActorAbsolute, MoveActorDelta, MovePlayer, RequestChunkRadius, SetActorMotion, SetTime,
    };
    use crate::tick::Output;
    use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};

    thread_local! {
        /// The `PlayerAuthInput` and `RequestChunkRadius` packets the receive hooks saw (they
        /// run on the tick's thread).
        static RECEIVED: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    }

    /// Makes the state of a server built with `builder`'s hooks and the settings `edit` makes.
    fn configured(builder: ServerBuilder, edit: impl FnOnce(&mut crate::Config)) -> State {
        let mut state = new_state(settings(edit), Listeners::default());
        state.hooks = builder.setup.hooks;
        state
    }

    /// Counts the movement packets session 1 gets when the serial phases queue one of each kind
    /// and a send hook drops every movement packet it sees, on a server with
    /// `hook-movement-packets` set to `hooked`.
    fn movement_sent(hooked: bool) -> usize {
        let builder = Server::builder().on_send(|_, packet| !is_movement(packet.get()));
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

    /// `hook-player-input = false`: `PlayerAuthInput` skips the receive hooks; other packets
    /// still pass them.
    #[test]
    fn player_input_skips_the_receive_hooks_when_not_hooked() {
        for hooked in [true, false] {
            RECEIVED.set((0, 0));
            let builder = Server::builder().on_receive(|_, _, packet| {
                let (inputs, radii) = RECEIVED.get();
                match packet.get() {
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

    fn state_with(builder: ServerBuilder) -> State {
        configured(builder, |_| {})
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

    // api: DataPacketReceiveEvent, DataPacketDecodeEvent
    #[test]
    fn receive_hooks_change_or_drop_a_players_packets() {
        let builder = Server::builder()
            .on_receive(|cx, player, packet| {
                assert_eq!(cx.tick(), 1);
                if matches!(packet.get(), Packet::RequestChunkRadius(_))
                    && let Packet::RequestChunkRadius(p) = packet.get_mut()
                {
                    p.chunk_radius = 2;
                }
                player != id(2)
            })
            .on_receive(|_, player, packet| {
                assert_eq!(player, id(1), "a dropped packet reaches no later hook");
                if let Packet::RequestChunkRadius(p) = packet.get() {
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

    /// Receive hooks that only read a packet pass on the packet the input log holds, without a
    /// copy; one that changes it passes on its own copy.
    #[test]
    fn a_receive_hook_copies_a_packet_only_to_change_it() {
        let logged = Packet::RequestChunkRadius(RequestChunkRadius {
            chunk_radius: 4,
            ..RequestChunkRadius::default()
        });
        let reading = Server::builder()
            .on_receive(|_, _, packet| matches!(packet.get(), Packet::RequestChunkRadius(_)));
        let changing = Server::builder().on_receive(|_, _, packet| {
            if let Packet::RequestChunkRadius(p) = packet.get_mut() {
                p.chunk_radius = 2;
            }
            true
        });
        let mut passed = Vec::new();
        for builder in [reading, changing] {
            let mut state = state_with(builder);
            let (j, _rx) = join(1, 0);
            tick(&mut state, &mut vec![j], &mut Output::default());
            passed.push(
                receive(&mut state, SessionId::new(1), &logged).map(|p| match p {
                    Cow::Borrowed(p) => (true, p.clone()),
                    Cow::Owned(p) => (false, p),
                }),
            );
        }
        let changed = Packet::RequestChunkRadius(RequestChunkRadius {
            chunk_radius: 2,
            ..RequestChunkRadius::default()
        });
        assert_eq!(passed, [Some((true, logged)), Some((false, changed))]);
    }

    // api: DataPacketSendEvent
    #[test]
    fn send_hooks_change_or_drop_what_a_player_is_sent() {
        let builder = Server::builder().on_send(|player, packet| match packet.get() {
            Packet::ChunkRadiusUpdated(_) => {
                if let Packet::ChunkRadiusUpdated(c) = packet.get_mut() {
                    c.chunk_radius = 3;
                }
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
    fn a_send_hook_changes_a_shared_packet_for_its_player_only() {
        let builder = Server::builder().on_send(|player, packet| {
            if player == id(1)
                && matches!(packet.get(), Packet::PlayerList(_))
                && let Packet::PlayerList(list) = packet.get_mut()
            {
                list.entries.clear();
            }
            true
        });
        let mut state = state_with(builder);
        let [(first, _), (second, _)] = spawn_two(&mut state);
        let entries = |packets: &[Packet]| -> Vec<usize> {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::PlayerList(l) => Some(l.entries.len()),
                    _ => None,
                })
                .collect()
        };
        // Both join in one tick, so both windows send the one shared list of the two.
        assert_eq!(entries(&first), [0, 0]);
        assert_eq!(entries(&second), [2, 2]);
    }

    #[test]
    fn send_hooks_see_the_packets_of_the_serial_phases() {
        let builder = Server::builder().on_send(|player, packet| {
            !matches!(packet.get(), Packet::SetTime(_)) || player == id(1)
        });
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
            .on_receive(|cx, player, packet| {
                if matches!(packet.get(), Packet::RequestChunkRadius(_))
                    && let Packet::RequestChunkRadius(p) = packet.get_mut()
                {
                    p.chunk_radius = 3 + i32::try_from(cx.tick()).unwrap();
                }
                player != id(3)
            })
            .on_send(|_, packet| !matches!(packet.get(), Packet::SetHealth(_)))
            .start_game(|p| "Hooked".clone_into(&mut p.world_name))
    }

    #[test]
    fn hooks_change_nothing_from_run_to_run() {
        let once = hooked_run(hooks());
        assert_eq!(once, hooked_run(hooks()));
        assert_ne!(once, hooked_run(Server::builder()));
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
