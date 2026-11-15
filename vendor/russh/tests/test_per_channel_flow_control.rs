// Added for Shellmate (per-channel flow control, port of russh#730) - see ../SHELLMATE-PATCH.md
//
// A consumer that stops reading one channel must only throttle that channel: the peer's window for
// it drains to zero and the peer stops sending on it, while the session loop keeps serving every
// other channel (and the keepalives). Before the patch the loop awaited room in the channel's queue
// (`chan.send().await`) and handed the peer a fresh window on receipt, so one parked consumer
// stopped the whole connection.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelMsg, client};
use ssh_key::PrivateKey;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep, timeout};

const CHANNEL_BUFFER_SIZE: usize = 2;
const CHUNK: usize = 8 * 1024;

fn addr() -> SocketAddr {
    TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap()
}

async fn wait_for_server(addr: SocketAddr) {
    while TcpStream::connect(addr).is_err() {
        sleep(Duration::from_millis(10)).await;
    }
}

struct Client;

impl client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, _: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn connect(addr: SocketAddr, config: client::Config) -> client::Handle<Client> {
    let key = Arc::new(PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519).unwrap());
    let mut session = client::connect(Arc::new(config), addr, Client).await.unwrap();
    let auth = session
        .authenticate_publickey(
            "user",
            PrivateKeyWithHashAlg::new(key, session.best_supported_rsa_hash().await.unwrap().flatten()),
        )
        .await
        .unwrap();
    assert!(auth.success(), "authentication rejected");
    session
}

/// What the server does with the session channels it is given, in order of opening.
#[derive(Clone)]
struct Server {
    /// Hands the channel to the test, which decides when to read it.
    channels: Option<mpsc::UnboundedSender<Channel<Msg>>>,
    /// Floods the first channel with this many chunks (and does nothing on the others).
    flood_chunks: usize,
    opened: Arc<AtomicUsize>,
    /// How much of the flood the server has managed to hand to its session.
    written: Arc<AtomicUsize>,
    /// How much data clients have sent to the server (seen by the `data` callback).
    received: Arc<AtomicUsize>,
}

impl Server {
    async fn run(self, addr: SocketAddr, window_size: u32) {
        let config = Arc::new(server::Config {
            keys: vec![PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519).unwrap()],
            window_size,
            channel_buffer_size: CHANNEL_BUFFER_SIZE,
            ..Default::default()
        });
        let mut server = self;
        server.run_on_address(config, addr).await.unwrap();
    }
}

impl russh::server::Server for Server {
    type Handler = Self;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        self.clone()
    }
}

impl russh::server::Handler for Server {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _: &str, _: server::VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let index = self.opened.fetch_add(1, Ordering::SeqCst);
        if let Some(channels) = &self.channels {
            let _ = channels.send(channel);
            return Ok(());
        }
        let id = channel.id();
        let handle = session.handle();
        let (flood_chunks, written) = (self.flood_chunks, self.written.clone());
        tokio::spawn(async move {
            // The server only sends here, through the handle; the channel is not needed.
            drop(channel);
            if index == 0 {
                for _ in 0..flood_chunks {
                    if handle.data(id, vec![0xAA; CHUNK]).await.is_err() {
                        return;
                    }
                    written.fetch_add(CHUNK, Ordering::SeqCst);
                }
                let _ = handle.eof(id).await;
            }
        });
        Ok(())
    }

    async fn data(&mut self, _: russh::ChannelId, data: &[u8], _: &mut Session) -> Result<(), Self::Error> {
        self.received.fetch_add(data.len(), Ordering::SeqCst);
        Ok(())
    }
}

