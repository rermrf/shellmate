//! TCP/IP forwarding on an established connection: `direct-tcpip` channels (the client asks the
//! server to connect somewhere, `-L` / `-D`) and `tcpip-forward` requests (the server listens and
//! hands the connections back as `forwarded-tcpip` channels, `-R`).

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use russh::client::{ChannelOpenHandle, Msg};
use russh::{Channel, ChannelOpenFailure, ChannelStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use crate::connection::{Connection, WeakConnection};
use crate::error::{SshError, TcpipFailure};
use crate::handler::Shared;

/// The byte stream of a `direct-tcpip` channel: what is written goes to the target the server
/// connected to, what that target sends can be read.
///
/// Holds the connection open for as long as it lives, like a shell channel does; dropping it
/// closes the channel. Shutting down the write side sends EOF (half-close).
pub struct TcpipStream {
    stream: ChannelStream<Msg>,
    connection: Connection,
}

/// A connection the server accepted on a remote-forwarded port and passed on to us.
pub struct ForwardedStream {
    stream: ChannelStream<Msg>,
    connection: Connection,
    /// Address of the peer that connected to the server's port.
    pub originator_address: String,
    /// Port of that peer.
    pub originator_port: u16,
}

macro_rules! stream_io {
    ($name:ident) => {
        impl AsyncRead for $name {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.stream).poll_read(cx, buf)
            }
        }

        impl AsyncWrite for $name {
            fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.stream).poll_write(cx, buf)
            }

            fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.stream).poll_flush(cx)
            }

            fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.stream).poll_shutdown(cx)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($name))
            }
        }
    };
}

stream_io!(TcpipStream);
stream_io!(ForwardedStream);

macro_rules! connection_closed {
    ($name:ident) => {
        impl $name {
            /// Resolves when the SSH connection under this stream is gone. A stream can tell the
            /// end of the data from the loss of the connection only by this: both read as end of file.
            pub fn connection_closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
                let connection = self.connection.clone();
                async move { connection.closed().await }
            }
        }
    };
}

connection_closed!(TcpipStream);
connection_closed!(ForwardedStream);

