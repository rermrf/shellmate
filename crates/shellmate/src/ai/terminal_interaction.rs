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
use shellmate_core::terminal_interaction::{
    Action, Controller, ErrorCode, Guard, InputResult, InteractionError, Observe, Region, Screen,
};
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
        guards: Vec<Guard>,
    },
    Input {
        screen_id: String,
        guards: Vec<Guard>,
        actions: Vec<Action>,
    },
    Observe {
        screen_id: Option<String>,
        fingerprint: Option<String>,
        region: Option<Region>,
    },
    Release,
}
pub enum Reply {
    Screen { screen: Screen, owned: bool },
    Input(InputResult),
    Observation(shellmate_term::ObservationWatch),
    Released,
}
pub fn apply(
    t: &mut shellmate_term::Terminal,
    op: Operation,
    owner: Arc<Controller>,
    cx: &mut gpui::Context<shellmate_term::Terminal>,
) -> Result<Reply, InteractionError> {
    match op {
        Operation::Read { region } => {
            let owned = t.interaction_owned(&owner);
            t.interaction_snapshot(region.as_ref())
                .map(|screen| Reply::Screen { screen, owned })
                .map_err(Into::into)
        }
        Operation::Check { screen_id, guards } => {
            let owned = t.interaction_owned(&owner);
            t.interaction_check_guarded(&screen_id, &owner, &guards)
                .map(|screen| Reply::Screen { screen, owned })
        }
        Operation::Input {
            screen_id,
            actions,
            guards,
        } => t
            .interaction_input_guarded(&screen_id, &actions, owner, &guards, cx)
            .map(Reply::Input),
        Operation::Observe {
            screen_id,
            fingerprint,
            region,
        } => t
            .interaction_observe(
                screen_id.as_deref(),
                fingerprint.as_deref(),
                region.as_ref(),
            )
            .map(Reply::Observation),
        Operation::Release => t
            .interaction_release(&owner, cx)
            .map(|_| Reply::Released)
            .map_err(Into::into),
    }
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
    let e = if let Ok(e) = router.catalog.lookup(id)
        && e.independent
    {
        e
    } else {
        router.resolve(Some(id))?
    };
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
) -> Result<Reply, InteractionError> {
    let current = endpoint(router, &e.target.session_id)?;
    if !owner.active() {
        return Err(InteractionError::new(
            ErrorCode::Stopped,
            "任务已停止，输入未发送",
        ));
    }
    if !Arc::ptr_eq(&current.binding.alive, &e.binding.alive) {
        return Err(InteractionError::new(
            ErrorCode::ConnectionChanged,
            "终端连接已变化",
        ));
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
    rx.await
        .map_err(|_| InteractionError::new(ErrorCode::Stopped, "终端交互已取消"))?
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
        Err(e) => return failure(e),
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
                    Err(e) => failure(e),
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
    tokio::select! {biased;()=control.urgent()=>{executor.tasks.release_interaction();LocalResult::error(serde_json::json!({"code":"stopped","message":"用户已更新指令，停止本次终端交互；已发送的输入不会撤销，不要自动重发。程序仍在原终端运行","actions_sent":null}).to_string())},()=super::exec::disconnected(&e.lease)=>LocalResult::error("终端已断开"),out=work=>out}
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
            Err(e) => failure(e),
        };
    }
    let (screen, owned) = match request(
        router,
        e,
        owner.clone(),
        Operation::Check {
            screen_id: input.screen_id.clone(),
            guards: input.guards.clone(),
        },
    )
    .await
    {
        Ok(Reply::Screen { screen, owned }) => (screen, owned),
        Err(e) => return failure(e),
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
        Err(e) => return failure(e),
    };
    if router.policy(&current) != policy {
        return LocalResult::error("目标权限已改变，请重新发起交互");
    }
    // Pin the observation baseline and register for output before sending anything.
    let watching = match request(
        router,
        e,
        owner.clone(),
        Operation::Observe {
            screen_id: Some(input.screen_id.clone()),
            fingerprint: None,
            region: input.observe.region.clone(),
        },
    )
    .await
    {
        Ok(Reply::Observation(w)) => w,
        Err(e) => return failure(e),
        _ => return LocalResult::error("观察响应无效"),
    };
    let result = match request(
        router,
        e,
        owner.clone(),
        Operation::Input {
            screen_id: input.screen_id,
            actions: input.actions,
            guards: input.guards,
        },
    )
    .await
    {
        Ok(Reply::Input(result)) => result,
        Err(e) => return failure(e),
        _ => return LocalResult::error("输入响应无效"),
    };
    let sent = result.actions_sent;
    if result.error.is_some() {
        return LocalResult{attachments:vec![],subagent:None,is_error:true,output_id:None,inline:serde_json::json!({"target":e.target,"actions_sent":sent,"error":result.error,"code":"write_failed","screen":result.screen}).to_string()};
    }
    control.set_phase(shellmate_ai::interaction::Phase::Waiting(vec![format!(
        "terminal:{}",
        e.target.session_id
    )]));
    match observe_after(router,e,owner,&input.observe,watching).await{
        Ok(out)=>LocalResult::text(serde_json::json!({"target":e.target,"actions_sent":sent,"changed":out.changed,"condition_matched":out.matched,"observation_status":out.status,"screen":out.screen,"note":"输入只发送一次；画面变化/条件满足不等于业务成功。无进展时检查布局或程序内帮助，勿自动重发。"}).to_string()),
        Err(mut e)=>{e.actions_sent=sent;failure(e)},
    }
}
fn failure(error: impl Into<InteractionError>) -> LocalResult {
    let e = error.into();
    LocalResult::error(serde_json::to_string(&e).unwrap())
}
struct Observed {
    screen: Screen,
    changed: bool,
    matched: Option<bool>,
    status: &'static str,
}
async fn next_screen(
    router: &Router,
    e: &Endpoint,
    owner: Arc<Controller>,
    region: Region,
) -> Result<Screen, InteractionError> {
    match request(
        router,
        e,
        owner,
        Operation::Read {
            region: Some(region),
        },
    )
    .await?
    {
        Reply::Screen { screen, .. } => {
            if screen.paused {
                let mut error = InteractionError::new(
                    ErrorCode::UserTakeover,
                    "用户已接管，停止自动观察与输入",
                );
                error.screen = Some(Box::new(screen));
                return Err(error);
            }
            Ok(screen)
        }
        _ => Err(InteractionError::new(
            ErrorCode::InvalidReference,
            "读屏响应无效",
        )),
    }
}
async fn wait_event(updates: &async_channel::Receiver<()>, until: tokio::time::Instant) {
    tokio::select! {event = updates.recv()=>{if event.is_err(){tokio::time::sleep_until(until).await;}},_ = tokio::time::sleep_until(until)=>{}}
}
async fn observe_after(
    router: &Router,
    e: &Endpoint,
    owner: Arc<Controller>,
    args: &Observe,
    watch: shellmate_term::ObservationWatch,
) -> Result<Observed, InteractionError> {
    let base = watch.screen;
    let region = base.region.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(args.timeout_ms);
    let mut previous = base.fingerprint.clone();
    let mut stable = Instant::now();
    let explicit = args.text.is_some() || args.absent_text.is_some() || args.stable_ms.is_some();
    loop {
        let screen = next_screen(router, e, owner.clone(), region.clone()).await?;
        if screen.rows != base.rows
            || screen.columns != base.columns
            || screen.alternate_screen != base.alternate_screen
        {
            return Ok(Observed {
                screen,
                changed: true,
                matched: None,
                status: "state_changed",
            });
        }
        let changed = screen.fingerprint != base.fingerprint;
        if screen.fingerprint != previous {
            previous = screen.fingerprint.clone();
            stable = Instant::now();
        }
        let matched = args.text.as_ref().is_none_or(|t| screen.text.contains(t))
            && args
                .absent_text
                .as_ref()
                .is_none_or(|t| !screen.text.contains(t))
            && args
                .stable_ms
                .is_none_or(|ms| stable.elapsed() >= Duration::from_millis(ms));
        let done = if explicit {
            matched
        } else {
            changed && stable.elapsed() >= Duration::from_millis(60)
        };
        if screen.ended || done || tokio::time::Instant::now() >= deadline {
            return Ok(Observed {
                status: if screen.ended {
                    "ended"
                } else if explicit && matched {
                    "condition_met"
                } else if changed {
                    "changed"
                } else {
                    "no_change"
                },
                screen,
                changed,
                matched: explicit.then_some(matched),
            });
        }
        let quiet = Duration::from_millis(args.stable_ms.unwrap_or(60));
        let needs_quiet = args.stable_ms.is_some() || (!explicit && changed);
        let until = if needs_quiet && stable.elapsed() < quiet {
            deadline.min(tokio::time::Instant::now() + quiet.saturating_sub(stable.elapsed()))
        } else {
            deadline
        };
        wait_event(&watch.updates, until).await;
    }
}
async fn wait(router: &Router, e: &Endpoint, owner: Arc<Controller>, args: Wait) -> LocalResult {
    let watch = match request(
        router,
        e,
        owner.clone(),
        Operation::Observe {
            screen_id: args.screen_id,
            fingerprint: args.fingerprint,
            region: args.region,
        },
    )
    .await
    {
        Ok(Reply::Observation(w)) => w,
        Err(e) => return failure(e),
        _ => return LocalResult::error("观察响应无效"),
    };
    // Own this baseline for the whole wait; later reads cannot evict it.
    let base = watch.baseline;
    let region = watch.screen.region.clone();
    let mut screen = watch.screen;
    let mut stable = Instant::now();
    let mut previous = screen.fingerprint.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(args.timeout_ms);
    loop {
        if screen.paused {
            let mut e = InteractionError::new(ErrorCode::UserTakeover, "用户已接管，停止等待");
            e.screen = Some(Box::new(screen));
            return failure(e);
        }
        if base.as_ref().is_some_and(|b| {
            b.rows != screen.rows
                || b.columns != screen.columns
                || b.alternate_screen != screen.alternate_screen
        }) {
            let mut e =
                InteractionError::new(ErrorCode::ScreenChanged, "终端尺寸或模式已变化，请重新读屏");
            e.screen = Some(Box::new(screen));
            return failure(e);
        }
        if screen.fingerprint != previous {
            previous = screen.fingerprint.clone();
            stable = Instant::now();
        }
        let changed = base
            .as_ref()
            .is_some_and(|b| b.fingerprint != screen.fingerprint);
        let matched = args.text.as_ref().is_none_or(|t| screen.text.contains(t))
            && args
                .absent_text
                .as_ref()
                .is_none_or(|t| !screen.text.contains(t))
            && (base.is_none() || changed)
            && args
                .stable_ms
                .is_none_or(|ms| stable.elapsed() >= Duration::from_millis(ms))
            && (!args.exited || screen.ended);
        if screen.ended || matched || tokio::time::Instant::now() >= deadline {
            return LocalResult::text(serde_json::json!({"target":e.target,"matched":matched,"changed":changed,"timed_out":!matched&&!screen.ended,"observation_status":if matched{"condition_met"}else if screen.ended{"ended"}else{"timed_out"},"screen":screen}).to_string());
        }
        let until = if let Some(ms) = args.stable_ms
            && stable.elapsed() < Duration::from_millis(ms)
        {
            deadline.min(
                tokio::time::Instant::now()
                    + Duration::from_millis(ms).saturating_sub(stable.elapsed()),
            )
        } else {
            deadline
        };
        wait_event(&watch.updates, until).await;
        screen = match next_screen(router, e, owner.clone(), region.clone()).await {
            Ok(s) => s,
            Err(e) => return failure(e),
        };
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::ai::{exec::Binding, host::Executor, session_catalog::Catalog, tasks::Tasks};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    fn observed_reply(screen: Screen, base: bool) -> Reply {
        let (_tx, updates) = async_channel::bounded(1);
        Reply::Observation(shellmate_term::ObservationWatch {
            baseline: base.then(|| screen.clone()),
            screen,
            updates,
        })
    }
    fn screen() -> Screen {
        Screen {
            ended: false,
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
    pub(crate) fn fixture() -> (
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
            guards: vec![],
            observe: Observe {
                timeout_ms: 0,
                ..Default::default()
            },
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
                        Operation::Observe { screen_id, .. } => {
                            let _ = reply.send(Ok(observed_reply(screen(), screen_id.is_some())));
                        }
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
            guards: vec![],
            observe: Observe {
                timeout_ms: 0,
                ..Default::default()
            },
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
        for (matched, ended) in [(false, false), (true, false), (false, true)] {
            let (_dir, executor, rx, id) = fixture();
            let bridge = tokio::spawn(async move {
                while let Ok(Ui::TerminalInteraction {
                    operation, reply, ..
                }) = rx.recv().await
                {
                    let s = Screen { ended, ..screen() };
                    let result = if matches!(operation, Operation::Observe { .. }) {
                        observed_reply(s, false)
                    } else {
                        Reply::Screen {
                            screen: s,
                            owned: false,
                        }
                    };
                    let _ = reply.send(Ok(result));
                }
            });
            let out = execute(
                &executor,
                Request::Wait(Wait {
                    screen_id: None,
                    absent_text: None,
                    exited: false,
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
            assert_eq!(result["timed_out"], !matched && !ended);
            assert_eq!(result["screen"]["ended"], ended);
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
                            Operation::Observe { screen_id, .. } => {
                                let _ =
                                    reply.send(Ok(observed_reply(screen(), screen_id.is_some())));
                            }
                            Operation::Read { .. } => {
                                let _ = reply.send(Ok(Reply::Screen {
                                    screen: screen(),
                                    owned: true,
                                }));
                            }
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
                guards: vec![],
                observe: Observe {
                    timeout_ms: 0,
                    ..Default::default()
                },
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
                assert!(matches!(
                    operation,
                    Operation::Observe { .. } | Operation::Read { .. }
                ));
                let result = if matches!(operation, Operation::Observe { .. }) {
                    observed_reply(screen(), false)
                } else {
                    Reply::Screen {
                        screen: screen(),
                        owned: true,
                    }
                };
                let _ = reply.send(Ok(result));
            }
        });
        let work = execute(
            &executor,
            Request::Wait(Wait {
                screen_id: None,
                absent_text: None,
                exited: false,
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
    #[tokio::test]
    async fn short_observation_waits_for_output_and_never_repeats_input() {
        for (change, expected) in [
            (false, None),
            (true, None),
            (true, Some("not there")),
            (true, Some("selected")),
        ] {
            let (_dir, executor, rx, id) = fixture();
            let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let n = writes.clone();
            let content = Arc::new(Mutex::new(screen()));
            let shared = content.clone();
            let (tx, updates) = async_channel::bounded(1);
            let bridge = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        Ui::TuiApproval { call, reply, .. } => {
                            let _ = reply.send(Approval::Approve {
                                command: call.command,
                                target: Target::Terminal,
                            });
                        }
                        Ui::TerminalInteraction {
                            operation, reply, ..
                        } => {
                            let response = match operation {
                                Operation::Check { .. } | Operation::Read { .. } => Reply::Screen {
                                    screen: shared.lock().unwrap().clone(),
                                    owned: true,
                                },
                                Operation::Observe { .. } => {
                                    Reply::Observation(shellmate_term::ObservationWatch {
                                        screen: shared.lock().unwrap().clone(),
                                        baseline: Some(screen()),
                                        updates: updates.clone(),
                                    })
                                }
                                Operation::Input { .. } => {
                                    n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if change {
                                        let shared = shared.clone();
                                        let tx = tx.clone();
                                        tokio::spawn(async move {
                                            tokio::time::sleep(Duration::from_millis(30)).await;
                                            let mut s = shared.lock().unwrap();
                                            s.text = "selected".into();
                                            s.fingerprint = "after".into();
                                            drop(s);
                                            let _ = tx.try_send(());
                                        });
                                    }
                                    Reply::Input(InputResult {
                                        screen: screen(),
                                        actions_sent: 1,
                                        error: None,
                                    })
                                }
                                Operation::Release => panic!("unexpected release"),
                            };
                            let _ = reply.send(Ok(response));
                        }
                        _ => {}
                    }
                }
            });
            let mut request = input(&id, "down");
            if let Request::Input(a) = &mut request {
                a.observe = Observe {
                    timeout_ms: 160,
                    text: expected.map(str::to_owned),
                    ..Default::default()
                };
            }
            let result = execute(&executor, request, ExecLimits::default(), None, "input").await;
            assert!(!result.is_error, "{}", result.inline);
            let v: serde_json::Value = serde_json::from_str(&result.inline).unwrap();
            assert_eq!(v["changed"], change);
            assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
            if let Some(expected) = expected {
                assert_eq!(v["condition_matched"], expected == "selected");
            } else {
                assert!(v["condition_matched"].is_null());
            }
            assert_eq!(
                v["screen"]["text"],
                if change { "selected" } else { "menu" }
            );
            bridge.abort();
        }
    }
}
