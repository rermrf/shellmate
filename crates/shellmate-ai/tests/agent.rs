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
        timeouts: Timeouts::default(),
    }
}
fn conversation() -> Conversation {
    Conversation {
        turns: vec![Turn {
            user: UserTurn::redacted("检查容器", ContextSnapshot::default()),
            answer: None,
        }],
    }
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
        assert!(!result.contains("hidden"));
        if a {
            assert_eq!(bodies[0]["tool_choice"]["disable_parallel_tool_use"], true);
        } else {
            assert_eq!(bodies[0]["parallel_tool_calls"], false);
        }
        let answer = c.turns[0].answer.as_ref().unwrap();
        assert!(matches!(
            &answer.items[..],
            [
                AnswerItem::Text(_),
                AnswerItem::Step { .. },
                AnswerItem::Text(_)
            ]
        ));
        assert!(!answer.wire_text().contains("hidden"));
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
                assert!(!result.contains("private"));
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            }
            assert!(
                c.turns[0]
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
        let calls: Vec<_> = (0..16)
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
        let host = Host::new((0..16).map(|_| completed("ok")));
        let mut c = conversation();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        let resume = loop {
            match stream.next().await.unwrap() {
                AgentEvent::StepLimit { steps, resume } => {
                    assert_eq!(steps, 15);
                    break resume;
                }
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        };
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(host.calls.lock().unwrap().len(), 15);
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
            (0..16).map(|i| format!("echo {i}")).collect::<Vec<_>>()
        );
        let b = bodies(&http);
        for i in 0..16 {
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
    assert!(!format!("{:?}", c.turns[0].answer).contains("PRIVATE"));
    c.turns.push(Turn {
        user: UserTurn::redacted("继续说明", ContextSnapshot::default()),
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
async fn old_steps_are_plain_text_and_count_toward_history_budget() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "回答", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut c = conversation();
        c.turns[0].answer = Some(Answer {
            items: vec![
                AnswerItem::Step {
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
        c.turns.push(Turn {
            user: UserTurn::redacted("为什么", ContextSnapshot::default()),
            answer: None,
        });
        assert_eq!(c.prepare(1).unwrap().dropped_turns, 1);
        agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let b = bodies(&http);
        let messages = b[0]["messages"].to_string();
        assert!(messages.contains("已执行（后台）：docker ps → 退出码 0"));
        assert!(messages.contains("用户拒绝执行：rm x（保留）"));
        assert!(!messages.contains("tool_result"));
        assert!(!messages.contains("tool_calls"));
        assert!(!messages.contains("tool_use"));
    }
}
#[tokio::test]
async fn output_budget_stops_and_capabilities_reject_unavailable_targets() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(
            a,
            &[("call-1", "run_command", args("docker ps"))],
            "",
            false,
        )]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("x".repeat(1000).as_str())]);
        let mut c = conversation();
        let events = agent::run(
            &client,
            &mut c,
            capabilities(),
            &host,
            Limits {
                output_chars: 100,
                ..Limits::default()
            },
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::ToolOutputLimit))
        ));
        assert_eq!(http.requests().len(), 1);
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
        let http = SequencedHttpClient::new([rejected(), rejected()]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
        assert_eq!(http.requests().len(), 2);
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
        matches!(&events[..], [AgentEvent::StepProposed(_),AgentEvent::StepFinished{..},AgentEvent::TextDelta(t),AgentEvent::TextDelta(_),AgentEvent::Done(_)] if t=="后置文字")
    );
    assert!(
        matches!(&c.turns[0].answer.as_ref().unwrap().items[..],[AnswerItem::Step{..},AnswerItem::Text(t)] if t=="后置文字最终回答")
    );
}

#[tokio::test]
async fn default_output_budget_is_cumulative_across_resume() {
    for a in [false, true] {
        let calls: Vec<_> = (0..5)
            .map(|i| (format!("call-{i}"), args("docker ps")))
            .collect();
        let borrowed: Vec<_> = calls
            .iter()
            .map(|(id, args)| (id.as_str(), "run_command", args.clone()))
            .collect();
        let http = SequencedHttpClient::new([response(a, &borrowed, "", false)]);
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
                steps: 2,
                ..Default::default()
            },
        )
        .unwrap();
        let resume = loop {
            match stream.next().await.unwrap() {
                AgentEvent::StepLimit { resume, .. } => break resume,
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        };
        drop(stream);
        let events = agent::resume(&client, &mut c, &host, *resume, Limits::default())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::ToolOutputLimit))
        ));
        assert_eq!(host.calls.lock().unwrap().len(), 4);
        assert_eq!(http.requests().len(), 1);
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
            assert!(tool_result(&b[1], a, "bad").contains(error));
            if a {
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
                &c.turns[0].answer.as_ref().unwrap().items[..],
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
                    steps: 2,
                    ..Default::default()
                },
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            let resume = match events.into_iter().last().unwrap() {
                AgentEvent::StepLimit { steps, resume } => {
                    assert_eq!(steps, 2);
                    resume
                }
                _ => panic!("invalid calls must count toward the step limit"),
            };
            assert_eq!(http.requests().len(), 2);
            assert!(host.calls.lock().unwrap().is_empty());
            assert!(c.turns[0].answer.as_ref().unwrap().items.is_empty());
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
                    Some(AgentEvent::Error(AiError::InvalidResponse))
                ));
                assert!(host.calls.lock().unwrap().is_empty());
                assert!(c.turns[0].answer.as_ref().unwrap().items.is_empty());
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
                c.turns[0].answer.as_ref().unwrap().items,
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
                    Some(AgentEvent::Error(AiError::InvalidResponse))
                ));
                assert!(host.calls.lock().unwrap().is_empty());
                assert_eq!(http.requests().len(), 1);
                assert!(c.turns[0].answer.as_ref().unwrap().items.is_empty());
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
