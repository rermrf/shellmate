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
async fn channels_are_chosen_per_call_independently_of_task_waiting_on_both_wires() {
    for a in [false, true] {
        let calls = [
            ("visible-wait", "run_command", json!({"command":"ls","session_id":"first","target":"terminal","run_in_background":false})),
            ("visible-async", "run_command", json!({"command":"make test","session_id":"second","target":"terminal","run_in_background":true})),
            ("separate-wait", "run_command", json!({"command":"ls","session_id":"first","target":"background","run_in_background":false})),
            ("separate-async", "run_command", json!({"command":"make check","session_id":"second","target":"background","run_in_background":true})),
        ];
        let http = SequencedHttpClient::new([
            response(a, &calls, "", false), response(a, &[], "done", false)
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Sessions {
            inner: Host::new((0..4).map(|_| completed("result"))), requests: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        };
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("在第1和第2个终端执行 ls，让后台任务继续，然后通过独立通道检查文件", Default::default()));
        let events = agent::run(&client, &mut log, Capabilities::default(), &host, fast_limits()).unwrap().collect::<Vec<_>>().await;
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
        let seen = host.inner.calls.lock().unwrap();
        assert_eq!(seen.iter().map(|c| (c.target, c.run_in_background)).collect::<Vec<_>>(), [
            (Target::Terminal, false), (Target::Terminal, true),
            (Target::Background, false), (Target::Background, true),
        ]);
        let requests = bodies(&http);
        let tools = &requests[0]["tools"];
        let command = tools.as_array().unwrap().iter().map(|t| if a {t} else {&t["function"]}).find(|t| t["name"] == "run_command").unwrap();
        let schema = &command[if a {"input_schema"} else {"parameters"}];
        assert_eq!(schema["properties"]["target"]["enum"], json!(["terminal", "background"]));
        assert_eq!(schema["required"], json!(["command", "target"]));
        let system = requests[0].to_string();
        assert!(system.contains("无需加入操作范围"));
        assert!(system.contains("序号对应对话中用户指向的那次列表"));
    }
}

#[tokio::test]
async fn service_intent_is_async_preserves_channel_and_rejects_invalid_readiness_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    (
                        "service",
                        "run_command",
                        json!({"command":"npm run dev","target":"background","service":{"ready_pattern":"Local:"}}),
                    ),
                    (
                        "visible",
                        "run_command",
                        json!({"command":"npm run dev","target":"terminal","service":{}}),
                    ),
                    (
                        "invalid",
                        "run_command",
                        json!({"command":"npm run dev","target":"background","service":{"ready_pattern":"","startup_timeout_secs":1000}}),
                    ),
                ],
                "",
                false,
            ),
            response(a, &[], "服务已启动，继续检查访问地址", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("ready"), completed("pending")]);
        let mut log = conversation();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(|e| !matches!(e, AgentEvent::Error(_))));
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|c| c.run_in_background && c.service.is_some())
        );
        assert_eq!(calls[0].target, Target::Background);
        assert_eq!(calls[1].target, Target::Terminal);
        assert_eq!(calls[0].service.as_ref().unwrap().startup_timeout_secs, 10);
        assert!(tool_result(&bodies(&http)[1], a, "invalid").contains("service"));
        let text = bodies(&http)[0].to_string();
        assert!(text.contains("常驻服务退出"));
        assert!(text.contains("ready_pattern"));
    }
}
