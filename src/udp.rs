//! The one UDP port both transports share (`server-port`).
//!
//! One task owns the port's two sockets (IPv4 and IPv6), the RakNet driver and the NetherNet
//! driver: it hands each datagram to the transport its first bytes name and runs both drivers'
//! commands and timers. So no connection state needs a lock, no datagram is copied to another
//! task, and a server behind NAT needs this one port forwarded for either transport.

use std::fmt;
use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, trace};

use crate::{nethernet, raknet};

/// Kernel socket buffers: a burst from go-raknet's dialer, which has no congestion control,
/// is several MiB. Linux caps this at `net.core.rmem_max` without an error.
const SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
/// Datagrams read from one socket before the task serves its other inputs.
const RECV_BATCH: usize = 256;
/// How often both drivers' timers run: RakNet ACKs, resends, pings and timeouts; NetherNet
/// str0m timeouts, deadlines and session flags.
pub(crate) const UPDATE_INTERVAL: Duration = Duration::from_millis(10);
/// The receive buffer: larger than any datagram either transport accepts, so that an oversized
/// one is seen.
pub(crate) const RECV_BUFFER_BYTES: usize = 2048;
const _: () = assert!(RECV_BUFFER_BYTES > raknet::MAX_DATAGRAM_BYTES);
const _: () = assert!(RECV_BUFFER_BYTES > nethernet::MAX_DATAGRAM);

/// A failure to open the port's sockets.
#[derive(Debug)]
pub(crate) enum Error {
    /// Neither an IPv4 nor an IPv6 address was given.
    NoAddress,
    /// The UDP socket on `addr` could not be opened.
    Bind { addr: SocketAddr, source: io::Error },
}

/// A UDP port result.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAddress => f.write_str("no address to open a udp socket on"),
            Self::Bind { addr, .. } => write!(f, "could not open the udp socket {addr}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoAddress => None,
            Self::Bind { source, .. } => Some(source),
        }
    }
}

/// The port's sockets, shared by the task that reads them and the drivers that send on them.
#[derive(Debug)]
pub(crate) struct Sockets {
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    local_addrs: Vec<SocketAddr>,
}

impl Sockets {
    /// The sockets bound on `v4` and `v6` (IPv6 only) on the current tokio runtime; at least one
    /// address is needed.
    pub(crate) fn bind(v4: Option<SocketAddrV4>, v6: Option<SocketAddrV6>) -> Result<Arc<Self>> {
        let v4 = v4.map(|addr| bind(addr.into())).transpose()?;
        let v6 = v6.map(|addr| bind(addr.into())).transpose()?;
        let local_addrs: Vec<SocketAddr> = v4.iter().chain(&v6).map(|&(_, local)| local).collect();
        if local_addrs.is_empty() {
            return Err(Error::NoAddress);
        }
        Ok(Arc::new(Self {
            v4: v4.map(|(socket, _)| socket),
            v6: v6.map(|(socket, _)| socket),
            local_addrs,
        }))
    }

    /// The bound addresses (IPv4 first).
    pub(crate) fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// The IPv4 socket, for a test that reads it itself.
    #[cfg(test)]
    pub(crate) fn v4(&self) -> &UdpSocket {
        self.v4.as_ref().expect("an IPv4 socket")
    }

    /// Sends `datagram` to `to` without waiting; a full send buffer is an error.
    pub(crate) fn send(&self, to: SocketAddr, datagram: &[u8]) -> io::Result<()> {
        let socket = match to {
            SocketAddr::V4(_) => &self.v4,
            SocketAddr::V6(_) => &self.v6,
        };
        let Some(socket) = socket else {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no socket for the address family",
            ));
        };
        socket.try_send_to(datagram, to).map(drop)
    }
}

