use futures::StreamExt;
use rig_core::{
    http_client::{DynHttpClient, StatusCode},
    test_utils::{
        MockHttpResponse, PacedStreamingClient, SequencedHttpClient, SilentStreamingClient,
    },
};
use serde_json::{Value, json};
use shellmate_ai::*;
use std::time::Duration;
fn config(anthropic: bool) -> Config {
    Config {
        provider: if anthropic {
            Provider::Anthropic {
                base_url: "https://api.anthropic.com".into(),
            }
        } else {
            Provider::OpenAiCompatible {
                base_url: "https://custom.example/api/v1".into(),
            }
        },
        model: if anthropic {
            "claude-sonnet-4-6"
        } else {
            "custom-model"
        }
        .into(),
        api_key: ApiKey::new("fixture-api-key"),
        reasoning: ReasoningLevel::Default,
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Timeouts::default(),
    }
}
fn conversation() -> Conversation {
    Conversation::from_turns(vec![Turn {
        user: UserTurn::new(
            "找出占用 3000 端口的进程",
            ContextSnapshot {
                label: "prod-web-01".into(),
                protocol: "SSH".into(),
                text: "TOKEN=secret-context".into(),
            },
        ),
        answer: None,
    }])
}
fn openai(text: &str, tool: bool) -> String {
    let mut events = vec![
        json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"custom-model","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
    ];
    if tool {
        for (i, arg) in [
            "{\"command\":\"lsof -i :3000\",",
            "\"explanation\":\"查看监听进程\",\"risk\":\"medium\"}",
        ]
        .iter()
        .enumerate()
        {
            let function = if i == 0 {
                json!({"name":"propose_command","arguments":arg})
            } else {
                json!({"arguments":arg})
            };
            let mut call = json!({"index":0,"function":function});
            if i == 0 {
                call["id"] = json!("call-1");
                call["type"] = json!("function");
            }
            events.push(json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"custom-model","choices":[{"index":0,"delta":{"tool_calls":[call]},"finish_reason":null}]}));
        }
    }
    events.push(json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"custom-model","choices":[{"index":0,"delta":{},"finish_reason":if tool{"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}}));
    events
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}
fn anthropic(text: &str, tool: bool) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    if tool {
        events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"propose_command","input":{}}}));
        for fragment in [
            "{\"command\":\"lsof -i :3000\",",
            "\"explanation\":\"查看监听进程\",\"risk\":\"medium\"}",
        ] {
            events.push(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":fragment}}));
        }
        events.push(json!({"type":"content_block_stop","index":1}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":if tool{"tool_use"}else{"end_turn"},"stop_sequence":null},"usage":{"output_tokens":8}}));
    events.push(json!({"type":"message_stop"}));
    events
        .into_iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}
fn sse(a: bool, text: &str, tool: bool) -> String {
    if a {
        anthropic(text, tool)
    } else {
        openai(text, tool)
    }
}
#[tokio::test]
async fn both_wires_stream_tools_and_use_exact_request_shape() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
            sse(a, "先检查监听进程。", true),
            "text/event-stream",
        )]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut log = conversation();
        let mut stream = client.start(&mut log).unwrap();
        assert!(http.requests().is_empty(), "lazy until polled");
        let mut text = String::new();
        let mut cards = vec![];
        let mut done = false;
        while let Some(e) = stream.next().await {
            match e {
                Event::Context { .. } | Event::Compacting { .. } | Event::Compacted { .. } => {}
                Event::Reasoning { .. } | Event::Thinking => panic!("unexpected reasoning"),
                Event::TextDelta(d) => text.push_str(&d),
                Event::Command(c) => cards.push(c),
                Event::Done(u) => {
                    done = true;
                    assert_eq!(u.unwrap().input_tokens, Some(12));
                }
                Event::Error(e) => panic!("safe error: {e}"),
            }
        }
        assert_eq!(text, "先检查监听进程。");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].command, "lsof -i :3000");
        assert!(done);
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        let auth = &requests[0].headers[if a { "x-api-key" } else { "authorization" }];
        assert_eq!(
            auth.to_str().unwrap(),
            if a {
                "fixture-api-key"
            } else {
                "Bearer fixture-api-key"
            }
        );
        assert!(auth.is_sensitive());
        assert_eq!(
            requests[0].uri,
            if a {
                "https://api.anthropic.com/v1/messages"
            } else {
                "https://custom.example/api/v1/chat/completions"
            }
        );
        let b: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(b["stream"], true);
        assert_eq!(b["model"], config(a).model);
        let serialized = b.to_string();
        assert!(serialized.contains("secret-context"));
        assert!(serialized.contains("Shellmate"));
        assert!(serialized.contains("prod-web-01"));
        assert!(serialized.contains("propose_command"));
        assert!(serialized.contains("explanation"));
        assert_eq!(
            b["messages"].as_array().unwrap().last().unwrap()["role"],
            "user"
        );
        if a {
            assert!(b.get("system").is_some());
            assert_eq!(b["tools"][0]["name"], "propose_command");
        } else {
            assert_eq!(b["messages"][0]["role"], "system");
            assert_eq!(b["tools"][0]["function"]["name"], "propose_command");
        }
    }
}
#[tokio::test]
async fn fallback_code_blocks_and_multiturn_roles() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
            sse(a, "检查：\n```sh\nlsof -i :3000\n```", false),
            "text/event-stream",
        )]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut c = conversation();
        c.push_answer(Answer {
            items: vec![
                AnswerItem::Text("先查看服务".into()),
                AnswerItem::Command(CommandCard {
                    command: "ps aux".into(),
                    explanation: "进程".into(),
                    risk: Risk::Low,
                }),
            ],
        });
        c.push_turn(Turn {
            user: UserTurn::new("把刚才的命令改成只看 nginx", ContextSnapshot::default()),
            answer: None,
        });
        let events = client.start(&mut c).unwrap().collect::<Vec<_>>().await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e,Event::Command(c)if c.command=="lsof -i :3000"))
        );
        let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        let roles = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["role"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            if a {
                vec!["user", "assistant", "user"]
            } else {
                vec!["system", "user", "assistant", "user"]
            }
        );
    }
}
#[tokio::test]
async fn errors_are_chinese_and_never_echo_payloads() {
    for a in [false, true] {
        for (status, code, expected) in [
            (401, "invalid_api_key", AiError::Authentication),
            (403, "forbidden", AiError::Authentication),
            (404, "model_not_found", AiError::ModelNotFound),
            (429, "rate_limit", AiError::RateLimited),
            (429, "insufficient_quota", AiError::Quota),
            (402, "balance", AiError::Quota),
            (400, "context_length_exceeded", AiError::ContextTooLong),
            (500, "server_error", AiError::Unavailable),
        ] {
            let rejected = MockHttpResponse::error(StatusCode::from_u16(status).unwrap(), json!({"error":{"code":code,"type":code,"message":"fixture-api-key SECRET REQUEST AND ANSWER"}}).to_string());
            let http = SequencedHttpClient::new(vec![rejected; if status == 400 { 2 } else { 1 }]);
            let client = RigClient::with_http(&config(a), http).unwrap();
            let e = client
                .start(&mut conversation())
                .unwrap()
                .filter(|e| futures::future::ready(!matches!(e, Event::Compacting { .. })))
                .next()
                .await
                .unwrap();
            assert!(
                matches!(e,Event::Error(found)if found==expected),
                "{a} {status}"
            );
            assert!(!expected.to_string().contains("SECRET"));
        }
    }
}
#[tokio::test]
async fn timeout_and_cancellation_without_sockets() {
    for a in [false, true] {
        let mut cfg = config(a);
        cfg.timeouts.first = Duration::from_millis(5);
        let client = RigClient::with_http(&cfg, SilentStreamingClient).unwrap();
        assert!(matches!(
            client.start(&mut conversation()).unwrap().next().await,
            Some(Event::Error(AiError::Timeout))
        ));
        let http = SequencedHttpClient::new([]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        drop(client.start(&mut conversation()).unwrap());
        assert!(http.requests().is_empty());
        let client = RigClient::with_http(
            &cfg,
            PacedStreamingClient {
                chunks: vec![],
                gap: Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut log = conversation();
        let mut stream = client.start(&mut log).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), stream.next())
                .await
                .is_err()
        );
        drop(stream);
    }
}
#[test]
fn not_configured_never_dispatches_and_key_debug_is_redacted() {
    let http = SequencedHttpClient::new([]);
    let mut cfg = config(false);
    cfg.api_key = ApiKey::new("");
    assert!(matches!(
        RigClient::with_http(&cfg, DynHttpClient::new(http.clone())),
        Err(AiError::NotConfigured)
    ));
    assert!(http.requests().is_empty());
    assert!(!format!("{:?}", config(false)).contains("fixture-api-key"));
}

