use super::*;
use crate::ai::{host::Approval, session_catalog::Catalog};
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
fn config() -> Config {
    Config {
        provider: Default::default(),
        model: "fixture".into(),
        api_key: shellmate_ai::ApiKey::new("fixture"),
        reasoning: Default::default(),
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Default::default(),
    }
}
fn response(command: bool, text: &str) -> MockHttpResponse {
    let chunk = |delta: serde_json::Value, finish: serde_json::Value| serde_json::json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let mut events = vec![chunk(
        serde_json::json!({"role":"assistant","content":text}),
        serde_json::Value::Null,
    )];
    if command {
        events.push(chunk(serde_json::json!({"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"run_command","arguments":"{\"command\":\"uname -s\",\"target\":\"background\"}"}}]}),serde_json::Value::Null));
    }
    events.push(chunk(
        serde_json::json!({}),
        serde_json::json!(if command { "tool_calls" } else { "stop" }),
    ));
    MockHttpResponse::success_typed(
        events
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n",
        "text/event-stream",
    )
}
fn fixture(
    root: &std::path::Path,
    id: &str,
    session: usize,
    responses: Vec<MockHttpResponse>,
    db: SharedStore,
) -> (Arc<Runtime>, SequencedHttpClient) {
    let (ui, _) = async_channel::unbounded();
    let catalog = Catalog::new(ui.clone());
    catalog.update(vec![(
        Binding {
            session,
            connection: None,
            local: true,
            alive: Arc::new(AtomicBool::new(true)),
            capabilities: shellmate_ai::Capabilities {
                background: true,
                cwd: Some(root.to_string_lossy().into()),
                ..Default::default()
            },
        },
        format!("local-{session}"),
        "本机终端".into(),
        None,
        "window".into(),
    )]);
    let router = Router::new(catalog.clone(), db.clone(), root.into(), ui);
    router
        .add(&catalog.session(session).unwrap().target.session_id, true)
        .unwrap();
    let runtime = Runtime::new(
        id.into(),
        1,
        Conversation::default(),
        Default::default(),
        router,
        db,
        root.into(),
        Arc::new(shellmate_store::MemoryStore::default()),
    );
    let http = SequencedHttpClient::new(responses);
    let mock = http.clone();
    *runtime.factory.lock().unwrap() =
        Arc::new(move |config| shellmate_ai::RigClient::with_http(config, mock.clone()));
    (runtime, http)
}
async fn until(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn independent_conversations_keep_approvals_log_and_worker_after_view_is_gone() {
    let root = tempfile::tempdir().unwrap();
    let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
    let (a, http_a) = fixture(
        root.path(),
        "a",
        1,
        vec![response(true, "checking a"), response(false, "a done")],
        db.clone(),
    );
    let (b, http_b) = fixture(
        root.path(),
        "b",
        2,
        vec![response(false, "b done")],
        db.clone(),
    );
    let manager = crate::ai::conversation_manager::Manager::default();
    manager.insert(a.clone());
    manager.insert(b.clone());
    a.submit("task a".into(), Default::default());
    a.launch(config(), Default::default(), Default::default());
    until(|| !a.snapshot().approvals.is_empty()).await;
    b.submit("task b".into(), Default::default());
    b.launch(config(), Default::default(), Default::default());
    until(|| !b.snapshot().model.busy && !b.snapshot().model.conversation.turns().is_empty()).await;
    assert!(a.snapshot().model.busy);
    assert_eq!(http_a.requests().len(), 1);
    assert_eq!(http_b.requests().len(), 1);
    assert_eq!(
        b.snapshot()
            .model
            .conversation
            .latest_user()
            .unwrap()
            .question,
        "task b"
    );
    let reply = a
        .snapshot()
        .approvals
        .values()
        .next()
        .unwrap()
        .reply
        .clone()
        .unwrap();
    drop(a);
    let retained = manager.get("a").unwrap();
    reply.send(Approval::Deny { reason: None }).ok().unwrap();
    until(|| !retained.snapshot().model.busy).await;
    assert_eq!(http_a.requests().len(), 2);
    assert!(
        db.borrow()
            .ai_conversation("a")
            .unwrap()
            .unwrap()
            .conversation
            .turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .text()
            .contains("a done")
    );
    manager.clear();
}
#[tokio::test]
async fn new_user_turn_after_done_runs_once_without_replaying_old_tools() {
    let root = tempfile::tempdir().unwrap();
    let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
    let (r, http) = fixture(
        root.path(),
        "r",
        1,
        vec![response(false, "one"), response(false, "two")],
        db,
    );
    r.submit("first".into(), Default::default());
    r.launch(config(), Default::default(), Default::default());
    until(|| http.requests().len() == 1 && !r.snapshot().model.busy).await;
    r.submit("second".into(), Default::default());
    r.launch(config(), Default::default(), Default::default());
    until(|| http.requests().len() == 2 && !r.snapshot().model.busy).await;
    assert_eq!(r.snapshot().model.conversation.turns().len(), 2);
    assert!(r.control.snapshot().inputs.is_empty());
    r.retire();
}

#[tokio::test]
async fn failed_save_keeps_receipt_and_resumes_without_duplicate_turn_or_request() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.db");
    let db = SharedStore::new(shellmate_store::HostStore::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_runtime BEFORE INSERT ON ai_runtime BEGIN SELECT RAISE(ABORT,'test storage failure'); END;").unwrap();
    let (r, http) = fixture(
        root.path(),
        "retry",
        1,
        vec![response(false, "finished")],
        db.clone(),
    );
    r.submit("process once".into(), Default::default());
    r.launch(config(), Default::default(), Default::default());
    until(|| r.snapshot().error.is_some()).await;
    assert!(http.requests().is_empty());
    assert_eq!(r.pending_receipts.lock().unwrap().0.len(), 1);
    assert_eq!(r.snapshot().model.conversation.turns().len(), 1);
    connection
        .execute_batch("DROP TRIGGER fail_runtime;")
        .unwrap();
    r.launch(config(), Default::default(), Default::default());
    until(|| http.requests().len() == 1 && !r.snapshot().model.busy).await;
    assert!(r.pending_receipts.lock().unwrap().0.is_empty());
    assert!(r.control.snapshot().inputs.is_empty());
    assert_eq!(r.snapshot().model.conversation.turns().len(), 1);
    assert_eq!(
        db.borrow()
            .ai_conversation("retry")
            .unwrap()
            .unwrap()
            .conversation
            .turns()
            .len(),
        1
    );
    r.retire();
}

#[tokio::test]
async fn duplicate_registration_keeps_one_runtime_and_delete_cannot_recreate_history() {
    let root = tempfile::tempdir().unwrap();
    let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
    let (a, http) = fixture(
        root.path(),
        "dedup",
        1,
        vec![response(false, "done")],
        db.clone(),
    );
    let (duplicate, unused) = fixture(root.path(), "dedup", 2, vec![], db.clone());
    let manager = crate::ai::conversation_manager::Manager::default();
    manager.insert(a.clone());
    let selected = manager.insert(duplicate);
    assert!(Arc::ptr_eq(&a, &selected));
    selected.submit("only once".into(), Default::default());
    selected.launch(config(), Default::default(), Default::default());
    until(|| !selected.snapshot().model.busy && http.requests().len() == 1).await;
    assert!(unused.requests().is_empty());
    manager.remove("dedup");
    db.borrow_mut().delete_ai_conversation("dedup").unwrap();
    assert!(!a.save(&[], &[]));
    assert!(
        a.tasks
            .retained()
            .while_retained(|| panic!("deleted task wrote output"))
            .is_none()
    );
    assert!(db.borrow().ai_conversation("dedup").unwrap().is_none());
}

#[tokio::test]
async fn restored_watcher_does_not_start_a_model_until_explicit_submission() {
    let root = tempfile::tempdir().unwrap();
    let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
    let (r, http) = fixture(root.path(), "watch", 1, vec![response(false, "done")], db);
    r.watch(config(), Default::default(), Default::default());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(http.requests().is_empty());
    assert!(!r.snapshot().model.busy);
    r.submit("continue".into(), Default::default());
    r.launch(config(), Default::default(), Default::default());
    until(|| http.requests().len() == 1 && !r.snapshot().model.busy).await;
    assert_eq!(r.snapshot().model.conversation.turns().len(), 1);
    r.retire();
}