/// A handle to the task.
#[derive(Debug)]
pub(crate) struct Handle {
    local_addrs: Vec<SocketAddr>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Handle {
    /// The bound addresses (IPv4 first).
    pub(crate) fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// Stops accepting on both transports, closes every connection (each gets up to 5 s to
    /// deliver what it queued) and waits for the task to end. Dropping the handle does the same
    /// without waiting.
    pub(crate) async fn shutdown(self) {
        if self.stop.send(()).is_err() {
            debug!("udp task already stopped");
        }
        if let Err(error) = self.task.await {
            error!(
                error = &error as &(dyn std::error::Error + 'static),
                "udp task failed"
            );
        }
    }
}

/// The task: the sockets and both drivers.
struct Driver {
    sockets: Arc<Sockets>,
    raknet: raknet::Driver,
    nethernet: nethernet::Driver,
    buf: Vec<u8>,
}

impl Driver {
    async fn run(mut self, mut stopped: oneshot::Receiver<()>) {
        // Cloned so that the sockets stay borrowed while the drivers are borrowed mutably.
        let sockets = Arc::clone(&self.sockets);
        let mut interval = tokio::time::interval(UPDATE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stopping = false;
        loop {
            tokio::select! {
                Ok(socket) = readable(sockets.v4.as_ref()) => self.receive(socket),
                Ok(socket) = readable(sockets.v6.as_ref()) => self.receive(socket),
                true = self.raknet.serve() => {}
                true = self.nethernet.serve() => {}
                _ = interval.tick() => {
                    self.raknet.tick();
                    self.nethernet.tick();
                }
                _ = &mut stopped, if !stopping => {
                    stopping = true;
                    self.raknet.stop();
                    self.nethernet.stop();
                }
            }
            if stopping && self.raknet.is_empty() && self.nethernet.is_empty() {
                debug!("udp task stopped");
                return;
            }
        }
    }

    fn receive(&mut self, socket: &UdpSocket) {
        for _ in 0..RECV_BATCH {
            match socket.try_recv_from(&mut self.buf) {
                Ok((len, from)) => match self.buf.get(..len) {
                    Some(data) if len < RECV_BUFFER_BYTES => {
                        if nethernet::is_nethernet(data) {
                            self.nethernet.receive(from, data);
                        } else {
                            self.raknet.receive(from, data);
                        }
                    }
                    _ => trace!(%from, len, "udp oversized datagram dropped"),
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                // Windows reports ICMP port unreachable as WSAECONNRESET on the next receive
                // (tokio-rs/tokio#2017); that and other per-datagram errors are skipped.
                Err(error) => trace!(
                    error = &error as &(dyn std::error::Error + 'static),
                    "udp receive error skipped"
                ),
            }
        }
    }
}

/// Opens a nonblocking UDP socket on `addr` with large kernel buffers, and returns it with the
/// address it is bound to (the port the OS chose for port 0).
pub(crate) fn bind(addr: SocketAddr) -> Result<(UdpSocket, SocketAddr)> {
    open(addr).map_err(|source| Error::Bind { addr, source })
}

/// The UDP socket on `addr` as an operator reads it: protocol, address, the settings that decide
/// it and its use.
pub(crate) fn socket_text(addr: SocketAddr) -> String {
    let settings = if addr.is_ipv6() {
        "server-port, enable-ipv6"
    } else {
        "server-port"
    };
    format!("UDP {addr} ({settings}, RakNet and NetherNet)")
}

/// Spawns the task that runs both drivers on `sockets`, on the current tokio runtime.
pub(crate) fn spawn(
    sockets: Arc<Sockets>,
    raknet: raknet::Driver,
    nethernet: nethernet::Driver,
) -> Handle {
    let local_addrs = sockets.local_addrs().to_vec();
    let (stop, stopped) = oneshot::channel();
    let driver = Driver {
        sockets,
        raknet,
        nethernet,
        buf: vec![0; RECV_BUFFER_BYTES],
    };
    let task = tokio::spawn(driver.run(stopped));
    Handle {
        local_addrs,
        stop,
        task,
    }
}

fn open(addr: SocketAddr) -> io::Result<(UdpSocket, SocketAddr)> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        // Lets the IPv4 socket bind the same port on every OS.
        socket.set_only_v6(true)?;
    }
    socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES)?;
    socket.set_send_buffer_size(SOCKET_BUFFER_BYTES)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    let socket = UdpSocket::from_std(socket.into())?;
    let local_addr = socket.local_addr()?;
    Ok((socket, local_addr))
}

/// Waits until `socket` has a datagram and returns it; without a socket, waits forever. Its only
/// error is a runtime that is shutting down, which ends the task anyway.
async fn readable(socket: Option<&UdpSocket>) -> io::Result<&UdpSocket> {
    let Some(socket) = socket else {
        return std::future::pending().await;
    };
    socket.readable().await?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{Ipv4Addr, Ipv6Addr};

    use crate::raknet::tests::{dual_stack_ips, has_ipv6_loopback, run};

    /// The next datagram on `socket` and its sender.
    async fn receive(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut buf = vec![0; RECV_BUFFER_BYTES];
        let received = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf));
        let (len, from) = received.await.unwrap().unwrap();
        buf.truncate(len);
        (buf, from)
    }

    /// The IPv4 and the IPv6 socket share one port, each reads only its own family's datagrams,
    /// and both bind the port again right after they close.
    #[test]
    fn ipv4_and_ipv6_sockets_share_a_port_and_rebind_it() {
        run(async {
            if !has_ipv6_loopback() {
                return;
            }
            let (ip4, ip6) = dual_stack_ips();
            let (v4, local4) = bind(SocketAddr::from((ip4, 0))).unwrap();
            let port = local4.port();
            let (v6, _) = bind(SocketAddr::from((ip6, port))).unwrap();
            assert!(socket2::SockRef::from(&v6).only_v6().unwrap());

            let client4 = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let client6 = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
            // The IPv4 datagram goes first: an IPv6 socket taking IPv4 traffic would read it
            // first.
            client4
                .send_to(b"v4", (Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            client6
                .send_to(b"v6", (Ipv6Addr::LOCALHOST, port))
                .await
                .unwrap();
            let from4 = client4.local_addr().unwrap();
            let from6 = client6.local_addr().unwrap();
            assert_eq!(receive(&v4).await, (b"v4".to_vec(), from4));
            assert_eq!(receive(&v6).await, (b"v6".to_vec(), from6));

            drop((v4, v6));
            bind(SocketAddr::from((ip4, port))).unwrap();
            bind(SocketAddr::from((ip6, port))).unwrap();
        });
    }

    #[test]
    fn binding_needs_an_address() {
        run(async {
            assert!(matches!(Sockets::bind(None, None), Err(Error::NoAddress)));
            let sockets = Sockets::bind(Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)), None);
            let sockets = sockets.unwrap();
            let to = SocketAddr::from((Ipv6Addr::LOCALHOST, 1));
            assert!(sockets.send(to, b"no ipv6 socket").is_err());
        });
    }

    /// A port that is in use fails with the address of the socket that could not be opened.
    #[test]
    fn a_port_in_use_is_reported_with_its_address() {
        run(async {
            let localhost = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
            let taken = Sockets::bind(Some(localhost), None).unwrap();
            let SocketAddr::V4(addr) = taken.local_addrs()[0] else {
                panic!("the socket is not IPv4");
            };
            let Err(Error::Bind {
                addr: failed,
                source,
            }) = Sockets::bind(Some(addr), None)
            else {
                panic!("the port was free");
            };
            assert_eq!(failed, SocketAddr::V4(addr));
            assert_eq!(source.kind(), io::ErrorKind::AddrInUse);
        });
    }
}
