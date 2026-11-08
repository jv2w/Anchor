//! Server events: the start and commands.
//!
//! Packets are seen through packet hooks, not events.

use crate::command::Sender;

/// Raised once when the server starts, at the start of its first tick: the worlds are loaded and
/// no player has joined yet. A replay of the run raises it again in the same tick; a replay from a
/// later checkpoint does not, as the run had raised it before that checkpoint.
#[derive(Debug)]
pub struct ServerStart {}

/// Raised for each command line a player or the console sends, ahead of parsing. Cancelling skips
/// the command.
#[doc(alias = "CommandEvent")]
#[derive(Debug)]
pub struct Command {
    pub(crate) sender: Sender,
    pub(crate) line: String,
    pub(crate) cancelled: bool,
}

getters!(Command {
    /// Who runs it: a player or the console.
    #[doc(alias = "CommandEvent::getSender")]
    sender: Sender,
});

impl Command {
    /// The command line without the leading `/`.
    #[doc(alias = "CommandEvent::getCommand")]
    pub fn command(&self) -> &str {
        &self.line
    }

    /// Replaces the command line that runs, written without a leading `/`: one with it names no
    /// command.
    #[doc(alias = "CommandEvent::setCommand")]
    pub fn set_command(&mut self, line: impl Into<String>) {
        self.line = line.into();
    }
}

cancellable! {
    Command
        #[doc(alias = "CommandEvent::cancel")]
        #[doc(alias = "CommandEvent::uncancel")]
        #[doc(alias = "CommandEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::fixtures::player;

    // api: CommandEvent, CommandEvent::getSender, CommandEvent::getCommand,
    // api: CommandEvent::setCommand, CommandEvent::cancel, CommandEvent::uncancel,
    // api: CommandEvent::isCancelled
    #[test]
    fn command_line_can_change() {
        let mut e = Command {
            sender: Sender::Player(player(1)),
            line: "list".to_owned(),
            cancelled: false,
        };
        assert_eq!(
            (e.sender(), e.command()),
            (Sender::Player(player(1)), "list")
        );
        e.set_command("status".to_owned());
        assert_eq!(e.command(), "status");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }
}