#[tokio::test]
async fn stream_idle_timeout_and_network_failure_are_safe() {
    use rig_core::http_client::Error;
    use rig_core::test_utils::SequencedStreamingHttpClient;
    for a in [false, true] {
        let mut cfg = config(a);
        cfg.timeouts.idle = Duration::from_millis(5);
        let chunk: &'static str = if a {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"test\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n"
        } else {
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n"
        };
        let client = RigClient::with_http(
            &cfg,
            PacedStreamingClient {
                chunks: vec![chunk],
                gap: Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut log = conversation();
        let mut stream = client.start(&mut log).unwrap();
        assert!(matches!(stream.next().await, Some(Event::TextDelta(_))));
        assert!(matches!(
            stream.next().await,
            Some(Event::Error(AiError::Timeout))
        ));
        assert!(stream.next().await.is_none());
        let client = RigClient::with_http(
            &cfg,
            SequencedStreamingHttpClient::new(vec![Err(Error::Instance(Box::new(
                std::io::Error::other("DNS fixture secret that must never reach UI"),
            )))]),
        )
        .unwrap();
        let event = client.start(&mut conversation()).unwrap().next().await;
        assert!(matches!(event, Some(Event::Error(AiError::Network))));
    }
}

#[tokio::test]
async fn dropping_started_stream_releases_transport_body() {
    use rig_core::http_client::{
        self, Bytes, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
    };
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };
    #[derive(Clone)]
    struct Tracking {
        dropped: Arc<AtomicBool>,
    }
    struct Body {
        dropped: Arc<AtomicBool>,
    }
    impl futures::Stream for Body {
        type Item = http_client::Result<Bytes>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for Body {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl HttpClientExt for Tracking {
        fn send<T: Into<Bytes> + Send, U: From<Bytes> + Send + 'static>(
            &self,
            _: Request<T>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
        {
            std::future::ready(Err(http_client::Error::StreamEnded))
        }
        fn send_multipart<U: From<Bytes> + Send + 'static>(
            &self,
            _: Request<MultipartForm>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
        {
            std::future::ready(Err(http_client::Error::StreamEnded))
        }
        fn send_streaming<T: Into<Bytes> + Send>(
            &self,
            request: Request<T>,
        ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send {
            let dropped = self.dropped.clone();
            async move {
                let _ = request;
                let body: http_client::BoxedStream = Box::pin(Body { dropped });
                Ok(Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap())
            }
        }
    }
    for a in [false, true] {
        let dropped = Arc::new(AtomicBool::new(false));
        let client = RigClient::with_http(
            &config(a),
            Tracking {
                dropped: dropped.clone(),
            },
        )
        .unwrap();
        let mut log = conversation();
        let mut stream = client.start(&mut log).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), stream.next())
                .await
                .is_err()
        );
        assert!(!dropped.load(Ordering::SeqCst));
        drop(stream);
        assert!(dropped.load(Ordering::SeqCst));
    }
}

