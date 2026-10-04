//! Server events (PMMP `pocketmine\event\server`): commands.
//!
//! PMMP's packet events (`DataPacketReceiveEvent`, `DataPacketSendEvent`,
//! `DataPacketDecodeEvent`) are packet hooks in Anchor.

use crate::command::Sender;

/// Raised when a player or the console runs a command, before it is parsed. Cancelling skips the
/// command.
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

    /// Replaces the command line that runs.
    #[doc(alias = "CommandEvent::setCommand")]
    pub fn set_command(&mut self, line: String) {
        self.line = line;
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

    // pmmp: CommandEvent, CommandEvent::getSender, CommandEvent::getCommand, CommandEvent::setCommand, CommandEvent::cancel, CommandEvent::uncancel, CommandEvent::isCancelled
    #[test]
    fn command_line_can_change() {
        let mut e = Command {
            sender: Sender::Player(player(1)),
            line: "say hi".to_owned(),
            cancelled: false,
        };
        assert_eq!(
            (e.sender(), e.command()),
            (Sender::Player(player(1)), "say hi")
        );
        e.set_command("me waves".to_owned());
        assert_eq!(e.command(), "me waves");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }
}
