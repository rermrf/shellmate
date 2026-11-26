use super::*;
use shellmate_ai::{output::shape, policy::Class};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
struct Fake {
    calls: Arc<Mutex<Vec<ToolCall>>>,
    running: Arc<AtomicUsize>,
    wait: bool,
}
struct Guard(Arc<AtomicUsize>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Executor for Fake {
    fn execute(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call);
            self.running.fetch_add(1, Ordering::SeqCst);
            let _guard = Guard(self.running.clone());
            if self.wait {
                std::future::pending::<()>().await;
            }
            ToolOutcome::Completed(shape("ok", [], Some(0), Duration::ZERO, false, false))
        })
    }
}
struct Fixture {
    host: Host,
    ui: async_channel::Receiver<Ui>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    running: Arc<AtomicUsize>,
}
fn fixture(mode: PermissionMode, wait: bool) -> Fixture {
    let (tx, ui) = async_channel::unbounded();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let running = Arc::new(AtomicUsize::new(0));
    let host = Host {
        compaction: Default::default(),
        mode: Arc::new(Mutex::new(mode)),
        ui: tx,
        executor: Box::new(Fake {
            calls: calls.clone(),
            running: running.clone(),
            wait,
        }),
        alive: Arc::new(AtomicBool::new(true)),
        resolved: Mutex::new(None),
    };
    Fixture {
        host,
        ui,
        calls,
        running,
    }
}
fn call(class: Class) -> ToolCall {
    let command = match class {
        Class::ReadOnly => "uname -s",
        Class::Other => "touch /tmp/example",
        Class::RedLine => "systemctl reboot",
    };
    ToolCall {
        service: None,
        session_id: None,
        cwd: None, destination: None,
        id: "call-1".into(),
        command: command.into(),
        purpose: "测试".into(),
        target: Target::Background,
        timeout: Duration::from_secs(30),
        run_in_background: false,
        exec_limits: shellmate_ai::agent::ExecLimits::default(),
        class,
    }
}
#[tokio::test]
async fn approval_all_permission_class_combinations() {
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Full,
    ] {
        for class in [Class::ReadOnly, Class::Other, Class::RedLine] {
            let f = fixture(mode, false);
            let c = call(class);
            let command = c.command.clone();
            let policy = decide(mode, class);
            let ui = async {
                if policy != Decision::Run {
                    match f.ui.recv().await.unwrap() {
                        Ui::Approval { redline, reply, .. } => {
                            assert_eq!(redline, class == Class::RedLine);
                            reply
                                .send(Approval::Approve {
                                    command,
                                    target: Target::Background,
                                })
                                .ok()
                                .unwrap();
                        }
                        _ => panic!("expected approval"),
                    }
                }
                match f.ui.recv().await.unwrap() {
                    Ui::Running {
                        automatic, ready, ..
                    } => {
                        assert_eq!(automatic, policy == Decision::Run);
                        ready.send(()).unwrap();
                    }
                    _ => panic!("expected running"),
                }
            };
            let (out, ()) = tokio::join!(f.host.run(c), ui);
            assert!(matches!(out, ToolOutcome::Completed(_)));
            assert_eq!(f.calls.lock().unwrap().len(), 1);
        }
    }
}
#[tokio::test]
async fn approval_edit_reclassifies_retargets_and_records_actual_call() {
    let f = fixture(PermissionMode::Ask, false);
    let c = call(Class::ReadOnly);
    let ui = async {
        match f.ui.recv().await.unwrap() {
            Ui::Approval { reply, .. } => {
                reply
                    .send(Approval::Approve {
                        command: "systemctl reboot".into(),
                        target: Target::Terminal,
                    })
                    .ok()
                    .unwrap();
            }
            _ => panic!("approval"),
        }
        if let Ui::Running { ready, .. } = f.ui.recv().await.unwrap() {
            ready.send(()).unwrap();
        } else {
            panic!("running");
        }
    };
    let (out, ()) = tokio::join!(f.host.run(c.clone()), ui);
    assert!(matches!(out, ToolOutcome::Completed(_)));
    let resolved = f.host.resolved_call(&c);
    assert_eq!(resolved.command, "systemctl reboot");
    assert_eq!(resolved.target, Target::Terminal);
    assert_eq!(resolved.class, Class::RedLine);
    assert_eq!(*f.calls.lock().unwrap(), [resolved]);
}
#[tokio::test]
async fn approval_deny_reason_reaches_outcome_without_execution() {
    let f = fixture(PermissionMode::Ask, false);
    let ui = async {
        if let Ui::Approval { reply, .. } = f.ui.recv().await.unwrap() {
            reply
                .send(Approval::Deny {
                    reason: Some("先等维护窗口".into()),
                })
                .ok()
                .unwrap();
        } else {
            panic!("approval")
        }
    };
    let (out, ()) = tokio::join!(f.host.run(call(Class::Other)), ui);
    assert_eq!(
        out,
        ToolOutcome::Denied {
            reason: Some("先等维护窗口".into())
        }
    );
    assert!(f.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn approval_cancel_while_awaiting_closes_reply_and_card() {
    let f = fixture(PermissionMode::Ask, false);
    let mut run = f.host.run(call(Class::Other));
    let request = tokio::select! {event=f.ui.recv()=>event.unwrap(),_=&mut run=>panic!("ran")};
    let Ui::Approval { reply, .. } = request else {
        panic!("approval")
    };
    drop(run);
    assert!(reply.is_closed());
    assert!(matches!(f.ui.recv().await.unwrap(), Ui::CancelApproval(_)));
    assert!(f.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn approval_cancel_while_running_drops_executor() {
    let f = fixture(PermissionMode::Full, true);
    let mut run = f.host.run(call(Class::Other));
    let request = tokio::select! {event=f.ui.recv()=>event.unwrap(),_=&mut run=>panic!("finished")};
    if let Ui::Running { ready, .. } = request {
        ready.send(()).unwrap();
    } else {
        panic!("running")
    }
    tokio::select! {_=&mut run=>panic!("finished"),()=tokio::time::sleep(Duration::from_millis(10))=>{}}
    assert_eq!(f.running.load(Ordering::SeqCst), 1);
    drop(run);
    assert_eq!(f.running.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn approval_disconnect_cancels_pending_and_mode_change_applies_next_step() {
    let f = fixture(PermissionMode::Ask, false);
    let ui = async {
        let Ui::Approval { reply, .. } = f.ui.recv().await.unwrap() else {
            panic!("approval")
        };
        f.host.alive.store(false, Ordering::Release);
        reply
    };
    let (out, reply) = tokio::join!(f.host.run(call(Class::ReadOnly)), ui);
    assert_eq!(out, ToolOutcome::Disconnected);
    assert!(reply.is_closed());
    f.host.alive.store(true, Ordering::Release);
    *f.host.mode.lock().unwrap() = PermissionMode::Full;
    let _ = f.ui.recv().await.unwrap(); // card cleanup
    let ui = async {
        if let Ui::Running {
            ready, automatic, ..
        } = f.ui.recv().await.unwrap()
        {
            assert!(automatic);
            ready.send(()).unwrap();
        } else {
            panic!("mode change not applied")
        }
    };
    let (out, ()) = tokio::join!(f.host.run(call(Class::Other)), ui);
    assert!(matches!(out, ToolOutcome::Completed(_)));
}

#[tokio::test]
async fn local_output_tools_bypass_all_approval_modes_and_background_command_does_not() {
    use shellmate_ai::agent::{ExecLimits, LocalTool, ReadOutput};
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Full,
    ] {
        let f = fixture(mode, false);
        f.host
            .local_tool(
                LocalTool::ReadOutput(ReadOutput {
                    id: "out".into(),
                    offset_line: 0,
                    limit_lines: None,
                    grep: None,
                }),
                ExecLimits::default(),
            )
            .await;
        f.host
            .local_tool(
                LocalTool::StopTask { id: "1".into() },
                ExecLimits::default(),
            )
            .await;
        assert!(f.ui.try_recv().is_err());
        assert!(f.calls.lock().unwrap().is_empty());
    }
    let f = fixture(PermissionMode::Ask, false);
    let mut c = call(Class::ReadOnly);
    c.run_in_background = true;
    let mut run = f.host.run(c);
    let event = tokio::select! { event=f.ui.recv()=>event.unwrap(), _=&mut run=>panic!("approval skipped") };
    assert!(matches!(event, Ui::Approval { .. }));
}

#[tokio::test]
async fn memory_uses_local_executor_without_approval_or_task_registration() {
    use shellmate_ai::agent::{LocalTool, ToolHost};
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Full,
    ] {
        let root = tempfile::tempdir().unwrap();
        let files = shellmate_core::memory::Memory::new(root.path(), Some("local"));
        files
            .create("/memories/global/AGENTS.md", "instructions")
            .unwrap();
        let tasks = crate::ai::tasks::Tasks::new(root.path().to_path_buf(), "memory-test".into());
        let (ui, rx) = async_channel::unbounded();
        let alive = Arc::new(AtomicBool::new(true));
        let host = Host {
            compaction: Default::default(),
            mode: Arc::new(Mutex::new(mode)),
            executor: Box::new(crate::ai::exec::AppExecutor {
                lease: None,
            slots: None,
                router: None,
                subagents: None,
                memory: Some((files.clone(), "local".into())),
                tasks: tasks.clone(),
                binding: crate::ai::exec::Binding {
                    session: 1,
                    capabilities: shellmate_ai::Capabilities {
                        background: true,
                        ..Default::default()
                    },
                    connection: None,
                    local: true,
                    alive: alive.clone(),
                },
                ui: ui.clone(),
            }),
            ui,
            alive,
            resolved: Mutex::new(None),
        };
        let out = host
            .local_tool(
                LocalTool::Memory(shellmate_ai::memory::Command::Create {
                    path: "/memories/host/note.md".into(),
                    file_text: "remember".into(),
                }),
                Default::default(),
            )
            .await;
        assert!(!out.is_error);
        assert_eq!(out.inline, "已保存记忆 · host/note.md");
        assert!(rx.is_empty());
        assert!(tasks.list().is_empty());
        assert_eq!(files.read("/memories/host/note.md").unwrap(), "remember");
        assert!(
            host.memory_snapshot(false)
                .instructions
                .contains("instructions")
        );
    }
}

#[tokio::test]
async fn child_and_main_have_identical_approval_decisions_in_all_modes() {
    use shellmate_ai::{SubagentKind, SubagentRef};
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Full,
    ] {
        for command in [
            "pwd",
            "/bin/pwd",
            "/usr/bin/pwd",
            "/bin/ls -la",
            "bash -lc 'pwd; ls -la'",
            "uname -s",
            "touch /tmp/fixture",
            "reboot",
        ] {
            for kind in [
                None,
                Some(SubagentKind::General),
                Some(SubagentKind::Explore),
            ] {
                let f = fixture(mode, false);
                let mut c = call(Class::ReadOnly);
                c.command = command.into();
                let source = kind.map(|kind| SubagentRef {
                    id: "agent-1".into(),
                    description: "inspect".into(),
                    kind,
                });
                let expected_source = source.clone();
                let class = classify(command);
                let decision = decide(mode, class);
                let ui = async {
                    if decision != Decision::Run {
                        let Ui::Approval {
                            reply,
                            source,
                            redline,
                            ..
                        } = f.ui.recv().await.unwrap()
                        else {
                            panic!("approval")
                        };
                        assert_eq!(source, expected_source);
                        assert_eq!(redline, class == Class::RedLine);
                        reply
                            .send(Approval::Deny {
                                reason: Some("fixture denial".into()),
                            })
                            .ok()
                            .unwrap();
                    } else {
                        let Ui::Running {
                            ready,
                            source,
                            automatic,
                            ..
                        } = f.ui.recv().await.unwrap()
                        else {
                            panic!("running")
                        };
                        assert_eq!(source, expected_source);
                        assert!(automatic);
                        ready.send(()).unwrap();
                    }
                };
                let (out, ()) = tokio::join!(f.host.run_from(c, source), ui);
                if decision == Decision::Run {
                    assert!(matches!(out, ToolOutcome::Completed(_)));
                    assert_eq!(f.calls.lock().unwrap().len(), 1);
                } else {
                    assert!(
                        matches!(out,ToolOutcome::Denied{reason:Some(r)} if r=="fixture denial")
                    );
                    assert!(f.calls.lock().unwrap().is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn immediate_input_retracts_unapproved_command_instead_of_executing_it() {
    use shellmate_ai::interaction::{Control, Intent};
    struct Controlled {
        control: Control,
    }
    impl Executor for Controlled {
        fn control(&self) -> Option<Control> {
            Some(self.control.clone())
        }
        fn execute(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            panic!("unapproved command executed")
        }
    }
    let (ui, rx) = async_channel::unbounded();
    let control = Control::default();
    control.resume();
    let host = Host {
        compaction: Default::default(),
        mode: Default::default(),
        ui,
        executor: Box::new(Controlled {
            control: control.clone(),
        }),
        alive: Arc::new(AtomicBool::new(true)),
        resolved: Default::default(),
    };
    let mut run = host.run(call(Class::Other));
    let Ui::Approval { reply, .. } = (tokio::select! {event=rx.recv()=>event.unwrap(),_=&mut run=>panic!("must await approval")})
    else {
        panic!("approval")
    };
    control.request(Intent::SendNow);
    assert!(matches!(run.await, ToolOutcome::Denied { .. }));
    assert!(reply.is_closed());
    assert!(matches!(rx.recv().await.unwrap(), Ui::CancelApproval(_)));
}