async fn request_body(cfg: &Config, connection_test: bool) -> Value {
    let a = matches!(cfg.provider, Provider::Anthropic { .. });
    let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
        sse(a, "连接成功", false),
        "text/event-stream",
    )]);
    let client = RigClient::with_http(cfg, http.clone()).unwrap();
    let mut log = conversation();
    let mut stream = if connection_test {
        client.test_connection().unwrap()
    } else {
        client.start(&mut log).unwrap()
    };
    let mut done = false;
    while let Some(event) = stream.next().await {
        match event {
            Event::Error(e) => panic!("safe error: {e}"),
            Event::Done(_) => done = true,
            _ => {}
        }
    }
    assert!(done);
    serde_json::from_slice(&http.requests()[0].body).unwrap()
}

#[tokio::test]
async fn openai_reasoning_and_connection_test_use_independent_output_caps() {
    for (level, effort, cap) in [
        (ReasoningLevel::Default, None, 32000),
        (ReasoningLevel::Off, Some("none"), 32000),
        (ReasoningLevel::Minimal, Some("minimal"), 32000),
        (ReasoningLevel::Low, Some("low"), 32000),
        (ReasoningLevel::Medium, Some("medium"), 32000),
        (ReasoningLevel::High, Some("high"), 32000),
        (ReasoningLevel::XHigh, Some("xhigh"), 32000),
    ] {
        let mut cfg = config(false);
        cfg.reasoning = level;
        for connection_test in [false, true] {
            let body = request_body(&cfg, connection_test).await;
            assert_eq!(body["max_tokens"], cap);
            assert_eq!(body.get("reasoning_effort").and_then(Value::as_str), effort);
            if effort.is_none() {
                assert!(body.get("reasoning_effort").is_none());
            }
            if connection_test {
                assert_eq!(body["messages"][0]["content"], "请仅回答：连接成功");
            }
        }
    }
}

