//! Runtime and threads: the one module that creates threads. Each thread it starts, but the
//! console log's own, takes on the console log of the thread that starts it ([`log`]).

use std::future::Future;
use std::num::NonZeroUsize;
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

use crate::config::{self, Threads};
use crate::console::log;
use crate::error::{Error, Result};
use crate::nethernet::MAX_HOST_LOOKUPS;
use crate::pool::Pool;

/// Blocking threads for the engine's own work: the authorization fetch and key loading.
const ENGINE_BLOCKING_THREADS: usize = 4;
/// The tick thread and the save thread, outside both pools.
const FIXED_THREADS: usize = 2;
/// Threads tokio may start for blocking work: [`ENGINE_BLOCKING_THREADS`], [`MAX_HOST_LOOKUPS`]
/// for NetherNet's lookups of client-given names, and [`USER_BLOCKING_THREADS`] for the user's
/// jobs. tokio's default of 512 would let blocking work grow without bound. Blocking threads
/// wait on I/O and are not counted in the compute thread budget.
const MAX_BLOCKING_THREADS: usize =
    ENGINE_BLOCKING_THREADS + MAX_HOST_LOOKUPS + USER_BLOCKING_THREADS;
/// The fewest network threads `"auto"` gives: a session compressing a large batch holds its
/// worker until it is done, and a second worker meanwhile runs the one UDP task, which every
/// connection shares, whenever it is woken from another thread or queued behind other tasks. A
/// wake from the compressing session itself puts it in tokio's LIFO slot of that worker, which
/// no other worker steals, so it then waits for the compression.
const MIN_AUTO_NETWORK_THREADS: usize = 2;
/// The smallest thread budget, whatever the logical CPU count (`CLAUDE.md`).
const MIN_BUDGET: usize = 5;
/// The fewest threads of the CPU pool.
const MIN_CPU_THREADS: usize = 1;
// The smallest budget holds the fixed threads, the fewest `"auto"` network threads and the
// smallest CPU pool.
const _: () = assert!(FIXED_THREADS + MIN_AUTO_NETWORK_THREADS + MIN_CPU_THREADS <= MIN_BUDGET);
/// How long stopping the runtime waits for blocking work: the authorization fetch's limit,
/// so a fetch in progress finishes and its thread is joined. Its DNS lookups are
/// outside that limit, bounded only by the OS resolver; user jobs still running are abandoned
/// ([`Context::spawn_blocking`](crate::Context::spawn_blocking)).
const SHUTDOWN_GRACE: Duration = crate::auth::HTTP_TIMEOUT;
/// Stack size of every thread Anchor creates. Network workers drop NBT nested
/// [`MAX_DEPTH`](crate::nbt::MAX_DEPTH) deep, which recurses; one size for every thread keeps the
/// bound independent of the OS default and of `RUST_MIN_STACK`.
const STACK_BYTES: usize = 4 << 20;
/// Blocking jobs of the user ([`Context::spawn_blocking`](crate::Context::spawn_blocking)) that
/// run at once; the others wait their turn, so they never hold every blocking thread.
pub(crate) const USER_BLOCKING_THREADS: usize = 4;

/// Thread counts resolved from `network-threads` and `cpu-threads`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThreadCounts {
    /// Tokio worker threads.
    pub(crate) network: usize,
    /// Threads of the parallel game pool.
    pub(crate) cpu: usize,
}

impl ThreadCounts {
    /// The thread counts `"auto"` resolves to from `logical` CPUs (L): network = max([`MIN_AUTO_NETWORK_THREADS`],
    /// L/4), cpu = what the budget max(L, [`MIN_BUDGET`]) leaves after the [`FIXED_THREADS`] and
    /// network. Fails with the key that is too large if network + cpu + [`FIXED_THREADS`] exceeds
    /// the budget: the network count when it leaves no room for [`MIN_CPU_THREADS`], else the CPU
    /// count.
    pub(crate) fn resolve(
        network: Threads,
        cpu: Threads,
        logical: usize,
    ) -> Result<Self, config::Error> {
        let budget = logical.max(MIN_BUDGET);
        let network = match network {
            // Where L/4 is under the minimum and the budget is L, the added network thread comes
            // out of the CPU pool: the shared UDP task's latency weighs more than one pool thread.
            Threads::Auto => (logical / 4).max(MIN_AUTO_NETWORK_THREADS),
            Threads::Count(n) => usize::from(n.get()),
        };
        let usable = budget.saturating_sub(FIXED_THREADS);
        let max_network = usable.saturating_sub(MIN_CPU_THREADS);
        if network > max_network {
            return Err(Self::over_budget(
                "developer.network-threads",
                max_network,
                budget,
            ));
        }
        let max_cpu = usable.saturating_sub(network);
        let cpu = match cpu {
            Threads::Auto => max_cpu,
            Threads::Count(n) => usize::from(n.get()),
        };
        if cpu > max_cpu {
            return Err(Self::over_budget("developer.cpu-threads", max_cpu, budget));
        }
        Ok(Self { network, cpu })
    }

