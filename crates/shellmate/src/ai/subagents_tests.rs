use super::*;
use crate::ai::{
    host::{Approval, Executor},
    subagent_view,
    tasks::Kind,
};
use futures::{StreamExt, future::BoxFuture};
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
use serde_json::{Value, json};
use shellmate_ai::{
    ContextSnapshot, Conversation, Entry, LoggedCall, LoggedResult, Step, SubagentKind,
    SubagentRef, Usage, UserTurn,
    agent::{LocalTool, ReadOutput, Target, ToolCall, ToolOutcome},
    policy::Class,
};
use std::{sync::atomic::AtomicBool, time::Duration};
fn response(call: bool, text: &str) -> MockHttpResponse {
    let chunk = |delta: Value, finish: Value| json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let mut events = vec![chunk(
        json!({"role":"assistant","content":text}),
        Value::Null,
    )];
    if call {
        events.push(chunk(json!({"tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":"run_command","arguments":json!({"command":"uname -s","target":"background"}).to_string()}}]}),Value::Null));
    }
    events.push(chunk(
        json!({}),
        json!(if call { "tool_calls" } else { "stop" }),
    ));
    MockHttpResponse::success_typed(
        events
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
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
fn request(background: bool) -> SubagentRequest {
    SubagentRequest {
        description: "inspect".into(),
        prompt: "inspect".into(),
        kind: SubagentKind::General,
        background,
        model: None,
        reasoning: None,
    }
}
fn fixture(
    responses: Vec<MockHttpResponse>,
) -> (
    tempfile::TempDir,
    Service,
    async_channel::Receiver<Ui>,
    SequencedHttpClient,
) {
    let root = tempfile::tempdir().unwrap();
    let (ui, rx) = async_channel::unbounded();
    let http = SequencedHttpClient::new(responses);
    let factory_http = http.clone();
    let service = Service {
        tasks: Tasks::new(root.path().into(), "conversation".into()),
        config: config(),
        settings: Settings::default(),
        snapshot: Default::default(),
        limits: Default::default(),
        db: SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap()),
        context: ContextSnapshot {
            label: "local".into(),
            protocol: "本机终端".into(),
            text: String::new(),
        },
        binding: Binding {
            session: 1,
            capabilities: shellmate_ai::Capabilities {
                background: true,
                cwd: Some(root.path().to_string_lossy().into()),
                ..Default::default()
            },
            connection: None,
            local: true,
            alive: Arc::new(AtomicBool::new(true)),
        },
        memory: shellmate_core::memory::Memory::new(root.path(), Some("local")),
        ui,
        mode: Arc::new(Mutex::new(shellmate_ai::PermissionMode::Ask)),
        cancel: Default::default(),
        factory: Arc::new(move |config| RigClient::with_http(config, factory_http.clone())),
    };
    (root, service, rx, http)
}
async fn approval(
    ui: &async_channel::Receiver<Ui>,
) -> (SubagentRef, tokio::sync::oneshot::Sender<Approval>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), ui.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Ui::Approval {
                source: Some(source),
                reply,
                ..
            } => return (source, reply),
            _ => continue,
        }
    }
}
async fn settled(tasks: &Tasks) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while tasks.has_running() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn read(id: &str) -> LocalTool {
    LocalTool::ReadOutput(ReadOutput {
        id: id.into(),
        offset_line: 0,
        limit_lines: None,
        grep: None,
    })
}
#[tokio::test]
async fn foreground_report_and_stop_keep_partial_report_and_source() {
    let (_root, service, _, http) = fixture(vec![response(false, "FINAL REPORT")]);
    let result = service.start(request(false), "parentcall").await;
    assert!(!result.is_error);
    assert!(result.inline.contains("FINAL REPORT"));
    assert_eq!(result.subagent.unwrap().status, SubagentStatus::Completed);
    assert_eq!(http.requests().len(), 1);
    let (_root, service, ui, _) = fixture(vec![response(true, "PARTIAL")]);
    let mut run = service.start(request(false), "stopcall");
    let (source, reply) = tokio::select! {result=&mut run=>panic!("unexpected {:?}",result),value=approval(&ui)=>value};
    assert_eq!(source.description, "inspect");
    assert_eq!(source.id, "agent-1");
    service.cancel.cancel();
    let result = run.await;
    assert_eq!(result.subagent.unwrap().status, SubagentStatus::Stopped);
    assert!(result.inline.contains("PARTIAL"));
    assert!(reply.is_closed());
}
#[tokio::test]
async fn background_returns_immediately_survives_main_stop_limit_and_stop_task() {
    let (_root, mut service, ui, _) = fixture(vec![response(true, "BACKGROUND-PARTIAL")]);
    service.settings.subagent_limit = 1;
    let result = service.start(request(true), "background").await;
    assert!(
        result
            .inline
            .starts_with("子任务已在后台启动：agent-1（inspect）")
    );
    assert_eq!(result.subagent.unwrap().status, SubagentStatus::Running);
    let (_, reply) = approval(&ui).await;
    let error = service.start(request(false), "limit").await;
    assert!(error.is_error);
    assert_eq!(error.inline, "已有 1 个子任务在运行，请等待或停止其中一个");
    service.cancel.cancel();
    assert!(!service.tasks.stop_foreground());
    assert!(service.tasks.has_running());
    assert!(!reply.is_closed());
    let progress = service
        .tasks
        .local_tool(read("agent-1"), Default::default())
        .await;
    assert!(progress.inline.contains("运行中") && progress.inline.contains("uname -s"));
    let stopped = service
        .tasks
        .local_tool(
            LocalTool::StopTask {
                id: "agent-1".into(),
            },
            Default::default(),
        )
        .await;
    assert!(!stopped.is_error);
    assert!(reply.is_closed());
    settled(&service.tasks).await;
    let result = service
        .tasks
        .local_tool(read("agent-1"), Default::default())
        .await;
    assert_eq!(result.inline, "BACKGROUND-PARTIAL");
    assert_eq!(service.tasks.take_notices(), ["子任务 agent-1 已停止"]);
}
#[tokio::test]
async fn disconnect_fails_and_clear_stops_all_without_late_file_recreation() {
    for clear in [false, true] {
        let (root, service, ui, _) = fixture(vec![response(true, "PARTIAL")]);
        service.start(request(true), "call").await;
        let (_, reply) = approval(&ui).await;
        if clear {
            service.tasks.discard();
            shellmate_core::ai_output::delete_conversation(root.path(), "conversation").unwrap();
        } else {
            service.binding.alive.store(false, Ordering::Release);
        }
        // Retired entries deliberately remain Running in the old view; the cancellation reply proves release.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !reply.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if clear {
            service
                .tasks
                .local_tool(
                    LocalTool::StopTask {
                        id: "agent-1".into(),
                    },
                    Default::default(),
                )
                .await;
            assert!(!root.path().join("ai-outputs/conversation").exists());
        } else {
            settled(&service.tasks).await;
            assert_eq!(
                service.tasks.agent_views()[0].status,
                SubagentStatus::Failed(shellmate_ai::AiError::Disconnected)
            );
            assert!(
                service.tasks.agent_updates()[0]
                    .1
                    .inline
                    .contains("连接已断开")
            );
        }
    }
}
#[tokio::test]
async fn background_completion_updates_original_log_spools_transcript_and_reopens_lazily() {
    let (root, service, ui, _) = fixture(vec![
        response(true, "checking"),
        response(false, "FULL REPORT\nlast line\n"),
    ]);
    let initial = service.start(request(true), "parent-call").await;
    let (_, reply) = approval(&ui).await;
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("delegate", Default::default()));
    log.append(Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "parent-call".into(),
            name: "agent".into(),
            args_json: json!({"run_in_background":true}),
        },
        result: LoggedResult {
            inline: initial.inline,
            output_id: initial.output_id,
            subagent: initial.subagent,
            cleared: false,
            is_error: false,
        },
        display: None,
    }));
    reply
        .send(Approval::Deny {
            reason: Some("no changes".into()),
        })
        .ok()
        .unwrap();
    settled(&service.tasks).await;
    assert!(service.tasks.sync_agents(&mut log));
    assert!(!service.tasks.sync_agents(&mut log));
    let Entry::Step(step) = &log.entries()[1] else {
        panic!("step")
    };
    let display = step.result.subagent.as_ref().unwrap();
    assert_eq!(display.status, SubagentStatus::Completed);
    assert_eq!(display.transcript_id.as_deref(), Some("agent-1"));
    let restored = {
        let mut db = service.db.borrow_mut();
        db.save_ai_conversation(&shellmate_store::ai::SavedConversation {
            id: "conversation".into(),
            host_id: None,
            label: "local".into(),
            title: "delegate".into(),
            provider: "openai".into(),
            model: "fixture".into(),
            created_at: 1,
            updated_at: 2,
            conversation: log.clone(),
        })
        .unwrap();
        db.ai_conversation("conversation").unwrap().unwrap()
    };
    assert!(restored.conversation == log);
    assert!(
        root.path()
            .join("ai-outputs/conversation/agent-1.json")
            .exists()
    );
    assert!(
        root.path()
            .join("ai-outputs/conversation/agent-1.log")
            .exists()
    );
    assert_eq!(
        service
            .tasks
            .local_tool(read("agent-1"), Default::default())
            .await
            .inline,
        "FULL REPORT\nlast line\n"
    );
    let mut view =
        subagent_view::View::reopened("agent-1".into(), display, true, step.result.inline.clone());
    assert!(view.transcript.is_none());
    view.load_transcript(root.path(), "conversation");
    assert!(view.transcript.is_some());
    assert_eq!(view.step_list[0].command, "uname -s");
    let reopened = Tasks::new(root.path().into(), "conversation".into());
    reopened.remember_outputs(&restored.conversation);
    assert_eq!(
        reopened
            .local_tool(read("agent-1"), Default::default())
            .await
            .inline,
        "FULL REPORT\nlast line\n"
    );
    assert_eq!(service.tasks.take_notices(), ["子任务 agent-1 已完成"]);
    std::fs::write(
        root.path().join("ai-outputs/conversation/agent-1.json"),
        "corrupt",
    )
    .unwrap();
    view.transcript = None;
    view.load_transcript(root.path(), "conversation");
    assert!(view.transcript.is_none());
    shellmate_core::ai_output::delete_conversation(root.path(), "conversation").unwrap();
    view.load_transcript(root.path(), "conversation");
    assert!(view.transcript.is_none());
}