#[tokio::test]
async fn anthropic_custom_base_normalizes_endpoint_and_keeps_sensitive_auth() {
    for suffix in ["", "/", "/v1", "/messages", "/v1/messages", "/v1/messages/"] {
        let mut cfg = config(true);
        cfg.provider = Provider::Anthropic {
            base_url: format!("https://proxy.example/anthropic{suffix}"),
        };
        let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
            anthropic("ok", false),
            "text/event-stream",
        )]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        let requests = http.requests();
        assert_eq!(
            requests[0].uri,
            "https://proxy.example/anthropic/v1/messages"
        );
        let auth = &requests[0].headers["x-api-key"];
        // A boolean assertion never prints even the fixture credential on failure.
        assert!(
            auth.to_str()
                .is_ok_and(|value| value == cfg.api_key.expose())
        );
        assert!(auth.is_sensitive());
        assert!(!requests[0].headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn anthropic_reasoning_uses_catalog_effort_budget_and_off_shapes() {
    for connection_test in [false, true] {
        let mut cfg = config(true);
        let body = request_body(&cfg, connection_test).await;
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert_eq!(body["max_tokens"], 32000);
        for model in ["claude-sonnet-4-6", "unlisted-proxy-model"] {
            cfg.model = model.into();
            cfg.reasoning = ReasoningLevel::High;
            let body = request_body(&cfg, connection_test).await;
            assert_eq!(body["thinking"], json!({"type":"adaptive"}));
            assert_eq!(body["output_config"]["effort"], "high");
            assert_eq!(body["max_tokens"], 32000);
        }
        cfg.model = "claude-opus-4-6".into();
        cfg.reasoning = ReasoningLevel::Max;
        let body = request_body(&cfg, connection_test).await;
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["max_tokens"], 32000);
        // Dated ids must resolve with the same rule as rig's Messages wire.
        cfg.model = "claude-haiku-4-5-20251001".into();
        for (level, budget, cap) in [
            (ReasoningLevel::Low, 2048, 32000),
            (ReasoningLevel::Medium, 6144, 32000),
            (ReasoningLevel::High, 14336, 32000),
            (ReasoningLevel::Max, 30000, 34096),
        ] {
            cfg.reasoning = level;
            let body = request_body(&cfg, connection_test).await;
            assert_eq!(
                body["thinking"],
                json!({"type":"enabled", "budget_tokens":budget})
            );
            assert!(body.get("output_config").is_none());
            assert_eq!(body["max_tokens"], cap);
            let facts = rig_core::catalog::ModelFacts::default();
            let range = facts
                .for_model("anthropic", &cfg.model)
                .unwrap()
                .reasoning
                .budget()
                .unwrap();
            assert!(range.contains(&budget));
            assert!(budget >= 1024 && u64::from(budget) < body["max_tokens"].as_u64().unwrap());
        }
        for model in ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"] {
            cfg.model = model.into();
            cfg.reasoning = ReasoningLevel::Off;
            let body = request_body(&cfg, connection_test).await;
            assert_eq!(body["thinking"], json!({"type":"disabled"}));
            assert_eq!(body["max_tokens"], 32000);
        }
    }
}

