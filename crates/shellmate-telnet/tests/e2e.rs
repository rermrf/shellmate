//! Opt-in tests against the review VM's inetutils telnetd. No commands run unless
//! `--ignored` is requested AND SHELLMATE_TELNET_PASSWORD is present.
//!
//! Defaults: SHELLMATE_TELNET_HOST=192.168.139.32, SHELLMATE_TELNET_PORT=23,
//! SHELLMATE_TELNET_USER=tester. Supply SHELLMATE_TELNET_PASSWORD in the caller's
//! environment; there is deliberately no password default.
//!
//! cargo test --offline -p shellmate-telnet --test e2e -- --ignored --test-threads=1
//!
//! The client is the production Channel. A transparent, local relay observes
//! client-to-server credential frames so "exactly once" is checked on the wire,
//! not inferred from echoed usernames or password prompts. Neither credentials
//! nor captured terminal output are printed, including in failure diagnostics.
use shellmate_core::{OutputHandler, OutputSink, PtyDims, Secret, TerminalBackend};
use shellmate_telnet::{
    Channel,
    login::{authentication_result, display_text},
    protocol::Parser,
};
use std::{
    env,
    io::Write as _,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};
use zeroize::{Zeroize, Zeroizing};

struct Config {
    host: String,
    port: u16,
    user: String,
    password: Secret,
}
impl Config {
    fn load() -> Option<Self> {
        let password = match env::var("SHELLMATE_TELNET_PASSWORD") {
            Ok(value) if !value.is_empty() => Secret::new(value),
            _ => {
                // Bypass libtest's successful-test capture so the skip is visible.
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "SKIP: set SHELLMATE_TELNET_PASSWORD to run the real telnetd e2e suite (no connection attempted)"
                );
                return None;
            }
        };
        Some(Self {
            host: env::var("SHELLMATE_TELNET_HOST").unwrap_or_else(|_| "192.168.139.32".into()),
            port: env::var("SHELLMATE_TELNET_PORT")
                .unwrap_or_else(|_| "23".into())
                .parse()
                .expect("SHELLMATE_TELNET_PORT must be a TCP port number"),
            user: env::var("SHELLMATE_TELNET_USER").unwrap_or_else(|_| "tester".into()),
            password,
        })
    }
    fn use_wrong_password(&mut self) {
        // Derive a different password from the required environment value, with
        // a changed first character (also different if a server truncates it).
        let mut chars = self.password.expose().chars();
        let first = if chars.next() == Some('!') { '?' } else { '!' };
        self.password = Secret::new(format!("{first}{}", chars.as_str()));
    }
}

