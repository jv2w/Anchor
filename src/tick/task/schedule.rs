//! Tasks scheduled while the server runs: run once after a delay or again
//! every period, by due tick and then in the order they were scheduled.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use super::Context;
use crate::task::{Error, Handle, MAX_TASKS, Result};
use crate::tick::State;
use crate::tick::checkpoint::{self, Codec, Reader, Writer};

/// The scheduled tasks. `order` and `tasks` hold the same tasks; the running task is in neither
/// while it runs.
#[derive(Default)]
pub(super) struct Queue {
    /// (due tick, id) of every scheduled task: the run order.
    order: BTreeSet<(u64, u64)>,
    /// Every scheduled task by id.
    tasks: BTreeMap<u64, Entry>,
    /// The id of the next task; ids are never reused.
    next_id: u64,
    running: Option<Running>,
}

impl Queue {
    /// Writes the next id. A scheduled task is a closure, with no checkpoint form: it makes the
    /// checkpoint opaque. Nothing runs between ticks.
    pub(super) fn checkpoint(&self, w: &mut Writer) {
        let Self {
            order: _,
            tasks,
            next_id,
            running: _,
        } = self;
        w.opaque("scheduled tasks", tasks.len());
        Codec::encode(next_id, w);
    }

    /// Hashes the state kept between ticks (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            order,
            tasks,
            next_id,
            running: _,
        } = self;
        // Bodies are code; when and how often each task runs is state. Nothing runs between
        // ticks.
        h.u64(*next_id);
        h.usize(order.len());
        for &(due, id) in order {
            h.u64(due);
            h.u64(id);
            let period = match tasks.get(&id) {
                Some(entry) => entry.body.period().map_or(0, |p| u64::from(p.get())),
                None => u64::MAX,
            };
            h.u64(period);
        }
    }

    /// Whether nothing is scheduled or running (for leak tests).
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty() && self.tasks.is_empty() && self.running.is_none()
    }

    /// How many tasks are scheduled, the running one left out.
    pub(super) fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Reads what [`Self::checkpoint`] wrote: no task, and the next id.
    pub(super) fn restore(&mut self, r: &mut Reader<'_>) -> checkpoint::Result<()> {
        *self = Self {
            next_id: Codec::decode(r)?,
            ..Self::default()
        };
        Ok(())
    }

    /// Whether task `id` will run again.
    fn is_scheduled(&self, id: u64) -> bool {
        self.tasks.contains_key(&id) || self.running.is_some_and(|r| r.id == id && r.can_repeat())
    }

    /// How many tasks are scheduled, the running one included if it runs again.
    fn live(&self) -> usize {
        let running = self.running.is_some_and(Running::can_repeat);
        self.tasks.len().saturating_add(usize::from(running))
    }

    /// The tick task `id` runs next and its period if it repeats; `None` if it is not
    /// scheduled. The running task runs next a period after `now`.
    fn next_run(&self, id: u64, now: u64) -> Option<(u64, Option<NonZeroU32>)> {
        if let Some(entry) = self.tasks.get(&id) {
            return Some((entry.due, entry.body.period()));
        }
        let running = self.running.filter(|r| r.id == id && r.can_repeat())?;
        let period = running.period?;
        Some((now.saturating_add(u64::from(period.get())), Some(period)))
    }

    /// Cancels task `id`; false if it will not run again anyway.
    fn cancel(&mut self, id: u64) -> bool {
        if let Some(entry) = self.tasks.remove(&id) {
            self.order.remove(&(entry.due, id));
            return true;
        }
        match &mut self.running {
            Some(r) if r.id == id && r.can_repeat() => {
                r.cancelled = true;
                true
            }
            _ => false,
        }
    }

    /// Cancels every task, the running one included.
    fn cancel_all(&mut self) {
        self.tasks.clear();
        self.order.clear();
        if let Some(r) = &mut self.running {
            r.cancelled = true;
        }
    }

    /// Schedules `body` to run first at tick `due`.
    fn push(&mut self, due: u64, body: Body) -> Result<Handle> {
        if self.live() >= MAX_TASKS {
            return Err(Error::TooManyTasks);
        }
        let id = self.next_id;
        // 2^64 tasks never get scheduled; refuse rather than reuse an id.
        self.next_id = id.checked_add(1).ok_or(Error::TooManyTasks)?;
        self.order.insert((due, id));
        self.tasks.insert(id, Entry { due, body });
        Ok(Handle::new(id))
    }
}

/// A scheduled task waiting for its tick.
struct Entry {
    due: u64,
    body: Body,
}

