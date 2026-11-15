//! Tests against a real OpenSSH server (`#[ignore]`d). They use the same throwaway container as
//! the SSH and SFTP crates' e2e tests (see the SSH crate's e2e docs for how to start it), with one
//! difference: that image's sshd ships with `AllowTcpForwarding no`, which forwarding cannot work
//! with. Turn it on in the container's own configuration (never on a real server) and reload:
//!
//! ```text
//! docker exec -u root shellmate-e2e sh -c 'sed -i "s/^AllowTcpForwarding no/AllowTcpForwarding yes/" /config/sshd/sshd_config && kill -HUP $(pgrep -f "sshd.pam.*\[listener\]")'
//! SHELLMATE_E2E_KEY=/tmp/shellmate-e2e-key cargo test -p shellmate-forward --test forward e2e -- --ignored --nocapture
//! ```
//!
//! Settings (environment) as for the other e2e tests: `SHELLMATE_E2E_PORT` (2222),
//! `SHELLMATE_E2E_USER` (tester), `SHELLMATE_E2E_KEY`, `SHELLMATE_E2E_CONTAINER` (shellmate-e2e).
//! The container is used to run clients and servers of its own (`bash`, `nc`, `dd`, `sha256sum`);
//! everything listens on loopback only. The throughput tests print their MB/s.

use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use shellmate_core::AuthMethod;
use shellmate_forward::{ForwardKind, Forwarder};
use shellmate_ssh::{Connection, Route, SshHost};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::rig::{dial, free_port, pattern, rule};
use crate::socks5::{Dest, ask, greet};
use crate::support::{EchoServer, Env, eventually};

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn container() -> String {
    setting("SHELLMATE_E2E_CONTAINER", "shellmate-e2e")
}

fn ssh_port() -> u16 {
    setting("SHELLMATE_E2E_PORT", "2222").parse().expect("SHELLMATE_E2E_PORT is a port number")
}

/// Runs a shell snippet in the container as the SSH user and returns what it printed.
fn docker(script: &str) -> String {
    docker_with("sh", script)
}

/// Like [`docker`] with bash (for `/dev/tcp`, which busybox's `sh` does not have).
fn docker_bash(script: &str) -> String {
    docker_with("bash", script)
}

