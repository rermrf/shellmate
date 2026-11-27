use super::responses_tests::wire;
use super::*;
use base64::Engine;
fn sample(root: &std::path::Path, shade: u8) -> Attachment {
    let pixels = image::RgbImage::from_pixel(24, 16, image::Rgb([shade, 30, 40]));
    let mut bytes = std::io::Cursor::new(vec![]);
    pixels
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    media::import(root, "conversation", "终端截图.png", bytes.get_ref()).unwrap()
}
fn cfg(n: usize) -> Config {
    let mut c = config(n == 2);
    if n == 1 {
        c.provider = Provider::OpenAiResponses {
            base_url: "https://custom.example/v1".into(),
        };
    }
    c
}
fn response(n: usize) -> MockHttpResponse {
    if n == 1 {
        wire::reply(vec![wire::message(
            "msg_image",
            "看到了图片",
            "final_answer",
        )])
    } else {
        MockHttpResponse::success_typed(sse(n == 2, "看到了图片", false), "text/event-stream")
    }
}
fn image_bytes(value: &Value, found: &mut Vec<Vec<u8>>) {
    match value {
        Value::String(s) if s.starts_with("data:image/") => found.push(
            base64::engine::general_purpose::STANDARD
                .decode(s.split_once(",").unwrap().1)
                .unwrap(),
        ),
        Value::Object(o)
            if o.get("type").and_then(Value::as_str) == Some("base64")
                && o.contains_key("media_type") =>
        {
            found.push(
                base64::engine::general_purpose::STANDARD
                    .decode(o["data"].as_str().unwrap())
                    .unwrap(),
            )
        }
        Value::Object(o) => {
            for v in o.values() {
                image_bytes(v, found);
            }
        }
        Value::Array(a) => {
            for v in a {
                image_bytes(v, found);
            }
        }
        _ => {}
    }
}
#[tokio::test]
async fn all_three_protocols_send_actual_ordered_images_and_keep_history_light() {
    for n in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let images = vec![sample(root.path(), 80), sample(root.path(), 180)];
        let mut log = Conversation::default();
        log.push_user(
            UserTurn::new("", Default::default())
                .with_attachments(images.clone())
                .sent_now(),
        );
        let http = SequencedHttpClient::new([response(n)]);
        let client = RigClient::with_http(&cfg(n), http.clone())
            .unwrap()
            .with_image_root(root.path());
        let mut stream = client.start(&mut log).unwrap();
        let mut done = false;
        while let Some(event) = stream.next().await {
            match event {
                Event::Done(_) => done = true,
                Event::Error(e) => panic!("protocol {n}: {e}"),
                _ => {}
            }
        }
        drop(stream);
        assert!(done);
        let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        let mut sent = vec![];
        image_bytes(&body, &mut sent);
        assert_eq!(
            sent,
            images
                .iter()
                .map(|a| media::read(root.path(), a).unwrap())
                .collect::<Vec<_>>(),
            "protocol {n}"
        );
        let stored = serde_json::to_string(log.entries()).unwrap();
        assert!(!stored.contains("base64"));
        assert!(!stored.contains("data:image"));
        let restored = Conversation::from_entries(serde_json::from_str(&stored).unwrap());
        assert_eq!(restored.turns()[0].user.attachments, images);
        assert!(!body.to_string().contains("image omitted"));
        assert!(!body.to_string().contains("shellmate-image:"));
    }
}
#[tokio::test]
async fn known_text_only_models_fail_without_requests_or_losing_images() {
    for n in [0, 1] {
        let root = tempfile::tempdir().unwrap();
        let a = sample(root.path(), 80);
        let mut c = cfg(n);
        c.model = "gpt-3.5-turbo".into();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("", Default::default()).with_attachments(vec![a.clone()]));
        let http = SequencedHttpClient::new([]);
        let client = RigClient::with_http(&c, http.clone())
            .unwrap()
            .with_image_root(root.path());
        assert!(matches!(
            client.start(&mut log),
            Err(AiError::UnsupportedImages)
        ));
        assert!(http.requests().is_empty());
        assert_eq!(log.turns()[0].user.attachments, vec![a]);
    }
}
#[tokio::test]
async fn tool_results_send_images_in_each_protocol() {
    for n in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let a = sample(root.path(), 80);
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("重新看图片", Default::default()));
        log.append(Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: "call_image".into(),
                name: "view_image".into(),
                args_json: json!({"id":a.id}),
            },
            result: LoggedResult {
                attachments: vec![a.clone()],
                inline: "截图".into(),
                subagent: None,
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        let http = SequencedHttpClient::new([response(n)]);
        let client = RigClient::with_http(&cfg(n), http.clone())
            .unwrap()
            .with_image_root(root.path());
        let mut stream = client.start(&mut log).unwrap();
        while let Some(e) = stream.next().await {
            if let Event::Error(e) = e {
                panic!("protocol {n}: {e}");
            }
        }
        let body: Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        let mut sent = vec![];
        image_bytes(&body, &mut sent);
        assert_eq!(
            sent,
            vec![media::read(root.path(), &a).unwrap()],
            "protocol {n}"
        );
    }
}
#[tokio::test]
async fn view_image_works_without_command_execution_and_after_compaction() {
    let root = tempfile::tempdir().unwrap();
    let a = sample(root.path(), 80);
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("分析截图", Default::default()).with_attachments(vec![a.clone()]));
    log.append(Entry::Compaction(Compaction {
        summary: Some("已查看过一张截图，需要重新确认错误".into()),
        retained_user: vec!["分析截图".into()],
        state: RetainedState::default(),
        context: Default::default(),
        before_tokens: 1000,
        after_tokens: 50,
        trigger: CompactTrigger::Manual,
    }));
    let http = SequencedHttpClient::new([
        wire::reply(vec![wire::call(
            "image_read",
            "view_image",
            json!({"id":a.id}),
        )]),
        response(1),
    ]);
    let client = RigClient::with_http(&cfg(1), http.clone())
        .unwrap()
        .with_image_root(root.path());
    let mut stream = client.start(&mut log).unwrap();
    while let Some(e) = stream.next().await {
        if let Event::Error(e) = e {
            panic!("{e}");
        }
    }
    assert_eq!(http.requests().len(), 2);
    let requests = http.requests();
    let mut sent = vec![];
    image_bytes(
        &serde_json::from_slice(&requests[1].body).unwrap(),
        &mut sent,
    );
    assert_eq!(sent, vec![media::read(root.path(), &a).unwrap()]);
}
