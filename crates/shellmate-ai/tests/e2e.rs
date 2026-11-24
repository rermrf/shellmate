//! Explicit opt-in only. The API key comes from a file, never argv/environment.
use futures::StreamExt;
use shellmate_ai::*;
use std::io::Read;
use zeroize::Zeroizing;
fn config() -> Result<Config, &'static str> {
    let provider = match std::env::var("SHELLMATE_AI_PROVIDER").as_deref() {
        Ok("openai") => Provider::OpenAiCompatible {
            base_url: std::env::var("SHELLMATE_AI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        },
        Ok("anthropic") => Provider::Anthropic {
            base_url: std::env::var("SHELLMATE_AI_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into()),
        },
        _ => return Err("set SHELLMATE_AI_PROVIDER to openai or anthropic"),
    };
    let model = std::env::var("SHELLMATE_AI_MODEL").map_err(|_| "set SHELLMATE_AI_MODEL")?;
    let path = std::env::var_os("SHELLMATE_AI_KEY_FILE").ok_or("set SHELLMATE_AI_KEY_FILE")?;
    let file = std::fs::File::open(path).map_err(|_| "could not open key file")?;
    let mut key = Zeroizing::new(String::new());
    file.take(4097)
        .read_to_string(&mut key)
        .map_err(|_| "could not read key file")?;
    if key.len() > 4096 || key.trim().is_empty() {
        return Err("key file is empty or too large");
    }
    let reasoning =
        ReasoningLevel::from_id(&std::env::var("SHELLMATE_AI_REASONING").unwrap_or_default())
            .for_provider(&provider);
    Ok(Config {
        provider,
        model,
        api_key: ApiKey::new(key.trim()),
        reasoning,
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: reasoning.timeouts(),
    })
}
async fn answer(
    client: &dyn ProviderClient,
    c: &mut Conversation,
) -> Result<(Answer, usize), AiError> {
    let mut stream = client.start(c)?;
    let mut a = Answer::default();
    let mut deltas = 0;
    while let Some(event) = stream.next().await {
        match event {
            Event::Thinking
            | Event::Context { .. }
            | Event::Compacting { .. }
            | Event::Compacted { .. } => {}
            Event::TextDelta(text) => {
                deltas += 1;
                a.push_text(&text)
            }
            Event::Command(card) => a.items.push(AnswerItem::Command(card)),
            Event::Done(_) => return Ok((a, deltas)),
            Event::Error(e) => return Err(e),
        }
    }
    Err(AiError::InvalidResponse)
}
#[tokio::test]
#[ignore = "requires user API key file and real network; never run in sandbox"]
async fn real_two_turn_stream_commands_and_invalid_key() {
    let cfg = config().unwrap_or_else(|e| panic!("configuration: {e}"));
    let client = RigClient::new(&cfg).unwrap_or_else(|e| panic!("client: {e}"));
    let mut c = Conversation::default();
    c.push_turn(Turn {
        user: UserTurn::new(
            "给我一个列出进程的命令。先用一句中文解释用途，再通过 propose_command 提出命令，不要执行。",
            ContextSnapshot {
                label: "temporary-ai-e2e".into(),
                protocol: "SSH".into(),
                text: "$ uname -s\nLinux".into(),
            },
        ),
        answer: None,
    });
    let (a, n) = answer(&client, &mut c)
        .await
        .unwrap_or_else(|e| panic!("first turn failed: {e}"));
    assert!(n > 0, "first turn did not stream text");
    assert!(
        a.commands().next().is_some(),
        "first turn did not propose a command"
    );
    assert_eq!(c.turns()[0].answer.as_ref().unwrap().text(), a.text());
    c.push_turn(Turn {
        user: UserTurn::new("把刚才的命令改成只看 nginx", ContextSnapshot::default()),
        answer: None,
    });
    let (a, _) = answer(&client, &mut c)
        .await
        .unwrap_or_else(|e| panic!("follow-up failed: {e}"));
    // Models often write `grep '[n]ginx'` so grep does not match itself; ignore bracket expressions.
    let mentions = |command: &str, word: &str| {
        command
            .replace(['[', ']'], "")
            .to_lowercase()
            .contains(word)
    };
    assert!(
        a.commands().any(|card| mentions(&card.command, "nginx")),
        "follow-up command did not retain requested filter"
    );
    println!("PASS: two-turn continuity and streamed answer");
    assert_eq!(
        c.turns().last().unwrap().answer.as_ref().unwrap().text(),
        a.text()
    );
    c.push_turn(Turn {
        user: UserTurn::new("找出占用 3000 端口的进程", ContextSnapshot::default()),
        answer: None,
    });
    let (a, _) = answer(&client, &mut c)
        .await
        .unwrap_or_else(|e| panic!("port question failed: {e}"));
    assert!(
        a.commands().any(|card| card.command.contains("3000")),
        "no port command card"
    );
    println!("PASS: port command card");
    let mut invalid = cfg.clone();
    invalid.api_key = ApiKey::new("invalid-shellmate-e2e-key");
    let invalid = RigClient::new(&invalid).unwrap_or_else(|e| panic!("invalid-key client: {e}"));
    let result = answer(&invalid, &mut c).await;
    assert!(
        matches!(result, Err(AiError::Authentication)),
        "invalid key did not yield Key 无效或无权限"
    );
    println!("PASS: invalid-key Chinese error");
}

/// The only host in this test returns literals; it has no process or SSH access.
#[tokio::test]
#[ignore = "requires real API; fake host only; never run in sandbox"]
async fn real_agent_uses_canned_docker_output_without_execution() {
    use futures::future::BoxFuture;
    use std::{sync::Mutex, time::Duration};
    struct FakeHost(Mutex<Vec<ToolCall>>);
    impl ToolHost for FakeHost {
        fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            Box::pin(async move {
                self.0.lock().unwrap().push(call);
                ToolOutcome::Completed(shape(
                    "CONTAINER ID   IMAGE       STATUS       NAMES\na1b2c3         nginx:1.27  Up 2 hours   shellmate-web-fixture\n",
                    [],
                    Some(0),
                    Duration::from_millis(5),
                    false,
                    false,
                ))
            })
        }
    }
    let cfg = config().unwrap_or_else(|e| panic!("configuration: {e}"));
    let client = RigClient::new(&cfg).unwrap_or_else(|e| panic!("client: {e}"));
    let host = FakeHost(Mutex::new(Vec::new()));
    let mut c = Conversation::from_turns(vec![Turn {
        user: UserTurn::new(
            "这台机器上运行了哪些 docker 容器？",
            ContextSnapshot {
                label: "fake-host-only".into(),
                protocol: "SSH".into(),
                text: String::new(),
            },
        ),
        answer: None,
    }]);
    let mut stream = agent::run(
        &client,
        &mut c,
        Capabilities {
            background: true,
            ..Default::default()
        },
        &host,
        Limits::default(),
    )
    .unwrap();
    let mut done = false;
    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::Done(_) => done = true,
            AgentEvent::Error(e) => panic!("agent: {e}"),
            AgentEvent::Paused { reason, .. } => panic!("agent paused: {reason:?}"),
            AgentEvent::Retrying { .. } => {}
            _ => {}
        }
    }
    drop(stream);
    assert!(done);
    assert!(
        host.0
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.command.contains("docker"))
    );
    assert!(
        c.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .text()
            .contains("shellmate-web-fixture")
    );
    // This fake host's sole implementation returns the literal above and records calls.
    assert!(
        c.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|i| matches!(i, AnswerItem::Step { .. }))
            .count()
            == host.0.lock().unwrap().len()
    );
}
