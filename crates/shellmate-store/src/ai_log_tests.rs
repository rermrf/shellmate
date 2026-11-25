use super::*;
use shellmate_ai::{Entry, StepStatus, Target, policy::Class};

fn legacy_items() -> Vec<AnswerItem> {
    vec![
        AnswerItem::Text("before step".into()),
        AnswerItem::Command(CommandCard {
            command: "ls".into(),
            explanation: "list".into(),
            risk: shellmate_ai::Risk::Low,
        }),
        AnswerItem::Step {
            command: "pwd".into(),
            target: Target::Terminal,
            purpose: "directory".into(),
            class: Class::ReadOnly,
            status: StepStatus::Completed,
            exit_code: Some(0),
            duration_ms: 123,
            automatic: true,
            output: "标准输出：\n/srv\n标准错误：\n".into(),
        },
        AnswerItem::Step {
            command: "rm important".into(),
            target: Target::Background,
            purpose: "remove".into(),
            class: Class::Other,
            status: StepStatus::Denied {
                reason: Some("keep".into()),
            },
            exit_code: None,
            duration_ms: 0,
            automatic: false,
            output: String::new(),
        },
        AnswerItem::Text("after steps".into()),
    ]
}
fn v9(conn: &mut rusqlite::Connection, items: &str) {
    crate::hosts::migrate_for_tests(conn, 9);
    conn.execute("INSERT INTO ai_conversations VALUES('legacy',NULL,'host','question','anthropic','model',1,2)",[]).unwrap();
    conn.execute("INSERT INTO ai_messages VALUES('u','legacy','user','question','{\"label\":\"host\",\"protocol\":\"SSH\",\"text\":\"snapshot\"}','[]',1,0,8,'[]')",[]).unwrap();
    conn.execute("INSERT INTO ai_messages VALUES('a','legacy','assistant','untouched','legacy-context','[]',2,1,9,?1)",[items]).unwrap();
}
#[test]
fn migration_v9_v10_keeps_display_ids_timestamps_and_old_rows_unchanged() {
    let items = legacy_items();
    let json = serde_json::to_string(&items).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    v9(&mut conn, &json);
    drop(conn);
    let mut db = HostStore::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 11);
    let loaded = db.ai_conversation("legacy").unwrap().unwrap();
    assert_eq!(loaded.conversation.turns()[0].user.question, "question");
    assert_eq!(loaded.conversation.turns()[0].user.context.text, "snapshot");
    assert_eq!(
        loaded.conversation.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .items,
        items
    );
    let steps = loaded
        .conversation
        .entries()
        .iter()
        .filter_map(|e| {
            if let Entry::Step(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(steps[0].call.id, "legacy_1_2");
    assert_eq!(steps[1].call.id, "legacy_1_3");
    for step in steps {
        assert!(step.result.cleared);
        assert!(
            step.call
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        );
        assert!(step.result.output_id.is_none());
        assert_eq!(step.result.inline, step.display.as_ref().unwrap().output);
    }
    let timestamps = db
        .conn
        .prepare("SELECT created_at FROM ai_log ORDER BY position")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(timestamps, vec![1, 2, 2, 2, 2, 2]);
    db.save_ai_conversation(&loaded).unwrap();
    assert!(db.ai_conversation("legacy").unwrap().unwrap().conversation == loaded.conversation);
    let old:(String,String,String,String,i64)=db.conn.query_row("SELECT content,context,commands_json,items_json,redactions FROM ai_messages WHERE id='a'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(
        old,
        (
            "untouched".into(),
            "legacy-context".into(),
            "[]".into(),
            json,
            9
        )
    );
    let count: i64 = db
        .conn
        .query_row("SELECT count(*) FROM ai_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}
#[test]
fn corrupt_v9_conversion_rolls_back_the_whole_v10_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    v9(&mut conn, "not json");
    drop(conn);
    assert!(HostStore::open(&path).is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        9
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='ai_log'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT items_json FROM ai_messages WHERE id='a'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "not json"
    );
}

fn signed_response(tool: bool) -> rig_core::test_utils::MockHttpResponse {
    use serde_json::json;
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":0}}}),
    ];
    if tool {
        events.extend([
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"MEMORY-ONLY-THINKING"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIGNED-BYTES-ONLY"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"run_command","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}),
            json!({"type":"content_block_stop","index":1}),
        ]);
    } else {
        events.extend([
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"content_block_stop","index":0}),
        ]);
    }
    events.extend([json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"}else{"end_turn"}},"usage":{"output_tokens":8}}),json!({"type":"message_stop"})]);
    rig_core::test_utils::MockHttpResponse::success_typed(
        events
            .iter()
            .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
            .collect::<String>(),
        "text/event-stream",
    )
}
#[tokio::test]
async fn native_signed_reasoning_replays_in_run_but_never_reaches_sqlite_or_next_run() {
    use futures::StreamExt;
    use shellmate_ai::{agent, *};
    struct Host;
    impl ToolHost for Host {
        fn run(&self, _: ToolCall) -> futures::future::BoxFuture<'_, ToolOutcome> {
            Box::pin(async {
                ToolOutcome::Completed(shape(
                    "/srv",
                    [],
                    Some(0),
                    std::time::Duration::ZERO,
                    false,
                    false,
                ))
            })
        }
    }
    let http = rig_core::test_utils::SequencedHttpClient::new([
        signed_response(true),
        signed_response(false),
        signed_response(false),
    ]);
    let client = RigClient::with_http(
        &Config {
            provider: Provider::Anthropic {
                base_url: "https://fixture.test".into(),
            },
            model: "claude-sonnet-4-6".into(),
            api_key: ApiKey::new("fixture"),
            reasoning: ReasoningLevel::Default,
            max_output_tokens: OutputTokens::Auto,
            context_window: ContextWindow::Auto,
            autocompact_window: ContextWindow::Auto,
            timeouts: Timeouts::default(),
        },
        http.clone(),
    )
    .unwrap();
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("question", ContextSnapshot::default()));
    let caps = Capabilities {
        background: true,
        ..Default::default()
    };
    let events = agent::run(&client, &mut log, caps.clone(), &Host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    let request = String::from_utf8(http.requests()[1].body.to_vec()).unwrap();
    assert!(request.contains("SIGNED-BYTES-ONLY") && request.contains("MEMORY-ONLY-THINKING"));
    let mut db = HostStore::open_in_memory().unwrap();
    let saved = SavedConversation {
        id: "signed".into(),
        host_id: None,
        label: "host".into(),
        title: "question".into(),
        provider: "anthropic".into(),
        model: "fixture".into(),
        created_at: 1,
        updated_at: 2,
        conversation: log,
    };
    db.save_ai_conversation(&saved).unwrap();
    let json: String = db
        .conn
        .query_row("SELECT group_concat(json) FROM ai_log", [], |r| r.get(0))
        .unwrap();
    assert!(
        !json.contains("SIGNED-BYTES-ONLY")
            && !json.contains("MEMORY-ONLY-THINKING")
            && !json.contains("native")
    );
    let mut restored = db.ai_conversation("signed").unwrap().unwrap().conversation;
    restored.push_user(UserTurn::new("next", ContextSnapshot::default()));
    agent::run(&client, &mut restored, caps, &Host, Limits::default())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let request = String::from_utf8(http.requests()[2].body.to_vec()).unwrap();
    assert!(!request.contains("SIGNED-BYTES-ONLY") && !request.contains("MEMORY-ONLY-THINKING"));
    assert!(request.contains("tool_use") && request.contains("tool_result"));
}

