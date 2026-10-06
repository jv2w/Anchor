//! The console: commands typed on standard input (`console-input`), and the log
//! written to standard error ([`log`]).
//!
//! Reading standard input blocks and cannot be interrupted, so it runs on a thread of its own,
//! outside the tick and the runtime's pools. Each line, at most
//! [`MAX_LINE_BYTES`] bytes, goes to the tick without its
//! terminal escape sequences and control characters as a console input, which the input log records and the tick runs as the console sender. The
//! thread ends when standard input ends or fails, or at the first line after the tick stopped.
//!
//! A read of standard input cannot be stopped, so the thread is detached: when
//! [`Server::run`](crate::Server::run) returns, a thread still waiting for a line lives on until
//! that line comes or the process ends (it holds nothing but the queue sender, which no one reads
//! any more). Standard input is one per process: a second `run` in the same process starts a
//! second thread on the same input, and the first line typed after it may go to the old thread,
//! which drops it and ends; the lines after reach the new server.

use std::io::{self, BufRead};

use tracing::{debug, info, warn};

use crate::command::parse::MAX_LINE_BYTES;
use crate::tick::queue::{self, InputKind, InputSender, SessionId, SessionInputs};

pub(crate) mod log;
pub(crate) mod text;

/// Starts the console thread reading standard input into the tick's input queue.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the console thread blocks on standard input, which no pool may do"
)]
pub(crate) fn start(inputs: &InputSender) -> io::Result<()> {
    let session = inputs.session(SessionId::CONSOLE);
    std::thread::Builder::new()
        .name("anchor-console".to_owned())
        .spawn(log::logged(move || read(io::stdin().lock(), session)))?;
    info!("reading commands from the console");
    Ok(())
}

/// Queues every line of `input` as a console input until the input ends or the tick stops. A
/// line longer than the bound, or not UTF-8, is skipped with a warning; a full queue drops the
/// line (the tick is far behind) rather than waiting.
pub(crate) fn read(mut input: impl BufRead, mut session: SessionInputs) {
    let mut line = Vec::with_capacity(MAX_LINE_BYTES + 2);
    loop {
        line.clear();
        let (complete, more) = match read_bounded(&mut input, &mut line) {
            Ok(read) => read,
            Err(e) => {
                warn!(error = %e, "the console cannot be read; console commands are off");
                return;
            }
        };
        if !complete {
            warn!(max = MAX_LINE_BYTES, "console line too long; skipped");
        } else if let Ok(text) = std::str::from_utf8(trim_line_end(&line)) {
            let text = strip_terminal(text);
            if !text.trim().is_empty() {
                match session.try_send(InputKind::Console(text)) {
                    Ok(()) => {}
                    Err(queue::Error::Full) => {
                        warn!("the tick's input queue is full; console line dropped");
                    }
                    Err(queue::Error::Closed) => {
                        debug!("the tick has stopped; the console thread ends");
                        return;
                    }
                }
            }
        } else {
            warn!("console line is not utf-8; skipped");
        }
        if !more {
            debug!("the console input ended");
            return;
        }
    }
}

/// Reads one line into `line`, keeping at most `MAX_LINE_BYTES` bytes and its line end. Returns
/// whether the line fit, and whether more input may follow (false at the end of the input).
fn read_bounded(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<(bool, bool)> {
    let mut fits = true;
    loop {
        let buffer = match input.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buffer.is_empty() {
            return Ok((fits, false));
        }
        let (part, ends) = match buffer.iter().position(|&b| b == b'\n') {
            Some(i) => (buffer.get(..=i).unwrap_or(buffer), true),
            None => (buffer, false),
        };
        let used = part.len();
        // The line end may take two bytes (`\r\n`) beyond the bound.
        if fits && line.len().saturating_add(used) <= MAX_LINE_BYTES + 2 {
            line.extend_from_slice(part);
        } else {
            fits = false;
        }
        input.consume(used);
        if ends {
            return Ok((fits && trim_line_end(line).len() <= MAX_LINE_BYTES, true));
        }
    }
}

/// `line` without terminal escape sequences and control characters, as PMMP's console reader
/// strips them (cursor keys and other edits a terminal echoes into the line). A sequence is an
/// ECMA-48 control sequence: `ESC [`, parameter and intermediate bytes, then one final byte.
/// PMMP's pattern instead runs from `ESC [` to the last `~` of the line, which drops typed text.
fn strip_terminal(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b'
            && let Some(sequence) = chars.as_str().strip_prefix('[')
        {
            let end = sequence.trim_start_matches(|c| ('\x20'..='\x3f').contains(&c));
            let end = end
                .strip_prefix(|c| ('\x40'..='\x7e').contains(&c))
                .unwrap_or(end);
            chars = end.chars();
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// `line` without its `\n` or `\r\n`.
fn trim_line_end(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::queue::input_queue;

    fn lines(input: &[u8]) -> Vec<String> {
        let (sender, mut rx) = input_queue();
        read(input, sender.session(SessionId::CONSOLE));
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|i| {
                assert_eq!(i.session, SessionId::CONSOLE);
                match i.kind {
                    InputKind::Console(line) => line,
                    other => panic!("{other:?}"),
                }
            })
            .collect()
    }

    #[test]
    fn console_lines_are_queued_without_line_ends_and_bounded() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let fit = "y".repeat(MAX_LINE_BYTES);
        let input = format!("op steve\r\n\n  \nsay hi\n{long}\n{fit}\r\nlast");
        let mut bytes = input.into_bytes();
        bytes.extend_from_slice(b"\nbad \xff\nend");
        assert_eq!(lines(&bytes), ["op steve", "say hi", &fit, "last", "end"]);
    }

    #[test]
    fn terminal_escapes_and_control_characters_are_stripped() {
        let input = "\x1b[Asay hi\x07\n\x1b[3~\x1b[1;5Clist\x1b\n\x1b[B\t\n";
        assert_eq!(lines(input.as_bytes()), ["say hi", "list"]);
    }

    #[test]
    fn a_stopped_tick_ends_the_console_thread() {
        let (sender, rx) = input_queue();
        drop(rx);
        // Returns at the first line instead of reading on.
        read(
            &b"stop\nnever read\n"[..],
            sender.session(SessionId::CONSOLE),
        );
    }
}