struct FakeExecutor {
    calls: Arc<Mutex<Vec<ToolCall>>>,
}
impl Executor for FakeExecutor {
    fn execute(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call);
            ToolOutcome::Completed(shellmate_ai::shape(
                "ok",
                [],
                Some(0),
                Duration::from_millis(12),
                false,
                false,
            ))
        })
    }
}
fn call() -> ToolCall {
    ToolCall {
        id: "same-id".into(),
        command: "uname -s".into(),
        purpose: "inspect".into(),
        target: Target::Background,
        timeout: Duration::from_secs(5),
        run_in_background: false,
        exec_limits: Default::default(),
        class: Class::ReadOnly,
    }
}
#[tokio::test]
async fn approval_edits_follow_the_parent_policy_for_current_and_legacy_children() {
    use shellmate_ai::ToolHost;
    for kind in [SubagentKind::Explore, SubagentKind::General] {
        let (ui, rx) = async_channel::unbounded();
        let calls = Arc::new(Mutex::new(vec![]));
        let host = Host {
            compaction: Default::default(),
            mode: Default::default(),
            ui,
            executor: Box::new(FakeExecutor {
                calls: calls.clone(),
            }),
            alive: Arc::new(AtomicBool::new(true)),
            resolved: Mutex::new(None),
        };
        let source = SubagentRef {
            id: "agent-1".into(),
            description: "child".into(),
            kind,
        };
        let mut run = host.run_from(call(), Some(source.clone()));
        let Ui::Approval {
            reply,
            source: actual,
            ..
        } = (tokio::select! {_=&mut run=>panic!("must wait"),e=rx.recv()=>e.unwrap()})
        else {
            panic!("approval")
        };
        assert_eq!(actual, Some(source));
        reply
            .send(Approval::Approve {
                command: "touch /tmp/should-not-execute".into(),
                target: Target::Terminal,
            })
            .ok()
            .unwrap();
        let Ui::Running { ready, call, .. } =
            (tokio::select! {_=&mut run=>panic!("must wait for ready"),e=rx.recv()=>e.unwrap()})
        else {
            panic!("running")
        };
        assert_eq!(call.target, Target::Background);
        assert_eq!(call.class, Class::Other);
        ready.send(()).unwrap();
        assert!(matches!(run.await, ToolOutcome::Completed(_)));
        assert_eq!(calls.lock().unwrap()[0].target, Target::Background);
    }
}
#[tokio::test]
async fn approval_queue_is_fifo_independent_and_source_removal_cancels_replies() {
    let mut queue = crate::ai::approvals::Queue::default();
    let mut receivers = Vec::new();
    for (key, source) in [
        ("main", None),
        ("agent-1:call", Some("agent-1")),
        ("agent-2:call", Some("agent-2")),
    ] {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        queue.insert(key.into(), source.map(str::to_owned), tx);
        receivers.push(rx);
    }
    assert_eq!(
        queue.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        ["main", "agent-1:call", "agent-2:call"]
    );
    queue.remove("agent-1:call").unwrap().send(true).unwrap();
    assert!(receivers.remove(1).await.unwrap());
    assert_eq!(
        queue.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        ["main", "agent-2:call"]
    );
    queue.remove_source("agent-2");
    assert!(receivers.pop().unwrap().await.is_err());
    queue.take_main().unwrap().send(false).unwrap();
    assert!(!receivers.pop().unwrap().await.unwrap());
    assert!(queue.is_empty());
}
#[test]
fn event_mapping_has_steps_usage_report_and_tasks_share_existing_rows() {
    let mut view = subagent_view::View::new("agent-1".into(), &request(true), &config());
    let command = call();
    view.event(subagent::SubagentEvent::Agent(
        &shellmate_ai::AgentEvent::StepProposed(command.clone()),
    ));
    assert_eq!(view.latest_activity, "uname -s");
    let mut output = shellmate_ai::shape(
        "output",
        [],
        Some(0),
        Duration::from_millis(22),
        false,
        false,
    );
    output.output_id = Some("output-id".into());
    view.event(subagent::SubagentEvent::Agent(
        &shellmate_ai::AgentEvent::StepFinished {
            call: command,
            outcome: ToolOutcome::Completed(output),
        },
    ));
    view.event(subagent::SubagentEvent::Progress {
        steps: 3,
        usage: Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
        },
    });
    assert_eq!(view.steps, 3);
    assert_eq!(view.usage.input_tokens, Some(100));
    assert_eq!(view.step_list[0].exit_code, Some(0));
    assert_eq!(view.step_list[0].duration_ms, 22);
    assert_eq!(view.step_list[0].output_id.as_deref(), Some("output-id"));
    view.event(subagent::SubagentEvent::Agent(
        &shellmate_ai::AgentEvent::TextDelta("partial".into()),
    ));
    assert_eq!(view.report, "partial");
    let root = tempfile::tempdir().unwrap();
    let tasks = Tasks::new(root.path().into(), "test".into());
    let first = tasks
        .reserve_agent("one", &request(true), &config(), 3)
        .unwrap();
    let second = tasks
        .reserve_agent("two", &request(false), &config(), 3)
        .unwrap();
    assert_eq!(tasks.list().len(), 2);
    assert_eq!(tasks.running().len(), 1);
    assert_eq!(tasks.list()[0].kind, Kind::Agent);
    assert_eq!(tasks.list()[0].target_label(), "子任务");
    assert_eq!(tasks.list()[0].bar_id(), "1");
    assert!(tasks.list()[0].status_text().contains("运行中"));
    tasks.stop_all();
    assert!(first.cancel.is_cancelled() && second.cancel.is_cancelled());
}
#[test]
fn background_gate_tracks_ssh_local_and_excludes_other_protocols() {
    for protocol in [
        None,
        Some(shellmate_core::Protocol::Ssh),
        Some(shellmate_core::Protocol::Telnet),
        Some(shellmate_core::Protocol::Mosh),
        Some(shellmate_core::Protocol::Serial),
    ] {
        let ssh = protocol == Some(shellmate_core::Protocol::Ssh);
        let local = protocol.is_none();
        let (background, _) = crate::ai::model::execution_targets(protocol, true, ssh);
        assert_eq!(super::supported(background, local, ssh), ssh || local);
    }
    assert!(!super::supported(true, false, false));
    assert!(!super::supported(false, true, false));
}
#[tokio::test]
async fn model_override_and_completion_notice_reach_next_main_request() {
    use shellmate_ai::ToolHost;
    let (_root, service, _, _) = fixture(vec![response(false, "CHILD-REPORT")]);
    let mut settings = Settings {
        model: "child-model".into(),
        context_window: shellmate_ai::ContextWindow::Tokens(500000),
        max_output_tokens: shellmate_ai::OutputTokens::Tokens(8192),
        ..Default::default()
    };
    service.db.borrow_mut().save_ai_settings(&settings).unwrap();
    settings.model = "fixture".into();
    let r = SubagentRequest {
        model: Some("child-model".into()),
        ..request(true)
    };
    service.start(r, "call").await;
    settled(&service.tasks).await;
    let updates = service.tasks.agent_updates();
    assert_eq!(updates[0].1.subagent.as_ref().unwrap().model, "child-model");
    struct Notices(Tasks);
    impl ToolHost for Notices {
        fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            unreachable!()
        }
        fn subagent_results(&self) -> Vec<(String, LoggedResult)> {
            self.0.agent_updates()
        }
        fn take_notices(&self) -> Vec<String> {
            self.0.take_notices()
        }
    }
    let http = SequencedHttpClient::new([response(false, "main done")]);
    let client = RigClient::with_http(&config(), http.clone()).unwrap();
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("next", Default::default()));
    let events = shellmate_ai::agent::run(
        &client,
        &mut log,
        shellmate_ai::Capabilities {
            background: true,
            ..Default::default()
        },
        &Notices(service.tasks.clone()),
        Default::default(),
    )
    .unwrap()
    .collect::<Vec<_>>()
    .await;
    assert!(matches!(
        events.last(),
        Some(shellmate_ai::AgentEvent::Done(_))
    ));
    assert!(String::from_utf8_lossy(&http.requests()[0].body).contains("子任务 agent-1 已完成"));
}

