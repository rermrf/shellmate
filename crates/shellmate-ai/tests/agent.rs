use futures::{StreamExt, future::BoxFuture};
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
use serde_json::{Value, json};
use shellmate_ai::{agent, policy::Class, *};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn config(a: bool) -> Config {
    Config {
        provider: if a {
            Provider::Anthropic {
                base_url: "https://api.anthropic.com".into(),
            }
        } else {
            Provider::OpenAiCompatible {
                base_url: "https://example.test/v1".into(),
            }
        },
        model: if a {
            "claude-sonnet-4-6"
        } else {
            "custom-model"
        }
        .into(),
        api_key: ApiKey::new("fixture-only"),
        reasoning: ReasoningLevel::Default,
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Timeouts::default(),
    }
}
fn conversation() -> Conversation {
    Conversation::from_turns(vec![Turn {
        user: UserTurn::new("检查容器", ContextSnapshot::default()),
        answer: None,
    }])
}
fn capabilities() -> Capabilities {
    Capabilities {
        background: true,
        terminal: true,
        cwd: Some("/tmp".into()),
        ..Default::default()
    }
}
fn args(command: &str) -> Value {
    json!({"command":command,"purpose":"检查状态","target":"background","timeout_secs":999})
}
fn wire(a: bool, calls: &[(&str, &str, Value)], text: &str, thinking: bool) -> String {
    let mut events = Vec::new();
    if a {
        events.push(json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":0}}}));
        let mut index = 0;
        if thinking {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":"PRIVATE-THINKING"}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":"signature-"}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":"fixture"}}));
            events.push(json!({"type":"content_block_stop","index":index}));
            index += 1;
        }
        if !text.is_empty() {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}));
            events.push(json!({"type":"content_block_stop","index":index}));
            index += 1;
        }
        for (id, name, args) in calls {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
            events.push(json!({"type":"content_block_stop","index":index}));
            index += 1;
        }
        events.push(json!({"type":"message_delta","delta":{"stop_reason":if calls.is_empty(){"end_turn"}else{"tool_use"},"stop_sequence":null},"usage":{"output_tokens":8}}));
        events.push(json!({"type":"message_stop"}));
        events
            .into_iter()
            .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
            .collect()
    } else {
        let chunk = |delta: Value, finish: Value| json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"custom-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        events.push(chunk(
            json!({"role":"assistant","content":text}),
            Value::Null,
        ));
        for (index, (id, name, args)) in calls.iter().enumerate() {
            events.push(chunk(json!({"tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),Value::Null));
        }
        events.push(chunk(
            json!({}),
            json!(if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }),
        ));
        events
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n"
    }
}
fn response(
    a: bool,
    calls: &[(&str, &str, Value)],
    text: &str,
    thinking: bool,
) -> MockHttpResponse {
    MockHttpResponse::success_typed(wire(a, calls, text, thinking), "text/event-stream")
}
fn completed(text: &str) -> ToolOutcome {
    ToolOutcome::Completed(shape(
        text,
        [],
        Some(0),
        Duration::from_millis(12),
        false,
        false,
    ))
}
struct Host {
    outcomes: Mutex<VecDeque<ToolOutcome>>,
    calls: Mutex<Vec<ToolCall>>,
    active: AtomicUsize,
}
impl Host {
    fn new(outcomes: impl IntoIterator<Item = ToolOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
        }
    }
}
impl ToolHost for Host {
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            assert_eq!(self.active.fetch_add(1, Ordering::SeqCst), 0);
            self.calls.lock().unwrap().push(call);
            tokio::task::yield_now().await;
            let out = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected host call");
            self.active.fetch_sub(1, Ordering::SeqCst);
            out
        })
    }
}
fn bodies(http: &SequencedHttpClient) -> Vec<Value> {
    http.requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}
