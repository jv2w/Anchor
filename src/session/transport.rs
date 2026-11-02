//! The connection a session runs on: RakNet or NetherNet, behind one set of calls.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;

use crate::crypto::PublicKey;
use crate::nethernet;
use crate::raknet;
use crate::tick::queue::LeaveReason;

/// One accepted connection of either transport.
#[derive(Debug)]
pub(super) enum Transport {
    Raknet(raknet::Connection),
    Nethernet(nethernet::Connection),
}

impl Transport {
    /// The client address.
    pub(super) fn addr(&self) -> SocketAddr {
        match self {
            Self::Raknet(c) => c.addr(),
            Self::Nethernet(c) => c.addr(),
        }
    }

    /// Whether the transport carries Minecraft's own encryption after the handshake.
    /// RakNet does, whether or not `enable-encryption` turns it on; NetherNet relies on DTLS.
    pub(super) fn can_encrypt(&self) -> bool {
        matches!(self, Self::Raknet(_))
    }

    /// Why the connection ended, once [`recv`](Self::recv) returned `None`.
    pub(super) fn leave_reason(&self) -> LeaveReason {
        match self {
            Self::Raknet(c) => c.leave_reason(),
            Self::Nethernet(c) => c.leave_reason(),
        }
    }

    /// The latest round trip time the transport measured; `None` until it measured one.
    /// RakNet measures it with its connected ping, NetherNet with the ICE consent check.
    pub(super) fn ping(&self) -> Option<Duration> {
        match self {
            Self::Raknet(c) => c.ping(),
            Self::Nethernet(c) => c.ping(),
        }
    }

    /// The client key the NetherNet offer proved, which the Login must carry.
    /// `None` for RakNet and for offline offers without an identity.
    pub(super) fn proven_key(&self) -> Option<&PublicKey> {
        match self {
            Self::Raknet(_) => None,
            Self::Nethernet(c) => c.client_key(),
        }
    }

    /// The bytes sent on the connection and not delivered yet, as the transport reports them.
    /// That is what RakNet has not had acknowledged, or what NetherNet holds in its queue and
    /// SCTP. Changes wake [`watch::Receiver::changed`], which fails once the connection is gone.
    pub(super) fn undelivered(&self) -> watch::Receiver<usize> {
        match self {
            Self::Raknet(c) => c.undelivered(),
            Self::Nethernet(c) => c.undelivered(),
        }
    }

    /// Closes the connection after what is queued has been delivered.
    pub(super) fn close(&self) {
        match self {
            Self::Raknet(c) => c.close(),
            Self::Nethernet(c) => c.close(),
        }
    }

    /// Moves the connection out of the pre-login limits once the player logged in.
    pub(super) fn logged_in(&self) {
        match self {
            Self::Raknet(c) => c.logged_in(),
            Self::Nethernet(c) => c.logged_in(),
        }
    }

    /// Receives the next game message; `None` once the connection is closed.
    pub(super) async fn recv(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Raknet(c) => c.recv().await,
            Self::Nethernet(c) => c.recv().await,
        }
    }

    /// Queues a game message; waits while the transport is busy.
    pub(super) async fn send(&self, message: Vec<u8>) -> Result<()> {
        match self {
            Self::Raknet(c) => c.send(message).await.map_err(Error::Raknet),
            Self::Nethernet(c) => c.send(message).await.map_err(Error::Nethernet),
        }
    }
}

/// A transport failure; the connection is closed.
#[derive(Debug)]
pub(super) enum Error {
    Raknet(raknet::Error),
    Nethernet(nethernet::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Raknet(_) => f.write_str("raknet connection failed"),
            Self::Nethernet(_) => f.write_str("nethernet connection failed"),
        }
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

/// A result with the module's [`enum@Error`].
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;