#[tokio::test]
async fn scoped_registries_foreground_counts_and_step_limit_completion() {
    let (_root, mut service, ui, _) = fixture(vec![response(true, "partial")]);
    service.limits.max_steps = Some(1);
    service.settings.subagent_limit = 1;
    let mut foreground = service.start(request(false), "first");
    let (_, reply) = tokio::select! {_=&mut foreground=>panic!("wait"),a=approval(&ui)=>a};
    let result = service.start(request(true), "second").await;
    assert!(result.is_error);
    assert!(result.inline.contains("已有 1 个子任务"));
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    let result = foreground.await;
    assert_eq!(result.subagent.unwrap().status, SubagentStatus::StepLimit);
    assert!(result.inline.contains("结果不完整"));
    assert_eq!(
        service.tasks.list()[0].status,
        crate::ai::tasks::Status::StepLimit
    );
    let child = service.tasks.child_tasks();
    assert!(child.list().is_empty());
    assert!(child.take_notices().is_empty());
    assert!(!child.stop("agent-1"));
}
#[test]
fn transcript_retention_is_the_existing_spool_sweep() {
    let root = tempfile::tempdir().unwrap();
    super::super::subagent_files::write(root.path(), "c", "agent-1", &[]).unwrap();
    assert!(super::super::subagent_files::read(root.path(), "c", "agent-1").is_some());
    shellmate_core::ai_output::sweep(
        root.path(),
        std::time::SystemTime::now() + Duration::from_secs(31 * 86400),
    )
    .unwrap();
    assert!(super::super::subagent_files::read(root.path(), "c", "agent-1").is_none());
}

