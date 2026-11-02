//! The commands every server program answers from its process arguments instead of running the
//! server, with no code of its own: `<program> update-config`,
//! `<program> import <world folder> [<world name>]` and
//! `<program> export <new folder> [<world name>]`. Other arguments are the program's own and
//! start the server as usual.
//!
//! `update-config` is done by [`Config::load`](crate::Config::load), before it reads the file,
//! since an old file may not load until it is updated; [`Server::run`](crate::Server::run) then
//! returns. `import` and `export` are done by `run`, with the worlds, generators and custom
//! content the program registered.

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::Update;
use crate::error::{Error, Result};
use crate::{export, import};

/// What [`Error::Usage`] says: the syntax of the three commands.
pub(crate) const USAGE: &str = "usage: <program> update-config, \
    <program> import <world folder> [<world name>], \
    or <program> export <new folder> [<world name>]";

/// What the process arguments ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Task {
    /// Run the server: no argument, or the program's own.
    Run,
    /// `update-config`: add the missing keys to the settings file and remove the unknown ones.
    UpdateConfig,
    /// `import <world folder> [<world name>]`: import a Bedrock world; without a name, as the
    /// default world (`level-name`).
    Import { dir: PathBuf, world: Option<String> },
    /// `export <new folder> [<world name>]`: export a world, chosen by name, as a Bedrock world
    /// into a folder that does not exist yet; without a name, the default world
    /// (`level-name`).
    Export { dir: PathBuf, world: Option<String> },
}

/// The task this process's arguments ask; [`Error::Usage`] for `update-config`, `import` or
/// `export` given the wrong arguments.
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
    let task: fn(PathBuf, Option<String>) -> Task = match first.to_str() {
        Some("import") => |dir, world| Task::Import { dir, world },
        Some("export") => |dir, world| Task::Export { dir, world },
        _ => return Ok(Task::Run),
    };
    match rest.as_slice() {
        [dir] => Ok(task(dir.into(), None)),
        [dir, world] => match world.to_str() {
            Some(world) => Ok(task(dir.into(), Some(world.to_owned()))),
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
        return "the settings file is already up to date".to_owned();
    }
    let added = update.added().iter().map(|key| format!("added {key}"));
    let removed = update.removed().iter().map(|key| format!("removed {key}"));
    added.chain(removed).collect::<Vec<_>>().join("\n")
}

/// What `import` writes when the world is imported as `world` into `file`.
pub(crate) fn imported(world: &str, file: &Path, report: &import::Report) -> String {
    let spawn = report.spawn();
    let unknown = report.unknown_blocks().values().copied();
    let unknown = unknown.fold(report.unlisted_unknown_blocks(), u64::saturating_add);
    format!(
        "imported the world {world:?} as {}: {} chunks, {} block entities
left out: {} entities, {} Nether chunks, {} End chunks, {} players
unknown blocks, kept as they are: {unknown}
its spawn is kept from level.dat: {x} {y} {z}
a running server sees the world after its next start",
        file.display(),
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

/// What `export` writes when the world `world` is exported into the folder `dir`.
pub(crate) fn exported(world: &str, dir: &Path, report: &export::Report) -> String {
    format!(
        "exported the world {world:?} to {}: {} chunks, {} block entities
left out: {} entities, {} spills
chunks the world never saved are made by the Bedrock world's flat generator",
        dir.display(),
        report.chunks(),
        report.block_entities(),
        report.entities(),
        report.spills(),
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
    use super::*;

    use crate::import::Report;

    fn task(args: &[&str]) -> Result<Task> {
        parse(args.iter().map(Into::into).collect())
    }

    #[test]
    fn the_usage_names_the_export_folder_as_new() {
        let usage = Error::Usage.to_string();
        assert_eq!(usage, USAGE);
        assert!(
            usage.contains("export <new folder> [<world name>]"),
            "{usage}"
        );
        assert!(
            usage.contains("import <world folder> [<world name>]"),
            "{usage}"
        );
    }

    #[test]
    fn the_import_report_names_the_file_written() {
        let file = crate::storage::Place::world(std::path::Path::new("srv"), "lobby").file;
        let text = imported("lobby", &file, &Report::default());
        assert!(
            text.starts_with(&format!(
                "imported the world \"lobby\" as {}:",
                file.display()
            )),
            "{text}"
        );
        assert!(!text.contains("data/world"), "{text}");
    }

    #[test]
    fn other_arguments_run_the_server() {
        for args in [
            &[][..],
            &["--port", "1"],
            &["Import"],
            &["Export"],
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
                dir: PathBuf::from("C:/worlds/lobby"),
                world: None
            }
        );
        assert_eq!(
            task(&["import", "w", "my world"]).expect("ok"),
            Task::Import {
                dir: PathBuf::from("w"),
                world: Some("my world".to_owned())
            }
        );
        assert_eq!(
            task(&["export", "C:/server/worlds/lobby"]).expect("ok"),
            Task::Export {
                dir: PathBuf::from("C:/server/worlds/lobby"),
                world: None
            }
        );
        assert_eq!(
            task(&["export", "out", "my world"]).expect("ok"),
            Task::Export {
                dir: PathBuf::from("out"),
                world: Some("my world".to_owned())
            }
        );
        for args in [
            &["update-config", "x"][..],
            &["import"],
            &["import", "w", "a", "b"],
            &["export"],
            &["export", "w", "a", "b"],
        ] {
            assert!(matches!(task(args), Err(Error::Usage)), "{args:?}");
        }
    }
}