#[test]
fn live_queue_and_completion_receipts_are_atomic_scoped_and_deleted_with_the_conversation() {
    let mut db = HostStore::open_in_memory().unwrap();
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("task", Default::default()));
    let mut saved = SavedConversation {
        id: "live".into(),
        host_id: None,
        label: "local".into(),
        title: "task".into(),
        provider: "openai".into(),
        model: "fixture".into(),
        created_at: 1,
        updated_at: 1,
        conversation: log,
    };
    let c = shellmate_ai::interaction::Control::default();
    c.resume();
    c.enqueue("message".into(), "queued".into(), Default::default());
    let runtime = serde_json::json!({"queue":c.snapshot(),"scope":{"local":true,"cwd":"/project"}});
    db.save_ai_conversation_with_runtime(&saved, Some(&runtime))
        .unwrap();
    assert_eq!(db.ai_runtime("live").unwrap(), Some(runtime));
    assert!(db.ai_runtime("other").unwrap().is_none());
    db.save_ai_task_event("live", "event", "agent-1", "done")
        .unwrap();
    db.save_ai_task_event("live", "event", "agent-1", "done")
        .unwrap();
    assert_eq!(db.ai_task_events("live").unwrap().len(), 1);
    c.publish("event".into(), "agent-1".into(), "done".into());
    c.claim_completions();
    c.acknowledge_completions(&["event".into()]);
    let input = c.claim_first().unwrap();
    saved
        .conversation
        .push_user(UserTurn::new(&input.text, input.context));
    c.acknowledge(&[input.id]);
    db.save_ai_conversation_with_runtime(&saved, Some(&serde_json::json!({"queue":c.snapshot()})))
        .unwrap();
    assert!(db.ai_task_events("live").unwrap().is_empty());
    assert_eq!(
        db.ai_conversation("live")
            .unwrap()
            .unwrap()
            .conversation
            .turns()
            .len(),
        2
    );
    db.delete_ai_conversation("live").unwrap();
    db.save_ai_task_event("live", "late", "agent-1", "done")
        .unwrap();
    assert!(db.ai_task_events("live").unwrap().is_empty());
    assert!(db.ai_runtime("live").unwrap().is_none());
    db.set_setting(SAVE_CONVERSATIONS, "0").unwrap();
    assert!(
        !db.save_ai_conversation_with_runtime(&saved, Some(&serde_json::json!({})))
            .unwrap()
    );
}

