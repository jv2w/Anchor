//! Public API of `Server`: building, the thread budget, and running until shutdown.

#![cfg(test)]

use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anchor::config::Threads;
use anchor::{Config, Error, Server, ShutdownHandle};

/// A path in Cargo's test temporary folder, unique to this process and test; not created.
fn temp_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("server-{}-{name}", std::process::id()))
}

/// Settings that run without the network beyond loopback: offline login (no authorization
/// fetch) and a free UDP port.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the test finds a free port"
)]
fn local_config() -> Config {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("free port");
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = socket.local_addr().expect("local address").port();
    config
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn exists(path: &Path) -> bool {
    path.exists()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|e| e.expect("read a folder entry").path())
        .collect();
    entries.sort();
    entries
}

#[test]
fn the_error_and_the_handle_cross_threads() {
    fn shareable<T: Send + Sync + 'static>() {}
    shareable::<Error>();
    shareable::<ShutdownHandle>();
}

#[test]
fn default_settings_build() {
    Server::builder().build().unwrap();
}

#[test]
fn building_without_config_touches_no_file() {
    let data = temp_path("untouched");
    let cwd = std::env::current_dir().unwrap();
    let before = entries(&cwd);
    drop(Server::builder().data_dir(&data).build().unwrap());
    assert!(!exists(&data), "build() created the data folder");
    assert_eq!(entries(&cwd), before, "build() changed the working folder");
}

#[test]
fn thread_counts_over_the_budget_are_rejected_naming_the_keys() {
    let mut config = Config::default();
    config.developer.network_threads = Threads::Count(NonZeroU16::MAX);
    config.developer.cpu_threads = Threads::Count(NonZeroU16::MAX);
    let err = Server::builder().config(config).build().unwrap_err();
    assert!(matches!(
        err,
        Error::Threads {
            network: 65_535,
            cpu: 65_535,
            ..
        }
    ));
    let message = err.to_string();
    assert!(message.contains("network-threads") && message.contains("cpu-threads"));
}

#[test]
fn the_smallest_explicit_thread_counts_always_fit() {
    let one = Threads::Count(NonZeroU16::MIN);
    let mut config = Config::default();
    config.developer.network_threads = one;
    config.developer.cpu_threads = one;
    // 1 + 1 + 2 never exceeds max(logical CPUs, 4).
    Server::builder().config(config).build().unwrap();
}

#[test]
fn a_shutdown_before_run_stops_it_at_once() {
    let server = Server::builder()
        .config(local_config())
        .data_dir(temp_path("early"))
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    handle.shutdown();
    handle.shutdown();
    server.run().unwrap();
    // After the server stopped, a request does nothing.
    handle.shutdown();
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test stops the server from its own thread after a delay"
)]
fn a_shutdown_from_another_thread_ends_run() {
    let server = Server::builder()
        .config(local_config())
        .data_dir(temp_path("running"))
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    let started = Instant::now();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        handle.shutdown();
    });
    server.run().unwrap();
    stopper.join().unwrap();
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "run returned early");
    assert!(took < Duration::from_secs(5), "run took {took:?} to stop");
}
