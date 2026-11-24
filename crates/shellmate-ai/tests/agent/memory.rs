use super::*;
use shellmate_ai::{
    agent::{ExecLimits, LocalResult, LocalTool},
    memory::Snapshot,
};
use shellmate_core::memory::Memory;
use std::sync::atomic::AtomicBool;
struct MemoryHost {
    files: Memory,
    loads: AtomicUsize,
    compact_after_write: bool,
    pending: AtomicBool,
}
impl ToolHost for MemoryHost {
    fn memory_snapshot(&self, enabled: bool) -> Snapshot {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Snapshot::load(&self.files, "fixture-host", enabled)
    }
    fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        panic!("memory must never enter command execution or approval")
    }
    fn local_tool(&self, tool: LocalTool, _: ExecLimits) -> BoxFuture<'_, LocalResult> {
        Box::pin(async move {
            let LocalTool::Memory(command) = tool else {
                panic!("unexpected tool")
            };
            let result = command.execute(&self.files);
            if self.compact_after_write {
                self.pending.store(true, Ordering::SeqCst);
            }
            result
        })
    }
    fn take_compaction(&self) -> Option<Option<String>> {
        self.pending.swap(false, Ordering::SeqCst).then_some(None)
    }
}
fn fixture() -> (tempfile::TempDir, MemoryHost) {
    let dir = tempfile::tempdir().unwrap();
    let files = Memory::new(dir.path(), Some("saved"));
    files
        .create("/memories/global/AGENTS.md", "INSTRUCTIONS-ALWAYS")
        .unwrap();
    files
        .create("/memories/global/MEMORY.md", "INDEX-BEFORE")
        .unwrap();
    (
        dir,
        MemoryHost {
            files,
            loads: AtomicUsize::new(0),
            compact_after_write: false,
            pending: AtomicBool::new(false),
        },
    )
}
fn edit() -> Value {
    json!({"command":"str_replace","path":"/memories/global/MEMORY.md","old_str":"INDEX-BEFORE","new_str":"INDEX-AFTER"})
}
fn system(body: &Value, a: bool) -> String {
    if a {
        body["system"].to_string()
    } else {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .map(|m| m["content"].to_string())
            .collect()
    }
}
fn memory_calls(body: &Value, a: bool) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            if a {
                m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_use" && b["name"] == "memory")
                    .count()
            } else {
                m["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["function"]["name"] == "memory")
                    .count()
            }
        })
        .sum()
}
async fn drain(stream: agent::AgentStream<'_>) {
    let events = stream.collect::<Vec<_>>().await;
    assert!(
        matches!(events.last(), Some(AgentEvent::Done(_))),
        "agent did not finish"
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::StepProposed(_)
            | AgentEvent::StepFinished { .. }
            | AgentEvent::Error(_)
            | AgentEvent::Paused { .. }
    )));
}
#[tokio::test]
async fn snapshot_stays_fixed_until_compaction_or_new_conversation_on_both_wires() {
    for a in [false, true] {
        let (_dir, host) = fixture();
        let http = SequencedHttpClient::new([
            response(a, &[("memory1", "memory", edit())], "", false),
            response(a, &[], "saved", false),
            response(a, &[], "followup", false),
            response(a, &[], "new conversation", false),
            response(a, &[], "<summary>handoff</summary>", false),
            response(a, &[], "after compaction", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut c = conversation();
        let mut ui_log = c.clone();
        let mut stream =
            agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap();
        while let Some(event) = stream.next().await {
            assert!(!matches!(
                event,
                AgentEvent::Error(_) | AgentEvent::Paused { .. }
            ));
            for update in stream.take_updates() {
                ui_log.apply(update);
            }
        }
        drop(stream);
        assert!(ui_log == c);
        assert_eq!(
            serde_json::to_string(&ui_log.prepare(&capabilities()).unwrap().chat_history).unwrap(),
            serde_json::to_string(&c.prepare(&capabilities()).unwrap().chat_history).unwrap()
        );
        assert_eq!(
            host.files.read("/memories/global/MEMORY.md").unwrap(),
            "INDEX-AFTER"
        );
        assert!(c.entries().iter().any(|e| matches!(e, Entry::Step(s) if s.call.name == "memory" && s.display.is_none() && !s.result.is_error)));
        assert!(c.turns()[0].answer.as_ref().unwrap().items.iter().any(
            |item| matches!(item, AnswerItem::Notice(s) if s == "已更新记忆 · global/MEMORY.md")
        ));
        c.push_user(UserTurn::new("next question", ContextSnapshot::default()));
        drain(agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap()).await;
        assert_eq!(host.loads.load(Ordering::SeqCst), 1);
        let mut fresh = conversation();
        drain(
            agent::run(
                &client,
                &mut fresh,
                capabilities(),
                &host,
                Limits::default(),
            )
            .unwrap(),
        )
        .await;
        drain(agent::compact(&client, &mut c, capabilities(), &host, None).unwrap()).await;
        c.push_user(UserTurn::new(
            "after compaction",
            ContextSnapshot::default(),
        ));
        drain(agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap()).await;
        let requests = bodies(&http);
        for body in &requests[..3] {
            assert!(system(body, a).contains("INDEX-BEFORE"));
            assert!(!system(body, a).contains("INDEX-AFTER"));
            super::context_tests::assert_pairs(body, a);
        }
        assert_eq!(memory_calls(&requests[1], a), 1);
        assert_eq!(memory_calls(&requests[2], a), 1);
        assert!(system(&requests[3], a).contains("INDEX-AFTER"));
        assert!(system(&requests[5], a).contains("INDEX-AFTER"));
        assert_eq!(host.loads.load(Ordering::SeqCst), 3);
    }
}
#[tokio::test]
async fn queued_compaction_in_run_rebuilds_memory_on_both_wires() {
    for a in [false, true] {
        let (_dir, mut host) = fixture();
        host.compact_after_write = true;
        let http = SequencedHttpClient::new([
            response(a, &[("memory1", "memory", edit())], "", false),
            response(a, &[], "<summary>saved memory</summary>", false),
            response(a, &[], "finished", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut c = conversation();
        drain(agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap()).await;
        assert_eq!(host.loads.load(Ordering::SeqCst), 2);
        assert!(
            c.entries()
                .iter()
                .any(|e| matches!(e, Entry::Compaction(_)))
        );
        assert!(system(&bodies(&http)[2], a).contains("INDEX-AFTER"));
    }
}
#[tokio::test]
async fn replay_and_disabled_or_suggestion_sessions_use_text_without_tool_offer() {
    for a in [false, true] {
        for executable in [true, false] {
            let (_dir, host) = fixture();
            let http = SequencedHttpClient::new([
                response(a, &[("memory1", "memory", edit())], "", false),
                response(a, &[], "saved", false),
                response(a, &[], "reopened", false),
                response(a, &[], "disabled", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let mut c = conversation();
            drain(agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap())
                .await;
            // Only portable Step data survives storage; no native reasoning is needed.
            let entries =
                serde_json::from_str(&serde_json::to_string(c.entries()).unwrap()).unwrap();
            let mut reopened = Conversation::from_entries(entries);
            reopened.push_user(UserTurn::new("reopen", ContextSnapshot::default()));
            drain(
                agent::run(
                    &client,
                    &mut reopened,
                    capabilities(),
                    &host,
                    Limits::default(),
                )
                .unwrap(),
            )
            .await;
            let replay = &bodies(&http)[2];
            assert_eq!(memory_calls(replay, a), 1);
            super::context_tests::assert_pairs(replay, a);
            reopened.push_user(UserTurn::new("disabled", ContextSnapshot::default()));
            let caps = if executable {
                capabilities()
            } else {
                Capabilities::default()
            };
            drain(
                agent::run(
                    &client,
                    &mut reopened,
                    caps,
                    &host,
                    Limits {
                        auto_memory: false,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .await;
            let disabled = &bodies(&http)[3];
            assert_eq!(memory_calls(disabled, a), 0);
            assert!(!disabled["tools"].to_string().contains("\"memory\""));
            assert!(disabled["messages"].to_string().contains("已更新记忆"));
            assert!(system(disabled, a).contains("INSTRUCTIONS-ALWAYS"));
            assert!(!system(disabled, a).contains("INDEX-AFTER"));
            super::context_tests::assert_pairs(disabled, a);
        }
    }
}
#[tokio::test]
async fn unsolicited_memory_when_disabled_is_an_error_and_never_writes() {
    let (_dir, host) = fixture();
    let http = SequencedHttpClient::new([
        response(false, &[("bad", "memory", edit())], "", false),
        response(false, &[], "done", false),
    ]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let mut c = conversation();
    drain(
        agent::run(
            &client,
            &mut c,
            capabilities(),
            &host,
            Limits {
                auto_memory: false,
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    assert_eq!(
        host.files.read("/memories/global/MEMORY.md").unwrap(),
        "INDEX-BEFORE"
    );
    assert!(
        c.entries()
            .iter()
            .any(|e| matches!(e, Entry::Step(s) if s.call.name == "memory" && s.result.is_error))
    );
}

#[tokio::test]
async fn restored_log_compacts_with_auto_memory_off() {
    let (_dir, host) = fixture();
    let http =
        SequencedHttpClient::new([response(false, &[], "<summary>handoff</summary>", false)]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let mut c = conversation();
    drain(
        agent::compact_with_limits(
            &client,
            &mut c,
            capabilities(),
            &host,
            None,
            Limits {
                auto_memory: false,
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let request = c.prepare(&capabilities()).unwrap();
    let json = serde_json::to_string(&request.chat_history).unwrap();
    assert!(json.contains("INSTRUCTIONS-ALWAYS"));
    assert!(!json.contains("INDEX-BEFORE"));
    assert!(!request.tools.iter().any(|t| t.name.as_str() == "memory"));
}