#[tokio::test]
async fn unsupported_reasoning_is_safe_for_encoding_and_http_errors() {
    let mut cfg = config(true);
    cfg.model = "claude-opus-4-5".into();
    cfg.reasoning = ReasoningLevel::Max; // Catalog lists low / medium / high only.
    let http = SequencedHttpClient::new([]);
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    let mut log = conversation();
    for result in [client.start(&mut log), client.test_connection()] {
        assert!(matches!(result, Err(AiError::UnsupportedReasoning)));
    }
    assert!(http.requests().is_empty());
    for a in [false, true] {
        for field in [
            "reasoning_effort",
            "thinking",
            "budget_tokens",
            "output_config",
        ] {
            let rejected = MockHttpResponse::error(
                StatusCode::BAD_REQUEST,
                json!({"error":{"type":"invalid_request_error","message":format!("SECRET {field} rejected")}}).to_string(),
            );
            let http = SequencedHttpClient::new(vec![rejected; if a { 2 } else { 1 }]);
            let client = RigClient::with_http(&config(a), http).unwrap();
            let event = client.test_connection().unwrap().next().await;
            assert!(matches!(
                event,
                Some(Event::Error(AiError::UnsupportedReasoning))
            ));
        }
    }
    assert_eq!(
        AiError::UnsupportedReasoning.to_string(),
        "当前模型不支持所选思考等级，请在设置中改为「默认」或其他等级"
    );
}

fn reasoning_sse(a: bool) -> String {
    if a {
        let events = [
            json!({"type":"message_start","message":{"id":"msg_r","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"PRIVATE_REASONING"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"_MORE"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"PRIVATE_SIGNATURE"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"公开回答"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ];
        events
            .into_iter()
            .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
            .collect()
    } else {
        let reasoning = ["PRIVATE_REASONING", "_MORE"].into_iter().map(|text| {
            let v = json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"custom-model","choices":[{"index":0,"delta":{"reasoning_content":text},"finish_reason":null}]});
            format!("data: {v}\n\n")
        }).collect::<String>();
        reasoning + &openai("公开回答", false)
    }
}

#[tokio::test]
async fn reasoning_streams_separately_and_is_saved_without_replaying_as_text() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            MockHttpResponse::success_typed(reasoning_sse(a), "text/event-stream"),
            MockHttpResponse::success_typed(sse(a, "后续回答", false), "text/event-stream"),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut c = conversation();
        let events = client.start(&mut c).unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.first(), Some(Event::Thinking)));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Thinking))
                .count(),
            1
        );
        assert!(matches!(events.last(), Some(Event::Done(_))));
        let mut answer = Answer::default();
        let mut thinking=String::new();
        for event in events {
            match event {
                Event::Reasoning {block,..} => {thinking.push_str(&block.text);assert_eq!(block.kind,ThinkingKind::Process);},
                Event::TextDelta(text) => answer.push_text(&text),
                Event::Command(card) => answer.items.push(AnswerItem::Command(card)),
                Event::Error(e) => panic!("safe error: {e}"),
                _ => {}
            }
        }
        assert_eq!(thinking,"PRIVATE_REASONING_MORE");
        let saved=serde_json::to_string(c.entries()).unwrap();
        assert!(!saved.contains("PRIVATE_SIGNATURE"));
        let restored=Conversation::from_entries(serde_json::from_str(&saved).unwrap());
        assert!(matches!(&restored.turns()[0].answer.as_ref().unwrap().items[0],AnswerItem::Thinking(b)if b.text==thinking && b.complete));
        assert_eq!(answer.text(), "公开回答");
        assert!(answer.commands().next().is_none());
        assert_eq!(c.turns()[0].answer.as_ref().unwrap().text(), answer.text());
        c.push_turn(Turn {
            user: UserTurn::new("继续", ContextSnapshot::default()),
            answer: None,
        });
        let events = client.start(&mut c).unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        let body = String::from_utf8(http.requests()[1].body.to_vec()).unwrap();
        assert!(!body.contains("PRIVATE_"));
        assert!(!body.contains("thinking"));
        assert!(!body.contains("reasoning_content"));
        assert!(body.contains("公开回答"));
    }
}