    /// The error for `key` over its largest count `max` under the thread `budget`.
    fn over_budget(key: &str, max: usize, budget: usize) -> config::Error {
        config::Error::Invalid {
            key: key.to_owned(),
            expected: format!(
                "at most {max}: network-threads + cpu-threads + {FIXED_THREADS} (the tick and \
                 save threads) must fit the thread budget of {budget}, the larger of the \
                 logical CPU count and {MIN_BUDGET}"
            ),
        }
    }
}

/// The threads a running server has, as `/status` shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ThreadTotals {
    /// Tokio worker threads.
    pub(crate) network: usize,
    /// Threads of the CPU pool.
    pub(crate) cpu: usize,
    /// Whether the console thread reads standard input (`console-input`).
    pub(crate) console: bool,
    /// Whether the console log's thread writes standard error. Like the console thread, it waits
    /// on I/O and is not counted in the compute thread budget.
    pub(crate) log: bool,
}

impl ThreadTotals {
    /// The most blocking threads tokio starts.
    pub(crate) const BLOCKING: usize = MAX_BLOCKING_THREADS;
    /// The tick thread and the save thread.
    pub(crate) const FIXED: usize = FIXED_THREADS;

    /// The threads of `counts`, without a console thread or a log thread.
    pub(crate) fn new(counts: ThreadCounts) -> Self {
        Self {
            network: counts.network,
            cpu: counts.cpu,
            console: false,
            log: false,
        }
    }

    /// The same threads, with the console thread if `console`.
    pub(crate) fn with_console(self, console: bool) -> Self {
        Self { console, ..self }
    }

    /// The same threads, with the log thread if `log`.
    pub(crate) fn with_log(self, log: bool) -> Self {
        Self { log, ..self }
    }
}

/// The tokio runtime that runs networking, sessions and blocking work.
#[derive(Debug)]
pub(crate) struct Runtime(tokio::runtime::Runtime);

impl Runtime {
    /// A runtime of `threads.network` worker threads.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the runtime module creates the runtime"
    )]
    pub(crate) fn new(threads: ThreadCounts) -> Result<Self> {
        let log = log::inherited();
        tokio::runtime::Builder::new_multi_thread()
            .on_thread_start(move || {
                if let Some(log) = &log {
                    log::attach(log);
                }
            })
            .worker_threads(threads.network)
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .thread_stack_size(STACK_BYTES)
            .thread_name("anchor-network")
            .enable_all()
            .build()
            .map(Self)
            .map_err(Error::Start)
    }

    /// A handle that starts work on the runtime from any thread (the tick's blocking jobs).
    pub(crate) fn handle(&self) -> tokio::runtime::Handle {
        self.0.handle().clone()
    }

    /// Runs `future` to completion on the calling thread, inside the runtime.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: `Server::run` blocks its run thread until shutdown"
    )]
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.0.block_on(future)
    }

    /// Stops the runtime, waiting up to [`SHUTDOWN_GRACE`] for blocking work.
    pub(crate) fn shutdown(self) {
        self.0.shutdown_timeout(SHUTDOWN_GRACE);
    }
}

/// The logical CPU count, or 1 (a budget of [`MIN_BUDGET`] threads) if the OS does not tell.
pub(crate) fn logical_cpus() -> usize {
    std::thread::available_parallelism().map_or_else(
        |e| {
            warn!(
                error = &e as &(dyn std::error::Error + 'static),
                budget = MIN_BUDGET,
                "logical CPU count unknown; using the smallest thread budget"
            );
            1
        },
        NonZeroUsize::get,
    )
}

/// Starts the CPU pool of `threads` threads (at least 1). Each thread reports its end, so
/// [`Pool::shutdown`] can wait for all of them.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the runtime module creates the CPU pool threads and their exit channel"
)]
pub(crate) fn cpu_pool(threads: usize) -> Result<Pool> {
    let (exited, exits) = std::sync::mpsc::sync_channel(threads);
    let log = log::inherited();
    let pool = rayon::ThreadPoolBuilder::new()
        .start_handler(move |_| {
            if let Some(log) = &log {
                log::attach(log);
            }
        })
        .num_threads(threads)
        .stack_size(STACK_BYTES)
        .thread_name(|i| format!("anchor-cpu-{i}"))
        .exit_handler(move |_| {
            // The channel holds one message per thread, so this never blocks or fails while
            // the pool's owner waits; once it has given up, nobody needs the message.
            if exited.try_send(()).is_err() {
                warn!("a CPU pool thread ended after shutdown stopped waiting");
            }
        })
        .build()
        .map_err(|e| Error::Start(std::io::Error::other(e)))?;
    Ok(Pool::new(pool, threads, exits))
}

/// Starts the tick thread running `run`.
pub(crate) fn spawn_tick<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    spawn_thread("anchor-tick", log::logged(run)).map_err(Error::Start)
}

/// Starts the save thread running `run`: the one thread that reads and commits the data
/// files.
pub(crate) fn spawn_save<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    spawn_thread("anchor-save", log::logged(run)).map_err(Error::Start)
}

