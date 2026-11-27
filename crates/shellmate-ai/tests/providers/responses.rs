use super::*;
#[path = "../support/responses.rs"]
mod wire;
fn responses_config() -> Config {
    let mut c = config(false);
    c.provider = Provider::OpenAiResponses {
        base_url: "https://custom.example/api/v1".into(),
    };
    c.model = "gpt-5.4".into();
    c
}
#[tokio::test]
async fn response_endpoints_settings_and_streaming_connection_use_responses_only() {
    for (base, endpoint) in [
        (
            "https://proxy.example",
            "https://proxy.example/v1/responses",
        ),
        (
            "https://proxy.example/v1/",
            "https://proxy.example/v1/responses",
        ),
        (
            "https://proxy.example/api/v1/responses/",
            "https://proxy.example/api/v1/responses",
        ),
        (
            "https://proxy.example/custom",
            "https://proxy.example/custom/responses",
        ),
    ] {
        let mut cfg = responses_config();
        cfg.provider = Provider::OpenAiResponses {
            base_url: base.into(),
        };
        cfg.reasoning = ReasoningLevel::High;
        let http = SequencedHttpClient::new([wire::reply(vec![
            wire::reasoning("r"),
            wire::message("m", "连接成功", "final_answer"),
        ])]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
        assert!(events.iter().any(|e| matches!(e, Event::Thinking)));
        assert_eq!(
            events
                .iter()
                .filter_map(|e| if let Event::TextDelta(s) = e {
                    Some(s.as_str())
                } else {
                    None
                })
                .collect::<String>(),
            "连接成功"
        );
        assert!(matches!(
            events.last(),
            Some(Event::Done(Some(Usage {
                input_tokens: Some(32),
                output_tokens: Some(16)
            })))
        ));
        let requests = http.requests();
        assert_eq!(requests[0].uri, endpoint);
        assert!(requests[0].headers["authorization"].is_sensitive());
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 32000);
        for field in [
            "messages",
            "max_tokens",
            "max_completion_tokens",
            "previous_response_id",
            "conversation",
            "background",
        ] {
            assert!(body.get(field).is_none(), "{field}");
        }
        assert!(
            body["include"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "reasoning.encrypted_content")
        );
    }
}
#[tokio::test]
async fn suggestion_tools_are_flat_non_strict_and_usage_is_not_duplicated() {
    let http = SequencedHttpClient::new([wire::reply(vec![wire::call(
        "suggest",
        "propose_command",
        json!({"command":"pwd","explanation":"查看目录","risk":"low"}),
    )])]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let events = client
        .start(&mut conversation())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Command(_)))
            .count(),
        1
    );
    assert!(matches!(events.last(), Some(Event::Done(_))));
    let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
    assert!(body["instructions"].as_str().unwrap().contains("Shellmate"));
    assert_eq!(body["tools"][0]["name"], "propose_command");
    assert_eq!(body["tools"][0]["strict"], false);
    assert!(body["tools"][0].get("function").is_none());
}
#[tokio::test]
async fn endpoint_errors_do_not_switch_protocol_or_echo_payloads() {
    for (code, expected) in [
        ("not_found", AiError::UnsupportedProtocol),
        ("model_not_found", AiError::ModelNotFound),
    ] {
        let http = SequencedHttpClient::new([MockHttpResponse::error(
            StatusCode::NOT_FOUND,
            json!({"error":{"code":code,"message":"SECRET"}}).to_string(),
        )]);
        let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
        let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.last(),Some(Event::Error(e)) if *e==expected));
        assert_eq!(http.requests().len(), 1);
        assert!(http.requests()[0].uri.ends_with("/responses"));
    }
}
#[tokio::test]
async fn side_question_and_summarization_keep_stateless_protocol() {
    let http = SequencedHttpClient::new([wire::reply(vec![wire::message(
        "s",
        "只回答侧问",
        "final_answer",
    )])]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let c = conversation();
    let before = c.clone();
    let (answer, usage) = client.side_question(&c, "目前结论").await.unwrap();
    assert_eq!(answer, "只回答侧问");
    assert_eq!(usage.input_tokens, Some(32));
    assert!(c == before);
    let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
    assert_eq!(body["store"], false);
    assert!(body["tools"].as_array().is_none_or(|v| v.is_empty()));
}

#[tokio::test]
async fn responses_first_and_idle_deadlines_and_dropping_unstarted_stream() {
    let mut cfg = responses_config();
    cfg.timeouts.first = Duration::from_millis(30);
    cfg.timeouts.idle = Duration::from_millis(30);
    let client = RigClient::with_http(&cfg, SilentStreamingClient).unwrap();
    assert!(matches!(
        client.test_connection().unwrap().next().await,
        Some(Event::Error(AiError::Timeout))
    ));
    let http = SequencedHttpClient::new([]);
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    drop(client.test_connection().unwrap());
    assert!(http.requests().is_empty());
    const CHUNK: &str = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"m\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"m\",\"content_index\":0,\"delta\":\"partial\"}\n\n"
    );
    let http = PacedStreamingClient {
        chunks: vec![CHUNK],
        gap: Duration::from_millis(1),
    };
    let client = RigClient::with_http(&cfg, http).unwrap();
    let mut stream = client.test_connection().unwrap();
    assert!(matches!(stream.next().await,Some(Event::TextDelta(s)) if s=="partial"));
    assert!(matches!(
        stream.next().await,
        Some(Event::Error(AiError::Timeout))
    ));
}

#[tokio::test]
async fn incomplete_connection_test_is_not_reported_as_success() {
    let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
        wire::sse(
            vec![wire::message("m", "partial", "final_answer")],
            "incomplete",
        ),
        "text/event-stream",
    )]);
    let client = RigClient::with_http(&responses_config(), http).unwrap();
    let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Event::Error(AiError::InvalidResponse))
    ));
}