#[tokio::test]
async fn reasoning_activity_resets_idle_timeout() {
    // Each reasoning fragment arrives well within idle (60 ms of 300 ms, so a busy CI runner
    // has room), but together they keep the answer back for longer than idle.
    // PacedStreamingClient intentionally stays open after its last chunk.
    const REASONING: &str = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"custom-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"private\"},\"finish_reason\":null}]}\n\n";
    const ANSWER: &str = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"custom-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n";
    let gap = Duration::from_millis(60);
    let mut cfg = config(false);
    cfg.timeouts.idle = Duration::from_millis(300);
    let mut chunks = vec![REASONING; 8];
    chunks.push(ANSWER);
    assert!(gap * (chunks.len() as u32 - 1) > cfg.timeouts.idle);
    let client = RigClient::with_http(&cfg, PacedStreamingClient { chunks, gap }).unwrap();
    let mut log = conversation();
    let mut stream = client.start(&mut log).unwrap();
    assert!(matches!(stream.next().await, Some(Event::Thinking)));
    let mut thinking=String::new();
    loop {match stream.next().await {
        Some(Event::Reasoning{block,..})=>thinking.push_str(&block.text),
        Some(Event::TextDelta(text))=>{assert_eq!(text,"answer");break;},
        _=>panic!("reasoning activity must keep the stream alive"),
    }}
    assert_eq!(thinking,"private".repeat(8));
    assert!(matches!(stream.next().await,Some(Event::Error(AiError::Timeout))));
}