fn delegate_response(background: bool) -> MockHttpResponse {
    let args = json!({"description":"inspect","prompt":"independent task","type":"general","run_in_background":background});
    let chunks = [
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"delegate","type":"function","function":{"name":"agent","arguments":args.to_string()}}]},"finish_reason":null}]}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ];
    MockHttpResponse::success_typed(
        chunks
            .into_iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n",
        "text/event-stream",
    )
}
fn main_host(service: &Service) -> Host {
    Host {
        compaction: Default::default(),
        mode: service.mode.clone(),
        ui: service.ui.clone(),
        executor: Box::new(AppExecutor {
            subagents: Some(Arc::new(service.clone())),
            memory: Some((service.memory.clone(), "local".into())),
            binding: service.binding.clone(),
            tasks: service.tasks.clone(),
            ui: service.ui.clone(),
        }),
        alive: service.binding.alive.clone(),
        resolved: Mutex::new(None),
    }
}
#[tokio::test]
async fn actual_main_host_offers_routes_foreground_and_cancellation_logs_stopped() {
    for stop in [false, true] {
        let (_root, service, ui, _) = fixture(vec![response(
            stop,
            if stop { "PARTIAL CHILD" } else { "FINAL CHILD" },
        )]);
        let host = main_host(&service);
        let http =
            SequencedHttpClient::new([delegate_response(false), response(false, "main finished")]);
        let client = RigClient::with_http(&config(), http.clone()).unwrap();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("delegate", Default::default()));
        {
            let run = shellmate_ai::agent::run(
                &client,
                &mut log,
                service.binding.capabilities.clone(),
                &host,
                Default::default(),
            )
            .unwrap()
            .collect::<Vec<_>>();
            tokio::pin!(run);
            if stop {
                let (_, reply) =
                    tokio::select! {_=&mut run=>panic!("must wait approval"),a=approval(&ui)=>a};
                service.cancel.cancel();
                let events = run.await;
                assert!(matches!(
                    events.last(),
                    Some(shellmate_ai::AgentEvent::Error(
                        shellmate_ai::AiError::Cancelled
                    ))
                ));
                assert!(reply.is_closed());
            } else {
                assert!(matches!(
                    run.await.last(),
                    Some(shellmate_ai::AgentEvent::Done(_))
                ));
            }
        }
        let requests = http.requests();
        assert!(String::from_utf8_lossy(&requests[0].body).contains("\"name\":\"agent\""));
        let Entry::Step(step) = log
            .entries()
            .iter()
            .find(|e| matches!(e, Entry::Step(_)))
            .unwrap()
        else {
            panic!("step")
        };
        assert_eq!(
            step.result.subagent.as_ref().unwrap().status,
            if stop {
                SubagentStatus::Stopped
            } else {
                SubagentStatus::Completed
            }
        );
        assert!(
            step.result
                .inline
                .contains(if stop { "PARTIAL CHILD" } else { "FINAL CHILD" })
        );
    }
}

