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
        run_in_background: false,
        exec_limits: shellmate_ai::agent::ExecLimits::default(),
        class: classify(command),
    }
}
async fn local(
    call: &ToolCall,
    cwd: PathBuf,
    shell: &str,
    home: Option<&std::path::Path>,
) -> ToolOutcome {
    let tasks = super::super::tasks::Tasks::new(cwd.clone(), "test".into());
    run_local(&tasks, call.clone(), cwd, shell, home).await
}
async fn run_local(
    tasks: &super::super::tasks::Tasks,
    call: ToolCall,
    cwd: PathBuf,
    shell: &str,
    home: Option<&std::path::Path>,
) -> ToolOutcome {
    let shell = shell.to_owned();
    let home = home.map(std::path::Path::to_path_buf);
    let work = call.clone();
    tasks
        .run(call, None, move |spool| {
            Box::pin(async move { super::local(&work, cwd, &shell, home.as_deref(), &spool).await })
        })
        .await
}
async fn ssh(connection: &Connection, call: &ToolCall) -> ToolOutcome {
    let temp = Temp::new();
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "test".into());
    let connection = connection.clone();
    let work = call.clone();
    tasks
        .run(call.clone(), None, move |spool| {
            Box::pin(async move { super::ssh(&connection, &work, &spool).await })
        })
        .await
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
    assert_eq!(out.stdout(), "out\n[stderr] err");
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
async fn local_timeout_promotes_then_stop_kills_entire_process_group() {
    let temp = Temp::new();
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "test".into());
    let out = completed(
        run_local(
            &tasks,
            call(
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
    assert!(out.task_id.is_some());
    let ids = pids(&temp.0.join("pids")).await;
    assert!(ids.iter().all(|id| process_exists(*id)));
    let stopped = tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::StopTask {
                id: out.task_id.unwrap(),
            },
            Default::default(),
        )
        .await;
    assert!(stopped.inline.contains("已停止任务"));
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
    assert!(out.stdout().chars().count() < 2100);
    let path =
        shellmate_core::ai_output::path(&temp.0, "test", out.output_id.as_ref().unwrap()).unwrap();
    assert!(std::fs::metadata(path).unwrap().len() > 300000);
    assert_eq!(out.exit_code, Some(0));
}
struct PtyHarness {
    executor: AppExecutor,
    pty: Arc<Mutex<LocalPty>>,
    scan: Arc<Mutex<Option<(String, MarkerScanner)>>>,
    output: Arc<Mutex<Vec<u8>>>,
    typed: Arc<Mutex<Vec<Vec<u8>>>>,
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
        let typed = Arc::new(Mutex::new(Vec::new()));
        let sent = typed.clone();
        let pump = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                match event {
                    Ui::TerminalStart {
                        command,
                        nonce,
                        spool,
                        reply,
                        ..
                    } => {
                        if reply.is_closed() {
                            continue;
                        }
                        assert_eq!(nonce.len(), 12);
                        assert!(nonce.bytes().all(|b| b.is_ascii_alphanumeric()));
                        if s.lock().unwrap().is_some() {
                            let _ = reply.send(Err(Box::new(ToolOutcome::Busy)));
                            continue;
                        }
                        let (scanner, handle) = MarkerScanner::spooled(&nonce, spool);
                        *s.lock().unwrap() = Some((nonce.clone(), scanner));
                        let line = shellmate_term::agent::command_line(&command, &nonce);
                        sent.lock().unwrap().push(line.clone());
                        p.lock().unwrap().write(&line).unwrap();
                        if reply.send(Ok(handle)).is_err() {
                            p.lock().unwrap().write(b"\x03").unwrap();
                            *s.lock().unwrap() = None;
                        }
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
                subagents: None,
                memory: None,
                tasks: super::super::tasks::Tasks::new(temp.0.clone(), "pty".into()),
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
            typed,
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
        // The real terminal also enforces its 300 ms recent-output busy cooldown.
        tokio::time::sleep(Duration::from_millis(350)).await;
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
    assert_eq!(out.stdout(), "[stderr] problem");
    assert_eq!(out.exit_code, Some(3));
    let out = completed(
        ssh(
            &connection,
            &call("sleep 30", Target::Background, Duration::from_secs(1)),
        )
        .await,
    );
    assert!(out.timed_out);
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "docker".into());
    for (command, timeout, expected) in [
        (
            "printf HEAD; head -c 1048576 /dev/zero | tr '\\000' x; printf TAIL",
            Duration::from_secs(10),
            false,
        ),
        (
            "printf start; sleep 1; printf done",
            Duration::from_millis(50),
            true,
        ),
    ] {
        let c = call(command, Target::Background, timeout);
        let work = c.clone();
        let connection = connection.clone();
        let out = completed(
            tasks
                .run(c, None, move |spool| {
                    Box::pin(async move { super::ssh(&connection, &work, &spool).await })
                })
                .await,
        );
        assert_eq!(out.task_id.is_some(), expected);
        if expected {
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(
                tasks.list().last().unwrap().status,
                super::super::tasks::Status::Exited(Some(0))
            );
            assert!(!tasks.take_notices().is_empty());
        } else {
            assert_eq!(
                std::fs::metadata(
                    shellmate_core::ai_output::path(
                        &temp.0,
                        "docker",
                        out.output_id.as_ref().unwrap()
                    )
                    .unwrap()
                )
                .unwrap()
                .len(),
                1048584
            );
        }
    }
    let mut c = call(
        "while :; do printf tick; sleep 0.1; done",
        Target::Background,
        Duration::from_secs(10),
    );
    c.run_in_background = true;
    let work = c.clone();
    let link = connection.clone();
    let out = completed(
        tasks
            .run(c, None, move |spool| {
                Box::pin(async move { super::ssh(&link, &work, &spool).await })
            })
            .await,
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::StopTask {
                id: out.task_id.unwrap(),
            },
            Default::default(),
        )
        .await;
    assert!(stopped.inline.contains("已停止任务"));
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
    assert!(!completed(out).successful);
    assert_eq!(
        h.executor.tasks.list()[0].status,
        super::super::tasks::Status::Disconnected
    );
    h.settled().await;
}
#[tokio::test]
async fn terminal_cancel_before_start_reply_cleans_up_registration() {
    let (ui, rx) = async_channel::unbounded();
    let temp = Temp::new();
    let executor = AppExecutor {
        subagents: None,
        memory: None,
        tasks: super::super::tasks::Tasks::new(temp.0.clone(), "pty".into()),
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
    tokio::time::sleep(Duration::from_millis(25)).await;
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
async fn terminal_missing_marker_stays_busy_until_explicit_stop() {
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
    assert!(out.message.as_ref().unwrap().contains("已转到后台"));
    assert!(h.scan.lock().unwrap().is_some());
    let read = h
        .executor
        .tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                id: out.output_id.clone().unwrap(),
                offset_line: 0,
                limit_lines: None,
                grep: None,
            }),
            Default::default(),
        )
        .await;
    assert!(read.inline.contains("partial"));
    assert_eq!(
        h.executor
            .execute(call(
                "printf busy",
                Target::Terminal,
                Duration::from_secs(1)
            ))
            .await,
        ToolOutcome::Busy
    );
    h.executor
        .tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::StopTask {
                id: out.task_id.unwrap(),
            },
            Default::default(),
        )
        .await;
    h.settled().await;
}

