//! The console log: Anchor's own `tracing` subscriber, which shows every event as one line on
//! standard error (standard output stays the server program's).
//!
//! It is no global subscriber. [`Server::run`](crate::Server::run) makes it the default of its
//! calling thread while it runs, and every thread Anchor starts under it takes it on
//! ([`inherited`], [`logged`], [`attach`]); threads of the program keep their own default. A line
//! reads `2026-10-05 14:03:12.345Z INFO  player joined (session=3 name=Steve)`: the wall-clock
//! time in UTC, the level, the message and the event's fields. Control characters are escaped and
//! Minecraft format codes (`§` and the character after it) are left out, so a name or a chat line
//! cannot forge lines or steer the terminal. The text past 4 KiB is cut, with a marker.
//!
//! The thread that raises an event only formats its line and queues it: one log thread of its
//! own writes the lines, so a console that stops taking writes (a selection in a Windows console,
//! a full pipe) never holds up the tick or the network. A line that finds the queue full is
//! dropped, and the log thread writes how many were before its next line and at the end. The end
//! of the log waits [`END_WAIT`] at most for the thread to write what is queued: past it the
//! thread is left to finish on its own, so such a console does not hold up the end of `run`
//! either.

use std::cell::Cell;
use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::dispatcher::{self, DefaultGuard};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber};

use crate::clock;
use crate::config::LogLevel;

/// The lines that may wait for the log thread: with [`LINE_BYTES`], about 16 MiB at most.
const QUEUE_LINES: usize = 4096;
/// The bytes of a line's message and fields, the [`CUT`] marker included: the text past them is
/// left out. The time, the level, the brackets and the line end add 35 bytes.
const LINE_BYTES: usize = 4096;
/// Ends the text of a line that was cut.
const CUT: &str = "…(cut)";
/// How long the end of the log waits for the log thread to write the lines still queued. A
/// console that takes no writes for this long loses them if the program then exits.
const END_WAIT: Duration = Duration::from_secs(5);

/// The console log's subscriber: events (and `enabled!` hints) up to `max`, spans never.
#[derive(Debug)]
struct Logger {
    max: Level,
    /// The log thread's queue. Weak, so the subscriber's clones that outlive the run (a console
    /// thread still waiting for input) do not keep the log thread waiting for lines.
    lines: mpsc::WeakSender<Item>,
    dropped: Arc<Dropped>,
}

/// What the log thread's queue carries.
#[derive(Debug)]
enum Item {
    /// A line to write, with its line end.
    Line(String),
    /// Answered once every item before it has been written ([`Drained::wait`]).
    Drained(oneshot::Sender<()>),
}

/// Waits for the console log to write what was queued, without keeping it alive.
#[derive(Debug, Clone)]
pub(crate) struct Drained(mpsc::WeakSender<Item>);

impl Drained {
    /// Returns once the log thread has written every line queued before the call, or at once if
    /// the log has ended. Bound it with a timeout: a console that takes no writes holds it up.
    pub(crate) async fn wait(&self) {
        let Some(lines) = self.0.upgrade() else {
            return;
        };
        let (done, drained) = oneshot::channel();
        let sent = lines.send(Item::Drained(done)).await;
        // The log may end meanwhile: only the log keeps the thread waiting for lines.
        drop(lines);
        if sent.is_ok() {
            // The thread answers or, once it ends, drops the sender: both mean drained.
            match drained.await {
                Ok(()) | Err(oneshot::error::RecvError { .. }) => {}
            }
        }
    }
}

/// The lines the queue had no room for since the log thread last wrote their count.
#[derive(Debug, Default)]
struct Dropped(
    #[expect(
        clippy::disallowed_types,
        reason = "performance: any thread that logs, the tick included, counts a dropped line \
                  without waiting; the count is no game state"
    )]
    std::sync::atomic::AtomicU64,
);