#[tokio::test]
async fn completion_after_retiring_saved_conversation_persists_and_cannot_be_replaced_by_running_clone()
 {
    let (root, service, ui, _) = fixture(vec![response(true, "PARTIAL KEPT")]);
    let initial = service.start(request(true), "persisted-call").await;
    let (_, reply) = approval(&ui).await;
    let mut conversation = Conversation::default();
    conversation.push_user(UserTurn::new("keep", Default::default()));
    conversation.append(Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "persisted-call".into(),
            name: "agent".into(),
            args_json: json!({"run_in_background":true}),
        },
        result: LoggedResult {
            inline: initial.inline,
            output_id: initial.output_id,
            subagent: initial.subagent,
            cleared: false,
            is_error: false,
        },
        display: None,
    }));
    let saved = shellmate_store::ai::SavedConversation {
        id: "conversation".into(),
        host_id: None,
        label: "local".into(),
        title: "keep".into(),
        provider: "openai".into(),
        model: "fixture".into(),
        created_at: 1,
        updated_at: 1,
        conversation,
    };
    service
        .db
        .borrow_mut()
        .save_ai_conversation(&saved)
        .unwrap();
    service.tasks.retire();
    service
        .tasks
        .local_tool(
            LocalTool::StopTask {
                id: "agent-1".into(),
            },
            Default::default(),
        )
        .await;
    assert!(reply.is_closed());
    tokio::time::timeout(Duration::from_secs(2),async{loop {
        let c=service.db.borrow().ai_conversation("conversation").unwrap().unwrap();
        if matches!(&c.conversation.entries()[1],Entry::Step(s) if s.result.subagent.as_ref().unwrap().status==SubagentStatus::Stopped){break;}
        tokio::task::yield_now().await;
    }}).await.unwrap();
    assert!(
        root.path()
            .join("ai-outputs/conversation/agent-1.json")
            .exists()
    );
    service
        .db
        .borrow_mut()
        .save_ai_conversation(&saved)
        .unwrap();
    let c = service
        .db
        .borrow()
        .ai_conversation("conversation")
        .unwrap()
        .unwrap();
    assert!(
        matches!(&c.conversation.entries()[1],Entry::Step(s) if s.result.subagent.as_ref().unwrap().status==SubagentStatus::Stopped&&s.result.inline.contains("PARTIAL KEPT"))
    );
    let mut c = c.conversation;
    c.apply(shellmate_ai::LogUpdate::ClearResults(vec![1]));
    assert!(!service.tasks.sync_agents(&mut c));
    assert!(matches!(&c.entries()[1],Entry::Step(s) if s.result.cleared));
}

