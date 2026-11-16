//! Real Mosh tests, opt-in only. Same Ubuntu/key environment as monitor e2e:
//! SHELLMATE_MON_HOST (shellmate-mon.orb.local), MON_PORT (22), MON_USER (tester),
//! SHELLMATE_E2E_KEY (required). Optional SHELLMATE_MOSH_CLIENT overrides discovery.
//! cargo test --offline -p shellmate-ssh --test ssh mosh_e2e -- --ignored --nocapture
//! The test user must have no preexisting mosh-server processes. Only the PID
//! bootstrapped by a test is ever signalled. Terminal output/keys are not logged.
use crate::support::Env;
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    term::{Config, Term},
    vte::ansi::Processor,
};
use shellmate_core::{AuthMethod, PtyDims, TerminalBackend};
use shellmate_ssh::{Connection, Route, SshHost, mosh};
use shellmate_term::LocalPty;
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
fn setting(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}
async fn turn() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}
async fn connection(env: &Env) -> Option<Connection> {
    let Ok(key) = std::env::var("SHELLMATE_E2E_KEY") else {
        eprintln!("SKIP Mosh e2e: set SHELLMATE_E2E_KEY");
        return None;
    };
    let host = SshHost::new(
        setting("SHELLMATE_MON_HOST", "shellmate-mon.orb.local"),
        setting("SHELLMATE_MON_PORT", "22").parse().unwrap(),
        setting("SHELLMATE_MON_USER", "tester"),
        AuthMethod::PrivateKey,
    )
    .with_identity_file(key);
    Some(
        Connection::connect(Route::direct(host), env.trusting())
            .await
            .expect("SSH connect"),
    )
}
async fn server_pids(connection: &Connection) -> Vec<u32> {
    let out = connection
        .exec("pgrep -u \"$(id -u)\" -x mosh-server || :")
        .await
        .expect("inspect test user's Mosh processes");
    out.stdout_text()
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}
async fn no_servers(connection: &Connection) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server_pids(connection).await.is_empty() {
        assert!(
            Instant::now() < deadline,
            "test mosh-server remained after exit"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
#[derive(Clone, Copy)]
struct Size(usize, usize);
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}
#[derive(Clone)]
struct Listener(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);
impl EventListener for Listener {
    fn send_event(&self, e: Event) {
        if let Event::PtyWrite(text) = e {
            let _ = self.0.send(text.into_bytes());
        }
    }
}
struct Screen {
    term: Term<Listener>,
    parser: Processor,
}
struct Client {
    pty: LocalPty,
    screen: Arc<Mutex<Screen>>,
    events: Arc<tokio::sync::Notify>,
    replies: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    exit: tokio::sync::oneshot::Receiver<Option<u32>>,
    started: Instant,
}
impl Client {
    fn start(ready: mosh::Prepared) -> Self {
        let (tx, replies) = tokio::sync::mpsc::unbounded_channel();
        let screen = Arc::new(Mutex::new(Screen {
            term: Term::new(Config::default(), &Size(100, 30), Listener(tx)),
            parser: Processor::new(),
        }));
        let events = Arc::new(tokio::sync::Notify::new());
        let (copy, notify) = (screen.clone(), events.clone());
        let (done, exit) = tokio::sync::oneshot::channel();
        let lease = ready.lease.clone();
        let start = Instant::now();
        let pty = LocalPty::spawn_program(
            &ready.program,
            PtyDims {
                cols: 100,
                rows: 30,
                ..Default::default()
            },
            move |bytes| {
                let mut s = copy.lock().unwrap();
                let Screen { term, parser } = &mut *s;
                parser.advance(term, bytes);
                notify.notify_one();
            },
            move |code| {
                lease.client_exited(code, start.elapsed());
                let _ = done.send(code);
            },
        )
        .expect("spawn client");
        Self {
            pty,
            screen,
            events,
            replies,
            exit,
            started: start,
        }
    }
    async fn wait(&mut self, check: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let text = shellmate_term::replay::screen_text(&self.screen.lock().unwrap().term);
            if check(&text) {
                return;
            }
            tokio::select! {
                _=self.events.notified()=>{},
                Some(reply)=self.replies.recv()=>self.pty.write(&reply).expect("terminal reply"),
                _=tokio::time::sleep_until(deadline)=>panic!("expected Mosh screen text not received; output withheld"),
            }
        }
    }
    async fn command(&mut self, command: &str, expected: &str) {
        self.pty.write(command.as_bytes()).unwrap();
        self.pty.write(b"\r").unwrap();
        self.wait(|text| {
            text.lines()
                .any(|l| l.split_whitespace().eq(expected.split_whitespace()))
        })
        .await;
    }
}
#[tokio::test]
#[ignore = "requires real SSH/mosh-server, mosh-client and UDP to the Ubuntu VM"]
async fn roundtrip_resize_and_normal_exit() {
    let _turn = turn().await;
    let env = Env::new();
    let Some(connection) = connection(&env).await else {
        return;
    };
    assert!(
        server_pids(&connection).await.is_empty(),
        "use an isolated test account with no existing Mosh sessions"
    );
    let path = std::env::var_os("SHELLMATE_MOSH_CLIENT")
        .map(PathBuf::from)
        .unwrap_or_else(|| mosh::find_client().expect(mosh::MISSING_CLIENT));
    let ready = mosh::prepare(connection.clone(), path)
        .await
        .expect("bootstrap Mosh");
    let mut client = Client::start(ready);
    client
        .wait(|text| {
            text.lines()
                .any(|l| l.trim_end().ends_with(['$', '#', '>']))
        })
        .await;
    client.command("stty size", "30 100").await;
    client.screen.lock().unwrap().term.resize(Size(132, 40));
    client.pty.resize(132, 40, 0, 0).unwrap();
    // Mosh synchronizes resize over UDP. Retry a harmless query until its remote
    // terminal acknowledges the new geometry; don't assume a fixed RTT.
    for _ in 0..3 {
        client.pty.write(b"stty size\r").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    client
        .wait(|text| text.lines().any(|l| l.split_whitespace().eq(["40", "132"])))
        .await;
    client.command("echo mosh-$((6*7))", "mosh-42").await;
    // The normal-exit policy disarms cleanup only after the startup grace.
    // Keep the lease armed on failures so a failed assertion cannot orphan it.
    tokio::time::sleep(Duration::from_secs(3).saturating_sub(client.started.elapsed())).await;
    client.pty.write(b"exit\r").unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(15), &mut client.exit)
            .await
            .expect("client exit")
            .unwrap(),
        Some(0)
    );
    no_servers(&connection).await;
}
#[tokio::test]
#[ignore = "requires real SSH/mosh-server on the Ubuntu VM; local client is a stub"]
async fn immediate_client_exit_cleans_remote_server() {
    let _turn = turn().await;
    let env = Env::new();
    let Some(connection) = connection(&env).await else {
        return;
    };
    assert!(
        server_pids(&connection).await.is_empty(),
        "use an isolated test account"
    );
    let script = env.dir.path().join("failed-client");
    std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ready = mosh::prepare(connection.clone(), script)
        .await
        .expect("bootstrap Mosh");
    let mut client = Client::start(ready);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), &mut client.exit)
            .await
            .unwrap()
            .unwrap(),
        Some(1)
    );
    no_servers(&connection).await;
}
