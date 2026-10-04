//! Blocking work off the tick (PLAN.md 5, 10.8, PMMP `AsyncPool`/`AsyncTask`): the work runs on the
//! runtime's blocking pool, at most [`MAX_RUNNING_JOBS`] jobs at once, and its result comes
//! back as an input of a later tick ([`InputKind::Job`](super::super::queue::InputKind::Job)),
//! which the input log records.
//!
//! A job's result is its JSON form ([`MAX_RESULT_BYTES`] at most), made on the blocking thread;
//! the completion callback deserializes it in the tick. The log keeps the JSON, so a replay hands
//! the callback the same value without running the work: without a runner (a replay), a job is
//! numbered and its callback kept as in the live run, and the work is dropped unrun.
//!
//! A panic in the work is caught by the blocking pool, as any blocking task's is, and reaches the
//! callback as [`Error::Panicked`]: user work is neither the tick nor the save thread, whose panics
//! stop the server (CLAUDE.md 4.6), and a panicking job must not take the game down with it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use super::super::State;
use super::super::queue::{InputSender, JobOutcome};
use super::Context;
use crate::data::{JsonError, to_json};
use crate::task::{Error, MAX_JOBS, MAX_RESULT_BYTES, MAX_RUNNING_JOBS, Result};

/// A job's completion callback, taking the result's JSON or the job's error.
type Done = Box<dyn FnOnce(&mut Context<'_>, std::result::Result<&[u8], Error>) + Send>;

/// Where jobs run: the runtime's blocking pool, and the input queue their results go to.
struct Runner {
    handle: tokio::runtime::Handle,
    inputs: InputSender,
    /// One permit per job allowed to run at once, so jobs cannot take every blocking thread
    /// from the engine's own blocking work (the authorization fetch, pack and key loading).
    slots: Arc<Semaphore>,
}

/// The blocking jobs waiting for their result.
#[derive(Default)]
pub(crate) struct Jobs {
    /// The callbacks of the jobs whose result has not arrived, by job id.
    pending: BTreeMap<u64, Done>,
    /// The id of the next job; ids are never reused.
    next_id: u64,
    /// `None` until the server starts it, and in a replay, which runs no work.
    runner: Option<Runner>,
}

impl Jobs {
    /// Runs jobs from now on: on `handle`'s blocking pool, results into `inputs`.
    pub(super) fn start(&mut self, handle: tokio::runtime::Handle, inputs: &InputSender) {
        self.runner = Some(Runner {
            handle,
            inputs: inputs.clone(),
            slots: Arc::new(Semaphore::new(MAX_RUNNING_JOBS)),
        });
    }

    /// How many jobs wait for their result (for leak tests).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }

    /// The ids of the jobs waiting for their result, in order (for runs that answer them).
    #[cfg(test)]
    pub(super) fn waiting(&self) -> Vec<u64> {
        self.pending.keys().copied().collect()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::super::Fnv) {
        let Self {
            pending,
            next_id,
            runner: _,
        } = self;
        h.u64(*next_id);
        h.usize(pending.len());
        for id in pending.keys() {
            h.u64(*id);
        }
    }
}

impl fmt::Debug for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs")
            .field("pending", &self.pending.len())
            .field("next_id", &self.next_id)
            .field("running", &self.runner.is_some())
            .finish()
    }
}

/// The JSON of a job's result, made on the blocking thread.
fn encode<T: Serialize>(value: &T) -> JobOutcome {
    to_json(value, MAX_RESULT_BYTES).map_err(|e| match e {
        JsonError::TooLarge => Error::TooLarge,
        JsonError::Encode => Error::Encode,
    })
}

impl Runner {
    /// Starts `work` as job `id`. The result waits for a free slot's thread, then for room in the
    /// input queue; it is dropped if the tick has stopped.
    fn run<T, W>(&self, id: u64, work: W)
    where
        T: Serialize + Send + 'static,
        W: FnOnce() -> T + Send + 'static,
    {
        let inputs = self.inputs.clone();
        let slots = Arc::clone(&self.slots);
        drop(self.handle.spawn(async move {
            // The semaphore is never closed, so acquiring only waits.
            let Ok(permit) = slots.acquire_owned().await else {
                return;
            };
            let outcome = match tokio::task::spawn_blocking(move || encode(&work())).await {
                Ok(outcome) => outcome,
                Err(e) if e.is_panic() => {
                    warn!(job = id, "blocking work panicked");
                    Err(Error::Panicked)
                }
                // The runtime is shutting down: nobody waits for the result.
                Err(_) => return,
            };
            drop(permit);
            if inputs.job(id, outcome).await.is_err() {
                debug!(job = id, "the tick stopped before the job's result arrived");
            }
        }));
    }
}

