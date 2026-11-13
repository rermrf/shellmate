//! The background tokio runtime all SSH work runs on.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

use crate::config::{ConnectOptions, Route};
use crate::connection::Connection;
use crate::error::SshError;

/// A small multi-thread tokio runtime (2 workers). The app creates one at
/// startup and keeps it alive for as long as SSH sessions may exist; it is
/// cheap to clone.
///
/// Futures spawned here can be awaited from any executor (the returned
/// [`JoinHandle`] only needs a waker), so the gpui side simply awaits them.
#[derive(Clone)]
pub struct SshRuntime {
    runtime: Arc<Runtime>,
}

impl SshRuntime {
    /// Starts the runtime.
    pub fn new() -> io::Result<Self> {
        let runtime =
            Builder::new_multi_thread().worker_threads(2).thread_name("shellmate-ssh").enable_all().build()?;
        Ok(Self { runtime: Arc::new(runtime) })
    }

    /// Handle for spawning from other threads.
    pub fn handle(&self) -> Handle {
        self.runtime.handle().clone()
    }

    /// Runs `future` on the runtime.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    /// Connects along `route` on the runtime. Abort the returned handle to
    /// cancel a connection attempt in progress.
    pub fn connect(&self, route: Route, options: ConnectOptions) -> JoinHandle<Result<Connection, SshError>> {
        self.spawn(async move { Connection::connect(route, options).await })
    }
}

impl std::fmt::Debug for SshRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SshRuntime")
    }
}
