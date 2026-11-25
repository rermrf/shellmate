//! Exercise the actual PTY -> binding directory -> model prompt -> local executor chain.
use crate::ai::{
    exec::{AppExecutor, Binding, session_cwd},
    host::{Executor, Host, Ui},
    tasks::Tasks,
};
use futures::StreamExt;
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
use shellmate_ai::{
    Capabilities, Config, Conversation, Entry, PermissionMode, RigClient, Target, ToolCall,
    ToolOutcome, UserTurn, agent,
};
use shellmate_core::{Program, Secret};
use shellmate_term::{LocalPty, TerminalBackend};
use std::{
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

struct Shell {
    pty: LocalPty,
    output: Arc<Mutex<Vec<u8>>>,
}
impl Shell {
    async fn new(root: &Path) -> Self {
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let pty = LocalPty::spawn_program(
            &Program {
                executable: "/bin/sh".into(),
                args: vec!["-i".into()],
                cwd: Some(root.to_owned()),
                env: [
                    ("HOME".into(), Secret::new(root.to_str().unwrap())),
                    ("ENV".into(), Secret::new("/dev/null")),
                    ("PS1".into(), Secret::new("CWD_TEST_PROMPT> ")),
                ]
                .into(),
            },
            Default::default(),
            move |bytes| sink.lock().unwrap().extend_from_slice(bytes),
            |_| {},
        )
        .unwrap();
        let shell = Self { pty, output };
        let start = Instant::now();
        while !String::from_utf8_lossy(&shell.output.lock().unwrap()).contains("CWD_TEST_PROMPT> ")
        {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "shell did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shell
    }
    fn pid(&self) -> i32 {
        self.pty.child_pid().unwrap() as i32
    }
    async fn cd(&mut self, path: &Path) {
        let literal = format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        self.pty
            .write(format!("cd -- {literal}\n").as_bytes())
            .unwrap();
        let start = Instant::now();
        while session_cwd(true, Some(self.pid()), None).as_deref() != path.to_str() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "PTY cd was not observed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        self.pty.close();
    }
}
struct Bridge(tokio::task::JoinHandle<()>);
impl Drop for Bridge {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn response(command: Option<&str>) -> MockHttpResponse {
    let delta = if let Some(command) = command {
        serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"run_command","arguments":serde_json::json!({"command":command,"target":"background"}).to_string()}}]})
    } else {
        serde_json::json!({"role":"assistant","content":"done"})
    };
    let chunk = |delta, finish| serde_json::json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let events = [
        chunk(delta, serde_json::Value::Null),
        chunk(
            serde_json::json!({}),
            serde_json::json!(if command.is_some() {
                "tool_calls"
            } else {
                "stop"
            }),
        ),
    ];
    MockHttpResponse::success_typed(
        events
            .into_iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n",
        "text/event-stream",
    )
}
fn config() -> Config {
    Config {
        provider: Default::default(),
        model: "fixture".into(),
        api_key: shellmate_ai::ApiKey::new("fixture"),
        reasoning: Default::default(),
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Default::default(),
    }
}
fn executor(root: &Path, binding: Binding, ui: async_channel::Sender<Ui>) -> AppExecutor {
    AppExecutor {
        subagents: None,
        memory: None,
        binding,
        tasks: Tasks::new(root.into(), "test".into()),
        ui,
    }
}
fn call(command: &str) -> ToolCall {
    ToolCall {
        id: shellmate_core::HostId::generate().to_string(),
        command: command.into(),
        purpose: "test cwd".into(),
        target: Target::Background,
        timeout: Duration::from_secs(3),
        run_in_background: false,
        exec_limits: Default::default(),
        class: shellmate_ai::policy::classify(command),
    }
}
async fn run_main(executor: AppExecutor, command: &str) -> (Conversation, SequencedHttpClient) {
    let http = SequencedHttpClient::new([response(Some(command)), response(None)]);
    let client = RigClient::with_http(&config(), http.clone()).unwrap();
    let caps = executor.binding.capabilities.clone();
    let host = Host {
        compaction: Default::default(),
        mode: Arc::new(Mutex::new(PermissionMode::Full)),
        ui: executor.ui.clone(),
        alive: executor.binding.alive.clone(),
        resolved: Default::default(),
        executor: Box::new(executor),
    };
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("inspect current project", Default::default()));
    let events = agent::run(&client, &mut log, caps, &host, Default::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(shellmate_ai::AgentEvent::Done(_))
    ));
    (log, http)
}
fn output(log: &Conversation) -> &str {
    let step = log
        .entries()
        .iter()
        .find_map(|e| {
            if let Entry::Step(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    assert!(!step.result.is_error, "{}", step.result.inline);
    &step.result.inline
}
fn prompt(http: &SequencedHttpClient) -> String {
    let body: serde_json::Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].to_string())
        .collect()
}

#[tokio::test]
async fn no_osc7_pty_directory_is_shared_by_main_and_child_and_pinned_for_the_turn() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let project = root.join("项目 A ' quoted $(literal)");
    let other = root.join("project-B");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::fs::write(project.join("project-marker"), "PROJECT-A\n").unwrap();
    std::fs::write(other.join("project-marker"), "PROJECT-B\n").unwrap();
    let mut shell = Shell::new(&root).await;
    shell.cd(&project).await;
    let cwd = session_cwd(true, Some(shell.pid()), None).unwrap();
    assert_eq!(Path::new(&cwd), project);
    assert_eq!(
        session_cwd(true, Some(shell.pid()), Some("/stale/osc7")),
        Some(cwd.clone())
    );
    assert!(
        !shell
            .output
            .lock()
            .unwrap()
            .windows(4)
            .any(|w| w == b"\x1b]7;"),
        "test shell must not emit OSC 7"
    );
    let mut other_terminal = Shell::new(&root).await;
    other_terminal.cd(&other).await;
    assert_eq!(
        session_cwd(true, Some(shell.pid()), None),
        Some(cwd.clone())
    );
    assert_eq!(
        session_cwd(true, Some(other_terminal.pid()), None).as_deref(),
        other.to_str()
    );

    let (ui, rx) = async_channel::unbounded();
    let _bridge = Bridge(tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                Ui::Running { ready, .. } => {
                    ready.send(()).ok();
                }
                Ui::Approval { .. } | Ui::TerminalStart { .. } => {
                    panic!("read-only test commands must use local background execution")
                }
                _ => {}
            }
        }
    }));
    let binding = Binding {
        session: 1,
        capabilities: Capabilities {
            background: true,
            terminal: true,
            cwd: Some(cwd.clone()),
            ..Default::default()
        },
        local: true,
        connection: None,
        alive: Arc::new(AtomicBool::new(true)),
    };
    let command = "pwd -P; cat project-marker";
    let (main, main_http) = run_main(executor(&root, binding.clone(), ui.clone()), command).await;
    assert!(prompt(&main_http).contains(&cwd));
    assert!(output(&main).contains(&cwd));
    assert!(output(&main).contains("PROJECT-A"));
    let background = executor(&root, binding.clone(), ui.clone());
    let mut background_call = call(command);
    background_call.run_in_background = true;
    let ToolOutcome::Completed(started) = background.execute(background_call).await else {
        panic!("background start")
    };
    assert!(started.task_id.is_some());
    tokio::time::timeout(Duration::from_secs(3), async {
        while background.tasks.has_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let output = background
        .tasks
        .local_tool(
            shellmate_ai::agent::LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                id: started.output_id.unwrap(),
                offset_line: 0,
                limit_lines: None,
                grep: None,
            }),
            Default::default(),
        )
        .await;
    assert!(!output.is_error);
    assert!(output.inline.contains(&cwd));
    assert!(output.inline.contains("PROJECT-A"));

    let child_http = SequencedHttpClient::new([response(Some(command)), response(None)]);
    let factory_http = child_http.clone();
    let service = crate::ai::subagents::Service {
        tasks: Tasks::new(root.clone(), "child-test".into()),
        config: config(),
        settings: Default::default(),
        snapshot: Default::default(),
        limits: Default::default(),
        db: shellmate_store::SharedStore::new(
            shellmate_store::HostStore::open_in_memory().unwrap(),
        ),
        context: Default::default(),
        binding: binding.clone(),
        memory: shellmate_core::memory::Memory::new(&root, Some("local")),
        ui: ui.clone(),
        mode: Arc::new(Mutex::new(PermissionMode::Full)),
        cancel: Default::default(),
        factory: Arc::new(move |config| RigClient::with_http(config, factory_http.clone())),
    };
    // Changing the visible shell after the turn starts must not silently retarget its child.
    shell.cd(&other).await;
    let request = shellmate_ai::SubagentRequest {
        description: "inspect project".into(),
        prompt: "read the current project marker".into(),
        kind: shellmate_ai::SubagentKind::General,
        background: false,
        model: None,
        reasoning: None,
    };
    let result = tokio::time::timeout(Duration::from_secs(5), service.start(request, "delegate"))
        .await
        .unwrap();
    assert!(!result.is_error);
    let views = service.tasks.agent_views();
    let step = &views[0].step_list[0];
    assert_eq!(step.exit_code, Some(0));
    assert!(step.output.contains(&cwd));
    assert!(step.output.contains("PROJECT-A"));
    assert!(!step.output.contains("PROJECT-B"));
    assert!(prompt(&child_http).contains(&cwd));
    assert!(!prompt(&child_http).contains(other.to_str().unwrap()));

    // A new turn takes a new snapshot; a second terminal cannot affect the bound session.
    let mut next = binding;
    next.capabilities.cwd = session_cwd(true, Some(shell.pid()), None);
    let (next_log, next_http) = run_main(executor(&root, next, ui), command).await;
    assert!(prompt(&next_http).contains(other.to_str().unwrap()));
    assert!(self::output(&next_log).contains("PROJECT-B"));
}