impl Context<'_> {
    /// Runs `work` off the tick on the blocking pool and, in a later tick, `done` with its
    /// result, in the tick's input step. `work` cannot touch the game; anything it needs it takes
    /// by value. Its result travels as JSON and is recorded in the input log, so a replay gives
    /// `done` the same value without running `work` again (PMMP `AsyncTask` without its
    /// serialized task object).
    ///
    /// `done` gets [`Error::Panicked`] if `work` panicked, [`Error::TooLarge`] if the result's
    /// JSON is longer than [`MAX_RESULT_BYTES`], [`Error::Encode`] or [`Error::Decode`] if it does
    /// not go through JSON and back. At most [`MAX_RUNNING_JOBS`] jobs run at once; the others
    /// wait their turn. A
    /// result that arrives after the server stopped is dropped, and work still running 10
    /// seconds after the stop is abandoned. What the work wrote outside the world file stays
    /// after a crash (PLAN.md 6). Fails, running nothing, while [`MAX_JOBS`] jobs wait for their
    /// result.
    #[doc(alias = "AsyncPool::submitTask")]
    #[doc(alias = "AsyncTask")]
    #[doc(alias = "AsyncTask::onRun")]
    #[doc(alias = "AsyncTask::onCompletion")]
    pub fn spawn_blocking<T, W, D>(&mut self, work: W, done: D) -> Result<()>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        W: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Context<'_>, Result<T>) + Send + 'static,
    {
        let jobs = &mut self.state.tasks.jobs;
        if jobs.pending.len() >= MAX_JOBS {
            return Err(Error::TooManyJobs);
        }
        let id = jobs.next_id;
        // 2^64 jobs never start; refuse rather than reuse an id.
        jobs.next_id = id.checked_add(1).ok_or(Error::TooManyJobs)?;
        let finish: Done = Box::new(move |cx, outcome| {
            let result = outcome
                .and_then(|json| serde_json::from_slice::<T>(json).map_err(|_| Error::Decode));
            done(cx, result);
        });
        jobs.pending.insert(id, finish);
        if let Some(runner) = &jobs.runner {
            runner.run(id, work);
        }
        Ok(())
    }
}