struct WireAudit {
    parser: Parser,
    expected: [Secret; 2],
    line: Zeroizing<Vec<u8>>,
    count: usize,
    mismatch: bool,
    observing: bool,
    upstream_eof: bool,
}
impl WireAudit {
    fn new(config: &Config) -> Self {
        Self {
            parser: Parser::new(PtyDims::default()),
            expected: [Secret::new(&config.user), config.password.clone()],
            line: Zeroizing::new(Vec::new()),
            count: 0,
            mismatch: false,
            observing: true,
            upstream_eof: false,
        }
    }
    fn feed(&mut self, wire: &[u8]) {
        if !self.observing {
            return;
        }
        let (mut bytes, _) = self.parser.feed(wire);
        for &b in &bytes {
            if b == b'\r' || b == b'\n' {
                if self.line.is_empty() {
                    continue; // CR LF is one submission.
                }
                self.mismatch |= self
                    .expected
                    .get(self.count)
                    .is_none_or(|expected| self.line.as_slice() != expected.expose().as_bytes());
                self.count += 1;
                self.line.zeroize();
            } else {
                self.line.push(b);
                // Bound unexpected output from a broken client without printing it.
                if self.line.len()
                    > self
                        .expected
                        .iter()
                        .map(|s| s.expose().len())
                        .max()
                        .unwrap()
                        + 1024
                {
                    self.mismatch = true;
                    self.line.zeroize();
                }
            }
        }
        bytes.zeroize();
    }
    fn assert_once(&self) {
        assert!(
            self.count == 2 && !self.mismatch && self.line.is_empty(),
            "expected exactly one username frame then one password frame; wire contents withheld"
        );
    }
}
#[derive(Default)]
struct State {
    bytes: Zeroizing<Vec<u8>>,
    closed: bool,
    server_eof: bool,
}
#[derive(Default)]
struct Events {
    state: Mutex<State>,
    changed: Notify,
}
struct Sink(Arc<Events>);
impl OutputHandler for Sink {
    fn output(&self, bytes: &[u8]) {
        let mut state = self.0.state.lock().unwrap();
        assert!(
            state.bytes.len() + bytes.len() <= 1024 * 1024,
            "unexpectedly large test-server output (withheld)"
        );
        state.bytes.extend_from_slice(bytes);
        self.0.changed.notify_one();
    }
    fn exited(&self, _: Option<u32>) {
        self.0.state.lock().unwrap().closed = true;
        self.0.changed.notify_one();
    }
    fn disconnected(&self, reason: String) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.server_eof = reason.contains("服务器已关闭连接");
        self.0.changed.notify_one();
    }
}
struct Session {
    channel: Channel,
    events: Arc<Events>,
    audit: Arc<Mutex<WireAudit>>,
    relay: JoinHandle<()>,
}
fn has_output_line(bytes: &[u8], expected: &str) -> bool {
    display_text(&String::from_utf8_lossy(bytes))
        .lines()
        .any(|line| line.split_whitespace().eq(expected.split_whitespace()))
}
impl Drop for Session {
    fn drop(&mut self) {
        self.channel.close();
        self.relay.abort();
    }
}
impl Session {
    async fn connect(config: Config, dims: PtyDims) -> Self {
        let mut server = shellmate_telnet::connect(&config.host, config.port)
            .await
            .expect("could not connect to the configured test telnetd");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("could not bind local test relay");
        let addr = listener.local_addr().unwrap();
        let audit = Arc::new(Mutex::new(WireAudit::new(&config)));
        let tap = audit.clone();
        let relay = tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.expect("local relay accept failed");
            let mut upstream = Zeroizing::new([0u8; 8192]);
            let mut downstream = Zeroizing::new([0u8; 8192]);
            loop {
                tokio::select! {
                    read = client.read(&mut upstream[..]) => {
                        let Ok(n) = read else { break };
                        if n == 0 { break; }
                        tap.lock().unwrap().feed(&upstream[..n]);
                        if server.write_all(&upstream[..n]).await.is_err() { break; }
                        upstream.zeroize();
                    },
                    read = server.read(&mut downstream[..]) => {
                        let Ok(n) = read else { break };
                        if n == 0 { tap.lock().unwrap().upstream_eof = true; break; }
                        if client.write_all(&downstream[..n]).await.is_err() { break; }
                        downstream.zeroize();
                    },
                }
            }
        });
        let socket = TcpStream::connect(addr)
            .await
            .expect("local relay connect failed");
        let events = Arc::new(Events::default());
        let channel = Channel::start(
            socket,
            OutputSink::new(Sink(events.clone())),
            dims,
            config.user,
            Some(config.password),
            &tokio::runtime::Handle::current(),
        );
        Self {
            channel,
            events,
            audit,
            relay,
        }
    }
    async fn wait(
        &self,
        timeout: Duration,
        condition: impl Fn(&State) -> bool,
        failure: &'static str,
    ) {
        let result = tokio::time::timeout(timeout, async {
            loop {
                let changed = self.events.changed.notified();
                {
                    let state = self.events.state.lock().unwrap();
                    if condition(&state) {
                        break;
                    }
                    assert!(
                        !state.closed,
                        "server closed before the expected event; terminal output withheld"
                    );
                }
                changed.await;
            }
        })
        .await;
        assert!(result.is_ok(), "{failure}; terminal output withheld");
    }
    async fn login(&self) {
        self.wait(
            Duration::from_secs(35),
            |state| authentication_result(&String::from_utf8_lossy(&state.bytes)) == Some(true),
            "auto-login did not reach a shell prompt within the 30 s connection budget",
        )
        .await;
        let mut audit = self.audit.lock().unwrap();
        audit.assert_once();
        audit.observing = false; // Subsequent writes are explicit test commands.
    }
    async fn command_line(&mut self, command: &str, expected_line: &str) {
        let offset = self.events.state.lock().unwrap().bytes.len();
        self.channel
            .write(command.as_bytes())
            .expect("command write failed");
        // Same separate Enter write as the app's reviewed-command path.
        self.channel.write(b"\r").expect("Enter write failed");
        self.wait(
            Duration::from_secs(8),
            |state| has_output_line(&state.bytes[offset..], expected_line),
            "expected command output line not received",
        )
        .await;
    }
    async fn exit(&mut self) {
        self.channel.write(b"exit").expect("exit write failed");
        self.channel.write(b"\r").expect("Enter write failed");
        self.wait(
            Duration::from_secs(8),
            |state| state.closed,
            "server did not close after exit",
        )
        .await;
        assert!(
            self.events.state.lock().unwrap().server_eof,
            "expected an explicit server EOF notification"
        );
        assert!(
            self.audit.lock().unwrap().upstream_eof,
            "expected EOF from the real telnetd, not a relay error"
        );
    }
}

#[tokio::test]
#[ignore = "requires real inetutils telnetd and SHELLMATE_TELNET_PASSWORD"]
async fn negotiation_and_saved_password_login_once() {
    let Some(config) = Config::load() else { return };
    let mut session = Session::connect(config, PtyDims::default()).await;
    session.login().await;
    session.exit().await;
}