impl Connection {
    /// Asks the server to connect to `host:port` and returns the channel carrying that
    /// connection. `originator` is the address of the local client being served (informational,
    /// the server may log it). Names are resolved by the server.
    ///
    /// Gives up after the connection's time budget. Failures keep the reason the server gave
    /// (see [`TcpipFailure`]). Must run inside the SSH runtime.
    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
        originator: SocketAddr,
    ) -> Result<TcpipStream, SshError> {
        let fail = |reason| SshError::Tcpip { host: host.to_string(), port, reason };
        let opened = self.inner.handle.channel_open_direct_tcpip(
            host.to_string(),
            port.into(),
            originator.ip().to_string(),
            originator.port().into(),
        );
        let channel = match tokio::time::timeout(self.inner.watchdog_limit, opened).await {
            Err(_) => return Err(fail(TcpipFailure::Timeout)),
            Ok(Ok(channel)) => channel,
            Ok(Err(russh::Error::ChannelOpenFailure(failure))) => return Err(fail(failure_reason(&failure))),
            Ok(Err(_)) => return Err(fail(TcpipFailure::Disconnected)),
        };
        Ok(TcpipStream { stream: channel.into_stream(), connection: self.clone() })
    }

    /// Asks the server to listen on `address:port` (`tcpip-forward`; port 0 lets the server pick)
    /// and returns the registration that receives the connections it accepts. Connections to
    /// ports nobody registered are refused.
    ///
    /// [`RemoteForward::port`] is the port the server actually listens on. Dropping the
    /// registration stops delivery locally; [`cancel_remote_forward`](Self::cancel_remote_forward)
    /// also tells the server to stop listening. Must run inside the SSH runtime.
    pub async fn request_remote_forward(&self, address: &str, port: u16) -> Result<RemoteForward, SshError> {
        let (sender, incoming) = mpsc::channel(INCOMING_QUEUE);
        let shared = self.inner.shared.clone();
        let id = shared.remote.register(address, (port != 0).then_some(u32::from(port)), sender);
        // From here on a failure must take the registration back.
        let registration = Registration { shared, id };

        let asked = self.inner.handle.tcpip_forward(address, port.into());
        let reported = match tokio::time::timeout(self.inner.watchdog_limit, asked).await {
            Err(_) => return Err(SshError::Timeout { limit: self.inner.watchdog_limit }),
            Ok(Err(russh::Error::RequestDenied)) => return Err(SshError::RemoteForwardDenied),
            Ok(Err(err)) => return Err(SshError::Channel(format!("远程转发请求失败：{err}"))),
            Ok(Ok(reported)) => reported,
        };
        // Only a request for port 0 is answered with the port that was chosen.
        let actual = if port == 0 { reported } else { u32::from(port) };
        let actual = u16::try_from(actual)
            .ok()
            .filter(|actual| *actual != 0)
            .ok_or_else(|| SshError::Channel("服务器没有告知远程转发实际监听的端口".into()))?;
        registration.shared.remote.set_port(id, u32::from(actual));
        Ok(RemoteForward {
            address: address.to_string(),
            port: actual,
            incoming,
            connection: self.downgrade(),
            _registration: registration,
        })
    }

    /// Tells the server to stop listening on `address:port` (the address and the actual port
    /// of a [`RemoteForward`]) and forgets the registration.
    pub async fn cancel_remote_forward(&self, address: &str, port: u16) -> Result<(), SshError> {
        self.inner.shared.remote.remove_bound(address, u32::from(port));
        match tokio::time::timeout(
            self.inner.watchdog_limit,
            self.inner.handle.cancel_tcpip_forward(address, port.into()),
        )
        .await
        {
            Err(_) => Err(SshError::Timeout { limit: self.inner.watchdog_limit }),
            Ok(Err(russh::Error::RequestDenied)) => Err(SshError::Channel("服务器拒绝取消远程转发".into())),
            Ok(Err(err)) => Err(SshError::Channel(format!("取消远程转发失败：{err}"))),
            Ok(Ok(())) => Ok(()),
        }
    }
}

fn failure_reason(failure: &ChannelOpenFailure) -> TcpipFailure {
    match failure {
        ChannelOpenFailure::AdministrativelyProhibited => TcpipFailure::Prohibited,
        ChannelOpenFailure::ConnectFailed => TcpipFailure::ConnectFailed,
        ChannelOpenFailure::ResourceShortage => TcpipFailure::ResourceShortage,
        ChannelOpenFailure::UnknownChannelType | ChannelOpenFailure::Other { .. } => TcpipFailure::Other,
    }
}

/// Connections the server accepted that nobody has picked up yet; beyond this they are refused
/// with "resource shortage" instead of piling up.
const INCOMING_QUEUE: usize = 64;

/// A `tcpip-forward` the server accepted. Receives the connections made to the server's port.
pub struct RemoteForward {
    /// The address that was asked for (the server's listen address as the caller wrote it).
    pub address: String,
    /// The port the server listens on (the real one when 0 was asked for).
    pub port: u16,
    incoming: mpsc::Receiver<Incoming>,
    connection: WeakConnection,
    _registration: Registration,
}

impl RemoteForward {
    /// The next connection made to the server's port; `None` once the connection is gone.
    pub async fn accept(&mut self) -> Option<ForwardedStream> {
        let incoming = self.incoming.recv().await?;
        // A connection that is closing has nothing to forward over any more.
        let connection = self.connection.upgrade()?;
        Some(ForwardedStream {
            stream: incoming.channel.into_stream(),
            connection,
            originator_address: incoming.originator_address,
            originator_port: incoming.originator_port,
        })
    }
}

impl std::fmt::Debug for RemoteForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteForward({}:{})", self.address, self.port)
    }
}

/// Takes the registration back when dropped, so connections to a forward nobody serves any
/// more are refused.
struct Registration {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.shared.remote.remove(self.id);
    }
}

struct Incoming {
    channel: Channel<Msg>,
    originator_address: String,
    originator_port: u16,
}

