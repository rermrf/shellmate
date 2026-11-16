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
        Ok("anthropic") => Provider::Anthropic,
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
    Ok(Config {
        provider,
        model,
        api_key: ApiKey::new(key.trim()),
        timeouts: Default::default(),
    })
}
async fn answer(client: &dyn ProviderClient, c: &Conversation) -> Result<(Answer, usize), AiError> {
    let mut stream = client.start(c)?;
    let mut a = Answer::default();
    let mut deltas = 0;
    while let Some(event) = stream.next().await {
        match event {
            Event::TextDelta(text) => {
                deltas += 1;
                a.text.push_str(&text)
            }
            Event::Command(card) => a.commands.push(card),
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
    c.turns.push(Turn {
        user: UserTurn::redacted(
            "给我一个列出进程的命令。先用一句中文解释用途，再通过 propose_command 提出命令，不要执行。",
            ContextSnapshot {
                label: "temporary-ai-e2e".into(),
                protocol: "SSH".into(),
                text: "$ uname -s\nLinux".into(),
            },
        ),
        answer: None,
    });
    let (a, n) = answer(&client, &c)
        .await
        .unwrap_or_else(|e| panic!("first turn failed: {e}"));
    assert!(n > 0, "first turn did not stream text");
    assert!(
        !a.commands.is_empty(),
        "first turn did not propose a command"
    );
    c.turns[0].answer = Some(a);
    c.turns.push(Turn {
        user: UserTurn::redacted("把刚才的命令改成只看 nginx", ContextSnapshot::default()),
        answer: None,
    });
    let (a, _) = answer(&client, &c)
        .await
        .unwrap_or_else(|e| panic!("follow-up failed: {e}"));
    assert!(
        a.commands.iter().any(|card| card.command.contains("nginx")),
        "follow-up command did not retain requested filter"
    );
    println!("PASS: two-turn continuity and streamed answer");
    c.turns.last_mut().unwrap().answer = Some(a);
    c.turns.push(Turn {
        user: UserTurn::redacted("找出占用 3000 端口的进程", ContextSnapshot::default()),
        answer: None,
    });
    let (a, _) = answer(&client, &c)
        .await
        .unwrap_or_else(|e| panic!("port question failed: {e}"));
    assert!(
        a.commands.iter().any(|card| card.command.contains("3000")),
        "no port command card"
    );
    println!("PASS: port command card");
    let mut invalid = cfg.clone();
    invalid.api_key = ApiKey::new("invalid-shellmate-e2e-key");
    let invalid = RigClient::new(&invalid).unwrap_or_else(|e| panic!("invalid-key client: {e}"));
    let result = answer(&invalid, &c).await;
    assert!(
        matches!(result, Err(AiError::Authentication)),
        "invalid key did not yield Key 无效或无权限"
    );
    println!("PASS: invalid-key Chinese error");
}
