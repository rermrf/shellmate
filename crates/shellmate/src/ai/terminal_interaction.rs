//! Existing-terminal TUI control, scoped to one worker and revalidated after approval.
use super::{
    exec::AppExecutor,
    host::{Approval, Ui},
    session_catalog::Endpoint,
    target_router::Router,
};
use shellmate_ai::{
    agent::{ExecLimits, LocalResult, Target, ToolCall},
    policy::{Class, PermissionMode},
    session_tools::{Input, Request, Wait},
};
use shellmate_core::terminal_interaction::{Action, Controller, InputResult, Region, Screen};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
#[derive(Clone)]
pub enum Operation {
    Read {
        region: Option<Region>,
    },
    Check {
        screen_id: String,
    },
    Input {
        screen_id: String,
        actions: Vec<Action>,
    },
    Release,
}
pub enum Reply {
    Screen { screen: Screen, owned: bool },
    Input(InputResult),
    Released,
}
pub fn handles(request: &Request) -> bool {
    matches!(
        request,
        Request::Input(_)
            | Request::Wait(_)
            | Request::Read(shellmate_ai::session_tools::Read { screen: true, .. })
    )
}
fn endpoint(router: &Router, id: &str) -> Result<Endpoint, String> {
    let e = router.resolve(Some(id))?;
    if !e.binding.capabilities.terminal || (!e.binding.local && e.binding.connection.is_none()) {
        return Err("当前只支持已连接的本机和 SSH 交互终端".into());
    }
    Ok(e)
}
async fn request(
    router: &Router,
    e: &Endpoint,
    owner: Arc<Controller>,
    operation: Operation,
) -> Result<Reply, String> {
    let current = endpoint(router, &e.target.session_id)?;
    if !Arc::ptr_eq(&current.binding.alive, &e.binding.alive) || !owner.active() {
        return Err("终端连接或任务状态已变化".into());
    }
    let (reply, rx) = oneshot::channel();
    router
        .catalog
        .bridge
        .send(Ui::TerminalInteraction {
            session: e.binding.session,
            lease: e.binding.alive.clone(),
            owner,
            operation,
            reply,
        })
        .await
        .map_err(|_| "终端桥接已关闭")?;
    rx.await.map_err(|_| "终端交互已取消".to_owned())?
}
fn encode_screen(e: &Endpoint, screen: Screen) -> LocalResult {
    LocalResult::text(serde_json::json!({"target":e.target,"screen":screen}).to_string())
}
pub async fn execute(
    executor: &AppExecutor,
    request_args: Request,
    limits: ExecLimits,
    source: Option<shellmate_ai::SubagentRef>,
    call_id: &str,
) -> LocalResult {
    let Some(router) = &executor.router else {
        return LocalResult::error("终端目录不可用");
    };
    let id = match &request_args {
        Request::Read(a) => &a.session_id,
        Request::Input(a) => &a.session_id,
        Request::Wait(a) => &a.session_id,
        _ => return LocalResult::error("交互请求无效"),
    };
    let e = match endpoint(router, id) {
        Ok(e) => e,
        Err(e) => return LocalResult::error(e),
    };
    let owner = executor.tasks.interaction_owner(
        source
            .as_ref()
            .map(|s| s.description.clone())
            .unwrap_or_else(|| "Agent".into()),
    );
    let control = executor.tasks.control();
    let work = async {
        match request_args {
            Request::Read(a) => {
                match request(router, &e, owner, Operation::Read { region: a.region }).await {
                    Ok(Reply::Screen { screen, .. }) => encode_screen(&e, screen),
                    Ok(_) => LocalResult::error("读屏响应无效"),
                    Err(e) => LocalResult::error(e),
                }
            }
            Request::Input(input) => {
                input_action(router, &e, owner, input, limits, source, call_id, &control).await
            }
            Request::Wait(args) => {
                control.set_phase(shellmate_ai::interaction::Phase::Waiting(vec![format!(
                    "terminal:{}",
                    e.target.session_id
                )]));
                wait(router, &e, owner, args).await
            }
            _ => LocalResult::error("交互请求无效"),
        }
    };
    tokio::select! {biased;()=control.urgent()=>{executor.tasks.release_interaction();LocalResult::error("用户已更新指令，停止本次终端交互；程序仍在原终端运行")},()=super::exec::disconnected(&e.lease)=>LocalResult::error("终端已断开"),out=work=>out}
}
struct ApprovalGuard {
    ui: async_channel::Sender<Ui>,
    key: String,
}
impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        let _ = self.ui.try_send(Ui::CancelApproval(self.key.clone()));
    }
}
#[allow(clippy::too_many_arguments)]
async fn input_action(
    router: &Router,
    e: &Endpoint,
    owner: Arc<Controller>,
    input: Input,
    limits: ExecLimits,
    source: Option<shellmate_ai::SubagentRef>,
    call_id: &str,
    control: &shellmate_ai::interaction::Control,
) -> LocalResult {
    if input.release {
        return match request(router, e, owner, Operation::Release).await {
            Ok(_) => LocalResult::text("已交还此终端控制权，程序继续运行"),
            Err(e) => LocalResult::error(e),
        };
    }
    let (screen, owned) = match request(
        router,
        e,
        owner.clone(),
        Operation::Check {
            screen_id: input.screen_id.clone(),
        },
    )
    .await
    {
        Ok(Reply::Screen { screen, owned }) => (screen, owned),
        Err(e) => return LocalResult::error(e),
        _ => return LocalResult::error("校验响应无效"),
    };
    let policy = router.policy(e);
    // Only an existing, explicitly granted interaction permits navigation without
    // another prompt. Text, Enter and other keys can mutate state in any TUI.
    let navigation = shellmate_core::terminal_interaction::navigation(&input.actions);
    let text = input
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Text { text } | Action::Paste { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let class = if shellmate_ai::policy::classify(&text) == Class::RedLine {
        Class::RedLine
    } else {
        Class::Other
    };
    let approved = class != Class::RedLine
        && (policy == PermissionMode::Full || (owned && navigation && !screen.paused));
    let summary = input
        .actions
        .iter()
        .map(Action::summary)
        .collect::<Vec<_>>()
        .join(" → ");
    if !approved {
        control.set_phase(shellmate_ai::interaction::Phase::Approval);
        let call = ToolCall {
            service: None,
            cwd: None,
            session_id: Some(e.target.session_id.clone()),
            destination: Some(e.target.clone()),
            id: call_id.into(),
            command: summary.clone(),
            purpose: input.purpose.clone(),
            target: Target::Terminal,
            timeout: Duration::from_secs(10),
            run_in_background: false,
            exec_limits: limits,
            class,
        };
        let (reply, rx) = oneshot::channel();
        let _guard = ApprovalGuard {
            ui: router.ui.clone(),
            key: super::host::approval_key(source.as_ref(), call_id),
        };
        if router
            .ui
            .send(Ui::TuiApproval {
                source,
                call,
                reply,
            })
            .await
            .is_err()
        {
            return LocalResult::error("审批界面不可用");
        }
        match rx.await {
            Ok(Approval::Approve { command, target })
                if command == summary && target == Target::Terminal => {}
            Ok(Approval::Deny { reason }) => {
                return LocalResult::error(format!(
                    "用户拒绝终端交互{}",
                    reason.map(|s| format!("：{s}")).unwrap_or_default()
                ));
            }
            _ => return LocalResult::error("审批取消或动作已变化，本次未输入"),
        }
    }
    control.set_phase(shellmate_ai::interaction::Phase::Executing);
    let current = match endpoint(router, &input.session_id) {
        Ok(e) => e,
        Err(e) => return LocalResult::error(e),
    };
    if router.policy(&current) != policy {
        return LocalResult::error("目标权限已改变，请重新发起交互");
    }
    match request(router,e,owner,Operation::Input{screen_id:input.screen_id,actions:input.actions}).await {
        Ok(Reply::Input(result))=>LocalResult{attachments:vec![],subagent:None,is_error:result.error.is_some(),output_id:None,inline:serde_json::json!({"target":e.target,"actions_sent":result.actions_sent,"error":result.error,"screen":result.screen,"note":"输入已写入，不代表程序完成预期操作；等待并观察实际画面。"}).to_string()},
        Err(e)=>LocalResult::error(e),_=>LocalResult::error("输入响应无效"),
    }
}
async fn wait(router: &Router, e: &Endpoint, owner: Arc<Controller>, args: Wait) -> LocalResult {
    let start = Instant::now();
    let mut stable = Instant::now();
    let mut previous = String::new();
    loop {
        let screen = match request(
            router,
            e,
            owner.clone(),
            Operation::Read {
                region: args.region.clone(),
            },
        )
        .await
        {
            Ok(Reply::Screen { screen, owned }) => {
                if owned && screen.paused {
                    return LocalResult::error("用户已接管，停止等待");
                }
                screen
            }
            Err(e) => return LocalResult::error(e),
            _ => return LocalResult::error("读屏响应无效"),
        };
        if screen.fingerprint != previous {
            previous = screen.fingerprint.clone();
            stable = Instant::now();
        }
        let matched = args.text.as_ref().is_none_or(|t| screen.text.contains(t))
            && args
                .fingerprint
                .as_ref()
                .is_none_or(|f| f != &screen.fingerprint)
            && args
                .stable_ms
                .is_none_or(|ms| stable.elapsed() >= Duration::from_millis(ms));
        if matched || start.elapsed() >= Duration::from_millis(args.timeout_ms) {
            return LocalResult::text(serde_json::json!({"target":e.target,"matched":matched,"timed_out":!matched,"screen":screen}).to_string());
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{exec::Binding, host::Executor, session_catalog::Catalog, tasks::Tasks};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    fn screen() -> Screen {
        Screen {
            screen_id: "observed".into(),
            fingerprint: "before".into(),
            rows: 24,
            columns: 80,
            cursor: shellmate_core::terminal_interaction::Cursor {
                row: 1,
                column: 0,
                visible: true,
            },
            alternate_screen: true,
            application_cursor: true,
            bracketed_paste: false,
            mouse_reporting: false,
            region: Region {
                row: 0,
                column: 0,
                rows: 24,
                columns: 80,
            },
            lines: vec![],
            text: "menu".into(),
            truncated: false,
            controller: None,
            paused: false,
        }
    }
    fn fixture() -> (
        tempfile::TempDir,
        AppExecutor,
        async_channel::Receiver<Ui>,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (ui, rx) = async_channel::unbounded();
        let catalog = Catalog::new(ui.clone());
        let binding = Binding {
            session: 1,
            local: true,
            connection: None,
            alive: Arc::new(AtomicBool::new(true)),
            capabilities: shellmate_ai::Capabilities {
                terminal: true,
                background: true,
                cwd: Some(dir.path().to_string_lossy().into()),
                ..Default::default()
            },
        };
        catalog.update(vec![(
            binding.clone(),
            "local".into(),
            "本机终端".into(),
            None,
            "window".into(),
        )]);
        let id = catalog.list()[0].target.session_id.clone();
        let db = shellmate_store::SharedStore::new(
            shellmate_store::HostStore::open_in_memory().unwrap(),
        );
        let router = Router::new(catalog, db, dir.path().into(), ui.clone());
        let tasks = Tasks::new(dir.path().into(), "conversation".into());
        (
            dir,
            AppExecutor {
                lease: None,
                slots: None,
                router: Some(router),
                subagents: None,
                memory: None,
                binding,
                tasks,
                ui,
            },
            rx,
            id,
        )
    }
    fn input(id: &str, key: &str) -> Request {
        Request::Input(Input {
            session_id: id.into(),
            screen_id: "observed".into(),
            actions: vec![Action::Key {
                key: key.into(),
                repeat: 1,
            }],
            release: false,
            purpose: "选择菜单项".into(),
        })
    }
    #[tokio::test]
    async fn input_needs_permission_then_navigation_reuses_same_controller_and_release_is_not_stop()
    {
        let (_dir, executor, rx, id) = fixture();
        let sent = Arc::new(Mutex::new(vec![]));
        let state = sent.clone();
        let approvals = Arc::new(Mutex::new(0));
        let count = approvals.clone();
        let bridge = tokio::spawn(async move {
            let mut owner = None;
            while let Ok(event) = rx.recv().await {
                match event {
                    Ui::TerminalInteraction {
                        operation,
                        owner: caller,
                        reply,
                        ..
                    } => match operation {
                        Operation::Check { .. } | Operation::Read { .. } => {
                            let _ = reply.send(Ok(Reply::Screen {
                                screen: screen(),
                                owned: owner.as_ref() == Some(&caller.id),
                            }));
                        }
                        Operation::Input { actions, .. } => {
                            owner = Some(caller.id.clone());
                            state.lock().unwrap().extend(actions);
                            let _ = reply.send(Ok(Reply::Input(InputResult {
                                screen: screen(),
                                actions_sent: 1,
                                error: None,
                            })));
                        }
                        Operation::Release => {
                            owner = None;
                            let _ = reply.send(Ok(Reply::Released));
                        }
                    },
                    Ui::TuiApproval { call, reply, .. } => {
                        *count.lock().unwrap() += 1;
                        assert_eq!(call.session_id.as_deref(), Some(id.as_str()));
                        let _ = reply.send(Approval::Approve {
                            command: call.command,
                            target: Target::Terminal,
                        });
                    }
                    _ => {}
                }
            }
        });
        let id = executor.router.as_ref().unwrap().catalog.list()[0]
            .target
            .session_id
            .clone();
        for (n, key) in ["down", "up", "enter"].into_iter().enumerate() {
            let out = executor
                .local_tool_from(
                    shellmate_ai::agent::LocalTool::Sessions(input(&id, key)),
                    ExecLimits::default(),
                    None,
                    &format!("call-{n}"),
                )
                .await;
            assert!(!out.is_error, "{}", out.inline);
        }
        assert_eq!(*approvals.lock().unwrap(), 2);
        assert_eq!(sent.lock().unwrap().len(), 3);
        let release = Request::Input(Input {
            session_id: id,
            screen_id: String::new(),
            actions: vec![],
            release: true,
            purpose: "交还控制".into(),
        });
        let out = execute(&executor, release, ExecLimits::default(), None, "release").await;
        assert!(!out.is_error);
        assert_eq!(sent.lock().unwrap().len(), 3);
        bridge.abort();
    }
    #[tokio::test]
    async fn denied_or_stale_approval_never_sends_input() {
        for stale in [false, true] {
            let (_dir, executor, rx, id) = fixture();
            let alive = executor.binding.alive.clone();
            let inputs = Arc::new(Mutex::new(0));
            let seen = inputs.clone();
            let bridge = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        Ui::TerminalInteraction {
                            operation, reply, ..
                        } => {
                            assert!(matches!(operation, Operation::Check { .. }));
                            *seen.lock().unwrap() += 1;
                            let _ = reply.send(Ok(Reply::Screen {
                                screen: screen(),
                                owned: false,
                            }));
                        }
                        Ui::TuiApproval { call, reply, .. } => {
                            if stale {
                                alive.store(false, Ordering::Release);
                                let _ = reply.send(Approval::Approve {
                                    command: call.command,
                                    target: Target::Terminal,
                                });
                            } else {
                                let _ = reply.send(Approval::Deny { reason: None });
                            }
                        }
                        _ => {}
                    }
                }
            });
            assert!(
                execute(
                    &executor,
                    input(&id, "enter"),
                    ExecLimits::default(),
                    None,
                    "test"
                )
                .await
                .is_error
            );
            assert_eq!(*inputs.lock().unwrap(), 1);
            bridge.abort();
        }
    }
    #[tokio::test]
    async fn bounded_wait_matches_text_or_returns_timeout_with_screen() {
        for matched in [false, true] {
            let (_dir, executor, rx, id) = fixture();
            let bridge = tokio::spawn(async move {
                while let Ok(Ui::TerminalInteraction { reply, .. }) = rx.recv().await {
                    let _ = reply.send(Ok(Reply::Screen {
                        screen: screen(),
                        owned: false,
                    }));
                }
            });
            let out = execute(
                &executor,
                Request::Wait(Wait {
                    session_id: id,
                    region: None,
                    text: Some(if matched { "menu" } else { "unseen" }.into()),
                    fingerprint: None,
                    stable_ms: None,
                    timeout_ms: 100,
                }),
                ExecLimits::default(),
                None,
                "wait",
            )
            .await;
            assert!(!out.is_error);
            let result: serde_json::Value = serde_json::from_str(&out.inline).unwrap();
            assert_eq!(result["matched"], matched);
            assert_eq!(result["timed_out"], !matched);
            assert_eq!(result["screen"]["text"], "menu");
            bridge.abort();
        }
    }
    #[tokio::test]
    async fn full_access_sends_input_but_obvious_redline_text_still_requires_approval() {
        for dangerous in [false, true] {
            let (_dir, executor, rx, id) = fixture();
            executor
                .router
                .as_ref()
                .unwrap()
                .db
                .borrow_mut()
                .set_ai_permission("local", PermissionMode::Full)
                .unwrap();
            let sent = Arc::new(Mutex::new(0));
            let seen = sent.clone();
            let bridge = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        Ui::TerminalInteraction {
                            operation, reply, ..
                        } => match operation {
                            Operation::Check { .. } => {
                                let _ = reply.send(Ok(Reply::Screen {
                                    screen: screen(),
                                    owned: false,
                                }));
                            }
                            Operation::Input { .. } => {
                                *seen.lock().unwrap() += 1;
                                let _ = reply.send(Ok(Reply::Input(InputResult {
                                    screen: screen(),
                                    actions_sent: 1,
                                    error: None,
                                })));
                            }
                            _ => panic!("unexpected"),
                        },
                        Ui::TuiApproval { call, reply, .. } => {
                            assert!(dangerous);
                            assert_eq!(call.class, Class::RedLine);
                            let _ = reply.send(Approval::Deny { reason: None });
                        }
                        _ => {}
                    }
                }
            });
            let req = Request::Input(Input {
                session_id: id,
                screen_id: "observed".into(),
                actions: vec![Action::Text {
                    text: if dangerous { "rm -rf /" } else { ":wq" }.into(),
                }],
                release: false,
                purpose: "输入".into(),
            });
            let out = execute(&executor, req, ExecLimits::default(), None, "call").await;
            assert_eq!(out.is_error, dangerous);
            assert_eq!(*sent.lock().unwrap(), usize::from(!dangerous));
            bridge.abort();
        }
    }
    #[tokio::test]
    async fn stopping_wait_releases_controller_without_sending_interrupt() {
        let (_dir, executor, rx, id) = fixture();
        let owner = executor.tasks.interaction_owner("test".into());
        let control = executor.tasks.control();
        let bridge = tokio::spawn(async move {
            while let Ok(Ui::TerminalInteraction {
                operation, reply, ..
            }) = rx.recv().await
            {
                assert!(matches!(operation, Operation::Read { .. }));
                let _ = reply.send(Ok(Reply::Screen {
                    screen: screen(),
                    owned: true,
                }));
            }
        });
        let work = execute(
            &executor,
            Request::Wait(Wait {
                session_id: id,
                region: None,
                text: Some("never".into()),
                fingerprint: None,
                stable_ms: None,
                timeout_ms: 10000,
            }),
            ExecLimits::default(),
            None,
            "wait",
        );
        let (out, _) = tokio::join!(work, async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            control.request(shellmate_ai::interaction::Intent::StopReply);
        });
        assert!(out.is_error);
        assert!(!owner.active());
        bridge.abort();
    }
}
