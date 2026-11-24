use super::*;
use shellmate_ai::{ToolOutput, policy::classify};
use shellmate_core::{HostId, Program, Secret};
use shellmate_term::{
    LocalPty, TerminalBackend,
    agent::{CAP as TERM_CAP, MarkerScanner},
};
use std::sync::Mutex;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("shellmate-agent-{}", HostId::generate()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn call(command: &str, target: Target, timeout: Duration) -> ToolCall {
    ToolCall {
        id: HostId::generate().to_string(),
        command: command.into(),
        purpose: "测试".into(),
        target,
        timeout,
        class: classify(command),
    }
}
fn completed(out: ToolOutcome) -> ToolOutput {
    match out {
        ToolOutcome::Completed(out) => out,
        other => panic!("{other:?}"),
    }
}
#[tokio::test]
async fn local_stdout_stderr_exit_stdin_pager_and_cwd() {
    let temp = Temp::new();
    let out = completed(
        local(
            &call(
                "printf out; printf err >&2; exit 3",
                Target::Background,
                Duration::from_secs(3),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    assert_eq!(out.stdout(), "out");
    assert_eq!(out.stderr(), "err");
    assert_eq!(out.exit_code, Some(3));
    let out = completed(
        local(
            &call(
                "cat; printf '%s' \"$PAGER:$HOME:$PWD\"",
                Target::Background,
                Duration::from_secs(3),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    assert!(!out.timed_out);
    assert!(out.stdout().starts_with("cat:"));
    assert!(out.stdout().contains(temp.0.to_str().unwrap()));
    for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
        let out = completed(
            local(
                &call(
                    "printf '%s' \"$PAGER:$GIT_PAGER:$SYSTEMD_PAGER:$SYSTEMD_COLORS:$TERM\"",
                    Target::Background,
                    Duration::from_secs(3),
                ),
                temp.0.clone(),
                shell,
                Some(&temp.0),
            )
            .await,
        );
        assert_eq!(out.stdout(), "cat:cat::0:dumb", "{shell}");
    }
}
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 probes existence without changing the process.
    unsafe { libc::kill(pid, 0) == 0 }
}
async fn assert_gone(pids: &[i32]) {
    let start = Instant::now();
    while pids.iter().any(|p| process_exists(*p)) && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pids.iter().all(|p| !process_exists(*p)),
        "process group survived: {pids:?}"
    );
}
async fn pids(path: &std::path::Path) -> Vec<i32> {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            let p: Vec<i32> = s.lines().filter_map(|s| s.parse().ok()).collect();
            if p.len() == 3 {
                return p;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "pid file never ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
fn group_command() -> &'static str {
    "echo $$ > pids; sleep 60 & echo $! >> pids; sleep 60 & echo $! >> pids; wait"
}
#[tokio::test]
async fn local_timeout_kills_entire_process_group() {
    let temp = Temp::new();
    let out = completed(
        local(
            &call(
                group_command(),
                Target::Background,
                Duration::from_millis(200),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    assert!(out.timed_out);
    assert_gone(&pids(&temp.0.join("pids")).await).await;
}
#[tokio::test]
async fn local_drop_kills_entire_process_group() {
    let temp = Temp::new();
    let c = call(group_command(), Target::Background, Duration::from_secs(90));
    let mut run = Box::pin(local(&c, temp.0.clone(), "/bin/sh", Some(&temp.0)));
    let path = temp.0.join("pids");
    let ids = tokio::select! {ids=pids(&path)=>ids,_=&mut run=>panic!("finished early")};
    drop(run);
    assert_gone(&ids).await;
}
#[tokio::test]
async fn local_output_cap_drains_and_preserves_content() {
    let temp = Temp::new();
    let out = completed(
        local(
            &call(
                "printf 'TOKEN=hidden\\n'; head -c 300000 /dev/zero | tr '\\000' x",
                Target::Background,
                Duration::from_secs(4),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    assert!(out.truncated);
    assert!(out.stdout().contains("TOKEN=hidden"));
    assert!(out.stdout().len() < 9000);
    assert_eq!(out.exit_code, Some(0));
}
struct PtyHarness {
    executor: AppExecutor,
    pty: Arc<Mutex<LocalPty>>,
    scan: Arc<Mutex<Option<(String, MarkerScanner)>>>,
    output: Arc<Mutex<Vec<u8>>>,
    _temp: Temp,
    pump: tokio::task::JoinHandle<()>,
}
impl Drop for PtyHarness {
    fn drop(&mut self) {
        self.pty.lock().unwrap().close();
        self.pump.abort();
    }
}
impl PtyHarness {
    async fn new() -> Self {
        let temp = Temp::new();
        let scan = Arc::new(Mutex::new(None::<(String, MarkerScanner)>));
        let output = Arc::new(Mutex::new(Vec::new()));
        let (s, o) = (scan.clone(), output.clone());
        let alive = Arc::new(AtomicBool::new(true));
        let end = alive.clone();
        let program = Program {
            executable: "/bin/sh".into(),
            args: vec!["-i".into()],
            env: [
                ("HOME".into(), Secret::new(temp.0.to_str().unwrap())),
                ("ENV".into(), Secret::new("/dev/null")),
                ("PS1".into(), Secret::new("agent-test$ ")),
                ("TERM".into(), Secret::new("dumb")),
            ]
            .into(),
            cwd: Some(temp.0.clone()),
        };
        let pty = Arc::new(Mutex::new(
            LocalPty::spawn_program(
                &program,
                shellmate_term::PtyDims {
                    cols: 500,
                    rows: 30,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                move |bytes| {
                    let render = if let Some((_, scanner)) = &mut *s.lock().unwrap() {
                        scanner.feed(bytes)
                    } else {
                        bytes.to_vec()
                    };
                    let mut out = o.lock().unwrap();
                    if out.len() < TERM_CAP {
                        out.extend(render);
                    }
                },
                move |_| end.store(false, Ordering::Release),
            )
            .unwrap(),
        ));
        let start = Instant::now();
        while !String::from_utf8_lossy(&output.lock().unwrap()).contains("agent-test$ ") {
            assert!(start.elapsed() < Duration::from_secs(3));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (ui, rx) = async_channel::unbounded();
        let (p, s) = (pty.clone(), scan.clone());
        let pump = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                match event {
                    Ui::TerminalStart {
                        command,
                        nonce,
                        reply,
                        ..
                    } => {
                        if reply.is_closed() {
                            continue;
                        }
                        assert_eq!(nonce.len(), 12);
                        assert!(nonce.bytes().all(|b| b.is_ascii_alphanumeric()));
                        let (scanner, handle) = MarkerScanner::new(&nonce);
                        *s.lock().unwrap() = Some((nonce.clone(), scanner));
                        p.lock()
                            .unwrap()
                            .write(&shellmate_term::agent::command_line(&command, &nonce))
                            .unwrap();
                        if reply.send(Ok(handle)).is_err() {
                            p.lock().unwrap().write(b"\x03").unwrap();
                            *s.lock().unwrap() = None;
                        }
                    }
                    Ui::TerminalInterrupt { .. } => {
                        p.lock().unwrap().write(b"\x03").unwrap();
                    }
                    Ui::TerminalFinish { nonce, .. } => {
                        let mut s = s.lock().unwrap();
                        if let Some((id, scanner)) = &*s
                            && *id == nonce
                        {
                            if scanner.running() {
                                p.lock().unwrap().write(b"\x03").unwrap();
                            }
                            *s = None;
                        }
                    }
                    _ => panic!("unexpected bridge event"),
                }
            }
        });
        Self {
            executor: AppExecutor {
                binding: Binding {
                    session: 1,
                    capabilities: shellmate_ai::agent::Capabilities {
                        terminal: true,
                        ..Default::default()
                    },
                    connection: None,
                    local: true,
                    alive,
                },
                ui,
            },
            pty,
            scan,
            output,
            _temp: temp,
            pump,
        }
    }
    async fn settled(&self) {
        let start = Instant::now();
        while self.scan.lock().unwrap().is_some() {
            assert!(start.elapsed() < Duration::from_secs(3));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
#[tokio::test]
async fn terminal_real_pty_strips_echo_and_marker_and_parses_exit() {
    let h = PtyHarness::new().await;
    let out = completed(
        h.executor
            .execute(call(
                "printf 'a\\nb\\n'; false",
                Target::Terminal,
                Duration::from_secs(3),
            ))
            .await,
    );
    assert_eq!(out.stdout().trim(), "a\nb");
    assert_eq!(out.exit_code, Some(1));
    assert!(!out.timed_out);
    h.settled().await;
    assert!(
        !h.output
            .lock()
            .unwrap()
            .windows(7)
            .any(|b| b == b"\x1b]6973;")
    );
}
#[tokio::test]
async fn terminal_real_pty_timeout_interrupts_and_shell_remains_usable() {
    let h = PtyHarness::new().await;
    let out = completed(
        h.executor
            .execute(call("sleep 30", Target::Terminal, Duration::from_secs(1)))
            .await,
    );
    assert!(out.timed_out);
    h.settled().await;
    if out.exit_code.is_none() {
        assert!(out.stderr().contains("未检测到命令结束标记"));
    }
    let out = completed(
        h.executor
            .execute(call(
                "printf alive",
                Target::Terminal,
                Duration::from_secs(3),
            ))
            .await,
    );
    assert_eq!(out.stdout().trim(), "alive");
    assert_eq!(out.exit_code, Some(0));
}
#[tokio::test]
async fn terminal_drop_interrupts_and_removes_capture() {
    let h = PtyHarness::new().await;
    let mut run = h
        .executor
        .execute(call("sleep 30", Target::Terminal, Duration::from_secs(90)));
    tokio::select! {_=&mut run=>panic!("early completion"),()=tokio::time::sleep(Duration::from_millis(200))=>{}}
    assert!(h.scan.lock().unwrap().is_some());
    drop(run);
    h.settled().await;
    let out = completed(
        h.executor
            .execute(call(
                "printf cancelled",
                Target::Terminal,
                Duration::from_secs(3),
            ))
            .await,
    );
    assert_eq!(out.stdout().trim(), "cancelled");
}

/// Same isolated Docker OpenSSH fixture/settings as shellmate-ssh/tests/ssh/e2e.rs.
#[tokio::test]
#[ignore = "needs the shellmate-e2e Docker SSH server on 127.0.0.1:2222; never run in the sandbox"]
async fn ssh_background_docker_uname_nonzero_timeout_stderr() {
    use shellmate_ssh::{
        ConnectOptions, HostKeyRequest, HostKeyVerifier, NoInteraction, Route, SshHost,
    };
    struct Trust;
    #[async_trait::async_trait]
    impl HostKeyVerifier for Trust {
        async fn verify(&self, _: HostKeyRequest) -> bool {
            true
        }
    }
    let temp = Temp::new();
    let options = ConnectOptions {
        verifier: Arc::new(Trust),
        prompter: Arc::new(NoInteraction),
        known_hosts: temp.0.join("known_hosts"),
        ssh_dir: temp.0.clone(),
        agent_socket: Some(temp.0.join("no-agent.sock")),
        connect_timeout: Duration::from_secs(5),
        connect_watchdog: Duration::from_secs(10),
        keepalive_interval: Duration::from_secs(30),
        keepalive_max: 3,
        on_event: None,
    };
    let port = std::env::var("SHELLMATE_E2E_PORT")
        .unwrap_or_else(|_| "2222".into())
        .parse()
        .unwrap();
    let user = std::env::var("SHELLMATE_E2E_USER").unwrap_or_else(|_| "tester".into());
    let password =
        std::env::var("SHELLMATE_E2E_PASSWORD").unwrap_or_else(|_| "e2e-pass-123".into());
    let connection = Connection::connect(
        Route::direct(
            SshHost::new(
                "127.0.0.1",
                port,
                user,
                shellmate_core::AuthMethod::Password,
            )
            .with_password(Secret::new(password)),
        ),
        options,
    )
    .await
    .unwrap();
    let out = completed(
        ssh(
            &connection,
            &call("uname -s", Target::Background, Duration::from_secs(5)),
        )
        .await,
    );
    assert_eq!(out.stdout().trim(), "Linux");
    assert_eq!(out.exit_code, Some(0));
    let out = completed(
        ssh(
            &connection,
            &call(
                "printf problem >&2; exit 3",
                Target::Background,
                Duration::from_secs(5),
            ),
        )
        .await,
    );
    assert_eq!(out.stderr(), "problem");
    assert_eq!(out.exit_code, Some(3));
    let out = completed(
        ssh(
            &connection,
            &call("sleep 30", Target::Background, Duration::from_secs(1)),
        )
        .await,
    );
    assert!(out.timed_out);
}

#[tokio::test]
async fn terminal_bound_session_disconnect_removes_capture() {
    let h = PtyHarness::new().await;
    let run = h
        .executor
        .execute(call("sleep 30", Target::Terminal, Duration::from_secs(30)));
    let disconnect = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        h.executor.binding.alive.store(false, Ordering::Release);
    };
    let (out, ()) = tokio::join!(run, disconnect);
    assert_eq!(out, ToolOutcome::Disconnected);
    h.settled().await;
}
#[tokio::test]
async fn terminal_cancel_before_start_reply_cleans_up_registration() {
    let (ui, rx) = async_channel::unbounded();
    let executor = AppExecutor {
        binding: Binding {
            session: 17,
            capabilities: shellmate_ai::Capabilities {
                terminal: true,
                ..Default::default()
            },
            connection: None,
            local: false,
            alive: Arc::new(AtomicBool::new(true)),
        },
        ui,
    };
    let mut run = executor.execute(call("sleep 30", Target::Terminal, Duration::from_secs(30)));
    let request = tokio::select! {event=rx.recv()=>event.unwrap(),_=&mut run=>panic!("finished")};
    let Ui::TerminalStart {
        session,
        nonce,
        reply,
        ..
    } = request
    else {
        panic!("start")
    };
    assert_eq!(session, 17);
    drop(run);
    assert!(reply.is_closed());
    let Ui::TerminalFinish {
        session,
        nonce: cleanup,
    } = rx.recv().await.unwrap()
    else {
        panic!("cleanup")
    };
    assert_eq!(session, 17);
    assert_eq!(cleanup, nonce);
}

#[tokio::test]
async fn terminal_missing_marker_reports_partial_output_after_grace() {
    let h = PtyHarness::new().await;
    let out = completed(
        h.executor
            .execute(call(
                "printf partial # marker commented out",
                Target::Terminal,
                Duration::from_millis(100),
            ))
            .await,
    );
    assert!(out.timed_out);
    assert!(out.stdout().contains("partial"));
    assert!(
        out.stderr()
            .contains("未检测到命令结束标记（可能是不支持的 shell）")
    );
    assert!(out.duration_ms >= 2100);
    h.settled().await;
}
