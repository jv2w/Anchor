//! The connection a session runs on: RakNet or NetherNet, behind one set of calls.

use std::fmt;
use std::net::SocketAddr;

use crate::crypto::PublicKey;
use crate::nethernet;
use crate::raknet;

/// One accepted connection of either transport.
#[derive(Debug)]
pub(super) enum Transport {
    Raknet(raknet::Connection),
    Nethernet(nethernet::Connection),
}

/// A transport failure; the connection is closed.
#[derive(Debug)]
pub(super) enum Error {
    Raknet(raknet::Error),
    Nethernet(nethernet::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Raknet(_) => "raknet connection failed",
            Self::Nethernet(_) => "nethernet connection failed",
        })
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Raknet(e) => Some(e),
            Self::Nethernet(e) => Some(e),
        }
    }
}

impl Transport {
    /// The client address.
    pub(super) fn addr(&self) -> SocketAddr {
        match self {
            Self::Raknet(c) => c.addr(),
            Self::Nethernet(c) => c.addr(),
        }
    }

    /// The next game message, or `None` once the connection is closed.
    pub(super) async fn recv(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Raknet(c) => c.recv().await,
            Self::Nethernet(c) => c.recv().await,
        }
    }

    /// Queues a game message; waits while the transport is busy.
    pub(super) async fn send(&self, message: Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Raknet(c) => c.send(message).await.map_err(Error::Raknet),
            Self::Nethernet(c) => c.send(message).await.map_err(Error::Nethernet),
        }
    }

    /// The latest round trip time the transport measured: the RakNet connected ping, or the
    /// NetherNet ICE consent check; `None` until one was.
    pub(super) fn ping(&self) -> Option<std::time::Duration> {
        match self {
            Self::Raknet(c) => c.ping(),
            Self::Nethernet(c) => c.ping(),
        }
    }

    /// The player logged in: the connection leaves the pre-login limits.
    pub(super) fn logged_in(&self) {
        match self {
            Self::Raknet(c) => c.logged_in(),
            Self::Nethernet(c) => c.logged_in(),
        }
    }

    /// Closes the connection after what is queued has been delivered.
    pub(super) fn close(&self) {
        match self {
            Self::Raknet(c) => c.close(),
            Self::Nethernet(c) => c.close(),
        }
    }

    /// Whether game messages are encrypted after the handshake. NetherNet relies on DTLS and has
    /// no Minecraft encryption (PLAN.md 2.1).
    pub(super) fn encrypts(&self) -> bool {
        matches!(self, Self::Raknet(_))
    }

    /// The client key the NetherNet offer proved, which the Login must carry (PLAN.md 2.1).
    /// `None` for RakNet and for offline offers without an identity.
    pub(super) fn proven_key(&self) -> Option<&PublicKey> {
        match self {
            Self::Raknet(_) => None,
            Self::Nethernet(c) => c.client_key(),
        }
    }
}
