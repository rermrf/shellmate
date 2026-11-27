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

#[tokio::test]
async fn question_answer_is_atomic_persistent_and_does_not_block_other_conversations() {
    use shellmate_ai::questions::{Answer, Mode, Pending, Question, Request, Response, Status};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("question.db");
    let db = SharedStore::new(shellmate_store::HostStore::open(&path).unwrap());
    let (r, http) = fixture(
        root.path(),
        "question-runtime",
        1,
        vec![response(false, "按答案继续")],
        db.clone(),
    );
    let request = Request {
        questions: vec![Question {
            id: "env".into(),
            question: "哪个环境？".into(),
            options: vec![],
            multi_select: false,
        }],
        mode: Mode::Wait,
    };
    let call = shellmate_ai::LoggedCall {
        id: "q".into(),
        name: "ask_user".into(),
        args_json: serde_json::to_value(&request).unwrap(),
    };
    let question = Pending::new(
        "q-runtime".into(),
        call.clone(),
        request,
        Default::default(),
    );
    r.control.register_question(question.clone()).unwrap();
    {
        let mut d = r.data.lock().unwrap();
        d.model
            .conversation
            .push_user(UserTurn::new("检查部署", Default::default()));
        d.model.conversation.append(Entry::Step(shellmate_ai::Step {
            message_id: 1,
            call,
            result: shellmate_ai::LoggedResult {
                attachments: vec![],
                subagent: None,
                inline: question.receipt(),
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
    }
    assert!(r.save(&[], &[]));
    r.launch(config(), Default::default(), Default::default());
    until(|| r.control.phase() == shellmate_ai::interaction::Phase::UserInput).await;
    assert!(http.requests().is_empty());
    let (other, other_http) = fixture(
        root.path(),
        "other-question",
        2,
        vec![response(false, "其他任务完成")],
        db.clone(),
    );
    other.submit("独立任务".into(), Default::default());
    other.launch(config(), Default::default(), Default::default());
    until(|| {
        !other.snapshot().model.busy && !other.snapshot().model.conversation.turns().is_empty()
    })
    .await;
    assert_eq!(other_http.requests().len(), 1);
    assert!(r.snapshot().model.busy);
    let answer = Response {
        status: Status::Answered,
        answers: std::collections::BTreeMap::from([(
            "env".into(),
            Answer {
                selected: vec![],
                text: "开发环境".into(),
            },
        )]),
    };
    let sql = rusqlite::Connection::open(&path).unwrap();
    sql.execute_batch("CREATE TRIGGER reject_question BEFORE UPDATE ON ai_runtime BEGIN SELECT RAISE(FAIL, 'disk full'); END;").unwrap();
    assert!(r.answer_question("q-runtime", answer.clone()).is_err());
    assert!(r.control.question("q-runtime").unwrap().response.is_none());
    assert!(!r.control.pending());
    sql.execute_batch("DROP TRIGGER reject_question;").unwrap();
    r.answer_question("q-runtime", answer.clone()).unwrap();
    assert!(r.answer_question("q-runtime", answer).is_err());
    until(|| !r.snapshot().model.busy).await;
    assert_eq!(http.requests().len(), 1);
    let saved = db
        .borrow()
        .ai_conversation("question-runtime")
        .unwrap()
        .unwrap();
    assert_eq!(
        saved
            .conversation
            .entries()
            .iter()
            .filter(|e| matches!(e, Entry::QuestionAnswer { .. }))
            .count(),
        1
    );
    assert_eq!(saved.conversation.turns().len(), 1);
    let queue: shellmate_ai::interaction::Snapshot = serde_json::from_value(
        db.borrow().ai_runtime("question-runtime").unwrap().unwrap()["queue"].clone(),
    )
    .unwrap();
    assert!(queue.inputs.is_empty());
    assert!(queue.questions[0].response.is_some());
    r.retire();
    other.retire();
}

#[tokio::test]
async fn answering_restored_question_preserves_history_provider_before_authorization() {
    use shellmate_ai::questions::{Answer, Mode, Pending, Question, Request, Response, Status};
    let root = tempfile::tempdir().unwrap();
    let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
    let (r, _) = fixture(root.path(), "restored-question", 1, vec![], db.clone());
    let request = Request {
        questions: vec![Question {
            id: "q".into(),
            question: "哪个环境？".into(),
            options: vec![],
            multi_select: false,
        }],
        mode: Mode::Wait,
    };
    let call = shellmate_ai::LoggedCall {
        id: "q".into(),
        name: "ask_user".into(),
        args_json: serde_json::to_value(&request).unwrap(),
    };
    let q = Pending::new(
        "restored-id".into(),
        call.clone(),
        request,
        Default::default(),
    );
    {
        let mut d = r.data.lock().unwrap();
        d.model
            .conversation
            .push_user(UserTurn::new("调查", Default::default()));
        d.model.conversation.append(Entry::Step(shellmate_ai::Step {
            message_id: 1,
            call,
            result: shellmate_ai::LoggedResult {
                attachments: vec![],
                subagent: None,
                inline: q.receipt(),
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
    }
    assert!(r.save(&[], &[]));
    let mut saved = db.borrow().ai_conversation(&r.id).unwrap().unwrap();
    saved.provider = "anthropic".into();
    saved.model = "saved-model".into();
    db.borrow_mut().save_ai_conversation(&saved).unwrap();
    assert!(r.control.snapshot().questions.is_empty());
    r.answer_question(
        "restored-id",
        Response {
            status: Status::Answered,
            answers: std::collections::BTreeMap::from([(
                "q".into(),
                Answer {
                    selected: vec![],
                    text: "开发".into(),
                },
            )]),
        },
    )
    .unwrap();
    let saved = db.borrow().ai_conversation(&r.id).unwrap().unwrap();
    assert_eq!(saved.provider, "anthropic");
    assert_eq!(saved.model, "saved-model");
    assert!(r.control.pending());
    r.retire();
}