impl Dropped {
    fn add(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// The count, which starts again from 0.
    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// Anchor's console log: its subscriber and the thread that writes its lines. Dropping it lets
/// the thread write the lines already queued, then joins it, or leaves it after [`END_WAIT`].
#[derive(Debug)]
pub(crate) struct Log {
    dispatch: Dispatch,
    /// The one strong sender of the queue; dropping it ends the log thread once the queue is
    /// empty.
    lines: Option<mpsc::Sender<Item>>,
    drained: Drained,
    thread: Option<JoinHandle<()>>,
    /// Gets one message when the log thread has written its last line.
    ended: Receiver<()>,
}

/// The console log as the calling thread's default ([`Log::install`]). Dropping it removes the
/// default, then ends the log.
#[derive(Debug)]
pub(crate) struct Installed {
    _default: DefaultGuard,
    log: Log,
}

impl Installed {
    /// Waits for the log's queue to be written ([`Drained::wait`]).
    pub(crate) fn drained(&self) -> Drained {
        self.log.drained.clone()
    }
}

/// Starts the console log for `level`, writing on standard error; `None` for
/// [`LogLevel::Off`], which starts nothing. Fails if the log thread cannot start.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: only the log thread writes standard error"
)]
pub(crate) fn start(level: LogLevel) -> io::Result<Option<Log>> {
    start_writing(level, io::stderr())
}

/// [`start`] with the log thread writing on `out`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the log thread reports its end on a channel of one message"
)]
fn start_writing(level: LogLevel, out: impl Write + Send + 'static) -> io::Result<Option<Log>> {
    let max = match level {
        LogLevel::Off => return Ok(None),
        LogLevel::Error => Level::ERROR,
        LogLevel::Warn => Level::WARN,
        LogLevel::Info => Level::INFO,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Trace => Level::TRACE,
    };
    let (lines, queue) = mpsc::channel(QUEUE_LINES);
    let (end, ended) = std::sync::mpsc::sync_channel(1);
    let dropped = Arc::new(Dropped::default());
    let logger = Logger {
        max,
        lines: lines.downgrade(),
        dropped: Arc::clone(&dropped),
    };
    // The log thread takes on no subscriber: nothing it does is logged.
    let thread = crate::runtime::spawn_thread("anchor-log", move || {
        write_lines(queue, &dropped, out);
        // The one message fits; once the end has stopped waiting, nobody needs it.
        match end.try_send(()) {
            Ok(()) | Err(_) => {}
        }
    })?;
    Ok(Some(Log {
        dispatch: Dispatch::new(logger),
        drained: Drained(lines.downgrade()),
        lines: Some(lines),
        thread: Some(thread),
        ended,
    }))
}

