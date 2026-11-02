//! The CPU pool: the threads of the tick's parallel, read-only phases and of the bulk work of the
//! save thread, the import, the export and the replay.
//!
//! It is one rayon pool that `runtime` builds; this module is the only user of rayon's parallel
//! APIs (`.clippy.toml` forbids them elsewhere, since used outside the pool they would start
//! rayon's hidden global pool). Results come back in input order, so a phase gives the same
//! output whatever the thread count.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
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
    /// A pool of `threads` threads (at least 1) whose exit handler sends to `exits`. Only
    /// `runtime::cpu_pool` builds pools.
    pub(crate) fn new(pool: rayon::ThreadPool, threads: usize, exits: Receiver<()>) -> Self {
        Self {
            inner: pool,
            threads,
            exits: exits.into(),
        }
    }

    /// The number of threads: the items of one turn of [`Self::map_in_turns`].
    pub(crate) fn threads(&self) -> usize {
        self.threads
    }

    /// `f` of every item, in item order, computed on the pool's threads. `f` sees shared
    /// references only, as the parallel tick phases are read-only. Fewer than two items, or a
    /// pool of one thread, are mapped on the calling thread, which saves the hand-off (waking a
    /// pool thread and waiting on a lock) that could gain nothing.
    pub(crate) fn map<T, R>(&self, items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R>
    where
        T: Sync,
        R: Send,
    {
        let mut out = Vec::new();
        self.map_into(items, &mut out, f);
        out
    }

    /// [`map`](Self::map) for the save thread: in turns of one item per thread, each mapped after
    /// the last ended, with each turn's first item run on the calling thread. rayon's threads
    /// take work from outside the pool only when none is left to steal, so a tick's map would
    /// wait behind a whole turn if the turn held every thread; this way one is left for it. The
    /// pool thread that runs a tick's map can still pick up, while it waits in a join, a turn's
    /// item it finds unclaimed, so the tick may wait up to one item when a turn starts during a
    /// tick's parallel phase. The turns cost the save thread throughput when items differ in
    /// cost. A panic in any item reaches the caller after the turn's other items end, as with
    /// `map`.
    #[expect(
        clippy::expect_used,
        reason = "invariant: chunks are never empty and the scope ends only after every spawned item ran"
    )]
    pub(crate) fn map_in_turns<T, R>(
        &self,
        items: &[T],
        f: impl Fn(&T) -> R + Sync + Send,
    ) -> Vec<R>
    where
        T: Sync,
        R: Send,
    {
        let f = &f;
        let turn_len = self.threads();
        let mut out = Vec::with_capacity(items.len());
        let mut spawned: Vec<Option<R>> = Vec::with_capacity(turn_len - 1);
        for turn in items.chunks(turn_len) {
            let (first, rest) = turn.split_first().expect("a turn has a first item");
            spawned.resize_with(rest.len(), || None);
            // The rest are spawned first, so they run while this thread maps the first.
            let inline = self.inner.in_place_scope(|scope| {
                for (slot, item) in spawned.iter_mut().zip(rest) {
                    scope.spawn(move |_| *slot = Some(f(item)));
                }
                f(first)
            });
            out.push(inline);
            out.extend(
                spawned
                    .drain(..)
                    .map(|r| r.expect("the scope ran every spawned item")),
            );
        }
        out
    }

    /// [`map`](Self::map) into `out`, which is cleared first and keeps its capacity, so a phase
    /// that runs every tick allocates once.
    #[expect(
        clippy::disallowed_methods,
        reason = "consistency: the pool module is the one user of rayon's parallel iterators"
    )]
    pub(crate) fn map_into<T, R>(
        &self,
        items: &[T],
        out: &mut Vec<R>,
        f: impl Fn(&T) -> R + Sync + Send,
    ) where
        T: Sync,
        R: Send,
    {
        out.clear();
        if self.maps_inline(items.len()) {
            out.extend(items.iter().map(f));
            return;
        }
        self.inner
            .install(|| items.par_iter().map(f).collect_into_vec(out));
    }

    /// [`map`](Self::map) of items `f` takes by value: what a phase took out of the tick's state
    /// before it (the output phase's outboxes) moves to the pool's threads instead of being
    /// copied there.
    #[expect(
        clippy::disallowed_methods,
        reason = "consistency: the pool module is the one user of rayon's parallel iterators"
    )]
    pub(crate) fn map_owned<T, R>(&self, items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R>
    where
        T: Send,
        R: Send,
    {
        if self.maps_inline(items.len()) {
            return items.into_iter().map(f).collect();
        }
        self.inner
            .install(|| items.into_par_iter().map(f).collect())
    }

    /// Stops the pool and waits until its threads have ended (rayon's drop does not wait).
    /// Warns and returns `false` if one did not end in time; only tests read the result.
    #[expect(
        clippy::expect_used,
        reason = "invariant: nothing locks the exits mutex, so it is never poisoned"
    )]
    pub(crate) fn shutdown(self) -> bool {
        let Self {
            inner,
            threads,
            exits,
        } = self;
        drop(inner);
        let exits = exits.into_inner().expect("the exits mutex is never locked");
        for _ in 0..threads {
            if exits.recv_timeout(EXIT_WAIT).is_err() {
                warn!("a CPU pool thread did not end in time");
                return false;
            }
        }
        true
    }

    /// Whether `len` items are mapped on the calling thread: fewer than two have nothing to
    /// share, and with one pool thread the hand-off costs more than it gains.
    fn maps_inline(&self, len: usize) -> bool {
        len < 2 || self.threads == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime;

    #[test]
    fn a_pool_of_one_thread_maps_on_the_calling_thread() {
        let pool = runtime::cpu_pool(1).unwrap();
        let here = std::thread::current().id();
        let mapped = pool.map(&[(), ()], |()| std::thread::current().id());
        assert_eq!(mapped, [here, here]);
        let owned = pool.map_owned(vec![(), ()], |()| std::thread::current().id());
        assert_eq!(owned, [here, here]);
        assert!(pool.shutdown());
    }

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
            let owned = pool.map_owned(items.clone(), |x| x.wrapping_mul(2_654_435_761));
            assert_eq!(owned, want, "{threads} threads, owned");
            assert_eq!(pool.map_owned(vec![0_u64], |x| x), [0]);
            let turns = pool.map_in_turns(&items, |x| x.wrapping_mul(2_654_435_761));
            assert_eq!(turns, want, "{threads} threads in turns");
            assert!(pool.map_in_turns(&items[..0], |x| *x).is_empty());
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
        // Item 0 of a turn runs on the calling thread, item 1 on a pool thread.
        for failing in [0, 1] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool.map_in_turns(&items, |x| assert!(*x != failing, "job {x} failed"))
            }));
            assert!(result.is_err(), "item {failing} in turns");
        }
        // The pool still works after a job panicked.
        assert_eq!(pool.map(&items[..3], |x| x + 1), [1, 2, 3]);
        assert_eq!(pool.map_in_turns(&items[..3], |x| x + 1), [1, 2, 3]);
        assert!(pool.shutdown());
    }

    #[test]
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: test barriers hold a save turn open"
    )]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: test channel for the tick map's result"
    )]
    fn a_save_turn_leaves_a_thread_for_the_tick() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        for threads in [2, 4] {
            let pool = Arc::new(runtime::cpu_pool(threads).unwrap());
            // Every item of the turn and this thread meet twice: once all items run, and once
            // the tick's map has had its chance.
            let started = Arc::new(Barrier::new(threads + 1));
            let release = Arc::new(Barrier::new(threads + 1));
            let save = runtime::spawn_save({
                let pool = Arc::clone(&pool);
                let (started, release) = (Arc::clone(&started), Arc::clone(&release));
                move || {
                    let items: Vec<usize> = (0..threads).collect();
                    let on_workers = AtomicUsize::new(0);
                    let peak_on_workers = AtomicUsize::new(0);
                    let turn = pool.map_in_turns(&items, |&i| {
                        let name = std::thread::current().name().map(str::to_owned);
                        let on_worker = name.as_deref() != Some("anchor-save");
                        if on_worker {
                            let now = on_workers.fetch_add(1, Ordering::SeqCst) + 1;
                            peak_on_workers.fetch_max(now, Ordering::SeqCst);
                        }
                        started.wait();
                        release.wait();
                        if on_worker {
                            on_workers.fetch_sub(1, Ordering::SeqCst);
                        }
                        (i, name)
                    });
                    (turn, peak_on_workers.into_inner())
                }
            })
            .unwrap();
            started.wait();
            let (done, result) = std::sync::mpsc::sync_channel(1);
            let tick = runtime::spawn_thread("tick-map", {
                let pool = Arc::clone(&pool);
                move || done.send(pool.map(&[1_u8, 2], |x| x + 1)).unwrap()
            })
            .unwrap();
            // The timeout only bounds a failure: with every pool thread held, the map never ends.
            let got = result.recv_timeout(Duration::from_secs(30));
            release.wait();
            let (turn, peak_on_workers) = save.join().unwrap();
            tick.join().unwrap();
            assert_eq!(
                got.ok(),
                Some(vec![2, 3]),
                "{threads} threads: tick map ended"
            );
            assert_eq!(
                peak_on_workers,
                threads - 1,
                "{threads} threads: pool threads holding save items at once"
            );
            let order: Vec<usize> = turn.iter().map(|(i, _)| *i).collect();
            assert_eq!(order, (0..threads).collect::<Vec<_>>());
            let inline: Vec<usize> = turn
                .iter()
                .filter(|(_, name)| name.as_deref() == Some("anchor-save"))
                .map(|(i, _)| *i)
                .collect();
            assert_eq!(inline, [0], "{threads} threads: items on the save thread");
            assert!(Arc::try_unwrap(pool).unwrap().shutdown());
        }
    }
}
