//! The crate error returned by the public API.

use std::fmt;
use std::io;

use crate::{config, storage};

/// A failure to build or run a [`Server`](crate::Server).
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The settings are invalid; [`source`](std::error::Error::source) names the setting.
    Config(config::Error),
    /// `network-threads` + `cpu-threads` + 2 (the tick and save threads) exceeds the thread
    /// budget, max(logical CPUs, 4).
    Threads {
        /// The `network-threads` value, resolved if it was `"auto"`.
        network: usize,
        /// The `cpu-threads` value, resolved if it was `"auto"`.
        cpu: usize,
        /// The thread budget.
        budget: usize,
    },
    /// The runtime, the tick thread or the server key could not be started.
    Start(io::Error),
    /// The network ports could not be opened.
    Network(io::Error),
    /// The NetherNet identity key `data/identity.key` exists but cannot be read; it is left as it
    /// is, since a new key would make every player confirm the server again.
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
    /// `data/world.redb` failed its integrity check at start, or a value it holds is damaged.
    /// The server did not start, and the file is left exactly as it was; a backup from
    /// `data/backups/` can replace it.
    WorldDamaged(Box<dyn std::error::Error + Send + Sync>),
    /// `data/world.redb` was last saved by a newer Anchor; the file is left as it was.
    WorldNewerFormat {
        /// The file's format version.
        version: u16,
        /// The newest format version this Anchor reads.
        supported: u16,
    },
    /// `data/world.redb` could not be opened, read or saved: another process has it open, I/O
    /// failed, or a save still failed after its retries.
    Storage(Box<dyn std::error::Error + Send + Sync>),
    /// Two worlds have the same name.
    DuplicateWorld {
        /// The name.
        name: String,
    },
}

/// A result with the crate [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(_) => f.write_str("invalid settings"),
            Self::Threads {
                network,
                cpu,
                budget,
            } => write!(
                f,
                "network-threads ({network}) + cpu-threads ({cpu}) + 2 exceeds the thread budget \
                 of {budget}, the larger of the logical CPU count and 4"
            ),
            Self::Start(_) => f.write_str("could not start the runtime or the tick thread"),
            Self::Network(_) => f.write_str("could not open the network ports"),
            Self::IdentityKey(_) => f.write_str("the nethernet identity key is unreadable"),
            Self::TlsCertificate(_) => f.write_str("the nethernet tls certificate is unusable"),
            Self::Auth(_) => f.write_str("authorization keys are unavailable"),
            Self::TickPanicked { message } => write!(f, "the tick thread panicked: {message}"),
            Self::WorldDamaged(_) => f.write_str("the world file is damaged"),
            Self::WorldNewerFormat { version, supported } => write!(
                f,
                "the world file has format {version}, newer than the supported {supported}"
            ),
            Self::Storage(_) => f.write_str("the world file could not be opened, read or saved"),
            Self::DuplicateWorld { name } => write!(f, "two worlds are named {name:?}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::Start(e) | Self::Network(e) => Some(e),
            Self::Auth(e)
            | Self::IdentityKey(e)
            | Self::TlsCertificate(e)
            | Self::WorldDamaged(e)
            | Self::Storage(e) => Some(e.as_ref()),
            Self::Threads { .. }
            | Self::TickPanicked { .. }
            | Self::WorldNewerFormat { .. }
            | Self::DuplicateWorld { .. } => None,
        }
    }
}

impl From<config::Error> for Error {
    fn from(e: config::Error) -> Self {
        Self::Config(e)
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
