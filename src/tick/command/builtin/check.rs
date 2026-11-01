//! `/check`: checks the data files while the server runs (Anchor's own, PMMP has none).

use super::Msg;
use crate::command::{Args, Command, Output, Result, Sender};
use crate::permission;
use crate::tick::{Context, save};

const CHECK_START: Msg =
    Msg::own("Saving and checking the data files; you are told the result when it ends");
const CHECK_BUSY: Msg = Msg::own("A data file check is already running");

/// Adds `/check`.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.push(
        Command::new("check", "Checks the data files for damage")
            .permission(permission::COMMAND_CHECK)
            .overload(&[], check),
    );
}

/// Saves everything at the end of the tick, then has the save thread read every stored value of
/// that commit back and check it against its checksum, in steps between its other work, so the
/// tick does not wait for it (`/check`, Anchor's own). The result goes to the sender when the
/// check ends (a player who has left is told nothing) and to the log. One check at a time.
fn check(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Result<Output> {
    if !save::ask_check(cx.state, *sender) {
        return Err(CHECK_BUSY.error(&[]));
    }
    Ok(CHECK_START.output(&[]))
}
