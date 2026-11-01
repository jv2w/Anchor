//! What the tick knows of its own load and of the networking: the measured tick times, connection
//! counts, pings and process memory and uptime, which arrive as logged inputs ([`Measure`]), the
//! threads the server started with, and the controls the API asks of the networking ([`Control`]),
//! which leave after the tick's output.
//!
//! PMMP reads the wall clock and the network when asked; the tick reads neither, so the same
//! values come in through the input log and a replay reads what the run read.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

use super::queue::{Control, MAX_CONTROLS_PER_TICK, Measure, SessionId};
use super::{State, TICK};
use crate::runtime::ThreadTotals;

/// The ticks the averages cover: one second's.
const AVERAGED: u32 = 20;

/// The tick's view of its load and of the networking.
#[derive(Debug, Default)]
pub(super) struct Network {
    /// Microseconds each of the last 20 measured ticks took, the oldest replaced first (PMMP
    /// `Server::$useAverage`); 0 before a tick was measured.
    tick_times: [u32; AVERAGED as usize],
    /// The slot of the latest measured tick.
    latest: usize,
    /// The connections the networking held at its last report.
    connections: u32,
    /// Those of [`Self::connections`] whose login was verified.
    logged_in: u32,
    /// The process's resident memory in bytes at its last measure; 0 if unknown.
    memory: u64,
    /// Wall-clock milliseconds the server had run at the last process measure.
    uptime_ms: u64,
    /// The threads the server started with; all 0 without a running server (tests, replays).
    threads: ThreadTotals,
    /// The controls asked for this tick, in order; empty between ticks.
    controls: Vec<Control>,
    /// Where the controls go; `None` without networking (tests, replays), which drops them.
    link: Option<mpsc::Sender<Control>>,
}

impl Network {
    /// How long the latest measured tick took (PMMP `Server::getTickUsage`, which gives it as a
    /// percentage of 50 ms).
    pub(super) fn mspt(&self) -> Duration {
        let latest = self.tick_times.get(self.latest).copied().unwrap_or(0);
        Duration::from_micros(u64::from(latest))
    }

    /// The mean of the last 20 measured tick times (PMMP `Server::getTickUsageAverage`).
    pub(super) fn average_mspt(&self) -> Duration {
        let sum: u64 = self.tick_times.iter().map(|&t| u64::from(t)).sum();
        Duration::from_micros(sum / u64::from(AVERAGED))
    }

    /// The ticks a second the latest tick time allows, at most
    /// [`TICKS_PER_SECOND`](crate::Context::TICKS_PER_SECOND) (PMMP `Server::getTicksPerSecond`:
    /// `min(20, 1 / max(0.001, seconds))`).
    pub(super) fn tps(&self) -> f64 {
        tps_of(self.tick_times.get(self.latest).copied().unwrap_or(0))
    }

    /// The mean of the last 20 ticks' [`Self::tps`] (PMMP `Server::getTicksPerSecondAverage`).
    pub(super) fn average_tps(&self) -> f64 {
        // Summed in slot order, serially: the same on every run and thread count.
        let sum: f64 = self.tick_times.iter().map(|&t| tps_of(t)).sum();
        sum / f64::from(AVERAGED)
    }

    /// The connections the networking held at its last report, and those logged in.
    pub(super) fn connections(&self) -> (u32, u32) {
        (self.connections, self.logged_in)
    }

    /// The process's resident memory in bytes at its last measure; 0 if unknown.
    pub(super) fn memory(&self) -> u64 {
        self.memory
    }

    /// How long the server had run at the last process measure, by the wall clock.
    pub(super) fn uptime(&self) -> Duration {
        Duration::from_millis(self.uptime_ms)
    }

    /// The threads the server started with.
    pub(super) fn threads(&self) -> ThreadTotals {
        self.threads
    }

    /// Sends the controls to the networking from now on.
    pub(super) fn connect(&mut self, link: mpsc::Sender<Control>) {
        self.link = Some(link);
    }

    /// Takes the measures of a checkpoint's `restored` state, keeping where controls go and the
    /// threads.
    pub(super) fn restore(&mut self, restored: Self) {
        let link = self.link.take();
        *self = Self {
            link,
            threads: self.threads,
            ..restored
        };
    }

    /// Queues `control` for the end of the tick; false, queuing nothing, past
    /// [`MAX_CONTROLS_PER_TICK`] this tick.
    pub(super) fn control(&mut self, control: Control) -> bool {
        if self.controls.len() >= MAX_CONTROLS_PER_TICK {
            return false;
        }
        self.controls.push(control);
        true
    }