impl Log {
    /// Makes the log the calling thread's default until the returned guard drops.
    pub(crate) fn install(self) -> Installed {
        Installed {
            _default: dispatcher::set_default(&self.dispatch),
            log: self,
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        drop(self.lines.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        // A thread still writing after `END_WAIT` holds the queue's last lines, at most
        // `QUEUE_LINES`: it ends once the console takes them, or with the process. Dropping the
        // handle leaves it to that. A thread that panicked dropped its sender: nothing to wait for.
        if self.ended.recv_timeout(END_WAIT).is_ok() {
            // The thread only writes; a panic there leaves nothing to clean up or report.
            match thread.join() {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

/// The log thread: writes each queued line on `out` until the queue is closed and empty, and
/// the count of dropped lines before the next line, before answering a [`Drained`] and at the
/// end.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the log thread waits for lines, which no pool may do"
)]
fn write_lines(mut queue: mpsc::Receiver<Item>, dropped: &Dropped, mut out: impl Write) {
    while let Some(item) = queue.blocking_recv() {
        report_dropped(dropped, &mut out);
        match item {
            Item::Line(line) => write_line(&mut out, &line),
            Item::Drained(done) => {
                // As in `write_line`: a failed write has nowhere to be reported.
                match out.flush() {
                    Ok(()) | Err(_) => {}
                }
                match done.send(()) {
                    // A waiter that timed out no longer listens.
                    Ok(()) | Err(()) => {}
                }
            }
        }
    }
    report_dropped(dropped, &mut out);
}

/// Writes the count of dropped lines, if any, as a line of its own.
fn report_dropped(dropped: &Dropped, out: &mut impl Write) {
    let count = dropped.take();
    if count > 0 {
        let line = compose(
            clock::unix_millis(),
            Level::WARN,
            "console log lines left out: standard error did not take them in time",
            &format!("count={count}"),
        );
        write_line(out, &line);
    }
}

/// Writes `line` in one write, so a reader never sees half a line.
fn write_line(out: &mut impl Write, line: &str) {
    // Standard error is the log's only way out: when it fails there is nowhere left to report
    // that.
    match out.write_all(line.as_bytes()) {
        Ok(()) | Err(_) => {}
    }
}

/// The calling thread's default if it is Anchor's console log: what a thread it starts takes on.
pub(crate) fn inherited() -> Option<Dispatch> {
    dispatcher::get_default(|d| d.is::<Logger>().then(|| d.clone()))
}

/// `run` for a new thread, under the console log of the thread that makes it (if any) until
/// `run` returns.
pub(crate) fn logged<T>(run: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let log = inherited();
    move || {
        let _log = log.as_ref().map(dispatcher::set_default);
        run()
    }
}

thread_local! {
    /// The console log of a pool thread: set by the pool's thread-start hook, dropped with the
    /// thread's locals when it ends.
    static ATTACHED: Cell<Option<DefaultGuard>> = const { Cell::new(None) };
}

/// Makes `log` the calling thread's default until the thread ends: for the threads of pools,
/// whose start hooks cannot wrap the thread's work.
pub(crate) fn attach(log: &Dispatch) {
    ATTACHED.with(|attached| attached.set(Some(dispatcher::set_default(log))));
}

impl Subscriber for Logger {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.enabled(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        !metadata.is_span() && *metadata.level() <= self.max
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(self.max.into())
    }

    /// Never called: no span is enabled. Every span would share one ID, which nothing reads.
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let line = format_line(clock::unix_millis(), event);
        // Never waits for the log thread: a full queue, or a log that has ended, drops the line.
        let sent = self
            .lines
            .upgrade()
            .is_some_and(|lines| lines.try_send(Item::Line(line)).is_ok());
        if !sent {
            self.dropped.add();
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// The line of `event` at `millis` since the Unix epoch, with its line end.
fn format_line(millis: i64, event: &Event<'_>) -> String {
    let mut parts = Parts::default();
    event.record(&mut parts);
    compose(
        millis,
        *event.metadata().level(),
        &parts.message,
        &parts.fields,
    )
}

/// The line at `millis` since the Unix epoch of `message` and its `fields` (cleaned already),
/// with its line end.
fn compose(millis: i64, level: Level, message: &str, fields: &str) -> String {
    let level = match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN ",
        Level::INFO => "INFO ",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    };
    let mut line = String::with_capacity(32 + message.len() + fields.len());
    line.push_str(&timestamp(millis));
    line.push(' ');
    line.push_str(level);
    line.push(' ');
    line.push_str(message);
    if !fields.is_empty() {
        line.push_str(" (");
        line.push_str(fields);
        line.push(')');
    }
    line.push('\n');
    line
}

/// `millis` since the Unix epoch (0 or more) as `YYYY-MM-DD hh:mm:ss.mmmZ`, in UTC.
fn timestamp(millis: i64) -> String {
    let (secs, ms) = (millis.div_euclid(1000), millis.rem_euclid(1000));
    let (days, secs) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hour, minute, second) = (secs / 3600, secs % 3600 / 60, secs % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{ms:03}Z")
}

/// An event's message and its other fields as `name=value`, cleaned, [`LINE_BYTES`] together at
/// most.
struct Parts {
    message: String,
    fields: String,
    /// The bytes that still fit before the [`CUT`] marker; `None` once the text is cut.
    room: Option<usize>,
}

impl Default for Parts {
    fn default() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
            room: Some(LINE_BYTES - CUT.len()),
        }
    }
}

impl Parts {
    /// Writes `field`: the first `message` is the event's message (tracing records the format
    /// string first), a field the event names `message` as well is a field.
    fn put(&mut self, field: &Field, write: impl FnOnce(&mut Clean<'_>) -> fmt::Result) {
        if self.room.is_none() {
            return;
        }
        let message = field.name() == "message" && self.message.is_empty();
        let mut out = Clean {
            out: if message {
                &mut self.message
            } else {
                &mut self.fields
            },
            room: &mut self.room,
            code: false,
        };
        if !message {
            let gap = if out.out.is_empty() { "" } else { " " };
            if write!(out, "{gap}{}=", field.name()).is_err() {
                return;
            }
        }
        if write(&mut out).is_err() && out.room.is_some() {
            // A value's own `Debug` failed; the line keeps what it wrote. A cut ends the text.
            match out.write_str("<unprintable>") {
                Ok(()) | Err(fmt::Error) => {}
            }
        }
    }
}

impl Visit for Parts {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, |out| out.write_str(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, |out| write!(out, "{value:?}"));
    }
}

/// Writes text without Minecraft format codes and with control characters escaped, while it
/// fits in `room`. The character that does not fit is left out with the rest, [`CUT`] is written
/// instead, and every write fails from then on, so a long value is never formatted whole.
struct Clean<'a> {
    out: &'a mut String,
    /// [`Parts::room`].
    room: &'a mut Option<usize>,
    /// The last character was `§`: the next one is its code.
    code: bool,
}

impl fmt::Write for Clean<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if self.code {
                self.code = false;
                continue;
            }
            if c == '§' {
                self.code = true;
                continue;
            }
            let escaped = c.is_control();
            let len = if escaped {
                c.escape_default().len()
            } else {
                c.len_utf8()
            };
            let room = (*self.room).ok_or(fmt::Error)?;
            let Some(left) = room.checked_sub(len) else {
                *self.room = None;
                self.out.push_str(CUT);
                return Err(fmt::Error);
            };
            *self.room = Some(left);
            if escaped {
                self.out.extend(c.escape_default());
            } else {
                self.out.push(c);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use tracing::{debug, error, info, warn};

    use super::*;

    /// The last line `event` formats under a subscriber that records instead of writing.
    #[derive(Debug, Default)]
    struct Capture {
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: the test subscriber collects lines from the test thread"
        )]
        lines: std::sync::Mutex<Vec<String>>,
    }