/// The result of job `id` arrived: its callback runs now. A result of no waiting job (one that
/// was never started in this run) is ignored.
pub(in crate::tick) fn finish(state: &mut State, id: u64, outcome: &JobOutcome) {
    let Some(done) = state.tasks.jobs.pending.remove(&id) else {
        debug!(job = id, "the result of an unknown job is ignored");
        return;
    };
    done(&mut Context { state }, outcome.as_deref().map_err(|e| *e));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::super::super::queue::{Input, InputKind, SessionId, input_queue};
    use super::super::super::tests::{new_state, settings, tick};
    use super::super::super::{InputLog, Output, deliver, step};
    use super::*;
    use crate::event::Listeners;
    use crate::runtime::{Runtime, ThreadCounts};

    /// What a job's callback saw, shared with the callback.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: tests share results with 'static callbacks"
    )]
    type Seen<T> = Arc<std::sync::Mutex<Vec<(u64, Result<T>)>>>;

    fn runtime() -> Runtime {
        Runtime::new(ThreadCounts { network: 1, cpu: 1 }).unwrap()
    }

    /// A state whose jobs run on `runtime`, and the input queue they answer into.
    fn live(runtime: &Runtime) -> (State, mpsc::Receiver<Input>) {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let (inputs, queue) = input_queue();
        state.tasks.start(runtime.handle(), &inputs);
        (state, queue)
    }

    /// Waits for `n` job results on `queue`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test waits for the blocking pool"
    )]
    fn results(queue: &mut mpsc::Receiver<Input>, n: usize) -> Vec<Input> {
        let mut out = Vec::new();
        while out.len() < n {
            match queue.try_recv() {
                Ok(input) => out.push(input),
                Err(_) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        out
    }

    fn note<T: Send + 'static>(
        seen: &Seen<T>,
    ) -> impl FnOnce(&mut Context<'_>, Result<T>) + Send + 'static {
        let seen = Arc::clone(seen);
        move |cx, result| seen.lock().unwrap().push((cx.tick(), result))
    }

    // pmmp: AsyncPool::submitTask, AsyncTask, AsyncTask::onRun, AsyncTask::onCompletion
    #[test]
    fn blocking_work_runs_off_tick_and_its_result_reaches_done_in_a_later_tick() {
        let runtime = runtime();
        let (mut state, mut queue) = live(&runtime);
        let seen: Seen<(String, u32)> = Arc::default();
        let tick_thread = std::thread::current().id();
        let mut cx = Context { state: &mut state };
        cx.spawn_blocking(
            move || {
                assert_ne!(std::thread::current().id(), tick_thread);
                ("off tick".to_owned(), 7)
            },
            note(&seen),
        )
        .unwrap();
        assert!(seen.lock().unwrap().is_empty(), "done waits for a tick");
        let mut batch = results(&mut queue, 1);
        assert_eq!(batch[0].session, SessionId::JOBS);
        assert_eq!(batch[0].seq, 0);
        state.tick = 5;
        tick(&mut state, &mut batch, &mut Output::default());
        assert_eq!(*seen.lock().unwrap(), [(5, Ok(("off tick".to_owned(), 7)))]);
        assert_eq!(state.tasks.jobs.len(), 0);
        runtime.shutdown();
    }

    #[test]
    fn a_panic_an_oversized_result_or_a_type_that_does_not_round_trip_reach_done_as_errors() {
        let runtime = runtime();
        let (mut state, mut queue) = live(&runtime);
        let panicked: Seen<u8> = Arc::default();
        let large: Seen<Vec<u8>> = Arc::default();
        let fine: Seen<Vec<u8>> = Arc::default();
        let map: Seen<std::collections::BTreeMap<(u8, u8), u8>> = Arc::default();
        let mut cx = Context { state: &mut state };
        cx.spawn_blocking(|| -> u8 { panic!("user work failed") }, note(&panicked))
            .unwrap();
        // Each byte is at least two bytes of JSON ("0,").
        cx.spawn_blocking(|| vec![0_u8; MAX_RESULT_BYTES / 2], note(&large))
            .unwrap();
        cx.spawn_blocking(|| vec![1_u8; 1000], note(&fine)).unwrap();
        cx.spawn_blocking(
            || std::collections::BTreeMap::from([((1, 2), 3)]),
            note(&map),
        )
        .unwrap();
        let mut batch = results(&mut queue, 4);
        tick(&mut state, &mut batch, &mut Output::default());
        assert_eq!(*panicked.lock().unwrap(), [(0, Err(Error::Panicked))]);
        assert_eq!(*large.lock().unwrap(), [(0, Err(Error::TooLarge))]);
        assert_eq!(*fine.lock().unwrap(), [(0, Ok(vec![1; 1000]))]);
        assert_eq!(*map.lock().unwrap(), [(0, Err(Error::Encode))]);
        // A result whose JSON does not fit its type.
        let wrong: Seen<String> = Arc::default();
        Context { state: &mut state }
            .spawn_blocking(String::new, note(&wrong))
            .unwrap();
        drop(results(&mut queue, 1));
        let mut batch = vec![Input {
            session: SessionId::JOBS,
            seq: 4,
            kind: InputKind::Job(Ok(b"12".to_vec())),
        }];
        tick(&mut state, &mut batch, &mut Output::default());
        assert_eq!(*wrong.lock().unwrap(), [(1, Err(Error::Decode))]);
        runtime.shutdown();
    }

    #[test]
    fn outstanding_jobs_are_capped_and_return_to_zero() {
        // No runner: the work never runs, the jobs only wait (as in a replay).
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let start = state.hash();
        let mut cx = Context { state: &mut state };
        for _ in 0..MAX_JOBS {
            cx.spawn_blocking(|| 1_u8, |_, _| {}).unwrap();
        }
        assert_eq!(
            cx.spawn_blocking(|| 1_u8, |_, _| {}),
            Err(Error::TooManyJobs)
        );
        assert_ne!(state.hash(), start);
        for id in 0..u64::try_from(MAX_JOBS).unwrap() {
            finish(&mut state, id, &Ok(b"1".to_vec()));
        }
        // An unknown or repeated result changes nothing.
        finish(&mut state, 3, &Ok(b"1".to_vec()));
        assert_eq!(state.tasks.jobs.len(), 0);
        state.tasks.jobs.next_id = 0;
        assert_eq!(state.hash(), start);
    }

    /// What a scripted run gives: the hashes, what the callbacks saw, the log and how many works
    /// ran.
    type Run = (Vec<u64>, Vec<(u64, Result<u32>)>, InputLog, usize);

    /// Runs the ticks of `script`: tick `t` starts the jobs `script[t]` names (by a value each)
    /// and applies `results[t]`. Returns the hashes, what the callbacks saw and the log.
    fn scripted(
        state: &mut State,
        script: &[Vec<u32>],
        mut results: impl FnMut(&mut State, usize) -> Vec<Input>,
    ) -> Run {
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let seen: Seen<u32> = Arc::default();
        let ran: Seen<()> = Arc::default();
        let mut log = InputLog::default();
        let mut output = Output::default();
        let mut hashes = Vec::new();
        for (t, starts) in script.iter().enumerate() {
            let mut cx = Context { state: &mut *state };
            for &n in starts {
                let ran = Arc::clone(&ran);
                let work = move || {
                    ran.lock().unwrap().push((0, Ok(())));
                    n * 10
                };
                cx.spawn_blocking(work, note(&seen)).unwrap();
            }
            let mut batch = results(state, t);
            step(
                state,
                &mut batch,
                &mut Vec::new(),
                &pool,
                &mut output,
                &mut log,
            );
            deliver(state, &mut output, &mut log);
            hashes.push(state.hash());
        }
        assert!(pool.shutdown());
        let seen = std::mem::take(&mut *seen.lock().unwrap());
        let ran = ran.lock().unwrap().len();
        (hashes, seen, log, ran)
    }

    #[test]
    fn a_replay_feeds_the_logged_results_without_running_the_work_and_gives_the_same_hashes() {
        let runtime = runtime();
        let (mut state, mut queue) = live(&runtime);
        let script = vec![vec![1, 2], vec![], vec![3], vec![], vec![], vec![]];
        // Live: whatever results have arrived by each tick are applied there.
        let (live_hashes, live_seen, log, ran) = scripted(&mut state, &script, |_, t| {
            let want = match t {
                1 => 2,
                4 => 1,
                _ => 0,
            };
            results(&mut queue, want)
        });
        assert_eq!((live_seen.len(), ran), (3, 3));
        assert!(live_seen.iter().all(|(_, r)| r.is_ok()));
        runtime.shutdown();
        // The written log reads back, then replays with no runner: the work is not run.
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        let ticks = read.ticks();
        let mut replayed = new_state(settings(|_| {}), Listeners::default());
        let (replay_hashes, replay_seen, again, ran) = scripted(&mut replayed, &script, |_, t| {
            ticks[t]
                .inputs
                .iter()
                .map(|e| match &e.kind {
                    super::super::super::log::EntryKind::Job(outcome) => Input {
                        session: e.session,
                        seq: e.seq,
                        kind: InputKind::Job(outcome.clone()),
                    },
                    other => panic!("{other:?}"),
                })
                .collect()
        });
        assert_eq!(ran, 0, "a replay runs no work");
        assert_eq!(replay_hashes, live_hashes);
        assert_eq!(replay_seen, live_seen);
        assert_eq!(again.encode().unwrap(), log.encode().unwrap());
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test waits for the blocking pool"
    )]
    fn results_of_a_stopped_tick_are_dropped() {
        let runtime = runtime();
        let (mut state, queue) = live(&runtime);
        drop(queue);
        let mut cx = Context { state: &mut state };
        cx.spawn_blocking(|| 5_u8, |_, _| panic!("the tick stopped"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        runtime.shutdown();
        assert_eq!(state.tasks.jobs.len(), 1, "nothing arrived");
    }
}
