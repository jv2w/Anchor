//! The crate error returned by the public API.

use std::fmt;
use std::io;

use crate::args::USAGE;
use crate::{command, config, data, entity, export, import, pack, storage, udp};

/// A failure to build or run a [`Server`](crate::Server).
///
/// Its `Debug` form is the message followed by each cause, as `fn main() -> anchor::Result<()>`
/// prints it.
#[non_exhaustive]
pub enum Error {
    /// The settings are invalid; [`source`](std::error::Error::source) names the setting.
    Config(config::Error),
    /// The runtime, a thread (tick, save, CPU pool, console, console log) or the startup state
    /// (server key, random numbers, shared spawn batches, the blocking tasks that load resource
    /// packs, the NetherNet identity key and TLS files, the NetherNet DTLS certificate, the
    /// NetherNet signaling task) could not be started.
    Start(io::Error),
    /// A network socket could not be opened.
    Network {
        /// The socket as an operator reads it: protocol, address, the settings that decide it
        /// and its use, such as `TCP [::]:19132 (server-port, enable-ipv6, NetherNet signaling)`.
        socket: String,
        /// The cause.
        source: io::Error,
    },
    /// The NetherNet identity key `data/identity.key` exists but cannot be read or holds no valid
    /// key, or is missing and a new one cannot be made or written. A file that exists is left as
    /// it is, since a new key would make every player confirm the server again.
    IdentityKey(Box<dyn std::error::Error + Send + Sync>),
    /// `nethernet-tls-cert` or `nethernet-tls-key` cannot be read or used.
    TlsCertificate(Box<dyn std::error::Error + Send + Sync>),
    /// With `xbox-auth = true`, the authorization keys could neither be fetched nor read from
    /// `data/auth.json`, or that file is damaged (it is left as it is).
    Auth(Box<dyn std::error::Error + Send + Sync>),
    /// The tick thread panicked; the server stopped without saving.
    TickPanicked {
        /// The panic message, or a placeholder if the payload was not text.
        message: String,
    },
    /// A data file (`data/*.redb`, `data/worlds/*.redb`) failed its integrity check, or a value it
    /// holds is damaged. A start refuses such a file, an export that reads one fails with this,
    /// and a running server whose save keeps failing on a corrupted file stops with it. The file
    /// is left exactly as it was; a backup from its folder in `data/backups/` can replace it.
    WorldDamaged(Box<dyn std::error::Error + Send + Sync>),
    /// A data file was last saved by a newer Anchor; the file is left as it was.
    WorldNewerFormat {
        /// The file's format version.
        version: u16,
        /// The newest format version this Anchor reads.
        supported: u16,
    },
    /// A data file could not be opened, read or saved: another process has it open, I/O failed,
    /// a save still failed after its retries, or the save thread panicked.
    Storage(Box<dyn std::error::Error + Send + Sync>),
    /// Two worlds have the same name, or names that differ only in letter case. The default world
    /// (`level-name`) is one of the worlds, so [`ServerBuilder::world`](crate::ServerBuilder::world)
    /// cannot declare its name again.
    DuplicateWorld {
        /// The name.
        name: String,
    },
    /// A world's name cannot be its file's name (`data/worlds/<name>.redb`): see
    /// [`world::Error::InvalidName`](crate::world::Error::InvalidName) for the rule. Names that
    /// differ only in letter case are one world.
    InvalidWorldName {
        /// The name.
        name: String,
    },
    /// Two generator types have the same [`name`](crate::world::Generator::name).
    DuplicateGenerator {
        /// The name.
        name: String,
    },
    /// The spawn the generator of a new world gives is where a player would stand outside the
    /// world border ([`world::Error::OutOfRange`](crate::world::Error::OutOfRange)).
    GeneratorSpawn {
        /// The world's name.
        name: String,
    },
    /// The registered commands or permissions break a rule; the source says which.
    Command(command::Error),
    /// A registered custom entity type breaks a rule; the source says which.
    CustomEntity(entity::Error),
    /// The registered custom items, blocks or recipes break a rule; the source says which.
    Custom(crate::custom::Error),
    /// The registered user data types break a rule, or a saved server data value does not
    /// deserialize into its registered type (the value stays as saved); the source says which.
    Data(data::Error),
    /// A resource pack cannot be read or offered; the source says why. Lets a program's `main`
    /// use `?` on [`Pack::from_bytes`](crate::pack::Pack::from_bytes) and the other pack calls.
    Pack(pack::Error),
    /// [`Server::import_world`](crate::Server::import_world) could not import the world; the
    /// source says why. No world file is left.
    Import(import::Error),
    /// [`Server::export_world`](crate::Server::export_world) could not export the world; the
    /// source says why. No world folder is left.
    Export(export::Error),
    /// The program was started with `update-config`, `import` or `export` but not the arguments
    /// the command takes: `update-config` alone, `import <world folder> [<world name>]` or
    /// `export <new folder> [<world name>]`.
    Usage,
    /// The answer to a command of the process arguments could not be written to standard
    /// output.
    Output(io::Error),
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(f, self)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(_) => f.write_str("invalid settings"),
            Self::Start(_) => {
                f.write_str("could not start the runtime, a thread or the startup state")
            }
            Self::Network { socket, .. } => write!(f, "could not open {socket}"),
            Self::IdentityKey(_) => {
                f.write_str("the nethernet identity key could not be read or created")
            }
            Self::TlsCertificate(_) => f.write_str("the nethernet tls certificate is unusable"),
            Self::Auth(_) => f.write_str("could not load the authorization keys"),
            Self::TickPanicked { message } => write!(f, "the tick thread panicked: {message}"),
            Self::WorldDamaged(_) => f.write_str("a data file is damaged"),
            Self::WorldNewerFormat { version, supported } => write!(
                f,
                "a data file has format {version}, newer than the supported {supported}"
            ),
            Self::Storage(_) => f.write_str("a data file could not be opened, read or saved"),
            Self::DuplicateWorld { name } => write!(f, "two worlds are named {name:?}"),
            Self::InvalidWorldName { name } => {
                write!(f, "the world name {name:?} cannot be a file name")
            }
            Self::DuplicateGenerator { name } => {
                write!(f, "two generator types are named {name:?}")
            }
            Self::GeneratorSpawn { name } => write!(
                f,
                "the generator's spawn for the world {name:?} is outside the world border"
            ),
            Self::Command(_) => f.write_str("a registered command or permission is invalid"),
            Self::CustomEntity(_) => f.write_str("a registered custom entity type is invalid"),
            Self::Custom(_) => f.write_str("a registered custom item, block or recipe is invalid"),
            Self::Data(_) => f.write_str("a registered data type is invalid"),
            Self::Pack(_) => f.write_str("a resource pack is invalid"),
            Self::Import(_) => f.write_str("the world could not be imported"),
            Self::Export(_) => f.write_str("the world could not be exported"),
            Self::Output(_) => f.write_str("could not write to standard output"),
            Self::Usage => f.write_str(USAGE),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::Start(e) | Self::Output(e) => Some(e),
            Self::Network { source, .. } => Some(source),
            Self::Command(e) => Some(e),
            Self::CustomEntity(e) => Some(e),
            Self::Custom(e) => Some(e),
            Self::Data(e) => Some(e),
            Self::Pack(e) => Some(e),
            Self::Import(e) => Some(e),
            Self::Export(e) => Some(e),
            Self::Auth(e)
            | Self::IdentityKey(e)
            | Self::TlsCertificate(e)
            | Self::WorldDamaged(e)
            | Self::Storage(e) => Some(e.as_ref()),
            Self::TickPanicked { .. }
            | Self::WorldNewerFormat { .. }
            | Self::DuplicateWorld { .. }
            | Self::InvalidWorldName { .. }
            | Self::DuplicateGenerator { .. }
            | Self::GeneratorSpawn { .. }
            | Self::Usage => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<config::Error> for Error {
    fn from(e: config::Error) -> Self {
        Self::Config(e)
    }
}

impl From<pack::Error> for Error {
    fn from(e: pack::Error) -> Self {
        Self::Pack(e)
    }
}

impl From<import::Error> for Error {
    fn from(e: import::Error) -> Self {
        Self::Import(e)
    }
}

impl From<export::Error> for Error {
    fn from(e: export::Error) -> Self {
        Self::Export(e)
    }
}

impl From<udp::Error> for Error {
    fn from(e: udp::Error) -> Self {
        match e {
            udp::Error::NoAddress => Self::Start(io::Error::other(e)),
            udp::Error::Bind { addr, source } => Self::Network {
                socket: udp::socket_text(addr),
                source,
            },
        }
    }
}

impl From<storage::Error> for Error {
    fn from(e: storage::Error) -> Self {
        match e {
            storage::Error::NewerFormat { version } => Self::WorldNewerFormat {
                version,
                supported: storage::seal::FORMAT_VERSION,
            },
            e if e.is_damage() => Self::WorldDamaged(Box::new(e)),
            e => Self::Storage(Box::new(e)),
        }
    }
}

/// Writes `error` and then each of its causes, separated by `: `: the one form in which an
/// error and its causes appear on one line.
pub(crate) fn write_chain(
    out: &mut impl fmt::Write,
    error: &(dyn std::error::Error + 'static),
) -> fmt::Result {
    write!(out, "{error}")?;
    let mut source = error.source();
    while let Some(e) = source {
        write!(out, ": {e}")?;
        source = e.source();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_writes_the_message_and_its_causes_instead_of_the_variant() {
        let e = Error::Start(io::Error::other("no thread"));
        assert_eq!(
            format!("{e:?}"),
            "could not start the runtime, a thread or the startup state: no thread"
        );
    }

    #[test]
    fn the_start_message_covers_every_thread_and_startup_step() {
        let e = Error::Start(io::Error::other("no thread"));
        assert_eq!(
            e.to_string(),
            "could not start the runtime, a thread or the startup state"
        );
    }

    #[test]
    fn the_identity_key_message_covers_reading_and_creating() {
        let e = Error::IdentityKey(Box::new(io::Error::other("no key")));
        assert_eq!(
            e.to_string(),
            "the nethernet identity key could not be read or created"
        );
    }
}
