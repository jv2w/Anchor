//! The server and its builder.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::event::{self, Listeners, Priority};
use crate::runtime::{self, Runtime, ThreadCounts};
use crate::session;
use crate::tick::{self, queue::InputSender};

/// A Minecraft: Bedrock Edition server. Make one with [`Server::builder`], then call
/// [`Server::run`].
#[derive(Debug)]
pub struct Server {
    config: Arc<Config>,
    data_dir: PathBuf,
    threads: ThreadCounts,
    listeners: Listeners,
    shutdown: ShutdownHandle,
    shutdown_requests: mpsc::Receiver<()>,
}

/// Collects what a [`Server`] needs; [`build`](Self::build) checks it.
#[derive(Debug)]
pub struct ServerBuilder {
    config: Config,
    data_dir: PathBuf,
    listeners: Listeners,
}

/// Asks a [`Server`] to stop. Cheap to clone and usable from any thread.
#[derive(Debug, Clone)]
pub struct ShutdownHandle(mpsc::Sender<()>);

impl Server {
    /// A builder with [`Config::default`] and the data folder `data`.
    pub fn builder() -> ServerBuilder {
        ServerBuilder {
            config: Config::default(),
            data_dir: PathBuf::from("data"),
            listeners: Listeners::default(),
        }
    }

    /// A handle that stops this server from any thread, before or during [`run`](Self::run).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Runs the server until a [`ShutdownHandle`] or Ctrl+C stops it, blocking the calling
    /// thread.
    ///
    /// On shutdown every player sees `shutdown-message`, networking stops, and the threads are
    /// joined. Fails if the runtime or the tick thread cannot start, if networking cannot start,
    /// or if the tick thread panics.
    pub fn run(self) -> Result<()> {
        let Self {
            config,
            data_dir,
            threads,
            listeners,
            shutdown,
            shutdown_requests,
        } = self;
        // Built before any thread starts: encoding the shared spawn batches takes a while once.
        let shared = tick::Shared::new(config.batch_settings()?)
            .map_err(|e| Error::Start(std::io::Error::other(e)))?;
        let runtime = Runtime::new(threads)?;
        let pool = runtime::cpu_pool(threads.cpu)?;
        let (inputs, input_queue) = tick::queue::input_queue();
        let (stop_tick, stop) = mpsc::channel(1);
        let (done, tick_done) = oneshot::channel();
        let (crash, mut crash_log) = oneshot::channel();
        let state = tick::State::new(tick::Settings::new(&config), shared, listeners);
        let tick_thread = runtime::spawn_tick(move || {
            tick::run(state, pool, input_queue, stop, crash, done);
        })?;
        info!(
            network_threads = threads.network,
            cpu_threads = threads.cpu,
            "server starting"
        );
        let running = Running {
            shutdown,
            shutdown_requests,
            stop_tick,
            tick_done,
        };
        let served = runtime.block_on(running.serve(config, &data_dir, inputs));
        // `serve` returns only after the tick thread has ended, so this does not wait long.
        let joined = tick_thread.join();
        runtime.shutdown();
        match joined {
            Ok(()) => {
                info!("server stopped");
                served
            }
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                error!(%message, "tick thread panicked; stopped without saving");
                if let Ok(log) = crash_log.try_recv() {
                    match tick::write_crash_log(&data_dir, &log) {
                        Ok(path) => error!(
                            path = %path.display(),
                            truncated = log.is_truncated(),
                            "input log written"
                        ),
                        Err(e) => error!(error = %e, "input log not written"),
                    }
                }
                Err(Error::TickPanicked { message })
            }
        }
    }
}

/// The parts of a running server that `serve` waits on.
struct Running {
    /// Kept so that `shutdown_requests` stays open while the server runs.
    shutdown: ShutdownHandle,
    shutdown_requests: mpsc::Receiver<()>,
    stop_tick: mpsc::Sender<()>,
    /// Resolves when the tick thread ends, normally or by panic. Polled once at most.
    tick_done: oneshot::Receiver<()>,
}

