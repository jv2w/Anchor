//! The commands every server program answers from its process arguments instead of running the
//! server, with no code of its own: `<program> update-config` and
//! `<program> import <world folder> [<world name>]`. Other arguments are the program's own and
//! start the server as usual.
//!
//! `update-config` is done by [`Config::load`](crate::Config::load), before it reads the file,
//! since an old file may not load until it is updated; [`Server::run`](crate::Server::run) then
//! returns. `import` is done by `run`, with the worlds, generators and custom content the program
//! registered.

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::config::Update;
use crate::error::{Error, Result};
use crate::import::Report;

/// What the process arguments ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Task {
    /// Run the server: no argument, or the program's own.
    Run,
    /// `update-config`: add the missing keys to the settings file.
    UpdateConfig,
    /// `import <world folder> [<world name>]`: import a Bedrock world; without a name, as the
    /// default world (`level-name`).
    Import { dir: PathBuf, world: Option<String> },
}

/// The task this process's arguments ask; [`Error::Usage`] for `update-config` or `import`
/// given the wrong arguments.
pub(crate) fn current() -> Result<Task> {
    parse(env::args_os().skip(1).collect())
}

/// The task of the arguments `args`, the program's name left out.
fn parse(args: Vec<OsString>) -> Result<Task> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Task::Run);
    };
    let rest: Vec<OsString> = args.collect();
    if first == "update-config" {
        return match rest.as_slice() {
            [] => Ok(Task::UpdateConfig),
            _ => Err(Error::Usage),
        };
    }
    if first != "import" {
        return Ok(Task::Run);
    }
    match rest.as_slice() {
        [dir] => Ok(Task::Import {
            dir: dir.into(),
            world: None,
        }),
        [dir, world] => match world.to_str() {
            Some(world) => Ok(Task::Import {
                dir: dir.into(),
                world: Some(world.to_owned()),
            }),
            None => Err(Error::Usage),
        },
        _ => Err(Error::Usage),
    }
}

/// What `update-config` writes: what [`Config::load`](crate::Config::load) changed, `None` if
/// the program loaded no settings file with it.
pub(crate) fn updated(update: Option<&Update>) -> String {
    let Some(update) = update else {
        return "the program loads no settings file with Config::load: nothing was updated"
            .to_owned();
    };
    if update.is_empty() {
        return "the settings file already has every key".to_owned();
    }
    let added = update.added().iter().map(|key| format!("added {key}"));
    let renamed = update.renamed().iter();
    let renamed = renamed.map(|(old, new)| format!("renamed {old} to {new}"));
    added.chain(renamed).collect::<Vec<_>>().join("\n")
}

/// What `import` writes when the world is imported as `world`.
pub(crate) fn imported(world: &str, report: &Report) -> String {
    let spawn = report.spawn();
    let unknown = report.unknown_blocks().values().copied();
    let unknown = unknown.fold(report.unlisted_unknown_blocks(), u64::saturating_add);
    let bare = world
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let table = if bare {
        format!("[worlds.{world}]")
    } else {
        format!("[worlds.{world:?}]")
    };
    format!(
        "imported the world {world:?} as data/world/{world}.redb: {} chunks, {} block entities
left out: {} entities, {} Nether chunks, {} End chunks, {} players
unknown blocks, kept as they are: {unknown}
its spawn in level.dat is {x} {y} {z}; to use it, set spawn = [{x}, {y}, {z}] under {table}
a running server sees the world after its next start",
        report.chunks(),
        report.block_entities(),
        report.entities(),
        report.nether_chunks(),
        report.end_chunks(),
        report.players(),
        x = spawn.x,
        y = spawn.y,
        z = spawn.z,
    )
}

/// Writes `text` and a line ending to standard output: the answer to a command of the process
/// arguments.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the program runs a command of its arguments instead of a server"
)]
pub(crate) fn say(text: &str) -> io::Result<()> {
    writeln!(io::stdout().lock(), "{text}")
}

#[cfg(test)]
mod tests {
    use super::{Task, parse};
    use crate::Error;

    fn task(args: &[&str]) -> crate::Result<Task> {
        parse(args.iter().map(Into::into).collect())
    }

    #[test]
    fn other_arguments_run_the_server() {
        for args in [
            &[][..],
            &["--port", "1"],
            &["Import"],
            &["update"],
            &["x", "import"],
        ] {
            assert_eq!(task(args).expect("runs"), Task::Run, "{args:?}");
        }
    }

    #[test]
    fn the_commands_take_their_arguments() {
        assert_eq!(task(&["update-config"]).expect("ok"), Task::UpdateConfig);
        assert_eq!(
            task(&["import", "C:/worlds/lobby"]).expect("ok"),
            Task::Import {
                dir: "C:/worlds/lobby".into(),
                world: None
            }
        );
        assert_eq!(
            task(&["import", "w", "my world"]).expect("ok"),
            Task::Import {
                dir: "w".into(),
                world: Some("my world".to_owned())
            }
        );
        for args in [
            &["update-config", "x"][..],
            &["import"],
            &["import", "w", "a", "b"],
        ] {
            assert!(matches!(task(args), Err(Error::Usage)), "{args:?}");
        }
    }
}