    impl Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            self.lines
                .lock()
                .unwrap()
                .push(format_line(1_759_673_000_123, event));
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// The lines `f` logs on this thread, as the console would write them.
    pub(crate) fn captured(f: impl FnOnce()) -> Vec<String> {
        let capture = Arc::new(Capture::default());
        dispatcher::with_default(&Dispatch::from(Arc::clone(&capture)), f);
        capture.lines.lock().unwrap().clone()
    }

    #[test]
    fn a_line_has_the_utc_time_the_level_the_message_and_the_fields() {
        let name = "Steve";
        let lines = captured(|| {
            info!(session = 3, %name, ok = true, "player joined");
            warn!("no fields");
            error!(error = ?"bad", "quoted debug value");
        });
        assert_eq!(
            lines,
            [
                "2025-10-05 14:03:20.123Z INFO  player joined (session=3 name=Steve ok=true)\n",
                "2025-10-05 14:03:20.123Z WARN  no fields\n",
                "2025-10-05 14:03:20.123Z ERROR quoted debug value (error=\"bad\")\n",
            ]
        );
    }

    #[test]
    fn control_characters_are_escaped_and_format_codes_left_out() {
        let text = "§ahi\nforged line\x1b[31m§";
        let lines = captured(|| info!(%text, "said {}", "§lloud\r"));
        assert_eq!(
            lines,
            ["2025-10-05 14:03:20.123Z INFO  said loud\\r (text=hi\\nforged line\\u{1b}[31m)\n"]
        );
    }

    #[test]
    fn a_field_named_message_is_a_field_beside_the_message() {
        let message = "boom";
        let lines = captured(|| error!(%message, "tick panicked"));
        assert_eq!(
            lines,
            ["2025-10-05 14:03:20.123Z ERROR tick panicked (message=boom)\n"]
        );
    }

