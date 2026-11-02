//! Scheduled tasks and blocking work: the handles and errors of
//! [`Context::schedule`](crate::Context::schedule) and
//! [`Context::spawn_blocking`](crate::Context::spawn_blocking).
//!
//! Tasks run in the tick's serial step, after the tick's inputs: first the build-time tasks of
//! [`ServerBuilder::every`](crate::ServerBuilder::every) in registration order, then every
//! scheduled task that is due, by due tick and then in the order they were scheduled. A task
//! scheduled while the scheduled tasks run waits for the next tick at the earliest, so a task
//! that keeps scheduling itself cannot hold the tick; one a build-time task schedules with no
//! delay runs in the same tick. Scheduled tasks are code, so they are not saved: a
//! restart starts with none (as in PMMP).
//!
//! Blocking work runs off the tick on a bounded pool and never touches the game; its result comes
//! back as an input of a later tick, is recorded in the input log, and reaches its completion
//! callback in the tick's input step. A replay of the log hands the recorded result to the
//! callback instead of running the work again.

use std::fmt;

/// The most blocking jobs waiting for their result at once.
pub const MAX_JOBS: usize = 1024;
/// The largest result of a blocking job, in bytes of its JSON form: it travels through the input
/// log.
pub const MAX_RESULT_BYTES: usize = 1 << 20;
/// The most blocking jobs running at once; the others wait their turn, so the user's jobs never
/// hold every blocking thread.
pub const MAX_RUNNING_JOBS: usize = crate::runtime::USER_BLOCKING_THREADS;
/// The most tasks scheduled at once (no unbounded queue).
pub const MAX_TASKS: usize = 65_536;

/// A scheduled task, to cancel it or ask whether it is still scheduled. Ids are never reused, so
/// a handle kept after its task ended names nothing.
#[doc(alias = "TaskHandler")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(u64);

impl Handle {
    /// The task numbered `id`.
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The task's number: tasks are numbered in the order they were scheduled.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Why a task or blocking job failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// [`MAX_TASKS`] tasks are scheduled already; nothing was scheduled.
    TooManyTasks,
    /// [`MAX_JOBS`] blocking jobs wait for their result already; nothing was started.
    TooManyJobs,
    /// The blocking work panicked; the server goes on.
    Panicked,
    /// The result's JSON form is larger than [`MAX_RESULT_BYTES`].
    TooLarge,
    /// The result does not serialize to JSON.
    Encode,
    /// The result's JSON does not deserialize back into its type.
    Decode,
}

impl Error {
    /// The error a logged byte names.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::TooManyTasks,
            2 => Self::TooManyJobs,
            3 => Self::Panicked,
            4 => Self::TooLarge,
            5 => Self::Encode,
            6 => Self::Decode,
            _ => return None,
        })
    }

    /// The byte the input log writes for a job that failed this way.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::TooManyTasks => 1,
            Self::TooManyJobs => 2,
            Self::Panicked => 3,
            Self::TooLarge => 4,
            Self::Encode => 5,
            Self::Decode => 6,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyTasks => write!(f, "{MAX_TASKS} tasks are scheduled already"),
            Self::TooManyJobs => {
                write!(f, "{MAX_JOBS} blocking jobs wait for their result already")
            }
            Self::Panicked => f.write_str("the blocking work panicked"),
            Self::TooLarge => write!(f, "the result is larger than {MAX_RESULT_BYTES} bytes"),
            Self::Encode => f.write_str("the result does not serialize"),
            Self::Decode => f.write_str("the result does not deserialize"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the task [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_its_own_log_code() {
        let all = [
            Error::TooManyTasks,
            Error::TooManyJobs,
            Error::Panicked,
            Error::TooLarge,
            Error::Encode,
            Error::Decode,
        ];
        for e in all {
            assert_eq!(Error::from_code(e.code()), Some(e));
            assert!(!e.to_string().ends_with('.'));
        }
        assert_eq!(Error::from_code(0), None);
        assert_eq!(Error::from_code(7), None);
    }

    #[test]
    fn too_many_jobs_counts_the_jobs_waiting_for_their_result() {
        assert_eq!(
            Error::TooManyJobs.to_string(),
            format!("{MAX_JOBS} blocking jobs wait for their result already")
        );
    }
}