fn tool_response(name: &str, args: Value) -> MockHttpResponse {
    let chunks = [
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":null}]}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ];
    MockHttpResponse::success_typed(
        chunks
            .into_iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n",
        "text/event-stream",
    )
}

#[tokio::test]
async fn project_investigation_executes_real_local_commands_with_inherited_full_access() {
    let commands = [
        "pwd",
        "/bin/pwd",
        "/usr/bin/pwd",
        "/bin/ls -la",
        "bash -lc 'set -o pipefail\nroot=\"$PWD\"\nprintf \"CURRENT_PATH\\n%s\\n\" \"$root\"\n/bin/ls -la \"$root\"\nfor name in README.md Cargo.toml; do if [ -f \"$name\" ]; then cat \"$name\"; fi; done'",
    ];
    let mut responses = commands
        .iter()
        .map(|command| {
            tool_response(
                "run_command",
                json!({"command":command,"target":"background"}),
            )
        })
        .collect::<Vec<_>>();
    responses.push(response(
        false,
        "Rust project identified from README and Cargo.toml",
    ));
    let (root, service, ui, _) = fixture(responses);
    std::fs::write(root.path().join("README.md"), "# Fixture Rust project\n").unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    *service.mode.lock().unwrap() = shellmate_ai::PermissionMode::Full;
    let mut request = request(false);
    request.kind = SubagentKind::Explore; // old callers inherit too
    let mut run = service.start(request, "project-investigation");
    let result=tokio::time::timeout(Duration::from_secs(15),async{
        loop {tokio::select!{
            result=&mut run=>break result,
            event=ui.recv()=>match event.unwrap(){
                Ui::Running{ready,source,..}=>{assert!(source.is_some());ready.send(()).unwrap();}
                Ui::Approval{..}=>panic!("Full mode must match the main agent for ordinary commands"),
                _=>{}
            }
        }}
    }).await.unwrap();
    assert!(!result.is_error);
    let view = service.tasks.agent_views().pop().unwrap();
    assert_eq!(view.step_list.len(), commands.len());
    for step in &view.step_list {
        // macOS provides /bin/pwd, not /usr/bin/pwd. Missing executables must
        // reach the shell and return its real exit code, not a policy rejection.
        let expected =
            if step.command == "/usr/bin/pwd" && !std::path::Path::new("/usr/bin/pwd").exists() {
                127
            } else {
                0
            };
        assert_eq!(
            step.exit_code,
            Some(expected),
            "{}: {}",
            step.command,
            step.output
        );
        assert_eq!(step.status, shellmate_ai::StepStatus::Completed);
    }
    assert!(
        view.step_list
            .last()
            .unwrap()
            .output
            .contains("Fixture Rust project")
    );
}

#[tokio::test]
async fn subagent_memory_supports_the_same_mutations_and_reports_write_conflicts() {
    let path = "/memories/global/project.md";
    let operations = [
        json!({"command":"create","path":path,"file_text":"project: old\n"}),
        json!({"command":"str_replace","path":path,"old_str":"old","new_str":"new"}),
        json!({"command":"insert","path":path,"insert_line":1,"insert_text":"checked\n"}),
        json!({"command":"rename","old_path":path,"new_path":"/memories/global/renamed.md"}),
        json!({"command":"view","path":"/memories/global/renamed.md"}),
        json!({"command":"delete","path":"/memories/global/renamed.md"}),
    ];
    let mut responses = operations
        .into_iter()
        .map(|args| tool_response("memory", args))
        .collect::<Vec<_>>();
    responses.push(response(false, "memory updated"));
    let (root, service, _, http) = fixture(responses);
    let result = service.start(request(false), "memory").await;
    assert!(!result.is_error);
    let view = service.tasks.agent_views().pop().unwrap();
    let entries = view.transcript.unwrap();
    let steps = entries
        .iter()
        .filter_map(|e| {
            if let Entry::Step(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(steps.len(), 6);
    assert!(steps.iter().all(|s| !s.result.is_error));
    assert!(steps[4].result.inline.contains("project: new"));
    assert!(steps[4].result.inline.contains("checked"));
    assert!(!root.path().join("memory/global/renamed.md").exists());
    let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
    let memory = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "memory")
        .unwrap();
    assert_eq!(
        memory["function"]["parameters"]["properties"]["command"]["enum"],
        json!([
            "view",
            "create",
            "str_replace",
            "insert",
            "delete",
            "rename"
        ])
    );

    // Two child contexts share the main store's write lock and optimistic replacement behavior.
    service.memory.create(path, "before").unwrap();
    let first = service.memory.clone();
    let second = service.memory.clone();
    let a = tokio::task::spawn_blocking(move || first.str_replace(path, "before", "first"));
    let b = tokio::task::spawn_blocking(move || second.str_replace(path, "before", "second"));
    let (a, b) = tokio::join!(a, b);
    let results = [a.unwrap(), b.unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(matches!(
        service.memory.read(path).unwrap().as_str(),
        "first" | "second"
    ));
}

#[tokio::test]
async fn foreground_child_detaches_without_cancelling_its_pending_approval() {
    use shellmate_ai::interaction::Intent;
    let (_root, service, ui, _) =
        fixture(vec![response(true, "partial"), response(false, "finished")]);
    let mut run = service.start(request(false), "delegate");
    let (_, reply) = tokio::select! {_=&mut run=>panic!("must wait"),a=approval(&ui)=>a};
    service.tasks.control().request(Intent::SendNow);
    let result = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .unwrap();
    assert!(result.inline.contains("后台启动"));
    assert!(!reply.is_closed());
    assert!(service.tasks.has_running());
    assert!(service.tasks.agent_views()[0].background);
    service.tasks.control().take_intent();
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    settled(&service.tasks).await;
    assert_eq!(service.tasks.control().snapshot().completions.len(), 1);
    let completion = service.tasks.control().claim_completions();
    assert_eq!(completion.len(), 1);
    assert!(service.tasks.control().claim_completions().is_empty());
    service
        .tasks
        .control()
        .acknowledge_completions(&[completion[0].id.clone()]);
    assert!(!service.tasks.control().pending_completions());
}

#[tokio::test]
async fn ended_child_resumes_original_context_without_overwriting_the_previous_report() {
    let (root, service, _ui, http) = fixture(vec![
        response(false, "FIRST-REPORT"),
        response(false, "SECOND-REPORT"),
    ]);
    let first = service.start(request(false), "first").await;
    assert!(!first.is_error);
    let second = service
        .message("agent-1", "FOLLOW-UP", false, true, "second")
        .await;
    assert!(!second.is_error);
    settled(&service.tasks).await;
    let views = service.tasks.agent_views();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].report, "FIRST-REPORT");
    assert_eq!(views[1].report, "SECOND-REPORT");
    assert_eq!(
        service.tasks.latest_agent("agent-1").unwrap().0.id,
        "agent-2"
    );
    let requests = http.requests();
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(body.contains("FIRST-REPORT"));
    assert!(body.contains("FOLLOW-UP"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("ai-outputs/conversation/agent-1.log")).unwrap(),
        "FIRST-REPORT"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("ai-outputs/conversation/agent-2.log")).unwrap(),
        "SECOND-REPORT"
    );
}

#[tokio::test]
async fn running_child_receives_messages_and_only_one_resume_can_reserve_its_family() {
    let (_root, service, ui, http) = fixture(vec![
        response(true, "checking"),
        response(false, "after-message"),
    ]);
    service.start(request(true), "initial").await;
    let (_, reply) = approval(&ui).await;
    let sent = service
        .message("agent-1", "ADDITIONAL-REQUIREMENT", false, false, "message")
        .await;
    assert!(!sent.is_error);
    assert!(!reply.is_closed());
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    settled(&service.tasks).await;
    assert!(String::from_utf8_lossy(&http.requests()[1].body).contains("ADDITIONAL-REQUIREMENT"));
    let tasks = service.tasks.clone();
    let first = tasks
        .reserve_agent_family("new1", &request(true), &config(), 3, Some("agent-1"))
        .unwrap();
    assert!(
        tasks
            .reserve_agent_family("new2", &request(true), &config(), 3, Some("agent-1"))
            .is_err()
    );
    first.fail(shellmate_ai::AiError::Cancelled, Default::default());
}

#[tokio::test]
async fn restored_child_uses_saved_family_context_and_queued_message_checkpoint() {
    let (root, service, _ui, _) = fixture(vec![response(false, "SAVED-FIRST")]);
    let first = service.start(request(false), "first").await;
    assert!(!first.is_error);
    let mut saved = Conversation::default();
    saved.push_user(UserTurn::new("delegate", Default::default()));
    saved.append(Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "first".into(),
            name: "agent".into(),
            args_json: json!({"run_in_background":false}),
        },
        result: LoggedResult {
            inline: first.inline,
            output_id: first.output_id,
            subagent: first.subagent,
            cleared: false,
            is_error: false,
        },
        display: None,
    }));
    let (restored_root, mut restored, _ui, http) =
        fixture(vec![response(false, "RESTORED-SECOND")]);
    restored.tasks = Tasks::new(root.path().into(), "conversation".into());
    restored.tasks.remember_outputs(&saved);
    let mut checkpoint =
        super::super::subagent_files::read_checkpoint(root.path(), "conversation", "agent-1")
            .unwrap();
    checkpoint
        .inbox
        .inputs
        .push_back(shellmate_ai::interaction::Input {
            origin: None,
            id: "pending".into(),
            text: "RECOVERED-PENDING".into(),
            context: Default::default(),
            claimed: false,
            editing: false,
        });
    super::super::subagent_files::checkpoint(
        root.path(),
        "conversation",
        "agent-1",
        Some(&checkpoint.entries),
        checkpoint.inbox,
    )
    .unwrap();
    let result = restored
        .message("agent-1", "CONTINUE", false, true, "second")
        .await;
    assert!(!result.is_error);
    settled(&restored.tasks).await;
    let body = String::from_utf8_lossy(&http.requests()[0].body).into_owned();
    assert!(body.contains("SAVED-FIRST"));
    assert!(body.contains("RECOVERED-PENDING"));
    assert!(body.contains("CONTINUE"));
    assert!(!body.contains(restored_root.path().to_str().unwrap()));
    assert_eq!(
        restored.tasks.latest_agent("agent-1").unwrap().0.report,
        "RESTORED-SECOND"
    );
}

