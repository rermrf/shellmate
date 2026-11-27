use super::responses_tests::wire;
use super::*;
fn responses_config() -> Config {
    let mut c = config(false);
    c.provider = Provider::OpenAiResponses {
        base_url: "https://test.example/v1".into(),
    };
    c.model = "gpt-5.4".into();
    c
}
#[tokio::test]
async fn responses_summary_streams_and_opaque_data_never_enters_visible_blocks() {
    let visible = "先检查配置，再验证结果。";
    let item = json!({"type":"reasoning","id":"r","encrypted_content":"OPAQUE_CIPHER","summary":[{"type":"summary_text","text":visible}]});
    let mut body = wire::sse(
        vec![item, wire::message("m", "最终回答", "final_answer")],
        "completed",
    );
    let marker = "event: response.output_item.done";
    let pos = body.find(marker).unwrap();
    body.insert_str(pos,&wire::event(json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"r","summary_index":0,"delta":visible})));
    let http =
        SequencedHttpClient::new([MockHttpResponse::success_typed(body, "text/event-stream")]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let mut log = conversation();
    let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
    assert!(
        matches!(events.last(), Some(Event::Done(_))),
        "completion failed"
    );
    let blocks = events
        .iter()
        .filter_map(|e| {
            if let Event::Reasoning { block, .. } = e {
                Some(block)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        blocks.iter().map(|b| b.text.as_str()).collect::<String>(),
        visible
    );
    assert!(blocks.iter().any(|b| !b.complete));
    assert!(blocks.last().unwrap().complete);
    assert!(blocks.iter().all(|b| b.kind == ThinkingKind::Summary));
    assert_eq!(log.turns()[0].answer.as_ref().unwrap().text(), "最终回答");
    for block in blocks {
        assert!(!block.text.contains("OPAQUE_CIPHER"));
    }
    let request: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
    assert_eq!(request["reasoning"]["summary"], "auto");
}
#[tokio::test]
async fn encrypted_only_reasoning_has_no_empty_disclosure() {
    let http = SequencedHttpClient::new([wire::reply(vec![
        json!({"type":"reasoning","id":"r","encrypted_content":"CIPHER_ONLY","summary":[]}),
        wire::message("m", "回答", "final_answer"),
    ])]);
    let client = RigClient::with_http(&responses_config(), http).unwrap();
    let mut log = conversation();
    let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
    assert!(!events.iter().any(|e| matches!(e, Event::Reasoning { .. })));
    assert!(matches!(events.last(), Some(Event::Done(_))));
    assert!(
        !log.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .items
            .iter()
            .any(|i| matches!(i, AnswerItem::Thinking(_)))
    );
}
#[tokio::test]
async fn summary_option_refusal_retries_without_changing_effort_or_protocol() {
    let http = SequencedHttpClient::new([
        rejection("unsupported reasoning.summary"),
        wire::reply(vec![wire::message("m", "回答", "final_answer")]),
    ]);
    let mut config = responses_config();
    config.reasoning = ReasoningLevel::High;
    let client = RigClient::with_http(&config, http.clone()).unwrap();
    let mut log = conversation();
    let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
    assert!(matches!(events.last(), Some(Event::Done(_))));
    let requests = http.requests();
    assert_eq!(requests.len(), 2);
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(first["reasoning"]["summary"], "auto");
    assert!(second["reasoning"]["summary"].is_null());
    assert_eq!(second["reasoning"]["effort"], "high");
    assert!(requests[1].uri.ends_with("/responses"));
}

#[tokio::test]
async fn non_reasoning_models_and_disabled_thinking_do_not_request_summary() {
    for (model, level) in [
        ("gpt-4o", ReasoningLevel::Default),
        ("gpt-5.4", ReasoningLevel::Off),
    ] {
        let mut config = responses_config();
        config.model = model.into();
        config.reasoning = level;
        let http = SequencedHttpClient::new([wire::reply(vec![wire::message(
            "m",
            "回答",
            "final_answer",
        )])]);
        let client = RigClient::with_http(&config, http.clone()).unwrap();
        let events = client.test_connection().unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        assert!(body["reasoning"]["summary"].is_null());
    }
}
