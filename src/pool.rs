//! The CPU pool: the threads of the parallel, read-only tick phases.
//!
//! It is one rayon pool that `runtime` builds; this module is the only user of rayon's parallel
//! APIs (`.clippy.toml` forbids them elsewhere, since used outside the pool they would start
//! rayon's hidden global pool). Results come back in input order, so a phase gives the same
//! output whatever the thread count.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use tracing::warn;

/// How long [`Pool::shutdown`] waits for each thread to end. Threads end as soon as their queue
/// is empty, and the tick has stopped by then.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// The CPU pool. The tick and the save thread share it.
#[derive(Debug)]
pub(crate) struct Pool {
    inner: rayon::ThreadPool,
    threads: usize,
    /// One message per thread that ended (rayon's exit handler). The mutex only makes the pool
    /// shareable: it is never locked, [`Pool::shutdown`] takes the receiver out of it.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: never locked; it makes the receiver Sync until shutdown takes it"
    )]
    exits: std::sync::Mutex<Receiver<()>>,
}

impl Pool {
    /// Wraps a pool of `threads` threads whose exit handler sends to `exits`. Only
    /// `runtime::cpu_pool` builds pools.
    pub(crate) fn new(pool: rayon::ThreadPool, threads: usize, exits: Receiver<()>) -> Self {
        Self {
            inner: pool,
            threads,
            exits: exits.into(),
        }
    }

    /// `f` of every item, in item order, computed on the pool's threads. `f` sees shared
    /// references only, as the parallel tick phases are read-only. Fewer than two items are
    /// mapped on the calling thread, which saves the hand-off.
    #[expect(
        clippy::disallowed_methods,
        reason = "consistency: the pool module is the one user of rayon's parallel iterators"
    )]
    pub(crate) fn map<T, R>(&self, items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R>
    where
        T: Sync,
        R: Send,
    {
        if items.len() < 2 {
            return items.iter().map(f).collect();
        }
        self.inner.install(|| items.par_iter().map(f).collect())
    }

    /// Stops the pool and waits until its threads have ended (rayon's drop does not wait).
    /// Returns whether all of them did.
    pub(crate) fn shutdown(self) -> bool {
        let Self {
            inner,
            threads,
            exits,
        } = self;
        drop(inner);
        let exits = match exits.into_inner() {
            Ok(exits) => exits,
            Err(poisoned) => poisoned.into_inner(),
        };
        for _ in 0..threads {
            if exits.recv_timeout(EXIT_WAIT).is_err() {
                warn!("a CPU pool thread did not end in time");
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime;

    #[test]
    fn results_keep_input_order_for_any_thread_count() {
        let items: Vec<u64> = (0..10_000).collect();
        let want: Vec<u64> = items
            .iter()
            .map(|x| x.wrapping_mul(2_654_435_761))
            .collect();
        for threads in [1, 2, 7] {
            let pool = runtime::cpu_pool(threads).unwrap();
            let got = pool.map(&items, |x| x.wrapping_mul(2_654_435_761));
            assert_eq!(got, want, "{threads} threads");
            assert_eq!(pool.map(&items[..1], |x| *x), [0]);
            assert!(pool.map(&items[..0], |x| *x).is_empty());
            assert!(pool.shutdown());
        }
    }

    #[test]
    fn shutdown_waits_for_every_thread() {
        let mut pool = runtime::cpu_pool(4).unwrap();
        let exits = pool.exits.get_mut().unwrap();
        assert!(exits.try_recv().is_err(), "no thread ended yet");
        assert!(pool.shutdown());
    }

    #[test]
    fn a_panic_in_a_job_reaches_the_caller() {
        let pool = runtime::cpu_pool(2).unwrap();
        let items: Vec<u32> = (0..100).collect();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.map(&items, |x| assert!(*x != 50, "job {x} failed"))
        }));
        assert!(result.is_err());
        // The pool still works after a job panicked.
        assert_eq!(pool.map(&items[..3], |x| x + 1), [1, 2, 3]);
        assert!(pool.shutdown());
    }
}
