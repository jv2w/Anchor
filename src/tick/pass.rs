//! Input passes (`input-pass-us`): how the official server applies `PlayerAuthInput`s.
//!
//! Its server loop handles every packet that arrived since the last iteration first and only
//! then lets the inputs those packets queued take effect, so an attack resolved at the
//! attacker's next input sees a victim input that arrived earlier in the same iteration only
//! after the attack. The inputs of one pass take effect in rounds, as its movement catch-up loop
//! ticks every player that still has a queued input again: round r applies every player's r-th
//! input of the pass, players in arrival order, all of round r before round r + 1. A player's
//! deferred packets ([`Verdict::Defer`](crate::Verdict::Defer)) go when its first input of the
//! pass is handled, and for a later input just before that input's round.
//!
//! [`stage`] handles an input as it arrives in the pass and [`apply`] runs the rounds at the end
//! of the pass, in the same tick step, so nothing is staged between steps.

use super::queue::SessionId;
use super::{State, dispatch, hook};
use crate::protocol::packet::Packet;

/// A `PlayerAuthInput` handled in the pass, waiting for the pass's end.
#[derive(Debug)]
pub(super) struct Staged {
    session: SessionId,
    /// Its place among its player's inputs of the pass, from 1.
    round: u32,
    /// Its place among the pass's staged inputs: the order within a round.
    order: usize,
    /// The deferred packets that go just before its round: those deferred since the player's
    /// input before it in the pass (none for round 1, which releases them when handled).
    release: usize,
    packet: Packet,
}

/// What a player's handled inputs of the pass being handled hold; zero between passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Pending {
    /// The player's inputs handled in the pass: the round of the last.
    rounds: u32,
    /// Its deferred packets that inputs of later rounds of the pass will release.
    claimed: usize,
}

impl Pending {
    /// Whether an input of the player was handled but has not taken effect.
    pub(super) fn has_input(self) -> bool {
        self.rounds > 0
    }
}

/// Handles `packet`, a `PlayerAuthInput` of `session`, in the pass: it takes effect when the pass
/// ends ([`apply`]). The player's first input of the pass releases its deferred packets now.
/// `runs` counts the deferred callbacks of the tick.
pub(super) fn stage(state: &mut State, session: SessionId, packet: &Packet, runs: &mut u32) {
    let order = state.staged.len();
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let pending = &mut player.pass_pending;
    pending.rounds = pending.rounds.saturating_add(1);
    let round = pending.rounds;
    let release = if round == 1 {
        0
    } else {
        let waiting = player.deferred_packets.len();
        let release = waiting.saturating_sub(pending.claimed);
        pending.claimed = waiting;
        release
    };
    if round == 1 {
        hook::apply_deferred(state, session, usize::MAX, runs);
    }
    state.staged.push(Staged {
        session,
        round,
        order,
        release,
        packet: packet.clone(),
    });
}

/// Ends the pass: the staged inputs take effect round by round, each round's deferred packets
/// first, each input as the tick applies one ([`super::apply_received`]) followed by the
/// callbacks it deferred.
pub(super) fn apply(state: &mut State, runs: &mut u32) {
    let mut staged = std::mem::take(&mut state.staged);
    for s in &staged {
        if let Some(player) = state.players.get_mut(&s.session) {
            player.pass_pending = Pending::default();
        }
    }
    // Keys are unique per input, so the unstable sort is deterministic and allocates nothing.
    staged.sort_unstable_by_key(|s| (s.round, s.order));
    for round in staged.chunk_by(|a, b| a.round == b.round) {
        if round.first().is_some_and(|s| s.round > 1) {
            for s in round {
                hook::apply_deferred(state, s.session, s.release, runs);
            }
        }
        for s in round {
            super::apply_received(state, s.session, &s.packet);
            dispatch::run_deferred(state, runs);
        }
    }
    staged.clear();
    state.staged = staged;
}

#[cfg(test)]
mod tests {
    use super::super::queue::{Input, Outbound};
    use super::super::tests::{
        initialised, input, join, new_state, packet, radius, settings, tick,
    };
    use super::super::{InputLog, Output, Verdict};
    use super::*;
    use crate::Server;
    use crate::event::{Listeners, PlayerItemHeld, Priority};
    use crate::protocol::packet::{MobEquipment, PlayerAuthInput, Text};

    std::thread_local! {
        static HEARD: std::cell::Cell<Vec<String>> = const { std::cell::Cell::new(Vec::new()) };
    }

    fn hear(what: String) {
        HEARD.with(|h| {
            let mut heard = h.take();
            heard.push(what);
            h.set(heard);
        });
    }