impl Running {
    /// Starts networking, waits for a shutdown request or the end of the tick thread, then stops
    /// the tick (which disconnects every player) and networking.
    async fn serve(self, config: Arc<Config>, data_dir: &Path, inputs: InputSender) -> Result<()> {
        let Self {
            shutdown,
            mut shutdown_requests,
            stop_tick,
            mut tick_done,
        } = self;
        let ctrl_c = shutdown.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    info!("Ctrl+C received; shutting down");
                    ctrl_c.shutdown();
                }
                Err(e) => warn!(error = %e, "cannot listen for Ctrl+C"),
            }
        });
        let network = match session::start(config, data_dir, inputs).await {
            Ok(network) => network,
            Err(e) => {
                stop(&stop_tick, tick_done).await;
                return Err(e);
            }
        };
        info!("server started");
        let tick_ended = tokio::select! {
            // `None` cannot happen while `shutdown` is alive; both mean stop.
            _ = shutdown_requests.recv() => false,
            _ = &mut tick_done => true,
        };
        if !tick_ended {
            info!("shutting down");
            stop(&stop_tick, tick_done).await;
        }
        network.stop().await;
        drop(shutdown);
        Ok(())
    }
}

/// Asks the tick thread to stop and waits until it has ended.
async fn stop(stop_tick: &mpsc::Sender<()>, tick_done: oneshot::Receiver<()>) {
    match stop_tick.try_send(()) {
        // Full: a stop is already queued. Closed: the tick thread has already ended.
        Ok(()) | Err(TrySendError::Full(()) | TrySendError::Closed(())) => {}
    }
    match tick_done.await {
        // The thread never sends; the sender dropping when it ends is the signal.
        Ok(()) | Err(oneshot::error::RecvError { .. }) => {}
    }
}

/// The text of a panic payload.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is not text".to_owned())
}

impl ServerBuilder {
    /// Uses `config` instead of [`Config::default`]. Without this call Anchor reads and writes no
    /// settings file.
    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Uses `path` for the world, the authorization cache and crash reports instead of `data`.
    #[must_use]
    pub fn data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.data_dir = path.into();
        self
    }

    /// Calls `listener` on every `E` event at `priority`, in the tick, after the listeners of
    /// lower priority and those added earlier at the same priority.
    #[doc(alias = "PluginManager::registerEvent")]
    #[must_use]
    pub fn on<E: event::Event>(
        mut self,
        priority: Priority,
        listener: impl FnMut(&mut E) + Send + 'static,
    ) -> Self {
        self.listeners.push(priority, listener);
        self
    }

    /// Checks the settings and the thread budget, and puts the listeners in calling order.
    /// Touches no file.
    pub fn build(mut self) -> Result<Server> {
        self.config.validate()?;
        let developer = &self.config.developer;
        let threads = ThreadCounts::resolve(
            developer.network_threads,
            developer.cpu_threads,
            runtime::logical_cpus(),
        )?;
        self.listeners.sort();
        let (requests, shutdown_requests) = mpsc::channel(1);
        Ok(Server {
            config: Arc::new(self.config),
            data_dir: self.data_dir,
            threads,
            listeners: self.listeners,
            shutdown: ShutdownHandle(requests),
            shutdown_requests,
        })
    }
}

impl ShutdownHandle {
    /// Asks the server to stop: every player sees `shutdown-message`, then [`Server::run`]
    /// returns. Returns at once; repeated calls, and calls after the server stopped, do nothing.
    #[doc(alias = "Server::shutdown")]
    pub fn shutdown(&self) {
        match self.0.try_send(()) {
            // Full: a request is already pending. Closed: the server has already stopped.
            Ok(()) | Err(TrySendError::Full(()) | TrySendError::Closed(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_tick_thread_is_joined_with_its_message() {
        let thread = runtime::spawn_tick(|| panic!("tick {} failed", 7)).unwrap();
        let payload = thread.join().unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "tick 7 failed");
        let thread = runtime::spawn_tick(|| std::panic::panic_any(5_u8)).unwrap();
        let payload = thread.join().unwrap_err();
        assert_eq!(
            panic_message(payload.as_ref()),
            "a payload that is not text"
        );
        assert_eq!(panic_message(&"static"), "static");
    }
}
