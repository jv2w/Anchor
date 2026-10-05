//! `/check`: checks the world file while the server runs (Anchor's own, PMMP has none).

use super::Msg;
use crate::command::{Args, Command, Output, Result, Sender};
use crate::permission;
use crate::tick::{Context, save};

/// Adds `/check`.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.push(
        Command::new("check", "Checks the world file for damage")
            .permission(permission::COMMAND_CHECK)
            .overload(&[], check),
    );
}

const CHECK_START: Msg =
    Msg::own("Saving and checking the world file; you are told the result when it ends");
const CHECK_BUSY: Msg = Msg::own("A world file check is already running");

/// `/check` (Anchor's own): saves everything at the end of the tick, then the save thread reads
/// every stored value of that commit back and checks it against its checksum, in steps between
/// its other work, so the tick does not wait for it. The result goes to the sender when the
/// check ends (a player who has left is told nothing) and to the log. One check at a time.
fn check(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Result<Output> {
    if !save::ask_check(cx.state, *sender) {
        return Err(CHECK_BUSY.error(&[]));
    }
    Ok(CHECK_START.output(&[]))
}