    /// A long message or field is cut at a character boundary, so no line holds more than
    /// [`LINE_BYTES`] of text.
    #[test]
    fn a_long_line_is_cut_at_a_character_boundary() {
        let long = "a".repeat(1 << 20);
        // `é` is two bytes and the text before it is odd: the cut falls inside one.
        let wide = format!("a{}", "é".repeat(1 << 19));
        let lines = captured(|| {
            info!(x = %long, "long");
            info!(x = %wide, "wide");
            info!("{long}");
        });
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.len() <= LINE_BYTES + 35, "{} bytes", line.len());
        }
        assert!(lines[0].ends_with(&format!("{CUT})\n")));
        assert!(lines[1].ends_with(&format!("{CUT})\n")));
        assert!(lines[2].ends_with(&format!("{CUT}\n")));
    }

    #[test]
    fn timestamps_follow_the_civil_calendar() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00.000Z");
        // A leap day and the last millisecond of a leap year.
        assert_eq!(timestamp(951_782_400_000), "2000-02-29 00:00:00.000Z");
        assert_eq!(timestamp(1_735_689_599_999), "2024-12-31 23:59:59.999Z");
        assert_eq!(timestamp(4_107_542_400_000), "2100-03-01 00:00:00.000Z");
    }

    #[test]
    fn events_up_to_the_level_are_shown_and_spans_never() {
        let Some(log) = start_writing(LogLevel::Warn, io::sink()).unwrap() else {
            panic!("warn starts the log");
        };
        dispatcher::with_default(&log.dispatch, || {
            assert!(tracing::enabled!(tracing::Level::ERROR));
            assert!(tracing::enabled!(tracing::Level::WARN));
            assert!(!tracing::enabled!(tracing::Level::INFO));
            assert!(tracing::info_span!("work").is_disabled());
        });
        assert!(start_writing(LogLevel::Off, io::sink()).unwrap().is_none());
    }

    /// How long a test waits for work that must not block.
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Whether `work`, on a thread of its own, ends within [`LIMIT`].
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test runs work on its own thread and waits for it with a limit"
    )]
    fn ends_in_time(work: impl FnOnce() + Send + 'static) -> bool {
        let (done, ended) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            work();
            done.send(()).unwrap();
        });
        ended.recv_timeout(LIMIT).is_ok()
    }

    /// A writer that holds every write until it is opened, then keeps the text.
    #[derive(Clone, Default)]
    struct Gate(
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: the test writer waits on the log thread until the test opens it"
        )]
        Arc<(std::sync::Mutex<(bool, String)>, std::sync::Condvar)>,
    );

    impl Gate {
        fn opened() -> Self {
            let gate = Self::default();
            gate.open();
            gate
        }

        fn open(&self) {
            self.0.0.lock().unwrap().0 = true;
            self.0.1.notify_all();
        }

        fn text(&self) -> String {
            self.0.0.lock().unwrap().1.clone()
        }
    }

    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (lock, opened) = &*self.0;
            let mut written = lock.lock().unwrap();
            while !written.0 {
                written = opened.wait(written).unwrap();
            }
            written.1.push_str(std::str::from_utf8(buf).unwrap());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The `count=` of the dropped-lines reports in `text`, added up.
    fn reported_dropped(text: &str) -> u64 {
        text.lines()
            .filter(|l| l.contains("WARN  console log lines left out"))
            .map(|l| {
                let count = l.split_once("(count=").unwrap().1;
                count.trim_end_matches(')').parse::<u64>().unwrap()
            })
            .sum()
    }

    /// A console that takes no writes (a selection in a Windows console) never holds up a thread
    /// that logs: its lines past the queue are dropped and counted.
    #[test]
    fn a_console_that_takes_no_writes_never_holds_up_a_thread_that_logs() {
        let gate = Gate::default();
        let log = start_writing(LogLevel::Info, gate.clone())
            .unwrap()
            .unwrap();
        let dispatch = log.dispatch.clone();
        let logged = ends_in_time(move || {
            dispatcher::with_default(&dispatch, || {
                for n in 0..10_000 {
                    info!(n, "line");
                }
            });
        });
        gate.open();
        assert!(logged, "the thread that logs waited for the console");
        drop(log);
        let text = gate.text();
        let written = text.lines().filter(|l| l.contains("INFO  line")).count();
        let dropped = reported_dropped(&text);
        assert!(dropped > 0, "the queue holds fewer than 10,000 lines");
        assert_eq!(written as u64 + dropped, 10_000);
    }

    /// The end of the log writes every line queued before it, in order, and joins the thread.
    #[test]
    fn the_end_of_the_log_writes_every_queued_line() {
        let gate = Gate::opened();
        let log = start_writing(LogLevel::Info, gate.clone())
            .unwrap()
            .unwrap();
        let installed = log.install();
        for n in 0..10 {
            info!(n, "line");
        }
        drop(installed);
        let lines: Vec<String> = gate
            .text()
            .lines()
            .map(|l| {
                l.split_once(' ')
                    .unwrap()
                    .1
                    .split_once(' ')
                    .unwrap()
                    .1
                    .to_owned()
            })
            .collect();
        let wanted: Vec<String> = (0..10).map(|n| format!("INFO  line (n={n})")).collect();
        assert_eq!(lines, wanted);
    }

    /// A writer that needs 3 MiB of stack: more than std gives a thread (2 MiB), less than every
    /// thread Anchor starts has.
    struct Deep(Gate);

    impl Write for Deep {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            #[expect(
                clippy::large_stack_arrays,
                reason = "invariant: the test fills 3 MiB of the log thread's stack on purpose"
            )]
            let frame = [0_u8; 3 << 20];
            std::hint::black_box(&frame);
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The log thread has the stack of every thread Anchor starts, not std's default.
    #[test]
    fn the_log_thread_has_the_stack_of_every_anchor_thread() {
        let gate = Gate::opened();
        let log = start_writing(LogLevel::Info, Deep(gate.clone()))
            .unwrap()
            .unwrap();
        dispatcher::with_default(&log.dispatch, || info!("deep"));
        drop(log);
        assert!(gate.text().contains("INFO  deep"));
    }

    /// A console that takes no writes holds up the end of the log for [`END_WAIT`] at most: the
    /// log thread is left to write the rest and end once the console takes writes again.
    #[test]
    fn a_console_that_takes_no_writes_does_not_hold_up_the_end() {
        let gate = Gate::default();
        let log = start_writing(LogLevel::Info, gate.clone())
            .unwrap()
            .unwrap();
        dispatcher::with_default(&log.dispatch, || info!("held"));
        let ended = ends_in_time(move || drop(log));
        gate.open();
        assert!(ended, "the end waited for the console");
    }

    /// A thread that keeps the subscriber, as a console thread still waiting for input does,
    /// does not hold up the end of the log.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test starts a thread that keeps the subscriber"
    )]
    fn a_thread_that_keeps_the_subscriber_does_not_hold_up_the_end() {
        let gate = Gate::opened();
        let log = start_writing(LogLevel::Info, gate.clone())
            .unwrap()
            .unwrap();
        let kept = log.dispatch.clone();
        let held = Gate::default();
        let release = held.clone();
        let keeper = std::thread::spawn(move || {
            dispatcher::with_default(&kept, || {
                // Waits until the test opens `held`.
                let mut held = held;
                held.write_all(b"done").unwrap();
                info!("after the end");
            });
        });
        assert!(
            ends_in_time(move || drop(log)),
            "the end waited for the keeper"
        );
        release.open();
        keeper.join().unwrap();
        assert!(!gate.text().contains("after the end"));
    }

    #[test]
    fn threads_started_under_the_log_take_it_on_and_others_do_not() {
        let log = start_writing(LogLevel::Debug, io::sink()).unwrap().unwrap();
        let log = &log.dispatch;
        assert!(inherited().is_none(), "the test thread has no Anchor log");
        let under = dispatcher::with_default(log, || logged(|| inherited().is_some()));
        assert!(under(), "the wrapped work runs under the log");
        assert!(!logged(|| inherited().is_some())());
        // A pool thread keeps the log it attached until it ends.
        let attached = crate::runtime::cpu_pool(1).unwrap();
        let pool_log = attached.map(&[(), ()], |()| inherited().is_some());
        assert_eq!(pool_log, [false, false], "a pool made without the log");
        assert!(attached.shutdown());
        let pool = dispatcher::with_default(log, || crate::runtime::cpu_pool(2).unwrap());
        assert_eq!(
            pool.map(&[(), (), ()], |()| inherited().is_some()),
            [true; 3]
        );
        assert!(pool.shutdown());
        debug!("after the threads ended");
    }
}
