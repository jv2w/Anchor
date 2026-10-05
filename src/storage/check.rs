//! The check of one data file while the server runs (`/check`): every value of one read transaction
//! of the open file, read in steps of at most [`STEP_BYTES`] between the save thread's other
//! work, and checked against its seal (header and checksum, [`seal::verify`]). redb's own
//! integrity check needs the file closed, so it runs only at start (`integrity-check`); this
//! reads every page that holds a value and finds what redb reports damaged on the way.

use redb::Database;

use super::backup::{self, STEP_BYTES, Source};
use super::{Error, Result, seal};

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Checked {
    /// Every value read back whole: `values` of them.
    Passed { values: u64 },
    /// `damaged` of the `values` read are damaged; `first` tells the first one's table and fault.
    Damaged {
        values: u64,
        damaged: u64,
        first: String,
    },
    /// The check could not finish: why.
    Failed(String),
}

impl Checked {
    /// What a check of several files found: this, then `next`, the result of the file `file`.
    /// The first failure or damage is the one told.
    pub(crate) fn and(self, next: Self, file: &std::path::Path) -> Self {
        let (values, damaged, first) = match self {
            Self::Failed(_) => return self,
            Self::Passed { values } => (values, 0, None),
            Self::Damaged {
                values,
                damaged,
                first,
            } => (values, damaged, Some(first)),
        };
        let (more, more_damaged, next_first) = match next {
            Self::Failed(why) => return Self::Failed(format!("{}: {why}", file.display())),
            Self::Passed { values } => (values, 0, None),
            Self::Damaged {
                values,
                damaged,
                first,
            } => (
                values,
                damaged,
                Some(format!("{}: {first}", file.display())),
            ),
        };
        let values = values.saturating_add(more);
        match first.or(next_first) {
            Some(first) => Self::Damaged {
                values,
                damaged: damaged.saturating_add(more_damaged),
                first,
            },
            None => Self::Passed { values },
        }
    }
}

/// A check being made: the tables left of one read transaction, and what it found so far.
pub(crate) struct Check {
    /// The tables left to read, the next last.
    tables: Vec<Source>,
    values: u64,
    damaged: u64,
    /// The first damage's table and fault.
    first: Option<String>,
    /// Why the check ended early.
    failure: Option<String>,
}

impl std::fmt::Debug for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Check")
            .field("tables_left", &self.tables.len())
            .field("values", &self.values)
            .field("damaged", &self.damaged)
            .finish_non_exhaustive()
    }
}

impl Check {
    /// Starts a check of `source`'s last commit.
    pub(crate) fn begin(source: &Database) -> Result<Self> {
        Ok(Self {
            tables: backup::tables(source)?,
            values: 0,
            damaged: 0,
            first: None,
            failure: None,
        })
    }

    /// Ends the check early because of `why`, letting go of its read transaction; the next
    /// [`step`](Self::step) reports it.
    pub(crate) fn stop(&mut self, why: String) {
        self.tables.clear();
        self.failure.get_or_insert(why);
    }

    /// Reads and checks the next step: values while the step stays within [`STEP_BYTES`] (a
    /// larger value is a step alone). Returns what the check found once every table is read.
    /// Damage redb reports while reading a table ends that table, which is counted as one damaged
    /// value; any other read failure ends the check.
    pub(crate) fn step(&mut self) -> Option<Checked> {
        let mut bytes = 0usize;
        while let Some((name, entries)) = self.tables.last_mut() {
            while let Some(entry) = entries.peek() {
                let size = match entry {
                    Ok((key, value)) => key.value().len().saturating_add(value.value().len()),
                    Err(_) => 0,
                };
                if bytes > 0 && bytes.saturating_add(size) > STEP_BYTES {
                    return None;
                }
                bytes = bytes.saturating_add(size);
                let (fault, table_ends) = match entries.next() {
                    Some(Ok((key, value))) => {
                        self.values = self.values.saturating_add(1);
                        (seal::verify(key.value(), value.value()).err(), false)
                    }
                    Some(Err(e)) => {
                        let e = Error::from(e);
                        if !e.is_damage() {
                            self.failure = Some(format!("table {name}: {e}"));
                            self.tables.clear();
                            return Some(self.end());
                        }
                        // The table cannot be read on past it.
                        (Some(e), true)
                    }
                    None => (None, true),
                };
                if let Some(fault) = fault {
                    self.damaged = self.damaged.saturating_add(1);
                    self.first
                        .get_or_insert_with(|| format!("table {name}: {fault}"));
                }
                if table_ends {
                    break;
                }
            }
            self.tables.pop();
        }
        Some(self.end())
    }

    /// What the check found, once it ended.
    fn end(&mut self) -> Checked {
        if let Some(why) = self.failure.take() {
            Checked::Failed(why)
        } else if let Some(first) = self.first.take() {
            Checked::Damaged {
                values: self.values,
                damaged: self.damaged,
                first,
            }
        } else {
            Checked::Passed {
                values: self.values,
            }
        }
    }
}