    fn heard() -> Vec<String> {
        HEARD.with(std::cell::Cell::take)
    }

    const ATTACKER: u64 = 1;
    const VICTIM: u64 = 2;

    fn id(session: u64) -> crate::player::Id {
        crate::player::Id::new(SessionId::new(session))
    }

    /// Two spawned players, the attacker and the victim, whose receive hook defers every
    /// `MobEquipment` (standing for an attack or any deferred action), notes each input as it
    /// takes effect with the attacker's held slot, and notes at each `Text` which players have
    /// an input that has not taken effect; a listener notes each held slot change.
    fn players() -> (State, Output, Vec<tokio::sync::mpsc::Receiver<Outbound>>) {
        let builder = Server::builder().on_receive(|cx, player, packet| match packet {
            Packet::MobEquipment(_) => Verdict::Defer,
            Packet::PlayerAuthInput(_) => {
                let slot = cx.player(id(ATTACKER)).map(super::super::Player::held_slot);
                hear(format!("input {} sees {slot:?}", player.session().get()));
                Verdict::Apply
            }
            Packet::Text(_) => {
                let unapplied = [ATTACKER, VICTIM].map(|s| {
                    cx.player(id(s))
                        .is_some_and(super::super::Player::has_unapplied_input)
                });
                hear(format!("unapplied {unapplied:?}"));
                if let Some(p) = cx.player_mut(player) {
                    p.send_packet(packet.clone());
                }
                Verdict::Drop
            }
            _ => Verdict::Apply,
        });
        let mut listeners = Listeners::default();
        listeners.push(
            Priority::Normal,
            |e: &mut PlayerItemHeld, _: &mut super::super::Context<'_>| {
                hear(format!("held {}", e.slot()));
            },
        );
        let passes = settings(|c| {
            c.developer.input_order = crate::config::InputOrder::Arrival;
            c.developer.input_pass_us = 15_625;
        });
        let mut state = new_state(passes, listeners);
        state.hooks = builder.setup.hooks;
        let mut output = Output::default();
        let (a, a_rx) = join(ATTACKER, 0);
        let (v, v_rx) = join(VICTIM, 0);
        tick(
            &mut state,
            &mut vec![a, radius(ATTACKER, 1, 4), v, radius(VICTIM, 1, 4)],
            &mut output,
        );
        tick(
            &mut state,
            &mut vec![initialised(ATTACKER, 2, 1), initialised(VICTIM, 2, 2)],
            &mut output,
        );
        heard();
        (state, output, vec![a_rx, v_rx])
    }

    fn walk(session: u64, seq: u64) -> Input {
        let p = PlayerAuthInput {
            // As a decoded input has it, so that the log reads back the same.
            input_data: crate::protocol::InputFlags::from_set(&crate::protocol::FlagSet::default()),
            ..Default::default()
        };
        let packet = Packet::PlayerAuthInput(Box::new(p));
        input(session, seq, super::super::tests::packet(packet))
    }

    fn equip(seq: u64, slot: u8) -> Input {
        let p = MobEquipment {
            entity_runtime_id: 1,
            inventory_slot: slot,
            hot_bar_slot: slot,
            ..Default::default()
        };
        input(ATTACKER, seq, packet(Packet::MobEquipment(Box::new(p))))
    }

    fn text(session: u64, seq: u64) -> Input {
        let p = Text {
            message: "?".to_owned(),
            ..Default::default()
        };
        input(session, seq, packet(Packet::Text(Box::new(p))))
    }

    /// Applies pass `number` of `inputs`, arrived in that order, and delivers its output.
    fn run_pass(
        state: &mut State,
        number: u64,
        inputs: Vec<Input>,
        output: &mut Output,
        log: &mut InputLog,
    ) {
        let mut runs = 0;
        let inputs = inputs.into_iter().zip(1..);
        super::super::pass(state, number, inputs, &mut runs, output, log);
        super::super::deliver(state, output, log);
        log.end_pass();
    }

    /// The case passes exist for: the victim's input arrives first, then the attacker's, whose
    /// deferred attack goes when it is handled; the victim's input takes effect only at the end
    /// of the pass, so the attack sees the victim without it. Applied as they arrive, the attack
    /// would see it.
    #[test]
    fn an_attack_sees_the_victim_before_its_input_of_the_same_pass() {
        let (mut state, mut output, _rx) = players();
        let mut log = InputLog::default();
        tick(&mut state, &mut vec![equip(3, 3)], &mut output);
        let inputs = vec![walk(VICTIM, 3), walk(ATTACKER, 4)];
        run_pass(&mut state, 0, inputs, &mut output, &mut log);
        assert_eq!(
            heard(),
            ["held 3", "input 2 sees Some(3)", "input 1 sees Some(3)"]
        );
        let (mut state, mut output, _rx) = players();
        tick(&mut state, &mut vec![equip(3, 3)], &mut output);
        tick(
            &mut state,
            &mut vec![walk(VICTIM, 3), walk(ATTACKER, 4)],
            &mut output,
        );
        assert_eq!(
            heard(),
            ["input 2 sees Some(0)", "held 3", "input 1 sees Some(3)"]
        );
    }

    /// The victim's input of one pass has taken effect before a packet of the next pass is
    /// handled.
    #[test]
    fn inputs_of_a_pass_take_effect_at_its_end() {
        let (mut state, mut output, _rx) = players();
        let mut log = InputLog::default();
        tick(&mut state, &mut vec![equip(3, 3)], &mut output);
        run_pass(&mut state, 0, vec![walk(VICTIM, 3)], &mut output, &mut log);
        run_pass(
            &mut state,
            1,
            vec![walk(ATTACKER, 4)],
            &mut output,
            &mut log,
        );
        assert_eq!(
            heard(),
            ["input 2 sees Some(0)", "held 3", "input 1 sees Some(3)"]
        );
    }

    /// The rounds measured on the official server: with attacker inputs [A1, attack, A2] and a
    /// victim input V1 in one pass, the attack goes after A1 and V1 took effect and before A2
    /// does; an attack after A2 waits for the attacker's next input. Inputs handled but not
    /// applied are reported until the pass ends.
    #[test]
    fn a_pass_applies_its_inputs_in_rounds() {
        let (mut state, mut output, mut rx) = players();
        for queue in &mut rx {
            while queue.try_recv().is_ok() {}
        }
        let mut log = InputLog::default();
        let inputs = vec![
            walk(ATTACKER, 3),
            walk(VICTIM, 3),
            equip(4, 3),
            walk(ATTACKER, 5),
            text(VICTIM, 4),
            equip(6, 5),
        ];
        run_pass(&mut state, 0, inputs, &mut output, &mut log);
        assert_eq!(
            heard(),
            [
                "unapplied [true, true]",
                "input 1 sees Some(0)",
                "input 2 sees Some(0)",
                "held 3",
                "input 1 sees Some(3)",
            ]
        );
        let attacker = &state.players[&SessionId::new(ATTACKER)];
        assert!(!attacker.has_unapplied_input());
        assert_eq!(attacker.deferred_len(), 1);
        assert!(state.staged.is_empty());
        // What the pass queued for the victim left at its end, without waiting for a tick.
        let Ok(Outbound::Send { pass: 0, messages }) = rx[1].try_recv() else {
            panic!("the pass's output was not delivered");
        };
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, super::super::queue::Message::Packet(Packet::Text(_))))
        );
    }

    /// The logged inputs of `entries` with their arrivals, as a replay gives them.
    fn logged(entries: &[super::super::LoggedEntry]) -> Vec<(Input, u64)> {
        entries
            .iter()
            .map(|e| {
                let kind = match &e.kind {
                    super::super::EntryKind::Packet(p) => packet(p.clone()),
                    other => panic!("not a packet: {other:?}"),
                };
                let input = Input {
                    session: e.session,
                    seq: e.seq,
                    arrived: None,
                    kind,
                };
                (input, e.arrival_us)
            })
            .collect()
    }

    /// The log keeps each pass's number and its inputs with their arrivals, survives being
    /// written and read, and its replay reaches the same state.
    #[test]
    fn passes_are_logged_and_replay_to_the_same_state() {
        let (mut live, mut output, _rx) = players();
        let (mut replayed, _output, _replayed_rx) = players();
        assert_eq!(live.hash(), replayed.hash());
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let mut log = InputLog::starting_at(live.tick);
        let inputs = vec![walk(VICTIM, 3), equip(3, 3), walk(ATTACKER, 4)];
        run_pass(&mut live, 0, inputs, &mut output, &mut log);
        run_pass(&mut live, 1, vec![walk(ATTACKER, 5)], &mut output, &mut log);
        super::super::step(
            &mut live,
            &mut Vec::new(),
            &mut Vec::new(),
            &pool,
            &mut output,
            &mut log,
        );
        super::super::deliver(&mut live, &mut output, &mut log);
        super::super::boundary(&mut live, &mut output, &mut log);
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        let ticks = read.ticks();
        assert_eq!(ticks, log.ticks());
        let [tick] = ticks.as_slice() else {
            panic!("one tick: {ticks:?}");
        };
        let passes: Vec<(u64, Vec<u64>)> = tick
            .passes
            .iter()
            .map(|p| (p.number, p.inputs.iter().map(|e| e.arrival_us).collect()))
            .collect();
        assert_eq!(passes, [(0, vec![1, 2, 3]), (1, vec![1])]);
        let mut own = InputLog::starting_at(replayed.tick);
        let mut runs = 0;
        for pass in &tick.passes {
            let requests = super::super::replay_pass(
                &mut replayed,
                pass.number,
                logged(&pass.inputs),
                &pass.dropped,
                &mut runs,
                &mut own,
            );
            assert!(requests.is_empty());
        }
        let requests = super::super::replay_tick(
            &mut replayed,
            &mut Vec::new(),
            &mut Vec::new(),
            &tick.dropped,
            &pool,
            &mut own,
        );
        assert!(requests.is_empty());
        assert_eq!(live.hash(), replayed.hash());
        assert_eq!(own.ticks(), log.ticks());
        assert!(pool.shutdown());
    }

    /// The tick thread's passes: each input goes to the pass it arrived in, passes without
    /// inputs are skipped, an input past the tick's bound waits, and one the session did not
    /// stamp counts as arriving now.
    #[test]
    fn the_tick_thread_applies_each_input_in_the_pass_it_arrived_in() {
        let (mut state, mut output, _rx) = players();
        let period = std::time::Duration::from_micros(15_625);
        let epoch = crate::clock::now().checked_sub(period * 10).unwrap();
        let passes = crate::clock::Passes::new(epoch, std::num::NonZeroU32::new(15_625).unwrap());
        let in_pass = |pass: u32, input: Input| Input {
            arrived: Some(epoch + period * pass + std::time::Duration::from_micros(1)),
            ..input
        };
        let (tx, mut queue) = tokio::sync::mpsc::channel(8);
        for input in [
            in_pass(2, walk(VICTIM, 3)),
            in_pass(2, walk(ATTACKER, 3)),
            in_pass(5, walk(ATTACKER, 4)),
            equip(5, 3),
        ] {
            tx.try_send(input).unwrap();
        }
        let release = std::sync::Arc::new(crate::session::Release::new(passes, 10_000));
        let mut passing = super::super::Passing::new(passes, Some(std::sync::Arc::clone(&release)));
        let mut log = InputLog::starting_at(state.tick);
        passing.run_ended(&mut state, &mut queue, &mut output, &mut log);
        assert!(passing.next >= 10);
        // Every pass that ended is reported finished: a datagram releases them all.
        let released = release.subscribe();
        release.datagram();
        assert_eq!(*released.borrow(), passing.next);
        assert_eq!(
            passing.waiting.len(),
            1,
            "the unstamped input waits for its pass"
        );
        let ticks = log.ticks();
        let numbers: Vec<(u64, usize)> = ticks[0]
            .passes
            .iter()
            .map(|p| (p.number, p.inputs.len()))
            .collect();
        assert_eq!(numbers, [(2, 2), (5, 1)]);

        let (mut state, mut output, _rx) = players();
        for input in [in_pass(2, walk(VICTIM, 3)), in_pass(2, walk(ATTACKER, 3))] {
            tx.try_send(input).unwrap();
        }
        let mut passing = super::super::Passing::new(passes, None);
        passing.handled = super::super::MAX_INPUTS_PER_TICK - 1;
        let mut log = InputLog::starting_at(state.tick);
        passing.run_ended(&mut state, &mut queue, &mut output, &mut log);
        assert_eq!(passing.waiting.len(), 1, "past the tick's bound");
        assert_eq!(log.ticks()[0].passes.len(), 1);
        heard();
    }

    /// Driven by random inputs and pass ends, the round of each staged input is its player's
    /// count of staged inputs, and a player has an unapplied input exactly while one of its
    /// inputs is staged.
    #[test]
    fn rounds_count_each_players_inputs_of_the_pass() {
        let (mut state, _output, _rx) = players();
        let mut random = crate::random::Random::new(1, 2, 3);
        let mut runs = 0;
        for _ in 0..5_000 {
            if random.below(10) < 2 {
                apply(&mut state, &mut runs);
            } else {
                let session = SessionId::new(1 + u64::from(random.below(2)));
                let packet = Packet::PlayerAuthInput(Box::default());
                stage(&mut state, session, &packet, &mut runs);
                let want = state.staged.iter().filter(|s| s.session == session).count();
                let round = state.staged.last().map(|s| s.round);
                assert_eq!(round, Some(u32::try_from(want).unwrap()));
            }
            for (&session, player) in &state.players {
                let staged = state.staged.iter().any(|s| s.session == session);
                assert_eq!(player.has_unapplied_input(), staged);
            }
        }
        heard();
    }
}
