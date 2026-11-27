use futures::{StreamExt, future::BoxFuture};
use rig_core::test_utils::SequencedHttpClient;
use serde_json::{Value, json};
use shellmate_ai::{agent, *};
use shellmate_store::{HostStore, ai::SavedConversation};
use std::time::Duration;
#[path = "../../shellmate-ai/tests/support/responses.rs"]
mod wire;
struct Host;
impl ToolHost for Host {
    fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async {
            ToolOutcome::Completed(shape("/project", [], Some(0), Duration::ZERO, false, false))
        })
    }
}
#[tokio::test]
async fn sqlite_reopen_keeps_ciphertext_and_phase_out_of_visible_transcript() {
    let config = Config {
        provider: Provider::OpenAiResponses {
            base_url: "https://responses.test/v1".into(),
        },
        model: "gpt-5.4".into(),
        api_key: ApiKey::new("fixture-key"),
        reasoning: Default::default(),
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Default::default(),
    };
    let http = SequencedHttpClient::new([
        wire::reply(vec![
            wire::reasoning("saved"),
            wire::message("m", "检查项目", "commentary"),
            wire::call(
                "c",
                "run_command",
                json!({"command":"pwd","target":"background"}),
            ),
        ]),
        wire::reply(vec![wire::message("m2", "已完成", "final_answer")]),
        wire::reply(vec![wire::message("m3", "继续", "final_answer")]),
    ]);
    let client = RigClient::with_http(&config, http.clone()).unwrap();
    let mut conversation = Conversation::default();
    conversation.push_user(UserTurn::new("检查", Default::default()));
    let caps = Capabilities {
        background: true,
        cwd: Some("/project".into()),
        ..Default::default()
    };
    let events = agent::run(
        &client,
        &mut conversation,
        caps.clone(),
        &Host,
        Limits::default(),
    )
    .unwrap()
    .collect::<Vec<_>>()
    .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");
    let mut db = HostStore::open(&path).unwrap();
    let saved = SavedConversation {
        id: "response-chat".into(),
        host_id: None,
        label: "test".into(),
        title: "test".into(),
        provider: "openai_responses".into(),
        model: config.model.clone(),
        created_at: 1,
        updated_at: 2,
        conversation,
    };
    assert!(db.save_ai_conversation(&saved).unwrap());
    drop(db);
    let db = HostStore::open(&path).unwrap();
    let mut restored = db
        .ai_conversation("response-chat")
        .unwrap()
        .unwrap()
        .conversation;
    assert_eq!(restored.entries(), saved.conversation.entries());
    assert!(
        !restored.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .text()
            .contains("CIPHER")
    );
    let raw = serde_json::to_string(restored.entries()).unwrap();
    assert!(raw.contains("PRIVATE_REASONING"));
    restored.push_user(UserTurn::new("继续", Default::default()));
    let events = agent::run(&client, &mut restored, caps, &Host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    let body: Value = serde_json::from_slice(&http.requests()[2].body).unwrap();
    assert!(body.to_string().contains("CIPHER_saved"));
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["phase"] == "commentary")
    );
}
