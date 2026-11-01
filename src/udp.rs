//! The one UDP port both transports share (`server-port`).
//!
//! One task owns the port's two sockets (IPv4 and IPv6), the RakNet driver and the NetherNet
//! driver: it hands each datagram to the transport its first bytes name and runs both drivers'
//! commands and timers. So no connection state needs a lock, no datagram is copied to another
//! task, and a player behind NAT needs this one port forwarded for either transport.

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
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;
/// Datagrams read from one socket before the task serves its other inputs.
const RECV_BATCH: usize = 256;
/// How often both drivers' timers run: RakNet ACKs, resends, pings and timeouts; NetherNet
/// str0m timeouts, deadlines and session flags.
pub(crate) const UPDATE_INTERVAL: Duration = Duration::from_millis(10);
/// The receive buffer: larger than any datagram either transport accepts, so that an oversized
/// one is seen.
pub(crate) const RECV_BUFFER: usize = 2048;
const _: () = assert!(RECV_BUFFER > raknet::MAX_DATAGRAM);
const _: () = assert!(RECV_BUFFER > nethernet::MAX_DATAGRAM);

/// The port's sockets, shared by the task that reads them and the drivers that send on them.
#[derive(Debug)]
pub(crate) struct Sockets {
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    local_addrs: Vec<SocketAddr>,
}

impl Sockets {
    /// Binds `v4` and `v6` (IPv6 only) on the current tokio runtime; at least one is needed.
    pub(crate) fn bind(
        v4: Option<SocketAddrV4>,
        v6: Option<SocketAddrV6>,
    ) -> io::Result<Arc<Self>> {
        let v4 = v4.map(|addr| bind(addr.into())).transpose()?;
        let v6 = v6.map(|addr| bind(addr.into())).transpose()?;
        let local_addrs = v4
            .iter()
            .chain(&v6)
            .map(UdpSocket::local_addr)
            .collect::<io::Result<Vec<_>>>()?;
        if local_addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "udp needs an address",
            ));
        }
        Ok(Arc::new(Self {
            v4,
            v6,
            local_addrs,
        }))
    }

    /// The bound addresses (IPv4 first).
    pub(crate) fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
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

    /// The IPv4 socket, for a test that reads it itself.
    #[cfg(test)]
    pub(crate) fn v4(&self) -> &UdpSocket {
        self.v4.as_ref().expect("an IPv4 socket")
    }
}

/// Opens a nonblocking UDP socket on `addr` with large kernel buffers.
pub(crate) fn bind(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        // Lets the IPv4 socket bind the same port on every OS.
        socket.set_only_v6(true)?;
    }
    socket.set_recv_buffer_size(SOCKET_BUFFER)?;
    socket.set_send_buffer_size(SOCKET_BUFFER)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
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
        buf: vec![0; RECV_BUFFER],
    };
    let task = tokio::spawn(driver.run(stopped));
    Handle {
        local_addrs,
        stop,
        task,
    }
}

/// Controls the task.
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
            error!(%error, "udp task failed");
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
        let mut interval = tokio::time::interval(UPDATE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stopping = false;
        loop {
            tokio::select! {
                _ = readable(self.sockets.v4.as_ref()) => self.receive(false),
                _ = readable(self.sockets.v6.as_ref()) => self.receive(true),
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

    fn receive(&mut self, v6: bool) {
        let socket = if v6 {
            &self.sockets.v6
        } else {
            &self.sockets.v4
        };
        let Some(socket) = socket else {
            return;
        };
        for _ in 0..RECV_BATCH {
            match socket.try_recv_from(&mut self.buf) {
                Ok((len, from)) => match self.buf.get(..len) {
                    Some(data) if len < RECV_BUFFER => {
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
                Err(error) => trace!(%error, "udp receive error skipped"),
            }
        }
    }
}

/// Waits until `socket` has a datagram; never returns without a socket.
async fn readable(socket: Option<&UdpSocket>) -> io::Result<()> {
    match socket {
        Some(socket) => socket.readable().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::raknet::tests::{dual_stack_ips, has_ipv6_loopback, run};

    /// The next datagram on `socket` and its sender.
    async fn receive(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut buf = vec![0; RECV_BUFFER];
        let received = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf));
        let (len, from) = received.await.unwrap().unwrap();
        buf.truncate(len);
        (buf, from)
    }

    /// The IPv4 and the IPv6 socket share one port, each reads only its own family's datagrams,
    /// and both bind the port again right after they close.
    #[test]
    fn ipv4_and_ipv6_sockets_share_a_port_and_rebind_it() {
        run(share_a_port_and_rebind_it());
    }

    async fn share_a_port_and_rebind_it() {
        if !has_ipv6_loopback() {
            return;
        }
        let (ip4, ip6) = dual_stack_ips();
        let v4 = bind(SocketAddr::from((ip4, 0))).unwrap();
        let port = v4.local_addr().unwrap().port();
        let v6 = bind(SocketAddr::from((ip6, port))).unwrap();
        assert!(socket2::SockRef::from(&v6).only_v6().unwrap());

        let client4 = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client6 = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        // The IPv4 datagram goes first: an IPv6 socket taking IPv4 traffic would read it first.
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
    }

    #[test]
    fn binding_needs_an_address() {
        run(async {
            assert!(Sockets::bind(None, None).is_err());
            let sockets = Sockets::bind(Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)), None);
            let sockets = sockets.unwrap();
            let to = SocketAddr::from((Ipv6Addr::LOCALHOST, 1));
            assert!(sockets.send(to, b"no ipv6 socket").is_err());
        });
    }
}