#[tokio::test]
async fn output_tokens_resolve_catalog_and_clamp_on_both_protocols() {
    for a in [false, true] {
        let mut cfg = config(a);
        let known = catalog_spec(&cfg.provider, &cfg.model);
        if a {
            assert!(known.is_some());
        } else {
            assert!(known.is_none());
        }
        assert_eq!(
            request_body(&cfg, true).await["max_tokens"],
            known
                .and_then(|s| s.max_output_tokens)
                .unwrap_or(32000)
                .min(32000)
        );
        for (value, expected) in [(0, 1024), (1023, 1024), (12345, 12345), (u32::MAX, 128000)] {
            cfg.max_output_tokens = OutputTokens::Tokens(value);
            let maximum = catalog_spec(&cfg.provider, &cfg.model)
                .and_then(|s| s.max_output_tokens)
                .unwrap_or(u32::MAX);
            assert_eq!(
                request_body(&cfg, true).await["max_tokens"],
                expected.min(maximum)
            );
        }
    }
    let spec = rig_core::catalog::Catalog::builtin()
        .iter()
        .find(|s| {
            s.provider.vendor() != "openai"
                && s.max_output_tokens
                    .is_some_and(|n| (1024..32000).contains(&n))
        })
        .unwrap();
    let mut cfg = config(false);
    cfg.model = spec.id.clone();
    assert_eq!(catalog_spec(&cfg.provider, &cfg.model).unwrap().id, spec.id);
    assert_eq!(
        request_body(&cfg, true).await["max_tokens"],
        spec.max_output_tokens.unwrap()
    );
    cfg.max_output_tokens = OutputTokens::Tokens(128000);
    assert_eq!(
        request_body(&cfg, true).await["max_tokens"],
        spec.max_output_tokens.unwrap()
    );
}
fn rejection(message: &str) -> MockHttpResponse {
    MockHttpResponse::error(
        StatusCode::BAD_REQUEST,
        json!({"error":{"message":message,"type":"invalid_request_error"}}).to_string(),
    )
}
#[tokio::test]
async fn output_fallback_32000_8192_4096_is_scoped_and_remembered() {
    for a in [false, true] {
        for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            let http = SequencedHttpClient::new([
                rejection(&format!("unsupported {field}")),
                rejection(&format!("unsupported {field}")),
                MockHttpResponse::success_typed(sse(a, "ok", false), "text/event-stream"),
                MockHttpResponse::success_typed(sse(a, "again", false), "text/event-stream"),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            for _ in 0..2 {
                let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
                assert!(matches!(events.last(), Some(Event::Done(_))));
            }
            let caps: Vec<_> = http
                .requests()
                .iter()
                .map(|r| {
                    serde_json::from_slice::<Value>(&r.body).unwrap()["max_tokens"]
                        .as_u64()
                        .unwrap()
                })
                .collect();
            assert_eq!(caps, [32000, 8192, 4096, 4096]);
            assert_eq!(client.output_tokens(), 4096);
        }
        for message in [
            "bad request",
            "unsupported other_max_tokens_field",
            "unsupported cache_control_extra",
        ] {
            let http = SequencedHttpClient::new(vec![rejection(message); if a { 2 } else { 1 }]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            assert!(matches!(
                client.test_connection().unwrap().next().await,
                Some(Event::Error(AiError::InvalidResponse))
            ));
            let requests = http.requests();
            assert_eq!(requests.len(), if a { 2 } else { 1 });
            for request in requests {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["max_tokens"], 32000,
                    "an unrelated 400 must not lower the cap"
                );
            }
        }
    }
}
#[tokio::test]
async fn cache_is_anthropic_only_and_rejection_disables_it_for_this_client() {
    for a in [false, true] {
        let body = request_body(&config(a), false).await;
        if a {
            assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
        } else {
            assert!(!body.to_string().contains("cache"));
        }
    }
    let http = SequencedHttpClient::new([
        rejection("unsupported cache_control"),
        MockHttpResponse::success_typed(anthropic("ok", false), "text/event-stream"),
        MockHttpResponse::success_typed(anthropic("again", false), "text/event-stream"),
    ]);
    let client = RigClient::with_http(&config(true), http.clone()).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            client
                .start(&mut conversation())
                .unwrap()
                .collect::<Vec<_>>()
                .await
                .last(),
            Some(Event::Done(_))
        ));
    }
    let bodies: Vec<Value> = http
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0]["cache_control"], json!({"type":"ephemeral"}));
    assert!(bodies[1].get("cache_control").is_none());
    assert!(bodies[2].get("cache_control").is_none());
}