struct Entry {
    id: u64,
    address: String,
    /// `None` while a request for port 0 waits for the server to say which port it chose.
    port: Option<u32>,
    sender: mpsc::Sender<Incoming>,
}

/// Who gets the `forwarded-tcpip` channels the server opens. Lives in the connection's shared
/// state and is read by the handler running in russh's session task.
#[derive(Default)]
pub(crate) struct RemoteRegistry {
    entries: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

enum Route {
    Found(mpsc::Sender<Incoming>),
    /// Nothing matches yet, but a port-0 request is still waiting for its answer.
    Pending,
    Nobody,
}

/// How long a channel waits for a port-0 request whose answer is still on its way to the
/// requester (the server sends the answer first, but the requester has to wake up and note it).
const PENDING_WAIT: Duration = Duration::from_millis(10);
const PENDING_TRIES: usize = 50;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Addresses compare without case and brackets (`[::1]` and `::1` are the same).
fn same_address(a: &str, b: &str) -> bool {
    let plain = |text: &str| text.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    plain(a) == plain(b)
}

impl RemoteRegistry {
    fn register(&self, address: &str, port: Option<u32>, sender: mpsc::Sender<Incoming>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.entries).push(Entry { id, address: address.to_string(), port, sender });
        id
    }

    fn set_port(&self, id: u64, port: u32) {
        if let Some(entry) = lock(&self.entries).iter_mut().find(|entry| entry.id == id) {
            entry.port = Some(port);
        }
    }

    /// The connection ended: nobody receives channels any more, and everybody waiting for one
    /// is told so (their queues end once the queued channels are taken).
    pub(crate) fn close(&self) {
        lock(&self.entries).clear();
    }

    fn remove(&self, id: u64) {
        lock(&self.entries).retain(|entry| entry.id != id);
    }

    fn remove_bound(&self, address: &str, port: u32) {
        lock(&self.entries).retain(|entry| !(entry.port == Some(port) && same_address(&entry.address, address)));
    }

    /// Finds the registration for a channel the server opened for `(address, port)`: the exact
    /// pair if there is one, otherwise the only registration on that port — servers do not
    /// always echo the address as it was requested (`localhost` comes back as `127.0.0.1`).
    fn route(&self, address: &str, port: u32) -> Route {
        let entries = lock(&self.entries);
        let on_port: Vec<&Entry> = entries.iter().filter(|entry| entry.port == Some(port)).collect();
        if let Some(exact) = on_port.iter().find(|entry| same_address(&entry.address, address)) {
            return Route::Found(exact.sender.clone());
        }
        if let [only] = on_port.as_slice() {
            return Route::Found(only.sender.clone());
        }
        if on_port.is_empty() && entries.iter().any(|entry| entry.port.is_none()) {
            return Route::Pending;
        }
        Route::Nobody
    }

    /// Hands a channel the server opened to whoever registered for it, or refuses it.
    pub(crate) async fn dispatch(
        &self,
        channel: Channel<Msg>,
        address: &str,
        port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
    ) {
        for _ in 0..PENDING_TRIES {
            match self.route(address, port) {
                Route::Found(sender) => {
                    // Reserve the place first so a full queue can still refuse the channel.
                    let permit = match sender.try_reserve() {
                        Ok(permit) => permit,
                        Err(mpsc::error::TrySendError::Full(())) => {
                            reply.reject(ChannelOpenFailure::ResourceShortage).await;
                            return;
                        }
                        Err(mpsc::error::TrySendError::Closed(())) => {
                            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
                            return;
                        }
                    };
                    reply.accept().await;
                    permit.send(Incoming {
                        channel,
                        originator_address: originator_address.to_string(),
                        originator_port: u16::try_from(originator_port).unwrap_or(0),
                    });
                    return;
                }
                Route::Pending => tokio::time::sleep(PENDING_WAIT).await,
                Route::Nobody => break,
            }
        }
        log::debug!("refused a forwarded-tcpip channel for {address}:{port}: nobody registered it");
        reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
    }
}
