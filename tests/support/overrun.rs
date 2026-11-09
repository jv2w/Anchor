//! A first tick held past the tick pacer's overrun warning, and the message of a logged event.

use std::fmt;
use std::time::Duration;

use tracing::field::{Field, Visit};

/// How long the first tick is held: more than two tick periods of 50 ms, after which the tick
/// pacer warns that ticks overran, as it does on a loaded machine.
const OVERRUN: Duration = Duration::from_millis(200);

/// The message of an event.
#[derive(Default)]
pub(crate) struct Message(pub(crate) String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Holds the tick for [`OVERRUN`].
#[expect(
    clippy::disallowed_methods,
    reason = "performance: the tick pacer's overrun warning test blocks a tick on purpose"
)]
pub(crate) fn overrun_tick() {
    std::thread::sleep(OVERRUN);
}