/// Starts the run thread running `run`: the thread that creates, drives and stops the tokio
/// runtime, because tokio refuses all three on a thread that is inside another runtime, which
/// the caller of `Server::run` may be. It mostly waits, as the console and log threads do, so it
/// is not counted in the compute thread budget.
pub(crate) fn spawn_run<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    spawn_thread("anchor-run", log::logged(run)).map_err(Error::Start)
}

/// Starts a thread named `name` with [`STACK_BYTES`] of stack running `run` as it is: wrap `run`
/// in [`log::logged`] for the thread to take on the console log.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the runtime module creates threads"
)]
pub(crate) fn spawn_thread<T: Send + 'static>(
    name: &str,
    run: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<JoinHandle<T>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(STACK_BYTES)
        .spawn(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroU16;

    fn count(n: u16) -> Threads {
        Threads::Count(NonZeroU16::new(n).unwrap())
    }

    fn auto(logical: usize) -> ThreadCounts {
        ThreadCounts::resolve(Threads::Auto, Threads::Auto, logical).unwrap()
    }

    #[test]
    fn auto_follows_the_budget_formula() {
        for (logical, network, cpu) in [
            (1, 2, 1),
            (2, 2, 1),
            (4, 2, 1),
            (6, 2, 2),
            (8, 2, 4),
            (16, 4, 10),
            (64, 16, 46),
        ] {
            assert_eq!(
                auto(logical),
                ThreadCounts { network, cpu },
                "L = {logical}"
            );
        }
    }

    #[test]
    fn auto_always_fits_the_budget_with_the_fewest_network_threads() {
        for logical in 1..=512 {
            let t = auto(logical);
            assert!(t.network >= MIN_AUTO_NETWORK_THREADS, "L = {logical}");
            assert!(
                t.network + t.cpu + FIXED_THREADS <= logical.max(MIN_BUDGET),
                "L = {logical}"
            );
        }
    }

    #[test]
    fn explicit_counts_up_to_the_budget_are_accepted() {
        let t = ThreadCounts::resolve(count(3), count(3), 8).unwrap();
        assert_eq!(t, ThreadCounts { network: 3, cpu: 3 });
        // With one auto value, the auto value fills what is left.
        let t = ThreadCounts::resolve(count(5), Threads::Auto, 8).unwrap();
        assert_eq!(t, ThreadCounts { network: 5, cpu: 1 });
    }

    #[test]
    fn the_save_thread_is_named_and_runs_its_work() {
        let handle = spawn_save(|| {
            assert_eq!(std::thread::current().name(), Some("anchor-save"));
        })
        .unwrap();
        handle.join().unwrap();
    }

    /// The console log reaches the tick, save, network and blocking threads started under it,
    /// and none started without it.
    #[test]
    fn threads_started_under_the_console_log_take_it_on() {
        let has_log = || log::inherited().is_some();
        let start = || {
            let runtime = Runtime::new(ThreadCounts { network: 1, cpu: 1 }).unwrap();
            let tick = spawn_tick(has_log).unwrap();
            let save = spawn_save(has_log).unwrap();
            (runtime, tick, save)
        };
        let under = || {
            let _log = log::start(crate::config::LogLevel::Info)
                .unwrap()
                .unwrap()
                .install();
            start()
        };
        for (under_log, (runtime, tick, save)) in [(true, under()), (false, start())] {
            assert_eq!(tick.join().unwrap(), under_log);
            assert_eq!(save.join().unwrap(), under_log);
            let (worker, blocking) = runtime.block_on(async move {
                let worker = tokio::spawn(async move { has_log() }).await.unwrap();
                let blocking = tokio::task::spawn_blocking(has_log).await.unwrap();
                (worker, blocking)
            });
            assert_eq!((worker, blocking), (under_log, under_log));
            assert!(!has_log(), "the test thread itself has no log");
            runtime.shutdown();
        }
    }

    /// The key of the setting that is too large, in the error of counts over the budget.
    fn key_over_budget(network: Threads, cpu: Threads, logical: usize) -> String {
        match ThreadCounts::resolve(network, cpu, logical).unwrap_err() {
            config::Error::Invalid { key, .. } => key,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn explicit_counts_over_the_budget_are_rejected_naming_the_key() {
        assert_eq!(
            key_over_budget(count(4), count(3), 8),
            "developer.cpu-threads"
        );
        // Seven network threads leave no room for a CPU thread under a budget of 8.
        assert_eq!(
            key_over_budget(count(7), Threads::Auto, 8),
            "developer.network-threads"
        );
        assert_eq!(
            key_over_budget(count(u16::MAX), count(u16::MAX), 8),
            "developer.network-threads"
        );
        // An auto value cannot rescue an explicit one that is too large.
        assert_eq!(
            key_over_budget(Threads::Auto, count(3), 1),
            "developer.cpu-threads"
        );
        let message = ThreadCounts::resolve(count(4), count(3), 8)
            .unwrap_err()
            .to_string();
        assert!(message.contains("at most 2"), "{message}");
        assert!(message.contains("thread budget of 8"), "{message}");
    }
}
