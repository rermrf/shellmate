//! Real loopback tests (run outside sandboxes which deny bind()). No external server.
use shellmate_core::{OutputHandler, OutputSink, PtyDims, Secret, TerminalBackend};
use shellmate_telnet::{
    Channel,
    protocol::{DO, DONT, IAC, WILL, WONT},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[derive(Default)]
struct Events {
    bytes: Mutex<Vec<u8>>,
    closed: tokio::sync::Notify,
}
struct Handler(Arc<Events>);
impl OutputHandler for Handler {
    fn output(&self, b: &[u8]) {
        self.0.bytes.lock().unwrap().extend(b);
    }
    fn exited(&self, _: Option<u32>) {
        self.0.closed.notify_one();
    }
    fn disconnected(&self, _: String) {
        self.0.closed.notify_one();
    }
}
#[tokio::test]
async fn negotiation_resize_escaping_autologin_and_server_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (resize_tx, resize_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        s.write_all(&[
            IAC, WILL, 1, IAC, WILL, 3, IAC, DO, 24, IAC, DO, 31, IAC, DO, 99, IAC, WILL, 99,
        ])
        .await
        .unwrap();
        let expected = [
            IAC, DO, 1, IAC, DO, 3, IAC, WILL, 24, IAC, WILL, 31, IAC, 250, 31, 0, 80, 0, 24, IAC,
            240, IAC, WONT, 99, IAC, DONT, 99,
        ];
        let mut reply = vec![0; expected.len()];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, expected);
        s.write_all(&[IAC, 250, 24, 1, IAC, 240]).await.unwrap();
        let expected = [&[IAC, 250, 24, 0][..], b"xterm-256color", &[IAC, 240]].concat();
        let mut reply = vec![0; expected.len()];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, expected);
        s.write_all(b"Username:").await.unwrap();
        let mut user = [0; 7];
        s.read_exact(&mut user).await.unwrap();
        assert_eq!(&user, b"admin\r\n");
        s.write_all(b"\r\nPassword for admin:").await.unwrap();
        let mut pass = [0; 8];
        s.read_exact(&mut pass).await.unwrap();
        assert_eq!(&pass, b"secret\r\n");
        resize_tx.send(()).unwrap();
        let expected = [IAC, 250, 31, 0, 255, 255, 0, 40, IAC, 240];
        let mut reply = [0; 10];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, expected);
        let mut input = [0; 4];
        s.read_exact(&mut input).await.unwrap();
        assert_eq!(input, [IAC, IAC, b'\r', 0]);
        s.write_all(b"a\r\0b\r\n\xff\xff").await.unwrap();
    });
    let socket = shellmate_telnet::connect("127.0.0.1", addr.port())
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let mut channel = Channel::start(
        socket,
        OutputSink::new(Handler(events.clone())),
        PtyDims::default(),
        "admin".into(),
        Some(Secret::new("secret")),
        &tokio::runtime::Handle::current(),
    );
    tokio::time::timeout(Duration::from_secs(5), resize_rx)
        .await
        .unwrap()
        .unwrap();
    channel.resize(255, 40, 0, 0).unwrap();
    channel.write(&[IAC, b'\r']).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), events.closed.notified())
        .await
        .unwrap();
    assert!(events.bytes.lock().unwrap().ends_with(b"a\rb\r\n\xff"));
}
async fn no_login(prompt: &'static [u8], saved: bool, delay: Duration) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        if !delay.is_zero() {
            // First activity starts the 10 s prompt window; delaying the first
            // activity instead would now correctly allow auto-login.
            socket.write_all(b"Waiting for login\r\n").await.unwrap();
        }
        tokio::time::sleep(delay).await;
        socket.write_all(prompt).await.unwrap();
        let mut reply = [0; 32];
        assert!(
            tokio::time::timeout(Duration::from_millis(150), socket.read(&mut reply))
                .await
                .is_err()
        );
    });
    let socket = shellmate_telnet::connect("127.0.0.1", addr.port())
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let _channel = Channel::start(
        socket,
        OutputSink::new(Handler(events)),
        PtyDims::default(),
        "admin".into(),
        saved.then(|| Secret::new("secret")),
        &tokio::runtime::Handle::current(),
    );
    server.await.unwrap();
}
#[tokio::test]
async fn unmatched_prompt_stays_manual() {
    no_login(b"PIN:", true, Duration::ZERO).await;
}
#[tokio::test]
async fn no_saved_password_stays_manual() {
    no_login(b"login:", false, Duration::ZERO).await;
}
#[tokio::test]
async fn expired_prompt_stays_manual() {
    no_login(b"login:", true, Duration::from_millis(10100)).await;
}
#[tokio::test]
async fn editor_probe_authenticates_without_running_a_command() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        s.write_all(b"login:").await.unwrap();
        let mut b = [0; 7];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"admin\r\n");
        s.write_all(b"\r\nPassword:").await.unwrap();
        let mut b = [0; 8];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"secret\r\n");
        s.write_all(b"\r\nWelcome\r\nadmin@router:~# ")
            .await
            .unwrap();
        let mut b = [0; 16];
        assert_eq!(s.read(&mut b).await.unwrap(), 0);
    });
    let result = shellmate_telnet::test_connection(
        "127.0.0.1",
        addr.port(),
        "admin".into(),
        Some(Secret::new("secret")),
    )
    .await
    .unwrap();
    assert!(result.contains("认证方式 密码"));
    server.await.unwrap();
}

#[tokio::test]
async fn inetutils_waits_for_all_opening_replies_after_slow_reverse_dns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10100)).await;
        socket
            .write_all(&[
                IAC, WILL, 37, IAC, WILL, 38, IAC, DO, 24, IAC, DO, 32, IAC, DO, 35, IAC, DO, 39,
                IAC, DO, 36,
            ])
            .await
            .unwrap();
        let mut replies = [0; 21];
        socket.read_exact(&mut replies).await.unwrap();
        assert_eq!(
            replies,
            [
                IAC, DONT, 37, IAC, DONT, 38, IAC, WILL, 24, IAC, WONT, 32, IAC, WONT, 35, IAC,
                WONT, 39, IAC, WONT, 36
            ]
        );
        socket
            .write_all(&[IAC, 250, 24, 1, IAC, 240])
            .await
            .unwrap();
        let expected = [&[IAC, 250, 24, 0][..], b"xterm-256color", &[IAC, 240]].concat();
        let mut reply = vec![0; expected.len()];
        socket.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, expected);
        socket.write_all(b"login:").await.unwrap();
        let mut user = [0; 7];
        socket.read_exact(&mut user).await.unwrap();
        assert_eq!(&user, b"admin\r\n");
        socket.write_all(b"\r\nPassword:").await.unwrap();
        let mut password = [0; 8];
        socket.read_exact(&mut password).await.unwrap();
        assert_eq!(&password, b"secret\r\n");
    });
    let socket = shellmate_telnet::connect("127.0.0.1", addr.port())
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let _channel = Channel::start(
        socket,
        OutputSink::new(Handler(events)),
        PtyDims::default(),
        "admin".into(),
        Some(Secret::new("secret")),
        &tokio::runtime::Handle::current(),
    );
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .unwrap()
        .unwrap();
}
