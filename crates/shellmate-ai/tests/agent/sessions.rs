use super::*;
struct Sessions {
    inner: Host,
    requests: Mutex<Vec<String>>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}
impl ToolHost for Sessions {
    fn sessions_enabled(&self) -> bool {
        true
    }
    fn model_slots(&self) -> Option<std::sync::Arc<tokio::sync::Semaphore>> {
        Some(self.slots.clone())
    }
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.inner.run(call)
    }
    fn local_tool(
        &self,
        tool: agent::LocalTool,
        _: agent::ExecLimits,
    ) -> BoxFuture<'_, agent::LocalResult> {
        Box::pin(async move {
            assert!(matches!(tool, agent::LocalTool::Sessions(_)));
            self.requests.lock().unwrap().push("sessions".into());
            agent::LocalResult::text("fixture sessions")
        })
    }
}
#[tokio::test]
async fn both_provider_wires_keep_targets_and_continue_after_one_terminal_disconnects() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(a, &[("discover", "list_sessions", json!({}))], "", false),
            response(
                a,
                &[
                    (
                        "left",
                        "run_command",
                        json!({"command":"pwd","target":"background","session_id":"local-ref"}),
                    ),
                    (
                        "right",
                        "run_command",
                        json!({"command":"pwd","target":"background","session_id":"remote-ref"}),
                    ),
                ],
                "",
                false,
            ),
            response(a, &[], "other target remains usable", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let host = Sessions {
            inner: Host::new([ToolOutcome::Disconnected, completed("remote-result")]),
            requests: Default::default(),
            slots: slots.clone(),
        };
        let mut log = conversation();
        let mut stream = agent::run(
            &client,
            &mut log,
            Capabilities::default(),
            &host,
            fast_limits(),
        )
        .unwrap();
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error(e) = event {
                panic!("{e}");
            }
        }
        drop(stream);
        assert_eq!(host.requests.lock().unwrap().len(), 1);
        let calls = host.inner.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|c| c.session_id.as_deref())
                .collect::<Vec<_>>(),
            [Some("local-ref"), Some("remote-ref")]
        );
        let bodies = bodies(&http);
        assert_eq!(bodies.len(), 3);
        let schema = bodies[0]["tools"].to_string();
        for name in [
            "list_sessions",
            "read_terminal",
            "compare_directories",
            "session_id",
        ] {
            assert!(schema.contains(name), "{schema}");
        }
        assert!(tool_result(&bodies[2], a, "right").contains("remote-result"));
        assert_eq!(slots.available_permits(), 1);
    }
}

#[tokio::test]
async fn explicit_background_request_remains_background_only_with_multiple_sessions() {
    let http = SequencedHttpClient::new([response(false, &[], "done", false)]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let host = Sessions {
        inner: Host::new([]),
        requests: Default::default(),
        slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
    };
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("请在后台执行检查", Default::default()));
    let mut stream = agent::run(
        &client,
        &mut log,
        Capabilities::default(),
        &host,
        fast_limits(),
    )
    .unwrap();
    while let Some(e) = stream.next().await {
        if let AgentEvent::Error(e) = e {
            panic!("{e}");
        }
    }
    let request = bodies(&http);
    let tool = request[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "run_command")
        .unwrap();
    assert_eq!(
        tool["function"]["parameters"]["properties"]["target"]["enum"],
        json!(["background"])
    );
}