#[tokio::test]
async fn unknown_invalid_or_deleted_cwd_never_runs_in_home_or_the_app_directory() {
    let root = tempfile::tempdir().unwrap();
    let (ui, _rx) = async_channel::unbounded();
    let mut binding = Binding {
        session: 1,
        capabilities: Capabilities {
            background: true,
            ..Default::default()
        },
        local: true,
        connection: None,
        alive: Arc::new(AtomicBool::new(true)),
    };
    for cwd in [None, Some("".into()), Some("relative/path".into())] {
        binding.capabilities.cwd = cwd;
        let executor = executor(root.path(), binding.clone(), ui.clone());
        let result = executor.execute(call("pwd")).await;
        assert!(
            matches!(result,ToolOutcome::Failed(e) if e.contains("无法确定当前终端的工作目录"))
        );
        assert!(executor.tasks.list().is_empty());
        assert!(!root.path().join("ai-outputs").exists());
    }
    assert!(session_cwd(true, None, Some("/Users/other")).is_none());
    assert!(session_cwd(true, Some(-1), Some("/Users/other")).is_none());
    assert!(session_cwd(true, Some(i32::MAX), Some("/Users/other")).is_none());
    let deleted = root.path().join("deleted-project");
    std::fs::create_dir(&deleted).unwrap();
    std::fs::remove_dir(&deleted).unwrap();
    binding.capabilities.cwd = Some(root.path().join("deleted-project").to_string_lossy().into());
    let result = executor(root.path(), binding, ui)
        .execute(call("pwd"))
        .await;
    let ToolOutcome::Completed(out) = result else {
        panic!("expected captured spawn failure")
    };
    assert!(!out.successful);
    assert!(out.wire_text().contains("执行失败"));
    assert!(out.stdout().is_empty());
}

#[test]
fn remote_directories_are_never_resolved_from_the_local_transport_process() {
    let pid = std::process::id() as i32;
    assert_eq!(
        session_cwd(false, Some(pid), Some("/srv/remote")),
        Some("/srv/remote".into())
    );
    assert!(session_cwd(false, Some(pid), None).is_none());
    assert!(session_cwd(false, Some(pid), Some("relative")).is_none());
}