    /// Hands this tick's controls to the networking without waiting. The networking takes them
    /// as they come; if it has fallen four ticks behind, the rest are dropped with a warning.
    pub(super) fn flush(&mut self) {
        let Some(link) = &self.link else {
            self.controls.clear();
            return;
        };
        for control in self.controls.drain(..) {
            match link.try_send(control) {
                Ok(()) => {}
                Err(TrySendError::Full(control)) => {
                    warn!(?control, "the networking is not taking controls; dropped");
                }
                Err(TrySendError::Closed(_)) => debug!("networking stopped; control dropped"),
            }
        }
    }

    /// Hashes every field kept between ticks (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            tick_times,
            latest,
            connections,
            logged_in,
            memory,
            uptime_ms,
            threads: _,
            controls: _,
            link: _,
        } = self;
        for &t in tick_times {
            h.u64(u64::from(t));
        }
        h.usize(*latest);
        h.u64(u64::from(*connections));
        h.u64(u64::from(*logged_in));
        h.u64(*memory);
        h.u64(*uptime_ms);
    }
}

super::checkpoint::fields!(Network {
    tick_times,
    latest,
    connections,
    logged_in,
    memory,
    uptime_ms;
    threads,
    controls,
    link,
});

impl State {
    /// Records the threads the running server started with, which `/status` shows.
    pub(crate) fn set_threads(&mut self, threads: ThreadTotals) {
        self.network.threads = threads;
    }
}

/// Applies a logged measure of `session`: a tick time, a player's ping or the connection counts.
pub(super) fn apply(state: &mut State, session: SessionId, measure: Measure) {
    match measure {
        Measure::TickTime(micros) => {
            let n = &mut state.network;
            n.latest = (n.latest + 1) % n.tick_times.len();
            if let Some(slot) = n.tick_times.get_mut(n.latest) {
                *slot = micros;
            }
        }
        Measure::Ping(millis) => {
            if let Some(player) = state.players.get_mut(&session) {
                player.presence.set_ping(millis);
            }
        }
        Measure::Connections { all, logged_in } => {
            state.network.connections = all;
            state.network.logged_in = logged_in;
        }
        Measure::Process { memory, uptime_ms } => {
            state.network.memory = memory;
            state.network.uptime_ms = uptime_ms;
        }
    }
}