fn tool_result(body: &Value, a: bool, id: &str) -> String {
    let messages = body["messages"].as_array().unwrap();
    if a {
        let block = messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
            .expect("matching Anthropic tool result");
        block["content"].to_string()
    } else {
        let message = messages
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .expect("matching OpenAI tool result");
        message["content"].to_string()
    }
}
#[tokio::test]
async fn both_wires_complete_with_real_tool_results_and_parallel_disabled() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("call-1", "run_command", args("docker ps"))],
                "先检查。",
                false,
            ),
            response(a, &[], "web 容器正在运行。", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("web\nPASSWORD=hidden")]);
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        assert!(http.requests().is_empty());
        let mut order = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(s) => order.push(s),
                AgentEvent::StepProposed(call) => {
                    assert_eq!(call.class, Class::ReadOnly);
                    assert_eq!(call.timeout, Duration::from_secs(120));
                    order.push("proposed".into());
                    assert!(host.calls.lock().unwrap().is_empty());
                }
                AgentEvent::StepFinished { .. } => order.push("finished".into()),
                AgentEvent::Done(_) => order.push("done".into()),
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        drop(stream);
        assert_eq!(
            order,
            [
                "先检查。",
                "proposed",
                "finished",
                "web 容器正在运行。",
                "done"
            ]
        );
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        let bodies = bodies(&http);
        assert_eq!(bodies.len(), 2);
        let result = tool_result(&bodies[1], a, "call-1");
        assert!(result.contains("web"));
        assert!(result.contains("hidden"));
        if a {
            assert_eq!(bodies[0]["tool_choice"]["disable_parallel_tool_use"], true);
        } else {
            assert_eq!(bodies[0]["parallel_tool_calls"], false);
        }
        let answer = c.turns()[0].answer.as_ref().unwrap();
        assert!(matches!(
            &answer.items[..],
            [
                AnswerItem::Text(_),
                AnswerItem::Step { .. },
                AnswerItem::Text(_)
            ]
        ));
        assert!(answer.wire_text().contains("hidden"));
    }
}
#[tokio::test]
async fn denied_busy_failed_and_disconnected() {
    for a in [false, true] {
        for (outcome, expected, disconnect) in [
            (
                ToolOutcome::Denied {
                    reason: Some("正在维护 PASSWORD=private".into()),
                },
                "正在维护",
                false,
            ),
            (ToolOutcome::Busy, "终端正忙", false),
            (
                ToolOutcome::Failed("故障 PASSWORD=private".into()),
                "故障",
                false,
            ),
            (ToolOutcome::Disconnected, "会话已断开", true),
        ] {
            let http = SequencedHttpClient::new([
                response(
                    a,
                    &[("call-1", "run_command", args("docker ps"))],
                    "",
                    false,
                ),
                response(a, &[], "已停止。", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([outcome]);
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let body = bodies(&http);
            if disconnect {
                assert_eq!(body.len(), 1);
                assert!(matches!(
                    events.last(),
                    Some(AgentEvent::Error(AiError::Disconnected))
                ));
            } else {
                let result = tool_result(&body[1], a, "call-1");
                assert!(result.contains(expected));
                assert_eq!(result.contains("private"), expected != "终端正忙");
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            }
            assert!(
                c.turns()[0]
                    .answer
                    .as_ref()
                    .unwrap()
                    .wire_text()
                    .contains(expected)
            );
        }
    }
}
#[tokio::test]
async fn step_limit_resume_and_sequential_calls_preserve_pending_work() {
    for a in [false, true] {
        let calls: Vec<_> = (0..51)
            .map(|i| (format!("call-{i}"), args(&format!("echo {i}"))))
            .collect();
        let borrowed: Vec<_> = calls
            .iter()
            .map(|(id, args)| (id.as_str(), "run_command", args.clone()))
            .collect();
        let http = SequencedHttpClient::new([
            response(a, &borrowed, "", false),
            response(a, &[], "完成", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new((0..51).map(|_| completed("ok")));
        let mut c = conversation();
        let mut stream = agent::run(
            &client,
            &mut c,
            capabilities(),
            &host,
            Limits {
                max_steps: Some(50),
                ..Default::default()
            },
        )
        .unwrap();
        let resume = loop {
            match stream.next().await.unwrap() {
                AgentEvent::Paused {
                    reason: PauseReason::StepLimit,
                    steps,
                    resume,
                } => {
                    assert_eq!(steps, 50);
                    break resume;
                }
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        };
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(host.calls.lock().unwrap().len(), 50);
        assert_eq!(http.requests().len(), 1);
        let events = agent::resume(&client, &mut c, &host, *resume, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.command.clone())
                .collect::<Vec<_>>(),
            (0..51).map(|i| format!("echo {i}")).collect::<Vec<_>>()
        );
        let b = bodies(&http);
        for i in 0..51 {
            assert!(tool_result(&b[1], a, &format!("call-{i}")).contains("ok"));
        }
    }
}
#[tokio::test]
async fn anthropic_signed_thinking_replays_only_in_current_turn() {
    let http = SequencedHttpClient::new([
        response(
            true,
            &[("call-1", "run_command", args("docker ps"))],
            "检查",
            true,
        ),
        response(true, &[], "完成", false),
        response(true, &[], "下轮", false),
    ]);
    let mut cfg = config(true);
    cfg.reasoning = ReasoningLevel::High;
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    let host = Host::new([completed("web")]);
    let mut c = conversation();
    let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    for event in events {
        match event {
            AgentEvent::TextDelta(t) => assert!(!t.contains("PRIVATE")),
            AgentEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    let b = bodies(&http);
    let blocks = b[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert!(blocks.iter().any(|v| v["type"] == "thinking"
        && v["thinking"] == "PRIVATE-THINKING"
        && v["signature"] == "signature-fixture"));
    assert!(!format!("{:?}", c.turns()[0].answer).contains("PRIVATE"));
    c.push_turn(Turn {
        user: UserTurn::new("继续说明", ContextSnapshot::default()),
        answer: None,
    });
    agent::run(&client, &mut c, capabilities(), &host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let b = bodies(&http);
    let text = b[2].to_string();
    assert!(!text.contains("PRIVATE"));
    assert!(!text.contains("signature-fixture"));
}
#[tokio::test]
async fn drop_cancels_host_future_without_detached_work() {
    struct Pending {
        active: AtomicUsize,
    }
    struct Guard<'a>(&'a AtomicUsize);
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl ToolHost for Pending {
        fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            Box::pin(async move {
                self.active.fetch_add(1, Ordering::SeqCst);
                let _guard = Guard(&self.active);
                futures::future::pending().await
            })
        }
    }
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(
            a,
            &[("call-1", "run_command", args("docker ps"))],
            "",
            false,
        )]);
        let client = RigClient::with_http(&config(a), http).unwrap();
        let host = Pending {
            active: AtomicUsize::new(0),
        };
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        while !matches!(stream.next().await, Some(AgentEvent::StepProposed(_))) {}
        assert!(
            tokio::time::timeout(Duration::from_millis(10), stream.next())
                .await
                .is_err()
        );
        assert_eq!(host.active.load(Ordering::SeqCst), 1);
        drop(stream);
        assert_eq!(host.active.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn no_capabilities_keeps_suggestion_tool_and_fenced_fallback() {
    for a in [false, true] {
        for tool in [false, true] {
            let calls = if tool {
                vec![(
                    "call-1",
                    "propose_command",
                    json!({"command":"docker ps","explanation":"列出容器","risk":"low"}),
                )]
            } else {
                vec![]
            };
            let http = SequencedHttpClient::new([response(
                a,
                &calls,
                "检查：\n```sh\ndocker ps\n```",
                false,
            )]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([]);
            let mut c = conversation();
            let events = agent::run(
                &client,
                &mut c,
                Capabilities::default(),
                &host,
                Limits::default(),
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::Command(_)))
                    .count(),
                1
            );
            assert!(host.calls.lock().unwrap().is_empty());
            let body = bodies(&http);
            assert_eq!(body.len(), 1);
            assert!(body[0].to_string().contains("propose_command"));
            assert!(!body[0].to_string().contains("run_command"));
        }
    }
}
#[tokio::test]
async fn old_steps_replay_as_tool_pairs_without_dropping_history() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "回答", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        c.push_answer(Answer {
            items: vec![
                AnswerItem::Step {
                    automatic: false,
                    command: "docker ps".into(),
                    target: Target::Background,
                    purpose: "检查".into(),
                    class: Class::ReadOnly,
                    status: StepStatus::Completed,
                    exit_code: Some(0),
                    duration_ms: 12,
                    output: "web".into(),
                },
                AnswerItem::Step {
                    automatic: false,
                    command: "rm x".into(),
                    target: Target::Terminal,
                    purpose: "删除".into(),
                    class: Class::Other,
                    status: StepStatus::Denied {
                        reason: Some("保留".into()),
                    },
                    exit_code: None,
                    duration_ms: 0,
                    output: String::new(),
                },
                AnswerItem::Text("分析".into()),
            ],
        });
        c.push_turn(Turn {
            user: UserTurn::new("为什么", ContextSnapshot::default()),
            answer: None,
        });
        assert!(
            serde_json::to_string(&c.prepare(&capabilities()).unwrap().chat_history)
                .unwrap()
                .contains("docker ps")
        );
        agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let b = bodies(&http);
        let messages = b[0]["messages"].to_string();
        assert!(messages.contains("[较早的输出已移出上下文] 命令：docker ps · 退出码 0"));
        assert!(messages.contains("用户拒绝执行：rm x（保留）"));
        assert!(messages.contains(if a { "tool_result" } else { "tool_call_id" }));
        assert!(messages.contains(if a { "tool_use" } else { "tool_calls" }));
        context_tests::assert_pairs(&b[0], a);
    }
}
#[tokio::test]
async fn output_no_longer_stops_and_capabilities_reject_unavailable_targets() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("call-1", "run_command", args("docker ps"))],
                "",
                false,
            ),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("x".repeat(1000).as_str())]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(http.requests().len(), 2);
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("call-1", "run_command", args("docker ps"))],
                "",
                false,
            ),
            response(a, &[], "当前仅可使用终端。", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let caps = Capabilities {
            terminal: true,
            ..Default::default()
        };
        let events = agent::run(&client, &mut c, caps, &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(
            tool_result(&bodies(&http)[1], a, "call-1")
                .contains("执行目标 background 当前不可用，可用：terminal")
        );
        assert!(host.calls.lock().unwrap().is_empty());
        let body = bodies(&http);
        let schema = if a {
            &body[0]["tools"][0]["input_schema"]
        } else {
            &body[0]["tools"][0]["function"]["parameters"]
        };
        assert_eq!(schema["properties"]["target"]["enum"], json!(["terminal"]));
    }
    assert_eq!(
        Target::Terminal.timeout(Some(999)),
        Duration::from_secs(300)
    );
    assert_eq!(Target::Background.timeout(None), Duration::from_secs(30));
    assert_eq!(Target::Background.timeout(Some(0)), Duration::from_secs(1));
}
#[tokio::test]
async fn unsupported_parallel_gateway_retries_once_and_keeps_reasoning() {
    for a in [false, true] {
        let rejected = || {
            MockHttpResponse::error(rig_core::http_client::StatusCode::BAD_REQUEST,json!({"error":{"message":"unsupported parallel_tool_calls","type":"invalid_request_error"}}).to_string())
        };
        let http = SequencedHttpClient::new([rejected(), response(a, &[], "完成", false)]);
        let mut cfg = config(a);
        cfg.reasoning = ReasoningLevel::High;
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let b = bodies(&http);
        assert_eq!(b.len(), 2);
        if a {
            assert!(b[1]["tool_choice"]["disable_parallel_tool_use"].is_null());
            assert!(!b[1]["thinking"].is_null());
        } else {
            assert!(b[1]["parallel_tool_calls"].is_null());
            assert_eq!(b[1]["reasoning_effort"], "high");
        }
        let http = SequencedHttpClient::new(vec![rejected(); if a { 3 } else { 2 }]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
        let b = bodies(&http);
        assert_eq!(b.len(), if a { 3 } else { 2 });
        if a {
            assert_eq!(b[1]["cache_control"], json!({"type":"ephemeral"}));
            assert!(b[2].get("cache_control").is_none());
        }
    }
}

#[tokio::test]
async fn anthropic_text_after_a_call_stays_after_its_step() {
    // Move the response's text block behind the tool block and renumber indices.
    let input = wire(
        true,
        &[("call-1", "run_command", args("docker ps"))],
        "后置文字",
        false,
    );
    let mut values: Vec<Value> = input
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let mut text_blocks: Vec<_> = values.drain(1..4).collect();
    for v in &mut values {
        if v.get("index").is_some() {
            v["index"] = json!(0);
        }
    }
    for v in &mut text_blocks {
        v["index"] = json!(1);
    }
    let n = values.len() - 2;
    values.splice(n..n, text_blocks);
    let sse: String = values
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect();
    let http = SequencedHttpClient::new([
        MockHttpResponse::success_typed(sse, "text/event-stream"),
        response(true, &[], "最终回答", false),
    ]);
    let client = RigClient::with_http(&config(true), http).unwrap();
    let host = Host::new([completed("web")]);
    let mut c = conversation();
    let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(&events[..], [AgentEvent::Context { .. },AgentEvent::StepProposed(_),AgentEvent::StepFinished{..},AgentEvent::TextDelta(t),AgentEvent::TextDelta(_),AgentEvent::Context { .. },AgentEvent::Done(_)] if t=="后置文字")
    );
    assert!(
        matches!(&c.turns()[0].answer.as_ref().unwrap().items[..],[AnswerItem::Step{..},AnswerItem::Text(t)] if t=="后置文字最终回答")
    );
}