mod support;
#[tokio::test(start_paused = true)]
async fn active_stream_has_no_total_cap_and_first_and_idle_deadlines_still_apply() {
    for a in [false, true] {
        let mut cfg = config(a);
        cfg.reasoning = ReasoningLevel::High;
        let input = reasoning_sse(a);
        let chunks = input.split_inclusive("\n\n").map(str::to_owned).collect();
        let client = RigClient::with_http(
            &cfg,
            support::TimedStream {
                chunks,
                gap: Duration::from_secs(100),
            },
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        let events = client
            .start(&mut conversation())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        assert!(started.elapsed() > Duration::from_secs(390));
        let client = RigClient::with_http(&cfg, SilentStreamingClient).unwrap();
        let started = tokio::time::Instant::now();
        assert!(matches!(
            client.start(&mut conversation()).unwrap().next().await,
            Some(Event::Error(AiError::Timeout))
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(600));
        // A partial answer arrives, then the body stays open forever.
        let prefix = support::before_completion(&sse(a, "partial", false), a);
        let client = RigClient::with_http(
            &cfg,
            support::TimedStream {
                chunks: vec![prefix],
                gap: Duration::from_secs(1),
            },
        )
        .unwrap();
        let mut log = conversation();
        let mut stream = client.start(&mut log).unwrap();
        assert!(matches!(stream.next().await, Some(Event::TextDelta(_))));
        let started = tokio::time::Instant::now();
        assert!(matches!(
            stream.next().await,
            Some(Event::Error(AiError::Timeout))
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(300));
    }
}

#[tokio::test]
async fn openai_reasoning_models_send_max_completion_tokens() {
    let mut cfg = config(false);
    cfg.model = "gpt-5".into();
    let body = request_body(&cfg, false).await;
    assert_eq!(body["max_completion_tokens"], 32000);
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn generic_400_disables_anthropic_cache_once_but_never_retries_openai() {
    for message in ["Extra inputs are not permitted", "invalid request"] {
        for a in [false, true] {
            let http = SequencedHttpClient::new([
                rejection(message),
                MockHttpResponse::success_typed(sse(a, "ok", false), "text/event-stream"),
                MockHttpResponse::success_typed(sse(a, "again", false), "text/event-stream"),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let events = client
                .start(&mut conversation())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            if a {
                assert!(matches!(events.last(), Some(Event::Done(_))));
                assert!(matches!(
                    client
                        .test_connection()
                        .unwrap()
                        .collect::<Vec<_>>()
                        .await
                        .last(),
                    Some(Event::Done(_))
                ));
            } else {
                assert!(matches!(
                    &events[..],
                    [Event::Error(AiError::InvalidResponse)]
                ));
            }
            let bodies: Vec<Value> = http
                .requests()
                .iter()
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect();
            assert_eq!(bodies.len(), if a { 3 } else { 1 });
            if a {
                assert_eq!(bodies[0]["cache_control"], json!({"type":"ephemeral"}));
                assert!(bodies[1..].iter().all(|b| b.get("cache_control").is_none()));
            } else {
                assert!(bodies[0].get("cache_control").is_none());
            }
            assert!(bodies.iter().all(|b| b["max_tokens"] == 32000));
        }
    }
}

#[tokio::test]
async fn two_generic_400s_remain_terminal_after_one_cache_retry() {
    let http =
        SequencedHttpClient::new([rejection("invalid request"), rejection("invalid request")]);
    let client = RigClient::with_http(&config(true), http.clone()).unwrap();
    let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
    assert!(matches!(
        &events[..],
        [Event::Error(AiError::InvalidResponse)]
    ));
    let bodies: Vec<Value> = http
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["cache_control"], json!({"type":"ephemeral"}));
    assert!(bodies[1].get("cache_control").is_none());
}

#[tokio::test]
async fn token_fallback_precedes_generic_cache_fallback() {
    for failures in [1, 3] {
        let mut responses = vec![rejection("max_tokens rejected"); failures];
        responses.push(MockHttpResponse::success_typed(
            anthropic("ok", false),
            "text/event-stream",
        ));
        let http = SequencedHttpClient::new(responses);
        let client = RigClient::with_http(&config(true), http.clone()).unwrap();
        let events = client
            .start(&mut conversation())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        let bodies: Vec<Value> = http
            .requests()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(bodies.len(), failures + 1);
        assert_eq!(bodies[0]["max_tokens"], 32000);
        assert_eq!(bodies[1]["max_tokens"], 8192);
        assert!(
            bodies[..bodies.len().min(3)]
                .iter()
                .all(|b| b["cache_control"] == json!({"type":"ephemeral"}))
        );
        if failures == 3 {
            assert_eq!(bodies[2]["max_tokens"], 4096);
            assert_eq!(bodies[3]["max_tokens"], 4096);
            assert!(bodies[3].get("cache_control").is_none());
        }
    }
}

#[tokio::test]
async fn side_question_has_no_tools_and_does_not_modify_the_main_conversation() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
            sse(a, "side answer", false),
            "text/event-stream",
        )]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let log = conversation();
        let before = log.clone();
        let (answer, _) = client.side_question(&log, "SIDE QUESTION").await.unwrap();
        assert!(answer.contains("side answer"));
        assert!(log == before);
        let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        assert!(
            body.get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
        );
        assert!(body.to_string().contains("SIDE QUESTION"));
    }
}

#[path = "providers/responses.rs"]
mod responses_tests;

#[path = "providers/images.rs"]
mod image_tests;

#[path = "providers/thinking.rs"]
mod thinking_tests;