/// The ticks a second a tick of `micros` allows, at most a full-speed tick's (PMMP's 0.001
/// seconds floor lies below the full-speed tick, so the cap covers it).
fn tps_of(micros: u32) -> f64 {
    if u128::from(micros) <= TICK.as_micros() {
        1.0 / TICK.as_secs_f64()
    } else {
        1_000_000.0 / f64::from(micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::IpAddr;

    use crate::event::Listeners;
    use crate::pool::Pool;
    use crate::tick::log::{EntryKind, InputLog};
    use crate::tick::queue::{Input, InputKind, control_queue};
    use crate::tick::tests::{join, new_state, radius, settings, tick};
    use crate::tick::{Context, Output, deliver, step};

    fn measured(session: SessionId, seq: u64, measure: Measure) -> Input {
        Input {
            session,
            seq,
            kind: InputKind::Measure(measure),
        }
    }

    fn tick_time(tick: u64, micros: u32) -> Input {
        measured(SessionId::ENGINE, tick, Measure::TickTime(micros))
    }

    /// Applies one tick of `inputs` with `log`, then the delivery: the state hash after it.
    fn apply(state: &mut State, mut inputs: Vec<Input>, pool: &Pool, log: &mut InputLog) -> u64 {
        let mut output = Output::default();
        step(state, &mut inputs, &mut Vec::new(), pool, &mut output, log);
        deliver(state, &mut output, log);
        state.hash()
    }

    // pmmp: Server::getTicksPerSecond, Server::getTicksPerSecondAverage, Server::getTickUsage
    // pmmp: Server::getTickUsageAverage, World::getTickRateTime, Network::getConnectionCount
    // pmmp: Network::getValidConnectionCount
    /// The tick times and connection counts come in as logged inputs: the API reads them by
    /// PMMP's formulas, and a replay of the written log reads the same at every tick.
    #[test]
    fn measured_tick_times_and_connection_counts_read_back_and_replay_the_same() {
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let cx = Context { state: &mut state };
        // Before any tick was measured: 20 ticks a second in no time, no connections.
        assert_eq!((cx.tps(), cx.average_tps()), (20.0, 20.0));
        assert_eq!(
            (cx.mspt(), cx.average_mspt()),
            (Duration::ZERO, Duration::ZERO)
        );
        assert_eq!((cx.connection_count(), cx.valid_connection_count()), (0, 0));
        let connections = Measure::Connections {
            all: 5,
            logged_in: 3,
        };
        let ticks = vec![
            vec![tick_time(0, 10_000)],
            vec![
                tick_time(1, 100_000),
                measured(SessionId::NETWORK, 0, connections),
            ],
            vec![tick_time(2, 400)],
        ];
        let mut log = InputLog::default();
        let mut live = Vec::new();
        for (t, inputs) in ticks.into_iter().enumerate() {
            live.push(apply(&mut state, inputs, &pool, &mut log));
            let cx = Context { state: &mut state };
            match t {
                0 => {
                    assert_eq!(cx.tps(), 20.0);
                    assert_eq!(cx.mspt(), Duration::from_millis(10));
                }
                1 => {
                    // 100 ms allow 10 ticks a second; the mean of 19 × 20 and 10 is 19.5.
                    assert_eq!(cx.tps(), 10.0);
                    assert_eq!(cx.average_tps(), 19.5);
                    assert_eq!(cx.mspt(), Duration::from_millis(100));
                    assert_eq!(cx.average_mspt(), Duration::from_micros(110_000 / 20));
                    assert_eq!((cx.connection_count(), cx.valid_connection_count()), (5, 3));
                }
                _ => {
                    // Under a millisecond counts as one, which still allows 20.
                    assert_eq!(cx.tps(), 20.0);
                    assert_eq!(cx.mspt(), Duration::from_micros(400));
                }
            }
        }
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        let mut replay = new_state(settings(|_| {}), Listeners::default());
        let mut again = InputLog::default();
        let mut replayed = Vec::new();
        for t in read.ticks() {
            let inputs = t
                .inputs
                .iter()
                .map(|e| match &e.kind {
                    EntryKind::Measure(m) => measured(e.session, e.seq, *m),
                    other => panic!("{other:?}"),
                })
                .collect();
            replayed.push(apply(&mut replay, inputs, &pool, &mut again));
        }
        assert_eq!(replayed, live);
        assert_eq!(again.encode().unwrap(), log.encode().unwrap());
        assert!(pool.shutdown());
    }

    // pmmp: Server::getMotd, Network::getName, Network::setName, Network::updateName
    // pmmp: Network::blockAddress, Network::unblockAddress
    /// The network controls change the motd the tick reads at once and reach the networking in
    /// order after the tick; a name the `motd` setting refuses, and controls past the tick's
    /// bound, change nothing.
    #[test]
    fn network_controls_reach_the_networking_in_order_within_the_tick_bound() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let (link, mut controls) = control_queue();
        state.connect_network(link);
        let mut cx = Context { state: &mut state };
        assert_eq!(cx.motd(), "Anchor Server");
        for bad in ["", "a;b", "line\nbreak", &"x".repeat(257)] {
            assert!(!cx.set_motd(bad), "{bad:?}");
        }
        assert_eq!(cx.motd(), "Anchor Server");
        assert!(cx.set_motd("§bNew name"));
        assert_eq!(cx.motd(), "§bNew name");
        let ip = IpAddr::from([192, 0, 2, 7]);
        assert!(cx.block_address(ip, Duration::from_secs(300)));
        assert!(cx.unblock_address(ip));
        for _ in 3..MAX_CONTROLS_PER_TICK {
            assert!(cx.unblock_address(ip));
        }
        assert!(!cx.unblock_address(ip), "past the tick's bound");
        assert!(!cx.set_motd("Late"));
        assert_eq!(cx.motd(), "§bNew name");
        state.network.flush();
        let mut got = Vec::new();
        while let Ok(control) = controls.try_recv() {
            got.push(control);
        }
        assert_eq!(got.len(), MAX_CONTROLS_PER_TICK);
        assert_eq!(got[0], Control::Motd("§bNew name".to_owned()));
        let block = Control::Block {
            ip,
            duration: Duration::from_secs(300),
        };
        assert_eq!(got[1], block);
        assert_eq!(got[2], Control::Unblock(ip));
        // The next tick has room again.
        let mut cx = Context { state: &mut state };
        assert!(cx.unblock_address(ip));
    }

    /// A control that the quit of a session dropped in the delivery asks for leaves in that
    /// tick: no control waits past the tick, where its checkpoint would not keep it.
    #[test]
    fn controls_of_a_quit_in_the_delivery_leave_in_the_same_tick() {
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            |_: &mut crate::event::PlayerQuit, cx: &mut Context<'_>| {
                assert!(cx.set_motd("Bye"));
            },
        );
        let mut state = new_state(settings(|_| {}), listeners);
        let (link, mut controls) = control_queue();
        state.connect_network(link);
        let mut output = Output::default();
        let (j, rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        assert!(state.players.contains_key(&SessionId::new(1)));
        drop(rx);
        // A message gives the closed session output this tick.
        let mut cx = Context { state: &mut state };
        assert_eq!(
            cx.broadcast(crate::player::Message::Raw("hi".to_owned())),
            Ok(1)
        );
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(
            !state.players.contains_key(&SessionId::new(1)),
            "dropped in the delivery"
        );
        assert!(state.network.controls.is_empty());
        assert_eq!(controls.try_recv(), Ok(Control::Motd("Bye".to_owned())));
    }
}