#[tokio::test]
async fn tool_output_over_64k_continues_across_resume() {
    for a in [false, true] {
        let calls: Vec<_> = (0..5)
            .map(|i| (format!("call-{i}"), args(&format!("ls /tmp/d{i}"))))
            .collect();
        let borrowed: Vec<_> = calls
            .iter()
            .map(|(id, args)| (id.as_str(), "run_command", args.clone()))
            .collect();
        let http = SequencedHttpClient::new([
            response(a, &borrowed, "", false),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new((0..5).map(|_| {
            ToolOutcome::Completed(shape(
                "a".repeat(8192),
                "z".repeat(8192),
                Some(0),
                Duration::ZERO,
                false,
                false,
            ))
        }));
        let mut c = conversation();
        let mut stream = agent::run(
            &client,
            &mut c,
            capabilities(),
            &host,
            Limits {
                max_steps: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let resume = loop {
            match stream.next().await.unwrap() {
                AgentEvent::Paused {
                    reason: PauseReason::StepLimit,
                    resume,
                    ..
                } => break resume,
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        };
        drop(stream);
        let events = agent::resume(&client, &mut c, &host, *resume, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.calls.lock().unwrap().len(), 5);
        assert_eq!(http.requests().len(), 2);
    }
}

#[tokio::test]
async fn lenient_arguments_on_both_wires() {
    for a in [false, true] {
        for (terminal, background, expected_target) in [
            (false, true, Target::Background),
            (true, true, Target::Background),
            (true, false, Target::Terminal),
        ] {
            for (input, purpose, seconds) in [
                (
                    json!({"command":"docker ps", "purpose":"  检查状态 \n", "timeout_secs":"60", "extra":"ignored"}),
                    "检查状态",
                    60,
                ),
                (json!({"command":"docker ps"}), "（未说明用途）", 30),
                (
                    json!({"command":"docker ps", "purpose":" \n ", "timeout_secs":"invalid"}),
                    "（未说明用途）",
                    30,
                ),
                (
                    json!({"command":"docker ps", "timeout_secs":{"unexpected":true}}),
                    "（未说明用途）",
                    30,
                ),
            ] {
                let http = SequencedHttpClient::new([
                    response(a, &[("call-1", "run_command", input)], "", false),
                    response(a, &[], "完成", false),
                ]);
                let client = RigClient::with_http(&config(a), http.clone()).unwrap();
                let host = Host::new([completed("ok")]);
                let mut c = conversation();
                let events = agent::run(
                    &client,
                    &mut c,
                    Capabilities {
                        terminal,
                        background,
                        ..Default::default()
                    },
                    &host,
                    Limits::default(),
                )
                .unwrap()
                .collect::<Vec<_>>()
                .await;
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                let calls = host.calls.lock().unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].target, expected_target);
                assert_eq!(calls[0].purpose, purpose);
                assert_eq!(calls[0].timeout, Duration::from_secs(seconds));
                let b = bodies(&http);
                let schema = if a {
                    &b[0]["tools"][0]["input_schema"]
                } else {
                    &b[0]["tools"][0]["function"]["parameters"]
                };
                assert!(
                    !schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("target"))
                );
                assert!(
                    schema["properties"]["target"]["enum"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(expected_target.id()))
                );
            }
        }
    }
}

fn malformed_response(a: bool) -> MockHttpResponse {
    let input = wire(a, &[("bad", "run_command", json!({}))], "", false);
    let broken: String = input
        .lines()
        .map(|line| {
            if let Some(data) = line.strip_prefix("data: ").filter(|s| *s != "[DONE]") {
                let mut value: Value = serde_json::from_str(data).unwrap();
                if a && value["type"] == "content_block_delta" {
                    value["delta"]["partial_json"] = json!("{broken");
                } else if !a && value["choices"][0]["delta"]["tool_calls"].is_array() {
                    value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        json!("{broken");
                }
                format!("data: {value}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    MockHttpResponse::success_typed(broken, "text/event-stream")
}

#[tokio::test]
async fn invalid_calls_return_actionable_errors_then_recover_on_both_wires() {
    for a in [false, true] {
        let invalid = [
            (
                response(a, &[("bad", "unknown_tool", args("docker ps"))], "", false),
                "未知工具",
            ),
            (malformed_response(a), "无法解析 JSON"),
            (
                response(a, &[("bad", "run_command", json!({}))], "", false),
                "缺少 command",
            ),
            (
                response(
                    a,
                    &[("bad", "run_command", json!({"command":"  "}))],
                    "",
                    false,
                ),
                "command 不能为空",
            ),
            (
                response(
                    a,
                    &[("bad", "run_command", args(&"x".repeat(16385)))],
                    "",
                    false,
                ),
                "command 超过",
            ),
            (
                response(
                    a,
                    &[(
                        "bad",
                        "run_command",
                        json!({"command":"ls", "target":"terminal"}),
                    )],
                    "",
                    false,
                ),
                "执行目标 terminal 当前不可用，可用：background",
            ),
            (
                response(
                    a,
                    &[("bad", "run_command", json!({"command":3}))],
                    "",
                    false,
                ),
                "必须为字符串",
            ),
        ];
        for (bad, error) in invalid {
            let http = SequencedHttpClient::new([
                bad,
                response(a, &[("good", "run_command", args("docker ps"))], "", false),
                response(a, &[], "完成", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([completed("ok")]);
            let mut c = conversation();
            let events = agent::run(
                &client,
                &mut c,
                Capabilities {
                    background: true,
                    ..Default::default()
                },
                &host,
                Limits::default(),
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            assert!(
                matches!(events.last(), Some(AgentEvent::Done(_))),
                "{a}: {error}"
            );
            assert_eq!(host.calls.lock().unwrap().len(), 1);
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::StepProposed(_)))
                    .count(),
                1
            );
            let b = bodies(&http);
            assert_eq!(b.len(), 3);
            if error == "未知工具" {
                assert!(b[1]["messages"].to_string().contains(error));
            } else {
                assert!(tool_result(&b[1], a, "bad").contains(error));
            }
            if a && error != "未知工具" {
                assert!(
                    b[1]["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|m| m["content"].as_array().into_iter().flatten())
                        .any(|b| b["type"] == "tool_result"
                            && b["tool_use_id"] == "bad"
                            && b["is_error"] == true)
                );
            }
            assert!(matches!(
                &c.turns()[0].answer.as_ref().unwrap().items[..],
                [AnswerItem::Step { .. }, AnswerItem::Text(_)]
            ));
        }
    }
}

#[tokio::test]
async fn invalid_streak_counts_steps_survives_resume_and_resets_on_valid_call() {
    for a in [false, true] {
        for reset in [false, true] {
            let mut responses = vec![
                response(a, &[("bad-1", "run_command", json!({}))], "", false),
                response(a, &[("bad-2", "run_command", json!({}))], "", false),
            ];
            if reset {
                responses.push(response(
                    a,
                    &[("good", "run_command", args("ls"))],
                    "",
                    false,
                ));
            }
            responses.push(response(
                a,
                &[("bad-3", "run_command", json!({}))],
                "",
                false,
            ));
            if reset {
                responses.push(response(
                    a,
                    &[("bad-4", "run_command", json!({}))],
                    "",
                    false,
                ));
                responses.push(response(a, &[], "完成", false));
            }
            let http = SequencedHttpClient::new(responses);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new(if reset { vec![completed("ok")] } else { vec![] });
            let mut c = conversation();
            let events = agent::run(
                &client,
                &mut c,
                capabilities(),
                &host,
                Limits {
                    max_steps: Some(2),
                    ..Default::default()
                },
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            let resume = match events.into_iter().last().unwrap() {
                AgentEvent::Paused {
                    reason: PauseReason::StepLimit,
                    steps,
                    resume,
                } => {
                    assert_eq!(steps, 2);
                    resume
                }
                _ => panic!("invalid calls must count toward the step limit"),
            };
            assert_eq!(http.requests().len(), 2);
            assert!(host.calls.lock().unwrap().is_empty());
            assert!(c.turns()[0].answer.as_ref().unwrap().items.is_empty());
            let events = agent::resume(&client, &mut c, &host, *resume, Limits::default())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            if reset {
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                assert_eq!(host.calls.lock().unwrap().len(), 1);
                assert_eq!(http.requests().len(), 6);
            } else {
                assert!(matches!(
                    events.last(),
                    Some(AgentEvent::Paused {
                        reason: PauseReason::InvalidCalls,
                        ..
                    })
                ));
                assert!(host.calls.lock().unwrap().is_empty());
                assert!(c.turns()[0].answer.as_ref().unwrap().items.is_empty());
                assert_eq!(http.requests().len(), 3);
            }
            assert!(tool_result(&bodies(&http)[2], a, "bad-2").contains("缺少 command"));
        }
    }
}

#[tokio::test]
async fn text_without_tool_calls_finishes_on_both_wires() {
    for a in [false, true] {
        for thinking in [false, true] {
            let http = SequencedHttpClient::new([response(a, &[], "无需执行命令。", thinking)]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([]);
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            assert_eq!(
                c.turns()[0].answer.as_ref().unwrap().items,
                [AnswerItem::Text("无需执行命令。".into())]
            );
            assert!(host.calls.lock().unwrap().is_empty());
            assert_eq!(http.requests().len(), 1);
        }
    }
}

#[tokio::test]
async fn queued_invalid_calls_follow_the_same_recovery_and_streak_rules() {
    for a in [false, true] {
        for invalid_count in [1, 3] {
            let mut calls = vec![("bad-1", "run_command", json!({}))];
            if invalid_count == 3 {
                calls.extend([
                    ("bad-2", "run_command", json!({})),
                    ("bad-3", "run_command", json!({})),
                ]);
            }
            calls.push(("good", "run_command", args("ls")));
            let http = SequencedHttpClient::new([
                response(a, &calls, "", false),
                response(a, &[], "完成", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new(if invalid_count == 1 {
                vec![completed("ok")]
            } else {
                vec![]
            });
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            if invalid_count == 1 {
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                assert_eq!(host.calls.lock().unwrap().len(), 1);
                assert!(tool_result(&bodies(&http)[1], a, "bad-1").contains("缺少 command"));
                assert!(tool_result(&bodies(&http)[1], a, "good").contains("ok"));
            } else {
                assert!(matches!(
                    events.last(),
                    Some(AgentEvent::Paused {
                        reason: PauseReason::InvalidCalls,
                        ..
                    })
                ));
                assert!(host.calls.lock().unwrap().is_empty());
                assert_eq!(http.requests().len(), 1);
                assert!(c.turns()[0].answer.as_ref().unwrap().items.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn incomplete_tool_calls_still_fail_without_host_execution() {
    for a in [false, true] {
        let input = wire(a, &[("call-1", "run_command", args("ls"))], "", false);
        let mut truncated = String::new();
        for block in input.split("\n\n") {
            if block.contains("content_block_stop")
                || block.contains("\"finish_reason\":\"tool_calls\"")
            {
                break;
            }
            truncated.push_str(block);
            truncated.push_str("\n\n");
        }
        let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
            truncated,
            "text/event-stream",
        )]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::InvalidResponse))
        ));
        assert!(host.calls.lock().unwrap().is_empty());
        assert_eq!(http.requests().len(), 1);
    }
}

#[tokio::test]
async fn empty_model_responses_still_fail_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::InvalidResponse))
        ));
        assert!(host.calls.lock().unwrap().is_empty());
        assert_eq!(http.requests().len(), 1);
    }
}

#[tokio::test]
async fn approved_edit_and_target_reach_both_wires_events_and_history() {
    struct Edited;
    impl ToolHost for Edited {
        fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            Box::pin(async { completed("edited result") })
        }
        fn resolved_call(&self, call: &ToolCall) -> ToolCall {
            ToolCall {
                command: "pwd".into(),
                target: Target::Terminal,
                class: Class::ReadOnly,
                ..call.clone()
            }
        }
    }
    for anthropic in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                anthropic,
                &[("edit-1", "run_command", args("uname -s"))],
                "",
                false,
            ),
            response(anthropic, &[], "完成", false),
        ]);
        let client = RigClient::with_http(&config(anthropic), http.clone()).unwrap();
        let mut conversation = conversation();
        let mut stream = agent::run(
            &client,
            &mut conversation,
            capabilities(),
            &Edited,
            Limits::default(),
        )
        .unwrap();
        let mut finished = false;
        while let Some(event) = stream.next().await {
            if let AgentEvent::StepFinished { call, .. } = event {
                assert_eq!(call.command, "pwd");
                assert_eq!(call.target, Target::Terminal);
                finished = true;
            }
        }
        drop(stream);
        assert!(finished);
        let result = tool_result(&bodies(&http)[1], anthropic, "edit-1");
        assert!(result.contains("用户批准的实际命令：pwd"));
        assert!(result.contains("执行目标：终端"));
        assert!(
            matches!(&conversation.turns()[0].answer.as_ref().unwrap().items[0], AnswerItem::Step {command, target: Target::Terminal, ..} if command == "pwd")
        );
    }
}

#[test]
fn early_v9_steps_default_to_manual_and_automatic_tag_roundtrips() {
    let old = json!({"Step": {"command":"pwd","target":"background","purpose":"查看目录","class":"ReadOnly","status":"Completed","exit_code":0,"duration_ms":10,"output":"/tmp"}});
    let mut item: AnswerItem = serde_json::from_value(old).unwrap();
    assert!(matches!(
        item,
        AnswerItem::Step {
            automatic: false,
            ..
        }
    ));
    if let AnswerItem::Step { automatic, .. } = &mut item {
        *automatic = true;
    }
    let restored: AnswerItem =
        serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
    assert_eq!(restored, item);
}
/// Regression: rig's `CompletionRequest::message` inserts before the prompt, which once left
/// the question after the tool results; the model then treated it as new and re-ran commands.
#[tokio::test]
async fn continuation_keeps_question_before_tool_results_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("call-1", "run_command", args("docker ps"))],
                "",
                false,
            ),
            response(a, &[], "web 正在运行。", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("web")]);
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error(e) = event {
                panic!("{e}");
            }
        }
        drop(stream);
        let body = &bodies(&http)[1];
        let messages = body["messages"].as_array().unwrap();
        let text = |m: &Value| m.to_string();
        let question = messages
            .iter()
            .position(|m| text(m).contains("检查容器"))
            .expect("question present");
        let result = messages
            .iter()
            .position(|m| {
                m["role"] == "tool"
                    || m["content"]
                        .as_array()
                        .is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"))
            })
            .expect("tool result present");
        assert!(question < result, "question must precede the tool result");
        assert_eq!(result, messages.len() - 1, "tool result must be last");
        assert!(
            messages[question + 1..result]
                .iter()
                .any(|m| m["role"] == "assistant"),
            "the assistant tool call sits between question and result"
        );
    }
}
#[tokio::test]
async fn identical_consecutive_command_is_rerun_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("call-1", "run_command", args("docker ps"))],
                "",
                false,
            ),
            response(
                a,
                &[("call-2", "run_command", args("docker  ps"))],
                "",
                false,
            ),
            response(a, &[], "完成。", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("web"), completed("web again")]);
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error(e) = event {
                panic!("{e}");
            }
        }
        drop(stream);
        assert_eq!(host.calls.lock().unwrap().len(), 2);
        let bodies = bodies(&http);
        assert_eq!(bodies.len(), 3);
        assert!(tool_result(&bodies[2], a, "call-2").contains("web again"));
    }
}
#[test]
fn explicit_target_words_narrow_the_offered_targets() {
    use shellmate_ai::agent::{Target, explicit_target};
    for (q, want) in [
        ("你直接在当前终端运行命令看看", Some(Target::Terminal)),
        ("在终端里执行 ls -la", Some(Target::Terminal)),
        ("帮我在这个终端跑一下 top", Some(Target::Terminal)),
        ("后台跑一下 df -h", Some(Target::Background)),
        ("单独执行，不要在终端里", Some(Target::Background)),
        ("别在终端 跑", Some(Target::Background)),
        ("看看 docker 运行了哪些服务", None),
    ] {
        assert_eq!(explicit_target(q), want, "{q}");
    }
    let both = capabilities();
    let t = both.clone().for_question("在当前终端运行");
    assert!(t.terminal && !t.background);
    let b = both.clone().for_question("后台执行");
    assert!(!b.terminal && b.background);
    assert_eq!(both.clone().for_question("看看磁盘"), both);
    let only_bg = Capabilities {
        terminal: false,
        ..both
    };
    // Asking for an unavailable target leaves what exists.
    assert!(only_bg.clone().for_question("在终端里执行").background);
}