#[tokio::test]
async fn failed_message_checkpoint_does_not_deliver_the_child_instruction() {
    let (root, service, ui, http) = fixture(vec![
        response(true, "checking"),
        response(false, "finished"),
    ]);
    service.start(request(true), "initial").await;
    let (_, reply) = approval(&ui).await;
    std::fs::remove_file(
        root.path()
            .join("ai-outputs/conversation/agent-1.checkpoint.json"),
    )
    .unwrap();
    let sent = service
        .message("agent-1", "UNSAVED-INSTRUCTION", false, false, "message")
        .await;
    assert!(sent.is_error);
    assert!(
        service
            .tasks
            .latest_agent("agent-1")
            .unwrap()
            .2
            .unwrap()
            .control()
            .snapshot()
            .inputs
            .is_empty()
    );
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    settled(&service.tasks).await;
    assert!(!String::from_utf8_lossy(&http.requests()[1].body).contains("UNSAVED-INSTRUCTION"));
}

#[tokio::test]
async fn checkpoint_failure_pauses_at_tool_boundary_and_retries_without_reexecution() {
    let (root, service, ui, http) = fixture(vec![
        response(true, "checking"),
        response(false, "finished"),
    ]);
    service.start(request(true), "initial").await;
    let (_, reply) = approval(&ui).await;
    assert!(
        !service
            .message("agent-1", "KEEP-THIS-INPUT", false, false, "followup")
            .await
            .is_error
    );
    let path = root
        .path()
        .join("ai-outputs/conversation/agent-1.checkpoint.json");
    let backup = path.with_extension("backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if service.tasks.agent_views()[0]
                .latest_activity
                .contains("保存失败")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(http.requests().len(), 1);
    let child = service.tasks.latest_agent("agent-1").unwrap().2.unwrap();
    assert_eq!(child.control().snapshot().inputs.len(), 1);
    assert!(service.tasks.has_running());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    settled(&service.tasks).await;
    assert_eq!(http.requests().len(), 2);
    assert!(String::from_utf8_lossy(&http.requests()[1].body).contains("KEEP-THIS-INPUT"));
    let checkpoint =
        super::super::subagent_files::read_checkpoint(root.path(), "conversation", "agent-1")
            .unwrap();
    assert_eq!(
        checkpoint
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::Step(_)))
            .count(),
        1
    );
    assert_eq!(
        checkpoint
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::User { text, .. } if text == "KEEP-THIS-INPUT"))
            .count(),
        1
    );
    assert!(child.control().snapshot().inputs.is_empty());
}

