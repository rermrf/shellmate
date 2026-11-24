use super::*;
use shellmate_core::remote_instructions::{CONTEXT_KIND, Discovery, Document};
struct RemoteHost {
    discoveries: Mutex<VecDeque<Option<Discovery>>>,
    calls: Mutex<Vec<(Option<String>, bool)>>,
}
impl RemoteHost {
    fn new(discoveries: impl IntoIterator<Item = Option<Discovery>>) -> Self {
        Self {
            discoveries: Mutex::new(discoveries.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }
}
impl ToolHost for RemoteHost {
    fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        panic!("not a tool")
    }
    fn memory_snapshot(&self, auto_memory: bool) -> memory::Snapshot {
        memory::Snapshot {
            auto_memory,
            instructions: "GLOBAL-INSTRUCTIONS\nHOST-INSTRUCTIONS\n".into(),
            remote_instructions: String::new(),
            indexes: "GLOBAL-INDEX\nHOST-INDEX".into(),
        }
    }
    fn remote_instructions(
        &self,
        cwd: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, Option<Discovery>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((cwd, refresh));
            self.discoveries
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected discovery")
        })
    }
}
fn doc(path: &str, text: &str) -> Document {
    Document {
        path: path.into(),
        text: text.into(),
        bytes: text.len(),
        truncated: false,
    }
}
fn found(cwd: &str, files: Vec<Document>) -> Option<Discovery> {
    Some(Discovery {
        cancelled: false,
        cwd: cwd.into(),
        files,
    })
}
fn system(body: &Value, anthropic: bool) -> String {
    if anthropic {
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
async fn drain(stream: agent::AgentStream<'_>) -> Vec<AgentEvent> {
    let events = stream.collect::<Vec<_>>().await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    events
}
#[tokio::test]
async fn start_cwd_context_replay_and_compaction_placement_on_both_wires() {
    for a in [false, true] {
        let old = doc("/srv/AGENTS.md", "REMOTE-ROOT");
        let nested = doc("/srv/app/CLAUDE.md", "REMOTE-NESTED");
        let host = RemoteHost::new([
            found("/srv", vec![old.clone()]),
            found("/srv/app", vec![old.clone(), nested.clone()]),
            found(
                "/srv/app",
                vec![old, doc("/srv/app/CLAUDE.md", "REMOTE-REFRESHED")],
            ),
        ]);
        let http = SequencedHttpClient::new([
            response(a, &[], "first", false),
            response(a, &[], "second", false),
            response(a, &[], "third", false),
            response(a, &[], "<summary>handoff</summary>", false),
            response(a, &[], "compacted", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut caps = capabilities();
        caps.cwd = Some("/srv".into());
        let mut c = conversation();
        drain(agent::run(&client, &mut c, caps.clone(), &host, fast_limits()).unwrap()).await;
        let first = system(&bodies(&http)[0], a);
        assert!(first.find("HOST-INSTRUCTIONS").unwrap() < first.find("REMOTE-ROOT").unwrap());
        assert!(first.find("REMOTE-ROOT").unwrap() < first.find("GLOBAL-INDEX").unwrap());
        assert!(first.contains("系统规则仍优先"));
        caps.cwd = Some("/srv/app".into());
        c.push_user(UserTurn::new("second user", ContextSnapshot::default()));
        let mut ui = c.clone();
        let mut stream = agent::run(&client, &mut c, caps.clone(), &host, fast_limits()).unwrap();
        while stream.next().await.is_some() {
            for update in stream.take_updates() {
                ui.apply(update);
            }
        }
        drop(stream);
        assert!(ui == c);
        assert_eq!(system(&bodies(&http)[1], a), first);
        let position = c.entries().iter().position(|e| matches!(e, Entry::Notice { kind, text } if kind == CONTEXT_KIND && text.contains("REMOTE-NESTED"))).unwrap();
        assert!(
            matches!(&c.entries()[position + 1], Entry::User { text, .. } if text == "second user")
        );
        assert!(c.turns()[0].answer.as_ref().unwrap().items.iter().any(|item| matches!(item, AnswerItem::Notice(s) if s == "已加载远端说明 · /srv/app/CLAUDE.md")));
        let b = c
            .breakdown_for_budget(client.context_budget(), &caps)
            .unwrap();
        assert!(b.instructions_memory > 0);
        let raw = Conversation::from_entries(c.entries().to_vec())
            .breakdown_for_budget(client.context_budget(), &caps)
            .unwrap();
        assert_eq!(raw.instructions_memory, estimate_tokens(&nested.wrapped()));
        assert_eq!(
            b.categories().iter().map(|(_, n)| n).sum::<usize>(),
            b.total
        );
        c.push_user(UserTurn::new("third user", ContextSnapshot::default()));
        drain(agent::run(&client, &mut c, caps.clone(), &host, fast_limits()).unwrap()).await;
        assert_eq!(host.calls.lock().unwrap().len(), 2);
        assert_eq!(system(&bodies(&http)[2], a), first);
        for request in &bodies(&http)[1..3] {
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "user"
                        && m["content"].to_string().contains("REMOTE-NESTED"))
            );
            assert_eq!(request.to_string().matches("REMOTE-NESTED").count(), 1);
            super::context_tests::assert_pairs(request, a);
        }
        drain(agent::compact(&client, &mut c, caps.clone(), &host, None).unwrap()).await;
        c.push_user(UserTurn::new(
            "after compaction",
            ContextSnapshot::default(),
        ));
        drain(agent::run(&client, &mut c, caps.clone(), &host, fast_limits()).unwrap()).await;
        let final_body = &bodies(&http)[4];
        assert!(system(final_body, a).contains("REMOTE-REFRESHED"));
        assert!(!final_body.to_string().contains("REMOTE-NESTED"));
        assert_eq!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, r)| *r)
                .collect::<Vec<_>>(),
            [false, false, true]
        );
        // Portable context entries remain full user-role messages when restored.
        let entries: Vec<Entry> =
            serde_json::from_str(&serde_json::to_string(&c.entries()[..position + 2]).unwrap())
                .unwrap();
        let restored = Conversation::from_entries(entries);
        assert!(
            serde_json::to_string(&restored.prepare(&caps).unwrap().chat_history)
                .unwrap()
                .contains("REMOTE-NESTED")
        );
    }
}
#[tokio::test]
async fn failed_or_ignored_discovery_is_silent_and_unknown_cwd_does_not_repeat() {
    for a in [false, true] {
        let host = RemoteHost::new([None, found("/default", vec![])]);
        let http = SequencedHttpClient::new([
            response(a, &[], "first", false),
            response(a, &[], "same", false),
            response(a, &[], "new", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut caps = capabilities();
        caps.cwd = None;
        let mut c = conversation();
        drain(
            agent::run(
                &client,
                &mut c,
                caps.clone(),
                &host,
                Limits {
                    auto_memory: false,
                    ..fast_limits()
                },
            )
            .unwrap(),
        )
        .await;
        c.push_user(UserTurn::new("same cwd", ContextSnapshot::default()));
        drain(
            agent::run(
                &client,
                &mut c,
                caps.clone(),
                &host,
                Limits {
                    auto_memory: false,
                    ..fast_limits()
                },
            )
            .unwrap(),
        )
        .await;
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        let mut fresh = conversation();
        drain(agent::run(&client, &mut fresh, caps, &host, fast_limits()).unwrap()).await;
        for body in bodies(&http) {
            assert!(!body.to_string().contains("远端说明"));
        }
        assert!(!c.entries().iter().any(|e| matches!(e, Entry::Step(_))));
    }
}
#[tokio::test]
async fn tier_one_compaction_also_refreshes_remote_snapshot() {
    let http = SequencedHttpClient::new([response(false, &[], "done", false)]);
    let mut cfg = config(false);
    cfg.context_window = ContextWindow::Tokens(100_000);
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    let mut c = conversation();
    for n in 0..8 {
        c.append(Entry::Step(Step {
            message_id: n + 1,
            call: LoggedCall {
                id: format!("old{n}"),
                name: "run_command".into(),
                args_json: args("pwd"),
            },
            result: LoggedResult {
                inline: if n < 3 {
                    "line\n".repeat(30000)
                } else {
                    "small".into()
                },
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
    }
    let host = RemoteHost::new([
        found("/tmp", vec![doc("/tmp/AGENTS.md", "BEFORE")]),
        found("/tmp", vec![doc("/tmp/AGENTS.md", "AFTER")]),
    ]);
    let events =
        drain(agent::run(&client, &mut c, capabilities(), &host, fast_limits()).unwrap()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { summary: None, .. }))
    );
    assert_eq!(host.calls.lock().unwrap().len(), 2);
    assert!(system(&bodies(&http)[0], false).contains("AFTER"));
}

#[tokio::test]
async fn cancelled_trust_is_reasked_next_run_without_changing_the_snapshot() {
    let mut cancelled = found("/tmp", vec![]).unwrap();
    cancelled.cancelled = true;
    let host = RemoteHost::new([
        Some(cancelled),
        found("/tmp", vec![doc("/tmp/AGENTS.md", "TRUSTED-LATER")]),
    ]);
    let http = SequencedHttpClient::new([
        response(false, &[], "ignored this run", false),
        response(false, &[], "next", false),
    ]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let mut c = conversation();
    drain(agent::run(&client, &mut c, capabilities(), &host, fast_limits()).unwrap()).await;
    c.push_user(UserTurn::new("next", ContextSnapshot::default()));
    drain(agent::run(&client, &mut c, capabilities(), &host, fast_limits()).unwrap()).await;
    assert_eq!(host.calls.lock().unwrap().len(), 2);
    let bodies = bodies(&http);
    assert_eq!(system(&bodies[0], false), system(&bodies[1], false));
    assert!(!bodies[0].to_string().contains("TRUSTED-LATER"));
    assert!(bodies[1]["messages"].to_string().contains("TRUSTED-LATER"));
}

#[tokio::test]
async fn resume_does_not_rediscover_or_insert_before_native_tool_pairs() {
    for anthropic in [false, true] {
        let mut cancelled = found("/tmp", vec![]).unwrap();
        cancelled.cancelled = true;
        let host = RemoteHost::new([Some(cancelled)]);
        let http = SequencedHttpClient::new([
            response(
                anthropic,
                &[(
                    "memory",
                    "memory",
                    json!({"command":"view","path":"/memories"}),
                )],
                "",
                anthropic,
            ),
            response(anthropic, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(anthropic), http.clone()).unwrap();
        let mut c = conversation();
        let events = agent::run(
            &client,
            &mut c,
            capabilities(),
            &host,
            Limits {
                max_steps: Some(1),
                ..fast_limits()
            },
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        let resume = events
            .into_iter()
            .find_map(|e| {
                if let AgentEvent::Paused { resume, .. } = e {
                    Some(resume)
                } else {
                    None
                }
            })
            .unwrap();
        drain(agent::resume(&client, &mut c, &host, *resume, fast_limits()).unwrap()).await;
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        super::context_tests::assert_pairs(&bodies(&http)[1], anthropic);
    }
}