#[test]
fn timing_heartbeat_preserves_log_queue_and_updated_at_and_never_recreates_history() {
    let mut db = HostStore::open_in_memory().unwrap();
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("task", Default::default()));
    let saved = SavedConversation {
        id: "timed".into(),
        host_id: None,
        label: "local".into(),
        title: "task".into(),
        provider: "openai".into(),
        model: "fixture".into(),
        created_at: 10,
        updated_at: 20,
        conversation: log,
    };
    let runtime = serde_json::json!({"queue":{"inputs":[{"id":"queued","text":"next"}],"delivered":["done"]},"scope":{"cwd":"/original"}});
    db.save_ai_conversation_with_runtime(&saved, Some(&runtime))
        .unwrap();
    let timing = serde_json::json!([{"id":"round","started_at_ms":10000,"ended_at_ms":null,"duration_ms":1500,"status":"running"}]);
    assert!(db.update_ai_run_timing("timed", &timing).unwrap());
    let restored = db.ai_runtime("timed").unwrap().unwrap();
    assert_eq!(restored["timing"], timing);
    assert_eq!(restored["queue"], runtime["queue"]);
    assert_eq!(restored["scope"], runtime["scope"]);
    let history = db.ai_conversation("timed").unwrap().unwrap();
    assert_eq!(history.updated_at, 20);
    assert!(history.conversation == saved.conversation);
    db.set_setting(SAVE_CONVERSATIONS, "0").unwrap();
    assert!(
        !db.update_ai_run_timing("timed", &serde_json::json!([]))
            .unwrap()
    );
    assert_eq!(db.ai_runtime("timed").unwrap().unwrap()["timing"], timing);
    db.set_setting(SAVE_CONVERSATIONS, "1").unwrap();
    db.delete_ai_conversation("timed").unwrap();
    assert!(!db.update_ai_run_timing("timed", &timing).unwrap());
    assert!(db.ai_runtime("timed").unwrap().is_none());
}
