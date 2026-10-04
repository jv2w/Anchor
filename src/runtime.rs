//! Runtime and threads: the one module that creates threads.

use std::future::Future;
use std::num::NonZeroUsize;
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

use crate::config::Threads;
use crate::error::{Error, Result};
use crate::pool::Pool;

/// Stack size of every thread Anchor creates. Network workers drop NBT nested 512 deep, which
/// recurses (PLAN.md 10.2); one size for every thread keeps the bound independent of the OS
/// default and of `RUST_MIN_STACK`.
const STACK_BYTES: usize = 4 << 20;
/// Blocking jobs of the user ([`Context::spawn_blocking`](crate::Context::spawn_blocking)) that
/// run at once; the others wait their turn, so they never hold every blocking thread.
pub(crate) const USER_BLOCKING_THREADS: usize = 4;
/// Threads tokio may start for blocking work: 4 for the engine's own (the authorization fetch,
/// pack and key loading) and [`USER_BLOCKING_THREADS`] for the user's jobs. tokio's default of 512
/// would let blocking work grow without bound (PLAN.md 5). Blocking threads wait on I/O and are
/// not counted in the compute thread budget.
const MAX_BLOCKING_THREADS: usize = 4 + USER_BLOCKING_THREADS;
/// How long stopping the runtime waits for blocking work: the authorization fetch's 10 s request
/// limit (PLAN.md 10.2), so a fetch in progress finishes and its thread is joined.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// The tick thread and the save thread, outside both pools.
const FIXED_THREADS: usize = 2;
/// The smallest thread budget, whatever the logical CPU count.
const MIN_BUDGET: usize = 4;

/// Thread counts resolved from `network-threads` and `cpu-threads` (PLAN.md 10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThreadCounts {
    /// Tokio worker threads.
    pub(crate) network: usize,
    /// Threads of the parallel game pool (created in M5).
    pub(crate) cpu: usize,
}

impl ThreadCounts {
    /// Resolves `"auto"` from `logical` CPUs: network = max(1, L/4), cpu = max(1, budget − 2 −
    /// network), budget = max(L, 4). Fails if network + cpu + 2 exceeds the budget.
    pub(crate) fn resolve(network: Threads, cpu: Threads, logical: usize) -> Result<Self> {
        let budget = logical.max(MIN_BUDGET);
        let network = match network {
            Threads::Auto => (logical / 4).max(1),
            Threads::Count(n) => usize::from(n.get()),
        };
        let cpu = match cpu {
            Threads::Auto => budget
                .saturating_sub(FIXED_THREADS)
                .saturating_sub(network)
                .max(1),
            Threads::Count(n) => usize::from(n.get()),
        };
        if network.saturating_add(cpu).saturating_add(FIXED_THREADS) > budget {
            return Err(Error::Threads {
                network,
                cpu,
                budget,
            });
        }
        Ok(Self { network, cpu })
    }
}

/// The logical CPU count, or 1 (a budget of 4 threads) if the OS does not tell.
pub(crate) fn logical_cpus() -> usize {
    std::thread::available_parallelism().map_or_else(
        |e| {
            warn!(error = %e, "logical CPU count unknown; using a budget of 4 threads");
            1
        },
        NonZeroUsize::get,
    )
}

/// The tokio runtime that runs networking, sessions and blocking work.
#[derive(Debug)]
pub(crate) struct Runtime(tokio::runtime::Runtime);

impl Runtime {
    /// Starts `threads.network` worker threads.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the runtime module creates the runtime"
    )]
    pub(crate) fn new(threads: ThreadCounts) -> Result<Self> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads.network)
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .thread_stack_size(STACK_BYTES)
            .thread_name("anchor-network")
            .enable_all()
            .build()
            .map(Self)
            .map_err(Error::Start)
    }

    /// Runs `future` to completion on the calling thread, inside the runtime.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: `Server::run` blocks its caller until shutdown"
    )]
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.0.block_on(future)
    }

    /// A handle that starts work on the runtime from any thread (the tick's blocking jobs).
    pub(crate) fn handle(&self) -> tokio::runtime::Handle {
        self.0.handle().clone()
    }

    /// Stops the runtime, waiting up to [`SHUTDOWN_GRACE`] for blocking work.
    pub(crate) fn shutdown(self) {
        self.0.shutdown_timeout(SHUTDOWN_GRACE);
    }
}

/// Starts the CPU pool of `threads` threads (PLAN.md 10.5). Each thread reports its end, so
/// [`Pool::shutdown`] can wait for all of them.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the runtime module creates the CPU pool threads and their exit channel"
)]
pub(crate) fn cpu_pool(threads: usize) -> Result<Pool> {
    let (exited, exits) = std::sync::mpsc::sync_channel(threads);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(STACK_BYTES)
        .thread_name(|i| format!("anchor-cpu-{i}"))
        .exit_handler(move |_| {
            // The channel holds one message per thread, so this never blocks or fails while
            // the pool's owner waits; once it has given up, nobody needs the message.
            if exited.try_send(()).is_err() {
                warn!("CPU pool thread ended after shutdown stopped waiting");
            }
        })
        .build()
        .map_err(|e| Error::Start(std::io::Error::other(e)))?;
    Ok(Pool::new(pool, threads, exits))
}

/// Starts the tick thread running `run`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the runtime module creates the tick thread"
)]
pub(crate) fn spawn_tick<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    std::thread::Builder::new()
        .name("anchor-tick".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(run)
        .map_err(Error::Start)
}

/// Starts the save thread running `run` (PLAN.md 10.6): the one thread that reads and commits
/// the world file.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the runtime module creates the save thread"
)]
pub(crate) fn spawn_save<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    std::thread::Builder::new()
        .name("anchor-save".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(run)
        .map_err(Error::Start)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use super::*;

    fn count(n: u16) -> Threads {
        Threads::Count(NonZeroU16::new(n).unwrap())
    }

    fn auto(logical: usize) -> ThreadCounts {
        ThreadCounts::resolve(Threads::Auto, Threads::Auto, logical).unwrap()
    }

    #[test]
    fn auto_follows_the_budget_formula() {
        for (logical, network, cpu) in [
            (1, 1, 1),
            (2, 1, 1),
            (4, 1, 1),
            (6, 1, 3),
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
    fn auto_always_fits_the_budget() {
        for logical in 1..=512 {
            let t = auto(logical);
            assert!(t.network + t.cpu + 2 <= logical.max(4), "L = {logical}");
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

    #[test]
    fn explicit_counts_over_the_budget_are_rejected() {
        let e = ThreadCounts::resolve(count(4), count(3), 8).unwrap_err();
        assert!(matches!(
            e,
            Error::Threads {
                network: 4,
                cpu: 3,
                budget: 8
            }
        ));
        // An auto value cannot rescue an explicit one that is too large.
        assert!(ThreadCounts::resolve(count(7), Threads::Auto, 8).is_err());
        assert!(ThreadCounts::resolve(Threads::Auto, count(3), 1).is_err());
    }
}