#[tokio::test]
async fn local_megabyte_and_disk_ring_preserve_head_tail_and_failure() {
    for (size, code) in [(1_048_576, 0), (1_048_576, 3), (65 * 1024 * 1024, 0)] {
        let temp = Temp::new();
        let command = format!(
            "printf 'HEAD\\n'; head -c {size} /dev/zero | tr '\\000' x; printf '\\nTAIL\\n'; exit {code}"
        );
        let out = completed(
            local(
                &call(&command, Target::Background, Duration::from_secs(60)),
                temp.0.clone(),
                "/bin/sh",
                Some(&temp.0),
            )
            .await,
        );
        assert_eq!(out.exit_code, Some(code));
        assert_eq!(out.successful, code == 0);
        assert!(out.stdout().contains("HEAD"));
        if code != 0 {
            assert!(out.stdout().contains("TAIL"));
            assert!(out.stdout().chars().count() <= 10000);
        } else {
            assert!(out.stdout().contains("完整输出已保存"));
        }
        let path =
            shellmate_core::ai_output::path(&temp.0, "test", out.output_id.as_ref().unwrap())
                .unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"HEAD\n"));
        assert!(bytes.ends_with(b"\nTAIL\n"));
        if size < 64 * 1024 * 1024 {
            assert_eq!(bytes.len(), size + 11);
        } else {
            assert!(bytes.len() < 64 * 1024 * 1024 + 256);
            assert!(
                bytes
                    .windows("省略中间输出".len())
                    .any(|w| w == "省略中间输出".as_bytes())
            );
        }
    }
}
#[tokio::test]
async fn terminal_megabyte_success_and_failure_spool_is_complete() {
    let h = PtyHarness::new().await;
    for code in [0, 3] {
        let cmd = format!(
            "printf 'HEAD\\n'; head -c 1048576 /dev/zero | tr '\\000' x; printf '\\nTAIL\\n'; (exit {code})"
        );
        let out = completed(
            h.executor
                .execute(call(&cmd, Target::Terminal, Duration::from_secs(15)))
                .await,
        );
        assert_eq!(out.exit_code, Some(code));
        assert!(out.stdout().contains("HEAD"));
        assert!(out.stdout().chars().count() <= if code == 0 { 2100 } else { 10000 });
        if code != 0 {
            assert!(out.stdout().contains("TAIL"));
        }
        let path =
            shellmate_core::ai_output::path(&h._temp.0, "pty", out.output_id.as_ref().unwrap())
                .unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() >= 1_048_576);
        assert!(bytes.starts_with(b"HEAD\r\n"));
        assert!(bytes.ends_with(b"TAIL\r\n"));
        assert!(!bytes.windows(5).any(|w| w == b"6973;"));
        h.settled().await;
    }
}
#[tokio::test]
async fn local_background_returns_immediately_read_running_then_notice_and_retained_inventory() {
    use shellmate_ai::agent::{LocalTool, ReadOutput};
    let temp = Temp::new();
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "test".into());
    let mut c = call(
        "cd .; printf 'zero\none\nmatch-a\n'; while [ ! -f release ]; do sleep 0.02; done; printf 'match-b\nend\n'",
        Target::Background,
        Duration::from_secs(30),
    );
    c.run_in_background = true;
    let now = Instant::now();
    let out = completed(run_local(&tasks, c, temp.0.clone(), "/bin/sh", Some(&temp.0)).await);
    assert!(now.elapsed() < Duration::from_secs(1));
    let message = out.message.unwrap();
    assert!(message.contains("后台启动"));
    assert!(message.contains("其中的 cd 不会影响后续命令"));
    assert!(!message.contains("命令仍在终端中运行"));
    let id = out.task_id.unwrap();
    let read = |offset, limit, grep: Option<&str>| {
        LocalTool::ReadOutput(ReadOutput {
            id: id.clone(),
            offset_line: offset,
            limit_lines: Some(limit),
            grep: grep.map(str::to_owned),
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let filtered = tasks
            .local_tool(read(0, 2000, Some("match")), Default::default())
            .await;
        if filtered.inline.contains("match-a") {
            assert!(!filtered.inline.contains("match-b"));
            break;
        }
        assert!(Instant::now() < deadline, "initial output never arrived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let running = tasks.local_tool(read(1, 1, None), Default::default()).await;
    assert!(running.inline.ends_with("one\n"));
    let (active, outputs) = tasks.state();
    assert_eq!(active.len(), 1);
    assert!(outputs[0].contains(out.output_id.as_ref().unwrap()));
    std::fs::write(temp.0.join("release"), []).unwrap();
    while tasks.has_running() {
        assert!(
            Instant::now() < deadline,
            "task did not finish after release"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        tasks.take_notices(),
        ["后台任务 #1 已结束 · 退出码 0 · 用 read_output 查看输出"]
    );
    assert!(tasks.take_notices().is_empty());
    assert!(tasks.state().0.is_empty());
    let filtered = tasks
        .local_tool(read(0, 2000, Some("match")), Default::default())
        .await;
    assert!(filtered.inline.contains("match-a\nmatch-b\n"));
    shellmate_core::ai_output::delete_conversation(&temp.0, "test").unwrap();
    assert_eq!(
        tasks
            .local_tool(read(0, 2000, None), Default::default())
            .await
            .inline,
        shellmate_core::ai_output::CLEARED
    );
}
#[tokio::test]
async fn timeout_finishes_later_sleep_is_stopped_and_cd_hint_is_present() {
    let temp = Temp::new();
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "test".into());
    let out = completed(
        run_local(
            &tasks,
            call(
                "cd .; sleep 0.3; printf done",
                Target::Background,
                Duration::from_millis(50),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    let message = out.message.unwrap();
    assert!(message.contains("其中的 cd 不会影响后续命令"));
    assert!(!message.contains("命令仍在终端中运行"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        tasks.list()[0].status,
        super::super::tasks::Status::Exited(Some(0))
    );
    assert_eq!(tasks.take_notices().len(), 1);
    let start = Instant::now();
    let out = completed(
        run_local(
            &tasks,
            call(
                "TOKEN=x sleep 30",
                Target::Background,
                Duration::from_millis(50),
            ),
            temp.0.clone(),
            "/bin/sh",
            Some(&temp.0),
        )
        .await,
    );
    assert!(out.timed_out && !out.successful);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(tasks.list()[1].status, super::super::tasks::Status::Stopped);
}
#[tokio::test]
async fn terminal_timeout_keeps_capture_busy_stop_task_interrupts_and_shell_recovers() {
    let h = PtyHarness::new().await;
    let out = completed(
        h.executor
            .execute(call(
                "printf 'started\\n'; sleep 30",
                Target::Terminal,
                Duration::from_millis(100),
            ))
            .await,
    );
    assert!(out.message.unwrap().contains("已转到后台"));
    assert!(h.scan.lock().unwrap().is_some());
    let read = h
        .executor
        .tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                id: out.output_id.unwrap(),
                offset_line: 0,
                limit_lines: None,
                grep: None,
            }),
            Default::default(),
        )
        .await;
    assert!(read.inline.contains("started"));
    assert_eq!(
        h.executor
            .execute(call("echo busy", Target::Terminal, Duration::from_secs(1)))
            .await,
        ToolOutcome::Busy
    );
    let mut busy = call("echo still-busy", Target::Terminal, Duration::from_secs(1));
    busy.run_in_background = true;
    assert_eq!(h.executor.execute(busy).await, ToolOutcome::Busy);
    let stopped = h
        .executor
        .tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::StopTask {
                id: out.task_id.unwrap(),
            },
            Default::default(),
        )
        .await;
    assert!(stopped.inline.contains("已停止任务"));
    h.settled().await;
    let out = completed(
        h.executor
            .execute(call(
                "printf usable",
                Target::Terminal,
                Duration::from_secs(3),
            ))
            .await,
    );
    assert_eq!(out.stdout().trim(), "usable");
}
#[tokio::test]
async fn dropping_registry_stops_background_process_groups() {
    let temp = Temp::new();
    let tasks = super::super::tasks::Tasks::new(temp.0.clone(), "test".into());
    let mut c = call(group_command(), Target::Background, Duration::from_secs(60));
    c.run_in_background = true;
    run_local(&tasks, c, temp.0.clone(), "/bin/sh", Some(&temp.0)).await;
    let ids = pids(&temp.0.join("pids")).await;
    drop(tasks);
    assert_gone(&ids).await;
}

#[tokio::test]
async fn terminal_directory_change_persists_after_timeout_or_background() {
    for run_in_background in [false, true] {
        let h = PtyHarness::new().await;
        let command = "cd /; printf 'directory changed\\n'; sleep 0.3; pwd";
        let mut c = call(command, Target::Terminal, Duration::from_millis(50));
        c.run_in_background = run_in_background;
        let out = completed(h.executor.execute(c).await);
        let message = out.message.unwrap();
        assert!(message.contains(if run_in_background {
            "后台启动"
        } else {
            "已转到后台"
        }));
        assert!(message.contains("命令仍在终端中运行"));
        assert!(message.contains("结束后终端状态（包括目录变化）会保留"));
        assert!(message.contains("在此之前终端处于占用状态"));
        assert!(!message.contains("其中的 cd 不会影响后续命令"));

        let deadline = Instant::now() + Duration::from_secs(5);
        while h.executor.tasks.has_running() {
            assert!(Instant::now() < deadline, "terminal command did not finish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        h.settled().await;
        {
            let typed = h.typed.lock().unwrap();
            assert_eq!(typed.len(), 1);
            // Preserve the pre-M3 input envelope and marker, with the approved
            // command itself typed verbatim, including its original quotes.
            let line = String::from_utf8_lossy(&typed[0]);
            assert!(line.starts_with(&format!("\x15 {command}; printf '\\033]6973;")));
            assert!(!line.contains("eval"));
        }
        let out = completed(
            h.executor
                .execute(call("pwd", Target::Terminal, Duration::from_secs(5)))
                .await,
        );
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout().trim(), "/");
    }
}