/// The server's consumer of channel A is parked: channel B on the same connection still works,
/// and A gets everything once its consumer reads again.
#[tokio::test]
async fn a_parked_server_side_consumer_does_not_block_a_sibling_channel() -> Result<(), anyhow::Error> {
    const A_WRITES: usize = CHANNEL_BUFFER_SIZE + 6;
    let addr = addr();
    let (channels_tx, mut channels_rx) = mpsc::unbounded_channel();
    let server = Server {
        channels: Some(channels_tx),
        flood_chunks: 0,
        opened: Arc::default(),
        written: Arc::default(),
        received: Arc::default(),
    };
    // Exactly what A is sent while parked: A's window ends up at zero.
    tokio::spawn(server.run(addr, (A_WRITES * CHUNK) as u32));
    wait_for_server(addr).await;
    let session = connect(addr, client::Config::default()).await;

    let chan_a = session.channel_open_session().await?;
    let server_a = channels_rx.recv().await.expect("server channel a");
    let chan_b = session.channel_open_session().await?;
    let mut server_b = channels_rx.recv().await.expect("server channel b");

    let b_bytes = Arc::new(AtomicUsize::new(0));
    let b_consumer = tokio::spawn({
        let b_bytes = b_bytes.clone();
        async move {
            while let Some(msg) = server_b.wait().await {
                if let ChannelMsg::Data { data } = msg {
                    b_bytes.fetch_add(data.len(), Ordering::SeqCst);
                }
            }
        }
    });

    // Flood A well past its queue: the rest waits in the session, A's window stays closed.
    let mut writer_a = chan_a.make_writer();
    for _ in 0..A_WRITES {
        timeout(Duration::from_secs(5), writer_a.write_all(&[0xAA; CHUNK]))
            .await
            .expect("writes to A go through while its window is open")?;
    }

    // B is served at once.
    let mut writer_b = chan_b.make_writer();
    timeout(Duration::from_secs(5), writer_b.write_all(&[0xBB; CHUNK]))
        .await
        .expect("B must not be held up by A")?;
    timeout(Duration::from_secs(5), async {
        while b_bytes.load(Ordering::SeqCst) < CHUNK {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("B's data reaches the server while A is parked");

    // A's consumer reads again: the backlog is delivered, the window re-opens, more can be sent.
    let consumer_a = tokio::spawn(async move {
        let mut server_a = server_a;
        let mut total = 0;
        while let Some(msg) = server_a.wait().await {
            match msg {
                ChannelMsg::Data { data } => total += data.len(),
                ChannelMsg::Eof => break,
                _ => {}
            }
        }
        total
    });
    timeout(Duration::from_secs(5), writer_a.write_all(&[0xAA; CHUNK]))
        .await
        .expect("A can send again once its consumer has caught up")?;
    chan_a.eof().await?;
    let total = timeout(Duration::from_secs(5), consumer_a).await.expect("A's backlog drains")?;
    assert_eq!(total, (A_WRITES + 1) * CHUNK);

    drop(b_consumer);
    Ok(())
}

/// The same from the other side: the client's consumer of channel A is parked while the server
/// floods A; the client's session loop still serves B (opening it, sending on it), A's flood is throttled (the server cannot write
/// more than window + queue), and the parked consumer gets every byte afterwards.
#[tokio::test]
async fn a_parked_client_side_consumer_does_not_block_a_sibling_channel() -> Result<(), anyhow::Error> {
    const FLOOD_CHUNKS: usize = 256; // 2 MiB
    let addr = addr();
    let written = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(AtomicUsize::new(0));
    let server = Server {
        channels: None,
        flood_chunks: FLOOD_CHUNKS,
        opened: Arc::default(),
        written: written.clone(),
        received: received.clone(),
    };
    tokio::spawn(server.run(addr, 1024 * 1024));
    wait_for_server(addr).await;
    let window = 64 * 1024;
    let session = connect(
        addr,
        client::Config {
            window_size: window,
            channel_buffer_size: CHANNEL_BUFFER_SIZE,
            ..Default::default()
        },
    )
    .await;

    // A is opened first and then not read at all.
    let mut chan_a = session.channel_open_session().await?;
    sleep(Duration::from_millis(500)).await;

    // B is opened and written to while A is parked: the session loop keeps serving requests.
    let chan_b = timeout(Duration::from_secs(5), session.channel_open_session())
        .await
        .expect("a channel can be opened while A is parked")?;
    timeout(Duration::from_secs(5), chan_b.make_writer().write_all(&[0xBB; CHUNK]))
        .await
        .expect("B can be written to while A is parked")?;
    timeout(Duration::from_secs(5), async {
        while received.load(Ordering::SeqCst) < CHUNK {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("B's data reaches the server while A is parked");

    // Stay parked for a while: the server is throttled by A's closed window, not by its eagerness.
    sleep(Duration::from_secs(1)).await;
    let stalled_at = written.load(Ordering::SeqCst);
    sleep(Duration::from_millis(500)).await;
    assert_eq!(written.load(Ordering::SeqCst), stalled_at, "nothing more flows while A is parked");
    let bound = window as usize + (CHANNEL_BUFFER_SIZE + 4) * 32 * 1024;
    assert!(stalled_at <= bound, "{stalled_at} bytes were taken from the server, more than window and queue ({bound})");
    assert!(stalled_at < FLOOD_CHUNKS * CHUNK, "the flood must not have gone through");

    // A is read: every byte arrives.
    let total = timeout(Duration::from_secs(10), async {
        let mut total = 0;
        while let Some(msg) = chan_a.wait().await {
            match msg {
                ChannelMsg::Data { data } => total += data.len(),
                ChannelMsg::Eof => break,
                _ => {}
            }
        }
        total
    })
    .await
    .expect("A's flood is delivered once A is read");
    assert_eq!(total, FLOOD_CHUNKS * CHUNK);
    Ok(())
}