/// What a scheduled task runs.
enum Body {
    Once(Once),
    Repeat { period: NonZeroU32, body: Repeat },
}

impl Body {
    /// The period of a repeating task; `None` for a task that runs once.
    fn period(&self) -> Option<NonZeroU32> {
        match self {
            Self::Once(_) => None,
            Self::Repeat { period, .. } => Some(*period),
        }
    }
}

/// A task that runs once.
type Once = Box<dyn FnOnce(&mut Context<'_>) + Send>;
/// A task that runs every period.
type Repeat = Box<dyn FnMut(&mut Context<'_>) + Send>;

/// The task running now.
#[derive(Debug, Clone, Copy)]
struct Running {
    id: u64,
    /// The period of a repeating task; `None` for a task that runs once.
    period: Option<NonZeroU32>,
    /// Cancelled while it ran: it does not run again.
    cancelled: bool,
}

impl Running {
    /// Whether the running task runs again: it repeats and was not cancelled.
    fn can_repeat(self) -> bool {
        self.period.is_some() && !self.cancelled
    }
}

impl Context<'_> {
    /// The scheduled task that is running now, if a scheduled task (and not a listener or a
    /// build-time task) called this: a repeating task cancels itself with it.
    #[doc(alias = "Task::getHandler")]
    pub fn current_task(&self) -> Option<Handle> {
        self.state.tasks.queue.running.map(|r| Handle::new(r.id))
    }

    /// Whether `task` will run again.
    #[doc(alias = "TaskScheduler::isQueued")]
    pub fn is_task_scheduled(&self, task: Handle) -> bool {
        self.state.tasks.queue.is_scheduled(task.get())
    }

    /// The engine tick `task` runs next (its first run, if it has not run yet); `None` if it
    /// will not run again. A delay is the distance from now to it.
    #[doc(alias = "TaskHandler::getNextRun")]
    pub fn task_next_run(&self, task: Handle) -> Option<u64> {
        let queue = &self.state.tasks.queue;
        queue
            .next_run(task.get(), self.state.tick)
            .map(|(due, _)| due)
    }

    /// The period of `task` in ticks if it repeats and will run again; `None` for a task that runs
    /// once or will not run again.
    #[doc(alias = "TaskHandler::getPeriod")]
    #[doc(alias = "TaskHandler::isRepeating")]
    pub fn task_period_ticks(&self, task: Handle) -> Option<NonZeroU32> {
        let queue = &self.state.tasks.queue;
        queue
            .next_run(task.get(), self.state.tick)
            .and_then(|(_, period)| period)
    }

    /// Cancels every scheduled task (not the build-time tasks of
    /// [`ServerBuilder::every`](crate::ServerBuilder::every)).
    #[doc(alias = "TaskScheduler::cancelAllTasks")]
    pub fn cancel_all_tasks(&mut self) {
        self.state.tasks.queue.cancel_all();
    }

    /// Cancels `task`: it never runs again (a task running now finishes this run). False if it
    /// was not scheduled: it ran for the last time, was cancelled, or never existed.
    #[doc(alias = "TaskHandler::cancel")]
    pub fn cancel_task(&mut self, task: Handle) -> bool {
        self.state.tasks.queue.cancel(task.get())
    }

    /// Runs `task` once, in the tick's task step `delay_ticks` ticks from now. With 0 it runs in
    /// this tick's step if the step's scheduled tasks have not started yet (a call from an input's
    /// listener or a build-time task), else in the next tick's. Returns its handle, to cancel it.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyTasks`] for a call while [`MAX_TASKS`] tasks are scheduled.
    #[doc(alias = "TaskScheduler::scheduleTask")]
    #[doc(alias = "TaskScheduler::scheduleDelayedTask")]
    #[doc(alias = "ClosureTask")]
    pub fn schedule(
        &mut self,
        delay_ticks: u32,
        task: impl FnOnce(&mut Context<'_>) + Send + 'static,
    ) -> Result<Handle> {
        let due = self.state.tick.saturating_add(u64::from(delay_ticks));
        self.state.tasks.queue.push(due, Body::Once(Box::new(task)))
    }

    /// Runs `task` from this tick's task step on (as [`Self::schedule`] runs a task with no
    /// delay), then every `period_ticks` ticks until it is cancelled ([`Self::cancel_task`]; the
    /// task itself can use [`Self::current_task`]). This is the runtime way, counted from now and
    /// cancellable; [`ServerBuilder::every`](crate::ServerBuilder::every) is the build-time one,
    /// fixed for the server's life and run on the ticks that are multiples of its period.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyTasks`] for a call while [`MAX_TASKS`] tasks are scheduled.
    #[doc(alias = "TaskScheduler::scheduleRepeatingTask")]
    pub fn schedule_repeating(
        &mut self,
        period_ticks: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) -> Result<Handle> {
        self.schedule_delayed_repeating(0, period_ticks, task)
    }

    /// Runs `task` as [`Self::schedule_repeating`] does, first `delay_ticks` ticks from now.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyTasks`] for a call while [`MAX_TASKS`] tasks are scheduled.
    #[doc(alias = "TaskScheduler::scheduleDelayedRepeatingTask")]
    pub fn schedule_delayed_repeating(
        &mut self,
        delay_ticks: u32,
        period_ticks: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) -> Result<Handle> {
        let due = self.state.tick.saturating_add(u64::from(delay_ticks));
        let body = Body::Repeat {
            period: period_ticks,
            body: Box::new(task),
        };
        self.state.tasks.queue.push(due, body)
    }
}

/// Runs the scheduled tasks due at the state's tick, by due tick then id. Tasks scheduled while
/// these run have larger ids and wait for the next tick, so a task that keeps scheduling itself
/// cannot hold the tick. Each task's body is taken out while it runs, so its [`Context`] lends
/// the whole state, the queue included.
pub(super) fn run(state: &mut State) {
    let now = state.tick;
    let fence = state.tasks.queue.next_id;
    loop {
        let queue = &mut state.tasks.queue;
        let Some(&(due, id)) = queue.order.first() else {
            return;
        };
        if due > now || id >= fence {
            return;
        }
        queue.order.pop_first();
        #[expect(
            clippy::expect_used,
            reason = "invariant: `order` and `tasks` hold the same tasks"
        )]
        let entry = queue
            .tasks
            .remove(&id)
            .expect("every ordered task is in the table");
        queue.running = Some(Running {
            id,
            period: entry.body.period(),
            cancelled: false,
        });
        match entry.body {
            Body::Once(body) => {
                body(&mut Context { state });
                state.tasks.queue.running = None;
            }
            Body::Repeat { period, mut body } => {
                body(&mut Context { state });
                let queue = &mut state.tasks.queue;
                let cancelled = queue.running.take().is_none_or(|r| r.cancelled);
                if !cancelled {
                    let due = now.saturating_add(u64::from(period.get()));
                    queue.order.insert((due, id));
                    queue.tasks.insert(
                        id,
                        Entry {
                            due,
                            body: Body::Repeat { period, body },
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::event::Listeners;
    use crate::tick::Tasks;
    use crate::tick::tests::{new_state, settings};

    /// What the tasks of a test saw: (tick, name), shared with the task bodies.
    #[derive(Clone, Default)]
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: tests share a log with 'static task bodies"
    )]
    struct Seen(Arc<std::sync::Mutex<Vec<(u64, String)>>>);

    impl Seen {
        fn note(&self, cx: &Context<'_>, name: &str) {
            self.0.lock().unwrap().push((cx.tick(), name.to_owned()));
        }

        fn take(&self) -> Vec<(u64, String)> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn state() -> State {
        new_state(settings(|_| {}), Listeners::default())
    }

    /// Runs the task step of ticks `from..to`.
    fn run_ticks(state: &mut State, from: u64, to: u64) {
        for t in from..to {
            state.tick = t;
            Tasks::run(state);
        }
    }

    fn named(seen: &Seen, name: &str) -> impl FnOnce(&mut Context<'_>) + Send + 'static {
        let (seen, name) = (seen.clone(), name.to_owned());
        move |cx| seen.note(cx, &name)
    }

    // api: TaskScheduler::scheduleTask, TaskScheduler::scheduleDelayedTask, ClosureTask
    // api: TaskHandler
    #[test]
    fn delayed_tasks_run_once_by_due_tick_then_in_scheduling_order() {
        let mut state = state();
        let seen = Seen::default();
        let mut cx = Context { state: &mut state };
        let late = cx.schedule(3, named(&seen, "late")).unwrap();
        let now = cx.schedule(0, named(&seen, "now")).unwrap();
        cx.schedule(1, named(&seen, "next a")).unwrap();
        cx.schedule(1, named(&seen, "next b")).unwrap();
        assert!(now > late, "handles count up in scheduling order");
        assert!(cx.is_task_scheduled(late));
        run_ticks(&mut state, 0, 6);
        let names: Vec<(u64, String)> = [(0, "now"), (1, "next a"), (1, "next b"), (3, "late")]
            .map(|(t, n)| (t, n.to_owned()))
            .to_vec();
        assert_eq!(seen.take(), names);
        let cx = Context { state: &mut state };
        assert!(!cx.is_task_scheduled(late), "a delayed task runs once");
        assert!(state.tasks.queue.is_empty());
    }

    // api: TaskHandler::getNextRun, TaskHandler::getPeriod, TaskHandler::isRepeating
    #[test]
    fn a_handle_tells_the_next_run_and_the_period_while_scheduled() {
        let mut state = state();
        state.tick = 10;
        let mut cx = Context { state: &mut state };
        let once = cx.schedule(5, |_| {}).unwrap();
        let three = NonZeroU32::new(3).unwrap();
        let every = cx.schedule_delayed_repeating(2, three, |_| {}).unwrap();
        assert_eq!(
            (cx.task_next_run(once), cx.task_period_ticks(once)),
            (Some(15), None)
        );
        assert_eq!(
            (cx.task_next_run(every), cx.task_period_ticks(every)),
            (Some(12), Some(three))
        );
        // While it runs, a repeating task's next run is a period away.
        let me = cx
            .schedule(0, |cx| {
                let me = cx.current_task().unwrap();
                assert_eq!(cx.task_next_run(me), None, "a task that runs once");
            })
            .unwrap();
        let seen = cx
            .schedule_repeating(three, |cx| {
                let me = cx.current_task().unwrap();
                assert_eq!(cx.task_next_run(me), Some(cx.tick() + 3));
                assert_eq!(cx.task_period_ticks(me), Some(NonZeroU32::new(3).unwrap()));
            })
            .unwrap();
        run_ticks(&mut state, 10, 16);
        let mut cx = Context { state: &mut state };
        assert_eq!(cx.task_next_run(me), None);
        assert_eq!(cx.task_next_run(once), None, "it ran");
        assert_eq!(cx.task_next_run(every), Some(18));
        assert!(cx.cancel_task(seen) && cx.cancel_task(every));
        assert_eq!(
            (cx.task_next_run(every), cx.task_period_ticks(every)),
            (None, None)
        );
    }

    // api: TaskScheduler::scheduleRepeatingTask, TaskScheduler::scheduleDelayedRepeatingTask
    // api: TaskHandler::cancel, Task::getHandler, TaskScheduler::isQueued
    #[test]
    fn repeating_tasks_run_every_period_until_cancelled_and_can_cancel_themselves() {
        let mut state = state();
        let seen = Seen::default();
        let mut cx = Context { state: &mut state };
        let every_two = {
            let seen = seen.clone();
            cx.schedule_repeating(NonZeroU32::new(2).unwrap(), move |cx| {
                seen.note(cx, "two");
            })
            .unwrap()
        };
        let three_times = {
            let seen = seen.clone();
            let mut runs = 0;
            cx.schedule_delayed_repeating(1, NonZeroU32::new(3).unwrap(), move |cx| {
                seen.note(cx, "three");
                runs += 1;
                if runs == 3 {
                    let me = cx.current_task().unwrap();
                    assert!(cx.is_task_scheduled(me));
                    assert!(cx.cancel_task(me));
                    assert!(!cx.is_task_scheduled(me));
                    assert!(!cx.cancel_task(me), "cancelled once");
                }
            })
            .unwrap()
        };
        assert_eq!(
            cx.current_task(),
            None,
            "no task runs outside the task step"
        );
        run_ticks(&mut state, 0, 9);
        let mut cx = Context { state: &mut state };
        assert!(cx.is_task_scheduled(every_two));
        assert!(!cx.is_task_scheduled(three_times));
        assert!(cx.cancel_task(every_two));
        assert!(!cx.cancel_task(every_two));
        run_ticks(&mut state, 9, 20);
        let got = seen.take();
        let ticks = |name: &str| -> Vec<u64> {
            got.iter()
                .filter(|(_, n)| n == name)
                .map(|(t, _)| *t)
                .collect()
        };
        assert_eq!(ticks("two"), [0, 2, 4, 6, 8]);
        assert_eq!(ticks("three"), [1, 4, 7]);
        assert!(state.tasks.queue.is_empty());
    }

    #[test]
    fn a_task_cancelled_by_an_earlier_task_of_the_same_tick_never_runs() {
        let mut state = state();
        let seen = Seen::default();
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: the test hands a handle to a 'static task body"
        )]
        let victim: Arc<std::sync::Mutex<Option<Handle>>> = Arc::default();
        let mut cx = Context { state: &mut state };
        // Same due tick: the canceller runs first, being scheduled first.
        let target = Arc::clone(&victim);
        let canceller = seen.clone();
        cx.schedule(2, move |cx| {
            canceller.note(cx, "canceller");
            let handle = target.lock().unwrap().unwrap();
            assert!(cx.cancel_task(handle));
        })
        .unwrap();
        let handle = cx.schedule(2, named(&seen, "victim")).unwrap();
        *victim.lock().unwrap() = Some(handle);
        // A repeating victim is cancelled for good as well.
        let repeating = cx
            .schedule_delayed_repeating(4, NonZeroU32::MIN, |_| panic!("cancelled"))
            .unwrap();
        cx.schedule(3, move |cx| assert!(cx.cancel_task(repeating)))
            .unwrap();
        run_ticks(&mut state, 0, 8);
        assert_eq!(seen.take(), [(2, "canceller".to_owned())]);
        assert!(state.tasks.queue.is_empty());
    }

    #[test]
    fn a_task_scheduled_while_tasks_run_waits_for_the_next_tick() {
        // A task that schedules itself each run runs once per tick.
        fn forever(cx: &mut Context<'_>) {
            cx.schedule(0, forever).unwrap();
        }
        let mut state = state();
        let seen = Seen::default();
        let mut cx = Context { state: &mut state };
        let inner = seen.clone();
        cx.schedule(0, move |cx| {
            inner.note(cx, "outer");
            let again = inner.clone();
            cx.schedule(0, move |cx| again.note(cx, "inner")).unwrap();
        })
        .unwrap();
        cx.schedule(0, forever).unwrap();
        run_ticks(&mut state, 0, 2);
        assert_eq!(
            seen.take(),
            [(0, "outer".to_owned()), (1, "inner".to_owned())]
        );
        assert_eq!(state.tasks.queue.len(), 1, "forever waits for tick 2");
    }

    // api: TaskScheduler::cancelAllTasks
    #[test]
    fn scheduling_past_the_limit_fails_and_cancel_all_empties_the_queue() {
        fn cancel_all(cx: &mut Context<'_>) {
            cx.cancel_all_tasks();
        }
        let mut state = state();
        let mut cx = Context { state: &mut state };
        for i in 0..MAX_TASKS {
            cx.schedule(u32::try_from(i % 7).unwrap(), |_| {}).unwrap();
        }
        assert_eq!(cx.schedule(0, |_| {}).err(), Some(Error::TooManyTasks));
        let period = NonZeroU32::MIN;
        assert_eq!(
            cx.schedule_repeating(period, |_| {}).err(),
            Some(Error::TooManyTasks)
        );
        cx.cancel_all_tasks();
        assert!(state.tasks.queue.is_empty());
        // A repeating task that cancels everything, itself included, does not come back.
        let mut cx = Context { state: &mut state };
        let all = cx.schedule_repeating(period, cancel_all).unwrap();
        cx.schedule(0, |_| panic!("cancelled before it ran"))
            .unwrap();
        run_ticks(&mut state, 0, 3);
        assert!(!Context { state: &mut state }.is_task_scheduled(all));
        assert!(state.tasks.queue.is_empty());
    }

    #[test]
    fn schedule_and_cancel_cycles_return_the_tables_to_their_start() {
        let mut state = state();
        let start = state.hash();
        for round in 0..1000_u32 {
            let mut cx = Context { state: &mut state };
            let a = cx.schedule(round % 5, |_| {}).unwrap();
            let b = cx
                .schedule_repeating(NonZeroU32::new(round % 3 + 1).unwrap(), |_| {})
                .unwrap();
            if round % 2 == 0 {
                assert!(cx.cancel_task(a));
            }
            assert!(cx.cancel_task(b));
            run_ticks(&mut state, u64::from(round) * 8, u64::from(round) * 8 + 8);
        }
        assert!(state.tasks.queue.is_empty());
        state.tick = 0;
        // Only the id counter remembers the cycles.
        assert_ne!(state.hash(), start);
        state.tasks.queue.next_id = 0;
        assert_eq!(state.hash(), start);
    }

    #[test]
    fn the_state_hash_covers_when_and_how_often_tasks_run() {
        let hash_of = |delay: u32, period: Option<u32>| {
            let mut state = state();
            let mut cx = Context { state: &mut state };
            match period {
                Some(p) => drop(
                    cx.schedule_delayed_repeating(delay, NonZeroU32::new(p).unwrap(), |_| {})
                        .unwrap(),
                ),
                None => drop(cx.schedule(delay, |_| {}).unwrap()),
            }
            state.hash()
        };
        assert_eq!(hash_of(3, None), hash_of(3, None));
        assert_ne!(hash_of(3, None), hash_of(4, None));
        assert_ne!(hash_of(3, None), hash_of(3, Some(1)));
        assert_ne!(hash_of(3, Some(1)), hash_of(3, Some(2)));
    }
}