fn fast_limits() -> Limits {
    Limits {
        backoff: |_| Duration::ZERO,
        ..Default::default()
    }
}
fn http_error(status: u16, message: &str) -> MockHttpResponse {
    MockHttpResponse::error(
        rig_core::http_client::StatusCode::from_u16(status).unwrap(),
        json!({"error":{"message":message,"type":"fixture_error"}}).to_string(),
    )
}
#[tokio::test]
async fn unlimited_runs_120_sequential_calls_and_explicit_50_resumes() {
    for a in [false, true] {
        for limit in [None, Some(50)] {
            let mut responses: Vec<_> = (0..120)
                .map(|i| {
                    response(
                        a,
                        &[(&format!("call-{i}"), "run_command", args("pwd"))],
                        "",
                        false,
                    )
                })
                .collect();
            responses.push(response(a, &[], "done", false));
            let http = SequencedHttpClient::new(responses);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new((0..120).map(|_| completed("ok")));
            let mut c = conversation();
            let events = agent::run(
                &client,
                &mut c,
                capabilities(),
                &host,
                Limits {
                    max_steps: limit,
                    ..Default::default()
                },
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            let last = events.into_iter().last().unwrap();
            if limit.is_some() {
                let AgentEvent::Paused {
                    reason: PauseReason::StepLimit,
                    steps: 50,
                    resume,
                } = last
                else {
                    panic!("expected 50-step pause")
                };
                assert_eq!(host.calls.lock().unwrap().len(), 50);
                let events = agent::resume(&client, &mut c, &host, *resume, Limits::default())
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await;
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            } else {
                assert!(matches!(last, AgentEvent::Done(_)));
            }
            assert_eq!(host.calls.lock().unwrap().len(), 120);
            assert_eq!(http.requests().len(), 121);
        }
    }
}
#[tokio::test]
async fn rate_limit_retries_and_ten_outages_pause_then_resume_the_same_request() {
    for a in [false, true] {
        for (status, failures) in [(429, 1), (503, 10)] {
            let mut responses = vec![http_error(status, "unavailable"); failures];
            responses.push(response(a, &[], "recovered", false));
            let http = SequencedHttpClient::new(responses);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([]);
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
                    .count(),
                failures.min(9)
            );
            for (i, event) in events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
                .enumerate()
            {
                assert!(
                    matches!(event, AgentEvent::Retrying { attempt, max: 10, delay, .. } if *attempt == i as u32 + 1 && delay.is_zero())
                );
            }
            if failures == 10 {
                let AgentEvent::Paused {
                    reason: PauseReason::Error(AiError::Unavailable),
                    resume,
                    ..
                } = events.into_iter().last().unwrap()
                else {
                    panic!("expected outage pause")
                };
                let events = agent::resume(&client, &mut c, &host, *resume, fast_limits())
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await;
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            } else {
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            }
            let bodies = bodies(&http);
            assert_eq!(bodies.len(), failures + 1);
            assert!(bodies.iter().all(|b| b == &bodies[0]));
        }
    }
}
#[tokio::test]
async fn authentication_is_terminal_without_retry() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([http_error(401, "bad key")]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            &events[..],
            [AgentEvent::Error(AiError::Authentication)]
        ));
        assert_eq!(http.requests().len(), 1);
    }
}
#[tokio::test]
async fn transient_after_text_pauses_without_retry_and_preserves_partial_answer() {
    use rig_core::{
        http_client::{Bytes, Error},
        test_utils::SequencedStreamingHttpClient,
    };
    for a in [false, true] {
        let full = wire(a, &[], "partial", false);
        let marker = if a {
            "event: content_block_stop"
        } else {
            "data: {\"choices\":[{\"delta\":{},"
        };
        let prefix = full.split(marker).next().unwrap().to_owned();
        let client = RigClient::with_http(
            &config(a),
            SequencedStreamingHttpClient::new(vec![
                Ok(Bytes::from(prefix)),
                Err(Error::Instance(Box::new(std::io::Error::other(
                    "fixture disconnect",
                )))),
            ]),
        )
        .unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Retrying { .. }))
        );
        let AgentEvent::Paused {
            reason: PauseReason::Error(AiError::Network),
            resume,
            ..
        } = events.into_iter().last().unwrap()
        else {
            panic!("expected network pause")
        };
        assert_eq!(c.turns()[0].answer.as_ref().unwrap().text(), "partial");
        let http = SequencedHttpClient::new([response(a, &[], "recovered", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let events = agent::resume(&client, &mut c, &host, *resume, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(!bodies(&http)[0]["messages"].to_string().contains("partial"));
        assert_eq!(
            c.turns()[0].answer.as_ref().unwrap().text(),
            "partialrecovered"
        );
    }
}
#[tokio::test]
async fn invalid_call_pause_resumes_pending_work() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    ("b1", "run_command", json!({})),
                    ("b2", "run_command", json!({})),
                    ("b3", "run_command", json!({})),
                    ("good", "run_command", args("pwd")),
                ],
                "",
                false,
            ),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("ok")]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let AgentEvent::Paused {
            reason: PauseReason::InvalidCalls,
            steps: 3,
            resume,
        } = events.into_iter().last().unwrap()
        else {
            panic!("expected invalid-call pause")
        };
        assert!(host.calls.lock().unwrap().is_empty());
        let events = agent::resume(&client, &mut c, &host, *resume, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        assert!(tool_result(&bodies(&http)[1], a, "b3").contains("缺少 command"));
    }
}
#[tokio::test(start_paused = true)]
async fn retry_after_is_capped_and_dropping_stream_cancels_wait() {
    use rig_core::http_client::{HeaderMap, HeaderValue, StatusCode};
    for seconds in [7, 90] {
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&seconds.to_string()).unwrap(),
        );
        let http = SequencedHttpClient::new([
            MockHttpResponse::error_with_headers(StatusCode::TOO_MANY_REQUESTS, "busy", headers),
            response(false, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(false), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        assert!(
            matches!(stream.next().await, Some(AgentEvent::Retrying { delay, .. }) if delay == Duration::from_secs(seconds.min(60)))
        );
        assert!(futures::poll!(stream.next()).is_pending());
        drop(stream);
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(http.requests().len(), 1);
    }
    assert_eq!(
        (1..=10)
            .map(agent::retry_backoff)
            .map(|d| d.as_secs())
            .collect::<Vec<_>>(),
        [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]
    );
}

/// A length stop with a tool whose JSON never finished. Anthropic's other blocks are closed.
fn length_response(a: bool, rich: bool) -> MockHttpResponse {
    let input = wire(
        a,
        &[("cut", "run_command", args("must never execute"))],
        if rich { "kept text" } else { "" },
        rich && a,
    );
    let body: String = input
        .split_inclusive('\n')
        .filter_map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return Some(line.to_owned());
            };
            let Ok(mut value) = serde_json::from_str::<Value>(data) else {
                return Some(line.to_owned());
            };
            if a {
                if value["type"] == "content_block_stop"
                    && value["index"] == if rich { 2 } else { 0 }
                {
                    return None;
                }
                if value["delta"]["partial_json"].is_string() {
                    value["delta"]["partial_json"] = json!("{\"command\":\"partial");
                }
                if value["type"] == "message_delta" {
                    value["delta"]["stop_reason"] = json!("max_tokens");
                }
            } else {
                if value["choices"][0]["delta"]["tool_calls"].is_array() {
                    value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        json!("{\"command\":\"partial");
                }
                if value["choices"][0]["finish_reason"] == "tool_calls" {
                    value["choices"][0]["finish_reason"] = json!("length");
                }
            }
            Some(format!("data: {value}\n"))
        })
        .collect();
    // Drop dangling SSE event labels as well as their data for the missing stop.
    let body = if a {
        body.replace("event: content_block_stop\n\n", "")
    } else {
        body
    };
    MockHttpResponse::success_typed(body, "text/event-stream")
}
#[tokio::test]
async fn truncated_calls_never_execute_and_continue_with_notice_and_signed_thinking() {
    for a in [false, true] {
        for rich in [false, true] {
            let http = SequencedHttpClient::new([
                length_response(a, rich),
                response(a, &[], "done", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([]);
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            assert!(host.calls.lock().unwrap().is_empty());
            let followup = &bodies(&http)[1];
            let text = followup.to_string();
            assert!(text.contains("上一次工具调用因达到单次输出上限（32000）被截断，未执行。请拆成更小的步骤（例如分多次写入文件）或缩短命令后重试。"), "a={a} rich={rich}: {text}");
            assert!(!text.contains("must never execute"));
            assert!(!text.contains("\"id\":\"cut\""));
            if rich {
                assert!(text.contains("kept text"));
                if a {
                    assert!(text.contains("signature-fixture"));
                    assert!(text.contains("PRIVATE-THINKING"));
                }
            } else {
                assert!(
                    !followup["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["role"] == "assistant")
                );
            }
        }
    }
}
#[tokio::test]
async fn three_truncations_pause_and_text_only_length_stops_normally() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            length_response(a, false),
            length_response(a, false),
            length_response(a, false),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let AgentEvent::Paused {
            reason: PauseReason::InvalidCalls,
            steps: 3,
            resume,
        } = events.into_iter().last().unwrap()
        else {
            panic!("expected invalid pause")
        };
        let events = agent::resume(&client, &mut c, &host, *resume, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let body = wire(a, &[], &"x".repeat(300_000), false)
            .replace("\"end_turn\"", "\"max_tokens\"")
            .replace("\"stop\"", "\"length\"");
        let client = RigClient::with_http(
            &config(a),
            SequencedHttpClient::new([MockHttpResponse::success_typed(body, "text/event-stream")]),
        )
        .unwrap();
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(c.turns()[0].answer.as_ref().unwrap().text().len(), 300_000);
    }
}
#[tokio::test]
async fn agent_option_fallbacks_and_credentials_reach_requests_and_answer_verbatim() {
    const RAW: &str = "PASSWORD=hunter2 sk-123456789abcdef";
    for a in [false, true] {
        let mut responses = vec![
            http_error(400, "unsupported max_tokens"),
            http_error(400, "unsupported max_output_tokens"),
        ];
        if a {
            responses.push(http_error(400, "unsupported cache_control"));
        }
        responses.extend([
            response(a, &[("call", "run_command", args("printf ok"))], RAW, false),
            response(a, &[], RAW, false),
        ]);
        let http = SequencedHttpClient::new(responses);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed(RAW)]);
        let mut c = Conversation::default();
        c.push_user(UserTurn::new(
            RAW,
            ContextSnapshot {
                text: RAW.into(),
                ..Default::default()
            },
        ));
        let events = agent::run(
            &client,
            &mut c,
            Capabilities {
                cwd: Some(RAW.into()),
                ..capabilities()
            },
            &host,
            fast_limits(),
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let bodies = bodies(&http);
        assert_eq!(bodies[0]["max_tokens"], 32000);
        assert_eq!(bodies[1]["max_tokens"], 8192);
        assert!(bodies[2..].iter().all(|b| b["max_tokens"] == 4096));
        assert!(bodies[0].to_string().matches(RAW).count() >= 3);
        assert!(tool_result(bodies.last().unwrap(), a, "call").contains(RAW));
        assert!(
            c.turns()[0]
                .answer
                .as_ref()
                .unwrap()
                .wire_text()
                .matches(RAW)
                .count()
                >= 3
        );
        if a {
            assert_eq!(bodies[0]["cache_control"], json!({"type":"ephemeral"}));
            assert!(bodies[3].get("cache_control").is_none());
        } else {
            assert!(!bodies[0].to_string().contains("cache"));
        }
    }
}

mod support;
#[tokio::test(start_paused = true)]
async fn agent_stream_has_no_total_cap_but_first_and_idle_still_pause() {
    use rig_core::test_utils::SilentStreamingClient;
    for a in [false, true] {
        let input = wire(a, &[], "done", true);
        let chunks = input.split_inclusive("\n\n").map(str::to_owned).collect();
        let client = RigClient::with_http(
            &config(a),
            support::TimedStream {
                chunks,
                gap: Duration::from_secs(100),
            },
        )
        .unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        let start = tokio::time::Instant::now();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(start.elapsed() > Duration::from_secs(195));
        let client = RigClient::with_http(&config(a), SilentStreamingClient).unwrap();
        let mut c = conversation();
        let start = tokio::time::Instant::now();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Paused {
                reason: PauseReason::Error(AiError::Timeout),
                ..
            })
        ));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
                .count(),
            9
        );
        assert!(start.elapsed() >= Duration::from_secs(6000));
        let prefix = wire(a, &[], "partial", false)
            .split(if a {
                "event: content_block_stop"
            } else {
                "data: {\"choices\":[{\"delta\":{},"
            })
            .next()
            .unwrap()
            .to_owned();
        let client = RigClient::with_http(
            &config(a),
            support::TimedStream {
                chunks: vec![prefix],
                gap: Duration::from_secs(1),
            },
        )
        .unwrap();
        let mut c = conversation();
        let mut stream = agent::run(&client, &mut c, capabilities(), &host, fast_limits()).unwrap();
        assert!(matches!(
            stream.next().await,
            Some(AgentEvent::TextDelta(_))
        ));
        let start = tokio::time::Instant::now();
        assert!(matches!(
            stream.next().await,
            Some(AgentEvent::Paused {
                reason: PauseReason::Error(AiError::Timeout),
                ..
            })
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(300));
    }
}

