use super::{
    drawer_stream,
    slash_ui::{LocalBody, LocalCard},
    state::ConversationState,
};
use crate::ai::{
    commands::{COMMANDS, Dispatch, dispatch},
    model::load_config,
};
use futures::{StreamExt, future::BoxFuture};
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
use serde_json::{Value, json};
use shellmate_ai::{
    ContextSnapshot, ContextWindow, OutputTokens, RigClient, UserTurn,
    agent::{self, AgentEvent, Capabilities, ToolCall, ToolHost, ToolOutcome},
};
use shellmate_store::{HostStore, MemoryStore, SecretKey, SecretStore, ai::Settings};
use std::{sync::Mutex, time::Duration};

#[derive(Default)]
struct Host {
    queue: Mutex<Option<Option<String>>>,
    calls: Mutex<Vec<ToolCall>>,
}
impl ToolHost for Host {
    fn take_compaction(&self) -> Option<Option<String>> {
        self.queue.lock().unwrap().take()
    }
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.calls.lock().unwrap().push(call);
        Box::pin(async {
            ToolOutcome::Completed(shellmate_ai::output::shape(
                "ok",
                [],
                Some(0),
                Duration::ZERO,
                false,
                false,
            ))
        })
    }
}
fn response(call: Option<usize>, text: &str) -> MockHttpResponse {
    let chunk = |delta: Value, finish: Value| json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let mut events = vec![chunk(
        json!({"role":"assistant","content":text}),
        Value::Null,
    )];
    if let Some(n) = call {
        events.push(chunk(json!({"tool_calls":[{"index":0,"id":format!("call-{n}"),"type":"function","function":{"name":"run_command","arguments":json!({"command":"uname -s","target":"background"}).to_string()}}]}),Value::Null));
    }
    events.push(chunk(
        json!({}),
        json!(if call.is_some() { "tool_calls" } else { "stop" }),
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
fn saved() -> (HostStore, Settings, shellmate_ai::Config) {
    let mut db = HostStore::open_in_memory().unwrap();
    let s = Settings {
        model: "fixture".into(),
        context_window: ContextWindow::Tokens(400000),
        autocompact_window: ContextWindow::Tokens(200000),
        max_output_tokens: OutputTokens::Tokens(8192),
        max_steps: Some(50),
        exec: agent::ExecLimits {
            default_timeout_secs: 37,
            max_timeout_secs: 91,
            tool_output_chars: 41000,
        },
        ..Default::default()
    };
    db.save_ai_settings(&s).unwrap();
    let s = db.ai_settings().unwrap();
    let secrets = MemoryStore::new();
    secrets
        .set(
            &SecretKey::Account(s.key_account()),
            &shellmate_core::Secret::new("fixture-key"),
        )
        .unwrap();
    let config = load_config(&s, &secrets).unwrap();
    (db, s, config)
}
fn state() -> ConversationState<()> {
    let mut s = ConversationState::default();
    s.model
        .conversation
        .push_user(UserTurn::new("检查服务", ContextSnapshot::default()));
    s
}
fn caps() -> Capabilities {
    Capabilities {
        background: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn saved_settings_reach_drawer_factory_and_pause_at_fifty() {
    let (_, s, config) = saved();
    assert_eq!(config.context_window, s.context_window);
    assert_eq!(config.autocompact_window, s.autocompact_window);
    assert_eq!(config.max_output_tokens, s.max_output_tokens);
    let http = SequencedHttpClient::new((0..51).map(|n| response(Some(n), "")).collect::<Vec<_>>());
    let client = RigClient::with_http(&config, http.clone()).unwrap();
    assert_eq!(client.context_budget().window, 400000);
    assert_eq!(client.context_budget().auto, 200000);
    let mut state = state();
    let host = Host::default();
    let mut paused = false;
    let mut stream = drawer_stream(
        &client,
        &mut state.model.conversation,
        caps(),
        &host,
        s.limits(),
        None,
        None,
    )
    .unwrap();
    while let Some(e) = stream.next().await {
        match e {
            AgentEvent::Paused { steps, reason, .. } => {
                assert_eq!(steps, 50);
                assert_eq!(reason, agent::PauseReason::StepLimit);
                paused = true;
            }
            AgentEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert!(paused);
    assert_eq!(host.calls.lock().unwrap().len(), 50);
    for c in host.calls.lock().unwrap().iter() {
        assert_eq!(c.timeout, Duration::from_secs(37));
        assert_eq!(c.exec_limits, s.exec);
    }
    for r in http.requests() {
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["max_tokens"], 8192);
        let schema = &v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "run_command")
            .unwrap()["function"]["parameters"]["properties"]["timeout_secs"];
        assert_eq!(schema["default"], 37);
        assert_eq!(schema["maximum"], 91);
        assert!(
            schema["description"]
                .as_str()
                .unwrap()
                .contains("默认 37、最长 91")
        );
    }
}

#[tokio::test]
async fn queued_compaction_waits_for_tool_boundary_then_run_continues() {
    let (_, s, config) = saved();
    let http = SequencedHttpClient::new([
        response(Some(1), "先检查"),
        response(None, "<summary>检查结果正常</summary>"),
        response(Some(2), "继续"),
        response(None, "完成"),
    ]);
    let client = RigClient::with_http(&config, http.clone()).unwrap();
    let mut state = state();
    let host = Host::default();
    let mut events = Vec::new();
    let mut queued = false;
    let mut stream = drawer_stream(
        &client,
        &mut state.model.conversation,
        caps(),
        &host,
        s.limits(),
        None,
        None,
    )
    .unwrap();
    while let Some(e) = stream.next().await {
        match e {
            AgentEvent::StepProposed(_) if !queued => {
                let Dispatch::QueueCompact(focus) =
                    dispatch("/compact 保留服务状态", true, COMMANDS)
                else {
                    panic!("queue")
                };
                assert!(host.calls.lock().unwrap().is_empty());
                *host.queue.lock().unwrap() = Some(focus);
                queued = true;
            }
            AgentEvent::StepFinished { .. } => events.push("step"),
            AgentEvent::Compacted { .. } => events.push("compact"),
            AgentEvent::Done(_) => events.push("done"),
            AgentEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    drop(stream);
    assert_eq!(events, ["step", "compact", "step", "done"]);
    let bodies = http
        .requests()
        .into_iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
        .collect::<Vec<_>>();
    assert!(bodies[1].to_string().contains("额外要求：保留服务状态"));
    assert!(bodies[1].to_string().contains("ok"));
    assert!(bodies[2].to_string().contains("检查结果正常"));
    assert_eq!(host.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn local_cards_never_enter_request_or_persistence_and_reload_clears_them() {
    let (mut db, s, config) = saved();
    let mut state = state();
    state.local_cards.push(LocalCard {
        after_turn: 1,
        body: LocalBody::Hint("LOCAL-ONLY-SENTINEL".into()),
    });
    state.local_cards.push(LocalCard {
        after_turn: 1,
        body: LocalBody::Help,
    });
    let http = SequencedHttpClient::new([response(None, "done")]);
    let client = RigClient::with_http(&config, http.clone()).unwrap();
    let host = Host::default();
    let mut stream = drawer_stream(
        &client,
        &mut state.model.conversation,
        caps(),
        &host,
        s.limits(),
        None,
        None,
    )
    .unwrap();
    while let Some(e) = stream.next().await {
        if let AgentEvent::Error(e) = e {
            panic!("{e}");
        }
    }
    drop(stream);
    let request = String::from_utf8(http.requests()[0].body.to_vec()).unwrap();
    assert!(!request.contains("LOCAL-ONLY-SENTINEL"));
    assert!(!request.contains("/help"));
    state.persist(&mut db, None, "openai", "fixture");
    let loaded = db.ai_conversation(&state.id).unwrap().unwrap();
    assert!(
        !serde_json::to_string(loaded.conversation.entries())
            .unwrap()
            .contains("LOCAL-ONLY-SENTINEL")
    );
    state.reset_conversation();
    state.model.conversation = loaded.conversation;
    assert!(state.local_cards.is_empty());
}

#[test]
fn slash_mode_uses_chip_persistence_for_the_bound_host() {
    let mut db = HostStore::open_in_memory().unwrap();
    let current = Mutex::new(shellmate_ai::PermissionMode::Ask);
    for (text, expected) in [
        ("/mode auto", shellmate_ai::PermissionMode::Auto),
        ("/mode ask", shellmate_ai::PermissionMode::Ask),
        ("/mode full", shellmate_ai::PermissionMode::Full),
    ] {
        let Dispatch::Run { name: "mode", args } = dispatch(text, false, COMMANDS) else {
            panic!("mode command")
        };
        let mode = crate::ai::commands::mode(&args).unwrap();
        // Full reaches this commit only after the shared FullAccess confirmation.
        crate::ai::model::save_permission(&mut db, "fixture-host", &current, mode).unwrap();
        assert_eq!(db.ai_permission("fixture-host").unwrap(), expected);
        assert_eq!(*current.lock().unwrap(), expected);
        assert_eq!(
            db.ai_permission("other-host").unwrap(),
            shellmate_ai::PermissionMode::Ask
        );
    }
}
