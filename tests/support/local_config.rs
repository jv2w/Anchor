//! Settings for the tests that run a server without the network beyond loopback.

use anchor::Config;

/// Settings with offline login (no authorization fetch), the loopback address, and port 0, so the
/// system picks every port at bind time: no test connects to them, and a port picked earlier and
/// released could be taken by another process first.
pub(crate) fn local_config() -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_ip = std::net::Ipv4Addr::LOCALHOST;
    config.general.server_port = 0;
    config
}