#[tokio::test]
async fn a_finished_call_beside_a_truncated_call_still_runs() {
    for a in [false, true] {
        let input = wire(
            a,
            &[
                ("good", "run_command", args("pwd")),
                ("cut", "run_command", args("never run")),
            ],
            "kept text",
            a,
        );
        let cut_index = if a { 3 } else { 1 };
        let mut blocks = Vec::new();
        for block in input.split("\n\n").filter(|b| !b.is_empty()) {
            let data = block
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let Ok(mut value) = serde_json::from_str::<Value>(data) else {
                blocks.push(format!("{block}\n\n"));
                continue;
            };
            if a {
                if value["type"] == "content_block_stop" && value["index"] == cut_index {
                    continue;
                }
                if value["type"] == "content_block_delta" && value["index"] == cut_index {
                    value["delta"]["partial_json"] = json!("{\"command\":\"cut");
                }
                if value["type"] == "message_delta" {
                    value["delta"]["stop_reason"] = json!("max_tokens");
                }
                blocks.push(format!(
                    "event: {}\ndata: {value}\n\n",
                    value["type"].as_str().unwrap()
                ));
            } else {
                if value["choices"][0]["delta"]["tool_calls"][0]["index"] == cut_index {
                    value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        json!("{\"command\":\"cut");
                }
                if value["choices"][0]["finish_reason"] == "tool_calls" {
                    value["choices"][0]["finish_reason"] = json!("length");
                }
                blocks.push(format!("data: {value}\n\n"));
            }
        }
        let http = SequencedHttpClient::new([
            MockHttpResponse::success_typed(blocks.concat(), "text/event-stream"),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("/tmp")]);
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        assert_eq!(host.calls.lock().unwrap()[0].command, "pwd");
        let body = &bodies(&http)[1];
        assert!(tool_result(body, a, "good").contains("/tmp"));
        assert!(body.to_string().contains("被截断，未执行"));
        assert!(!body.to_string().contains("\"id\":\"cut\""));
        if a {
            assert!(body.to_string().contains("signature-fixture"));
        }
    }
}

