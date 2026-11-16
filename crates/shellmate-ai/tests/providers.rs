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
            Provider::Anthropic
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
        timeouts: Timeouts::default(),
    }
}
fn conversation() -> Conversation {
    Conversation {
        turns: vec![Turn {
            user: UserTurn::redacted(
                "找出占用 3000 端口的进程",
                ContextSnapshot {
                    label: "prod-web-01".into(),
                    protocol: "SSH".into(),
                    text: "TOKEN=secret-context".into(),
                },
            ),
            answer: None,
        }],
    }
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
        let mut stream = client.start(&conversation()).unwrap();
        assert!(http.requests().is_empty(), "lazy until polled");
        let mut text = String::new();
        let mut cards = vec![];
        let mut done = false;
        while let Some(e) = stream.next().await {
            match e {
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
        assert!(!serialized.contains("secret-context"));
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
        c.turns[0].answer = Some(Answer {
            text: "先查看服务".into(),
            commands: vec![CommandCard {
                command: "ps aux".into(),
                explanation: "进程".into(),
                risk: Risk::Low,
            }],
        });
        c.turns.push(Turn {
            user: UserTurn::redacted("把刚才的命令改成只看 nginx", ContextSnapshot::default()),
            answer: None,
        });
        let events = client.start(&c).unwrap().collect::<Vec<_>>().await;
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
            let http=SequencedHttpClient::new([MockHttpResponse::error(StatusCode::from_u16(status).unwrap(),json!({"error":{"code":code,"type":code,"message":"fixture-api-key SECRET REQUEST AND ANSWER"}}).to_string())]);
            let client = RigClient::with_http(&config(a), http).unwrap();
            let e = client.start(&conversation()).unwrap().next().await.unwrap();
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
            client.start(&conversation()).unwrap().next().await,
            Some(Event::Error(AiError::Timeout))
        ));
        let http = SequencedHttpClient::new([]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        drop(client.start(&conversation()).unwrap());
        assert!(http.requests().is_empty());
        let client = RigClient::with_http(
            &cfg,
            PacedStreamingClient {
                chunks: vec![],
                gap: Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut stream = client.start(&conversation()).unwrap();
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
        let mut stream = client.start(&conversation()).unwrap();
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
        let event = client.start(&conversation()).unwrap().next().await;
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
        let mut stream = client.start(&conversation()).unwrap();
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