fn docker_with(shell: &str, script: &str) -> String {
    let output = Command::new("docker")
        .args(["exec", "-u", &setting("SHELLMATE_E2E_USER", "tester"), &container(), shell, "-c", script])
        .output()
        .expect("docker is installed");
    assert!(output.status.success(), "docker exec failed: {script}\n{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Session {
    connection: Connection,
    forwarder: Forwarder,
    _env: Env,
}

async fn session() -> Session {
    let env = Env::new();
    let key = std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts");
    let host = SshHost::new("127.0.0.1", ssh_port(), setting("SHELLMATE_E2E_USER", "tester"), AuthMethod::PrivateKey)
        .with_identity_file(key);
    let connection = Connection::connect(Route::direct(host), env.trusting()).await.expect("connect");
    let (events, _unused) = async_channel::unbounded();
    let forwarder = Forwarder::new(tokio::runtime::Handle::current(), events);
    forwarder.connection_restored(&connection);
    Session { connection, forwarder, _env: env }
}

async fn read_banner(client: &mut tokio::net::TcpStream) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let read = async {
        loop {
            client.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read).await.expect("a banner");
    String::from_utf8_lossy(&line).trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_a_local_forward_reaches_a_port_inside_the_container() {
    let s = session().await;
    // From the container's point of view its own sshd listens on 127.0.0.1:<ssh port>.
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", ssh_port()));
    assert!(s.forwarder.start(rule.clone()).is_running());
    let mut client = dial(rule.listen.port).await;
    let banner = read_banner(&mut client).await;
    eprintln!("banner through -L: {banner}");
    assert!(banner.starts_with("SSH-2.0-OpenSSH"), "{banner}");
    drop(client);
    drop(s.connection);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_a_socks_proxy_connects_by_address_and_by_name_and_reports_failures() {
    let s = session().await;
    let rule = rule(ForwardKind::Dynamic, &format!("127.0.0.1:{}", free_port()), "");
    assert!(s.forwarder.start(rule.clone()).is_running());
    let port = rule.listen.port;

    let mut by_ip = greet(port).await;
    assert_eq!(ask(&mut by_ip, 1, &Dest::V4(Ipv4Addr::LOCALHOST), ssh_port()).await, 0);
    let banner = read_banner(&mut by_ip).await;
    eprintln!("banner through -D (127.0.0.1): {banner}");
    assert!(banner.starts_with("SSH-2.0-OpenSSH"));

    // The name is resolved by the server (inside the container), where it means the container itself.
    let mut by_name = greet(port).await;
    assert_eq!(ask(&mut by_name, 1, &Dest::Domain("localhost"), ssh_port()).await, 0);
    let banner = read_banner(&mut by_name).await;
    eprintln!("banner through -D (localhost): {banner}");
    assert!(banner.starts_with("SSH-2.0-OpenSSH"));

    // Nothing listens on port 1 in the container: what OpenSSH answers is a refused connection.
    let mut refused = greet(port).await;
    let code = ask(&mut refused, 1, &Dest::V4(Ipv4Addr::LOCALHOST), 1).await;
    eprintln!("SOCKS reply for a closed port: {code:#04x}");
    assert_eq!(code, 0x05);
    let mut unknown = greet(port).await;
    let code = ask(&mut unknown, 1, &Dest::Domain("no-such-host.invalid"), 80).await;
    eprintln!("SOCKS reply for an unknown name: {code:#04x}");
    assert_ne!(code, 0);
    drop(s.connection);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_a_remote_forward_lets_the_container_reach_a_service_on_this_machine() {
    let s = session().await;
    let echo = EchoServer::start().await;
    let rule = rule(ForwardKind::Remote, "127.0.0.1:0", &format!("127.0.0.1:{}", echo.port));
    s.forwarder.start(rule.clone());
    let port = eventually("the remote forward", || match s.forwarder.state(&rule.id) {
        shellmate_forward::RuleState::Running { port } => Some(port),
        _ => None,
    })
    .await;
    eprintln!("the server chose port {port}");
    assert_ne!(port, 0);

    // A client inside the container, over bash's /dev/tcp.
    let said = docker_bash(&format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port}; printf 'hello-from-the-container\\n' >&3; IFS= read -r -t 10 line <&3; echo \"got:$line\""
    ));
    assert_eq!(said.trim(), "got:hello-from-the-container");
    assert_eq!(echo.accepted(), 1);
    eventually("the connection to be done", || (s.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    assert!(s.forwarder.stats(&rule.id).up > 0 && s.forwarder.stats(&rule.id).down > 0);

    // Stopping the rule makes the server stop listening.
    assert!(s.forwarder.stop(&rule.id));
    eventually("the container's port to close", || {
        let probe =
            docker_bash(&format!("(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo open || echo closed"));
        (probe.trim() == "closed").then_some(())
    })
    .await;
    drop(s.connection);
}

/// A server that takes `Remote` rules without GatewayPorts: asking for 0.0.0.0 still works, it
/// just is not reachable from outside the container. We only check the request is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_a_remote_forward_on_all_addresses_is_accepted_without_gateway_ports() {
    let s = session().await;
    let echo = EchoServer::start().await;
    let rule = rule(ForwardKind::Remote, "0.0.0.0:0", &format!("127.0.0.1:{}", echo.port));
    s.forwarder.start(rule.clone());
    let port = eventually("the remote forward", || match s.forwarder.state(&rule.id) {
        shellmate_forward::RuleState::Running { port } => Some(port),
        _ => None,
    })
    .await;
    let said = docker_bash(&format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port}; printf 'any\\n' >&3; IFS= read -r -t 10 line <&3; echo \"got:$line\""
    ));
    assert_eq!(said.trim(), "got:any");
    drop(s.connection);
}

fn sha256_hex(digest: impl AsRef<[u8]>) -> String {
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

const TRANSFER: usize = 128 * 1024 * 1024;

/// Starts `nc` in the container as a one-shot sink printing the sha256 of what it receives, and
/// waits until it listens.
fn start_sink(port: u16) -> std::thread::JoinHandle<String> {
    let user = setting("SHELLMATE_E2E_USER", "tester");
    let name = container();
    let sink = std::thread::spawn(move || {
        let output = Command::new("docker")
            .args(["exec", "-u", &user, &name, "sh", "-c", &format!("nc -l -p {port} | sha256sum")])
            .stderr(Stdio::null())
            .output()
            .expect("docker is installed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    });
    for _ in 0..50 {
        let listening = docker(&format!("netstat -ltn 2>/dev/null | grep -c ':{port} ' || true"));
        if listening.trim() != "0" {
            return sink;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the sink did not start listening on {port}");
}

/// Sends `size` bytes of pattern through `local_port` to a sink started with [`start_sink`] and
/// returns (what was sent, what the container says it received) as sha256 hex, and how long it took.
async fn upload_to_sink(
    local_port: u16,
    sink: std::thread::JoinHandle<String>,
    size: usize,
) -> (String, String, Duration) {
    let client = dial(local_port).await;
    let (_reader, mut writer) = client.into_split();
    let started = Instant::now();
    let mut sha = Sha256::new();
    let (mut left, mut block) = (size, 0u64);
    while left > 0 {
        let chunk = pattern(left.min(1 << 20), 1000 + block);
        sha.update(&chunk);
        writer
            .write_all(&chunk)
            .await
            .expect("the forward closed the connection: the SSH connection is frozen or gone");
        left -= chunk.len();
        block += 1;
    }
    writer.shutdown().await.unwrap();
    let remote = tokio::task::spawn_blocking(move || sink.join().unwrap()).await.unwrap();
    let elapsed = started.elapsed();
    let remote_sum = remote.split_whitespace().next().unwrap_or_default().to_string();
    (sha256_hex(sha.finalize()), remote_sum, elapsed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_throughput_of_a_128_mib_upload_through_a_local_forward() {
    let s = session().await;
    let sink_port = 20_000 + (std::process::id() % 20_000) as u16;
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{sink_port}"));
    assert!(s.forwarder.start(rule.clone()).is_running());
    let sink = start_sink(sink_port);

    let (sent, received, elapsed) = upload_to_sink(rule.listen.port, sink, TRANSFER).await;
    assert_eq!(received, sent, "the container received the same bytes");
    let rate = TRANSFER as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    eprintln!("UPLOAD {TRANSFER} bytes in {elapsed:.2?}: {rate:.1} MiB/s (includes producing the data)");
    let stats = s.forwarder.stats(&rule.id);
    eprintln!("stats: {stats:?}");
    assert_eq!(stats.up, TRANSFER as u64);
    drop(s.connection);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_throughput_of_a_128_mib_download_through_a_local_forward() {
    let s = session().await;
    let source_port = 20_000 + (std::process::id() % 20_000) as u16 + 1;
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{source_port}"));
    assert!(s.forwarder.start(rule.clone()).is_running());

    // A one-shot source of zeros in the container.
    let (user, name) = (setting("SHELLMATE_E2E_USER", "tester"), container());
    // busybox's nc does not close the connection when its input ends; `-w` makes it give up a
    // few seconds after the data is out.
    let script =
        format!("dd if=/dev/zero bs=1048576 count={} 2>/dev/null | nc -l -p {source_port} -w 5", TRANSFER >> 20);
    let source = std::thread::spawn(move || {
        Command::new("docker")
            .args(["exec", "-u", &user, &name, "sh", "-c", &script])
            .stderr(Stdio::null())
            .output()
            .expect("docker is installed");
    });
    for _ in 0..50 {
        let listening = docker(&format!("netstat -ltn 2>/dev/null | grep -c ':{source_port} ' || true"));
        if listening.trim() != "0" {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let mut client = dial(rule.listen.port).await;
    let started = Instant::now();
    let mut sha = Sha256::new();
    let mut length = 0usize;
    let mut buf = vec![0u8; 1 << 20];
    // The data is the whole point; the end of the connection is up to nc.
    while length < TRANSFER {
        let n = tokio::time::timeout(Duration::from_secs(60), client.read(&mut buf)).await.expect("progress").unwrap();
        assert_ne!(n, 0, "the connection ended after {length} bytes");
        let n = n.min(TRANSFER - length);
        sha.update(&buf[..n]);
        length += n;
    }
    let elapsed = started.elapsed();
    drop(client);
    source.join().unwrap();
    let mut expected = Sha256::new();
    let zeros = vec![0u8; 1 << 20];
    for _ in 0..TRANSFER >> 20 {
        expected.update(&zeros);
    }
    assert_eq!(length, TRANSFER);
    assert_eq!(sha256_hex(sha.finalize()), sha256_hex(expected.finalize()));
    let rate = TRANSFER as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    eprintln!("DOWNLOAD {TRANSFER} bytes in {elapsed:.2?}: {rate:.1} MiB/s (includes hashing)");
    drop(s.connection);
}

/// Stops the `nc` listeners of the container that a test started on these ports (they are ours:
/// found by their port), also when the test fails halfway.
struct StopListeners(Vec<u16>);

impl Drop for StopListeners {
    fn drop(&mut self) {
        for port in &self.0 {
            let _ = Command::new("docker")
                .args([
                    "exec",
                    &container(),
                    "sh",
                    "-c",
                    &format!(
                        "kill $(ps -o pid,args | grep 'nc -l -p {port}' | grep -v grep | awk '{{print $1}}') 2>/dev/null || true"
                    ),
                ])
                .output();
        }
    }
}

/// A shell on `connection` that answers within `within`; returns how long the answer took.
async fn shell_answers(connection: &Connection, within: Duration) -> Duration {
    use shellmate_core::{PtyDims, TerminalBackend};

    let dims = PtyDims { cols: 100, rows: 30, pixel_width: 0, pixel_height: 0 };
    let (recorder, sink) = crate::support::Recorder::sink();
    let pending = tokio::time::timeout(Duration::from_secs(10), connection.open_shell(dims))
        .await
        .expect("the connection is frozen: a shell could not be opened")
        .expect("open shell");
    let mut shell = pending.start(sink, dims);
    // Whatever the server prints first (a prompt, a banner) means the shell is up.
    tokio::time::timeout(Duration::from_secs(10), async {
        while recorder.text().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the connection is frozen: the shell said nothing");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    // The typed text is echoed back as written; only the shell's own answer is 42 + the marker.
    shell.write(b"echo $((6*7))stalled\r").unwrap();
    tokio::time::timeout(within, async {
        while !recorder.text().contains("42stalled") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the connection is frozen: the shell did not answer within {within:?}"));
    let took = started.elapsed();
    shell.close();
    took
}

/// The old behaviour of the library (and of the guard that papered over it): a local peer that
/// stops reading froze the whole SSH connection. A peer that stalls for 20 s must only pause its
/// own channel, everything else on the connection must go on, and it must get every byte after it
/// reads again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real OpenSSH server with AllowTcpForwarding (see the module docs)"]
async fn e2e_a_reader_that_stalls_for_20_seconds_holds_up_nothing_else() {
    const FIREHOSE: usize = 256 * 1024 * 1024;
    const UPLOAD: usize = 32 * 1024 * 1024;
    let s = session().await;
    let base = 20_000 + (std::process::id() % 19_000) as u16;
    let (firehose_port, sink_port) = (base + 2, base + 3);
    let cleanup = StopListeners(vec![firehose_port, sink_port]);

    // A source of zeros in the container, far more than the windows in between can hold.
    let (user, name) = (setting("SHELLMATE_E2E_USER", "tester"), container());
    let script =
        format!("dd if=/dev/zero bs=1048576 count={} 2>/dev/null | nc -l -p {firehose_port} -w 60", FIREHOSE >> 20);
    let source = std::thread::spawn(move || {
        Command::new("docker")
            .args(["exec", "-u", &user, &name, "sh", "-c", &script])
            .stderr(Stdio::null())
            .output()
            .expect("docker is installed");
    });
    for _ in 0..50 {
        let listening = docker(&format!("netstat -ltn 2>/dev/null | grep -c ':{firehose_port} ' || true"));
        if listening.trim() != "0" {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let firehose =
        rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{firehose_port}"));
    let other = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{sink_port}"));
    assert!(s.forwarder.start(firehose.clone()).is_running());
    assert!(s.forwarder.start(other.clone()).is_running());
    let sink = start_sink(sink_port);

    // The peer takes the first 64 KiB and then does not read for 20 s.
    let mut stalled = dial(firehose.listen.port).await;
    let mut sha = Sha256::new();
    let mut length = 0usize;
    let mut buf = vec![0u8; 1 << 20];
    while length < 64 * 1024 {
        let n = stalled.read(&mut buf).await.unwrap();
        assert_ne!(n, 0);
        sha.update(&buf[..n]);
        length += n;
    }
    let stall_began = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Meanwhile: a second forward moves 32 MiB, and a shell on the same connection answers.
    let (sent, received, elapsed) =
        tokio::time::timeout(Duration::from_secs(30), upload_to_sink(other.listen.port, sink, UPLOAD))
            .await
            .expect("the connection is frozen: the second forward moved no data while the first was stalled");
    assert_eq!(received, sent);
    eprintln!("{} MiB uploaded in {elapsed:.2?} while the other forward was stalled", UPLOAD >> 20);
    let took = shell_answers(&s.connection, Duration::from_secs(2)).await;
    eprintln!("a shell answered in {took:.2?} while the other forward was stalled");

    tokio::time::sleep(Duration::from_secs(20).saturating_sub(stall_began.elapsed())).await;
    eprintln!("the peer resumes after {:.1?}", stall_began.elapsed());

    // The peer reads on and gets everything.
    while length < FIREHOSE {
        let n = tokio::time::timeout(Duration::from_secs(60), stalled.read(&mut buf)).await.expect("progress").unwrap();
        assert_ne!(n, 0, "the connection ended after {length} bytes");
        let n = n.min(FIREHOSE - length);
        sha.update(&buf[..n]);
        length += n;
    }
    let mut expected = Sha256::new();
    let zeros = vec![0u8; 1 << 20];
    for _ in 0..FIREHOSE >> 20 {
        expected.update(&zeros);
    }
    assert_eq!(sha256_hex(sha.finalize()), sha256_hex(expected.finalize()), "every byte arrived intact");
    drop(stalled);

    // busybox's nc keeps its connection after the data; stop it so that the thread ends.
    drop(cleanup);
    let _ = source.join();
    drop(s.connection);
}