#[tokio::test]
async fn anthropic_in_band_provider_errors_pause_after_text_and_retry_before_content() {
    for (code, expected) in [
        ("overloaded_error", AiError::Unavailable),
        ("api_error", AiError::Unavailable),
        ("rate_limit_error", AiError::RateLimited),
    ] {
        for partial in [false, true] {
            let prefix = if partial {
                wire(true, &[], "partial", false)
                    .split("event: content_block_stop")
                    .next()
                    .unwrap()
                    .to_owned()
            } else {
                String::new()
            };
            let error = json!({"type":"error","error":{"type":code,"message":"private provider diagnostic"}});
            let http = SequencedHttpClient::new([
                MockHttpResponse::success_typed(
                    format!("{prefix}event: error\ndata: {error}\n\n"),
                    "text/event-stream",
                ),
                response(true, &[], "recovered", false),
            ]);
            let client = RigClient::with_http(&config(true), http.clone()).unwrap();
            let host = Host::new([]);
            let mut c = conversation();
            let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            if partial {
                assert!(
                    matches!(&events[..], [AgentEvent::TextDelta(text), AgentEvent::Paused { reason: PauseReason::Error(error), .. }] if text == "partial" && *error == expected)
                );
                assert_eq!(http.requests().len(), 1);
                assert_eq!(c.turns()[0].answer.as_ref().unwrap().text(), "partial");
                let AgentEvent::Paused { resume, .. } = events.into_iter().last().unwrap() else {
                    unreachable!()
                };
                let events = agent::resume(&client, &mut c, &host, *resume, fast_limits())
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await;
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                assert_eq!(
                    c.turns()[0].answer.as_ref().unwrap().text(),
                    "partialrecovered"
                );
            } else {
                assert!(
                    matches!(events.first(), Some(AgentEvent::Retrying { attempt: 1, max: 10, error, .. }) if *error == expected)
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
                        .count(),
                    1
                );
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            }
            let bodies = bodies(&http);
            assert_eq!(bodies.len(), 2);
            assert_eq!(
                bodies[0], bodies[1],
                "failed partial response must not enter request history"
            );
            assert!(
                !c.turns()[0]
                    .answer
                    .as_ref()
                    .unwrap()
                    .text()
                    .contains("private provider diagnostic")
            );
            assert!(host.calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn anthropic_finished_refusal_stays_invalid_response() {
    let body = wire(true, &[], "partial", false).replace(
        "\"stop_reason\":\"end_turn\"",
        "\"stop_reason\":\"refusal\",\"stop_details\":{\"explanation\":\"overloaded_error\"}",
    );
    let http =
        SequencedHttpClient::new([MockHttpResponse::success_typed(body, "text/event-stream")]);
    let client = RigClient::with_http(&config(true), http.clone()).unwrap();
    let host = Host::new([]);
    let mut c = conversation();
    let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        &events[..],
        [
            AgentEvent::TextDelta(_),
            AgentEvent::Error(AiError::InvalidResponse)
        ]
    ));
    assert_eq!(http.requests().len(), 1);
}

#[path = "agent/context.rs"]
mod context_tests;