#[tokio::test]
async fn initial_checkpoint_failure_does_not_start_a_model_request() {
    let (root, service, _, http) = fixture(vec![]);
    std::fs::create_dir_all(
        root.path()
            .join("ai-outputs/conversation/agent-1.checkpoint.json"),
    )
    .unwrap();
    let result = service.start(request(false), "initial").await;
    assert!(result.is_error);
    assert_eq!(
        service.tasks.agent_views()[0].status,
        SubagentStatus::Failed(shellmate_ai::AiError::Persistence)
    );
    assert!(http.requests().is_empty());
}

#[tokio::test]
async fn live_child_transcript_preserves_delegation_and_message_sources_before_and_after_reload() {
    use shellmate_ai::MessageOrigin;
    let (root, service, ui, _) = fixture(vec![
        response(true, "checking live"),
        response(false, "finished"),
    ]);
    service.start(request(true), "initial").await;
    let (_, reply) = approval(&ui).await;
    let running = service.tasks.agent_views().remove(0);
    assert!(
        matches!(&running.transcript.as_ref().unwrap()[0],Entry::User{text,origin:Some(MessageOrigin::Delegation),..} if text=="inspect")
    );
    assert!(
        running
            .transcript
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| matches!(e,Entry::Assistant{text,..} if text.contains("checking live")))
    );
    assert_eq!(running.active_tools.len(), 1);
    service
        .message("agent-1", "parent followup", false, false, "parent-message")
        .await;
    service
        .message_from(
            "agent-1",
            "user followup",
            false,
            false,
            "user-message",
            MessageOrigin::User,
        )
        .await;
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    settled(&service.tasks).await;
    let completed = service.tasks.agent_views().remove(0);
    assert!(completed.active_tools.is_empty());
    let entries = completed.transcript.as_ref().unwrap();
    assert!(entries.iter().any(|e|matches!(e,Entry::User{text,origin:Some(MessageOrigin::Parent),..} if text=="parent followup")));
    assert!(entries.iter().any(|e|matches!(e,Entry::User{text,origin:Some(MessageOrigin::User),..} if text=="user followup")));
    let mut restored = subagent_view::View::reopened(
        completed.id.clone(),
        &completed.display(),
        true,
        completed.report.clone(),
    );
    restored.load_transcript(root.path(), "conversation");
    assert_eq!(restored.transcript, completed.transcript);
    assert_eq!(restored.cwd, service.binding.capabilities.cwd);
}

#[tokio::test]
async fn stopped_child_keeps_undelivered_user_messages_visible_after_reopening() {
    use shellmate_ai::MessageOrigin;
    let (root, service, ui, _) = fixture(vec![response(true, "waiting")]);
    service.start(request(true), "initial").await;
    let (_, reply) = approval(&ui).await;
    service
        .message_from(
            "agent-1",
            "keep my queued request",
            false,
            false,
            "user-message",
            MessageOrigin::User,
        )
        .await;
    service.tasks.stop("agent-1");
    settled(&service.tasks).await;
    drop(reply);
    let stopped = service.tasks.agent_views().remove(0);
    assert_eq!(stopped.pending_inputs.len(), 1);
    assert_eq!(stopped.pending_inputs[0].origin, Some(MessageOrigin::User));
    let mut restored = subagent_view::View::reopened(
        stopped.id.clone(),
        &stopped.display(),
        true,
        stopped.report.clone(),
    );
    restored.load_transcript(root.path(), "conversation");
    assert_eq!(restored.pending_inputs, stopped.pending_inputs);
}