#[tokio::test]
#[ignore = "requires real inetutils telnetd and SHELLMATE_TELNET_PASSWORD"]
async fn naws_matches_stty_before_and_after_resize() {
    let Some(config) = Config::load() else { return };
    let mut session = Session::connect(
        config,
        PtyDims {
            cols: 100,
            rows: 30,
            ..Default::default()
        },
    )
    .await;
    session.login().await;
    session.command_line("stty size", "30 100").await;
    session
        .channel
        .resize(132, 40, 0, 0)
        .expect("resize write failed");
    session.command_line("stty size", "40 132").await;
    session.exit().await;
}

#[tokio::test]
#[ignore = "requires real inetutils telnetd and SHELLMATE_TELNET_PASSWORD"]
async fn command_round_trip() {
    let Some(config) = Config::load() else { return };
    let mut session = Session::connect(config, PtyDims::default()).await;
    session.login().await;
    session.command_line("echo tn-$((6*7))", "tn-42").await;
    session.exit().await;
}

#[tokio::test]
#[ignore = "requires real inetutils telnetd and SHELLMATE_TELNET_PASSWORD"]
async fn wrong_password_is_rejected_without_an_auto_retry() {
    let Some(mut config) = Config::load() else {
        return;
    };
    config.use_wrong_password();
    let session = Session::connect(config, PtyDims::default()).await;
    session
        .wait(
            Duration::from_secs(35),
            |state| authentication_result(&String::from_utf8_lossy(&state.bytes)) == Some(false),
            "server did not report a rejected login",
        )
        .await;
    let rejection = display_text(&String::from_utf8_lossy(
        &session.events.state.lock().unwrap().bytes,
    ))
    .len();
    // GNU login may put the rejection and the next prompt in the same read.
    session
        .wait(
            Duration::from_secs(8),
            |state| {
                let text = display_text(&String::from_utf8_lossy(&state.bytes));
                let lower = text.to_ascii_lowercase();
                let after = lower
                    .rfind("login incorrect")
                    .map_or(rejection, |i| i + "login incorrect".len());
                lower
                    .get(after..)
                    .is_some_and(|tail| tail.contains("login:"))
            },
            "server did not return to a manual login prompt",
        )
        .await;
    // Keep the connection open across the remaining auto-login interval. Any
    // retry is an extra audited frame; no raw/secret value is ever asserted.
    tokio::time::sleep(Duration::from_secs(10)).await;
    session.audit.lock().unwrap().assert_once();
    assert!(
        !session.events.state.lock().unwrap().closed,
        "session should wait for manual input after one rejected login"
    );
}

#[tokio::test]
#[ignore = "requires real inetutils telnetd and SHELLMATE_TELNET_PASSWORD"]
async fn exit_reports_server_close() {
    let Some(config) = Config::load() else { return };
    let mut session = Session::connect(config, PtyDims::default()).await;
    session.login().await;
    session.exit().await;
}

#[test]
fn output_matching_strips_decoration_and_keeps_the_last_redraw() {
    assert!(has_output_line(b"\x1b[?2004l\rtn-42\r\n", "tn-42"));
    assert!(has_output_line(b"\x1b[?2004l\rtn-42\r", "tn-42"));
    assert!(has_output_line(
        b"stty size\r\n\x1b[?2004l\r30 100\r\n\x1b[32mtester:~$\x1b[0m ",
        "30 100"
    ));
    assert!(has_output_line(
        b"\x1b]0;30 100\x07old\r\x1b[32m40\x1b[0m\t132\r\n\x1b]0;title\x1b\\",
        "40 132"
    ));
    assert!(!has_output_line(
        b"\x1b]0;tn-42\x07echo tn-$((6*7))\r\nold\rnew\r\n",
        "tn-42"
    ));
    assert!(!has_output_line(b"old\rnew\r\n", "old"));
    assert!(has_output_line(
        "\x1b[32m中文 ✓\x1b[0m\r\n".as_bytes(),
        "中文 ✓"
    ));
}

#[test]
fn wire_audit_ignores_negotiation_and_counts_split_credential_frames() {
    // Synthetic per-process fixture, never the VM's credential or environment.
    let config = Config {
        host: String::new(),
        port: 0,
        user: "fixture-user".into(),
        password: Secret::new(std::process::id().to_string()),
    };
    let mut audit = WireAudit::new(&config);
    let mut wire = Zeroizing::new(vec![255, 251, 24, 255, 250, 24, 0]);
    wire.extend_from_slice(b"xterm-256color");
    wire.extend_from_slice(&[255, 240]);
    wire.extend_from_slice(config.user.as_bytes());
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(config.password.expose().as_bytes());
    wire.extend_from_slice(b"\r\n");
    for byte in wire.iter() {
        audit.feed(&[*byte]);
    }
    audit.assert_once();
    audit.feed(config.password.expose().as_bytes());
    audit.feed(b"\r\n");
    assert!(
        audit.mismatch && audit.count == 3,
        "a repeated credential must fail the audit"
    );
}
