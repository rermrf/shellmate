use super::*;
use shellmate_ai::{
    agent::{ExecLimits, LocalResult, LocalTool},
    subagent::{self, Cancellation, ChildHost, SubagentContext, SubagentEvent},
};
use std::sync::atomic::AtomicBool;

fn request(kind: SubagentKind) -> SubagentRequest {
    SubagentRequest { image_ids: Vec::new(),
        session_ids: vec![],
        default_session_id: None,
        description: "检查入口服务".into(),
        prompt: "检查 nginx 状态，给出独立报告。即使写了终端也只用后台。".into(),
        kind,
        background: false,
        model: None,
        reasoning: None,
    }
}
fn delegation() -> Value {
    json!({"description":"检查入口服务","prompt":"检查 nginx 状态","run_in_background":false})
}
fn context() -> SubagentContext {
    SubagentContext { attachments: Vec::new(),
        sent_at_ms: None,
        origin: None,
        id: "agent-test".into(),
        snapshot: ContextSnapshot {
            label: "prod-web-01".into(),
            protocol: "SSH".into(),
            text: "CHILD-TERMINAL".into(),
        },
        cwd: Some("/srv/app".into()),
    }
}
fn snapshot() -> memory::Snapshot {
    memory::Snapshot {
        auto_memory: true,
        instructions: "\nGLOBAL-INSTRUCTIONS\nHOST-INSTRUCTIONS".into(),
        indexes: "\nGLOBAL-INDEX\nHOST-INDEX".into(),
    }
}
fn usage_response(
    a: bool,
    calls: &[(&str, &str, Value)],
    text: &str,
    input: u64,
    output: u64,
) -> MockHttpResponse {
    let body = wire(a, calls, text, false)
        .split_inclusive('\n')
        .map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return line.into();
            };
            let Ok(mut v) = serde_json::from_str::<Value>(data) else {
                return line.into();
            };
            if a && v["type"] == "message_start" {
                v["message"]["usage"]["input_tokens"] = json!(input);
            }
            if a && v["type"] == "message_delta" {
                v["usage"]["output_tokens"] = json!(output);
            }
            if !a && v["choices"][0]["finish_reason"].is_string() {
                v["usage"] = json!({"prompt_tokens":input,"completion_tokens":output});
            }
            format!("data: {v}\n")
        })
        .collect::<String>();
    MockHttpResponse::success_typed(body, "text/event-stream")
}
fn tool<'a>(body: &'a Value, anthropic: bool, name: &str) -> Option<&'a Value> {
    body["tools"]
        .as_array()?
        .iter()
        .map(|t| if anthropic { t } else { &t["function"] })
        .find(|t| t["name"] == name)
}
fn schema(tool: &Value, anthropic: bool) -> &Value {
    &tool[if anthropic {
        "input_schema"
    } else {
        "parameters"
    }]
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
struct Inner {
    seen: Mutex<Vec<(ToolCall, Option<SubagentRef>)>>,
    locals: Mutex<Vec<LocalTool>>,
    outcomes: Mutex<VecDeque<ToolOutcome>>,
    finished: AtomicUsize,
    mode: PermissionMode,
    sessions: bool,
    queue_compact: AtomicBool,
    compact_after_run: bool,
    wait: bool,
    active: AtomicUsize,
    entered: tokio::sync::Notify,
    local_text: Option<String>,
}
impl Inner {
    fn new(outcomes: impl IntoIterator<Item = ToolOutcome>) -> Self {
        Self {
            seen: Mutex::new(vec![]),
            locals: Mutex::new(vec![]),
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            finished: AtomicUsize::new(0),
            mode: PermissionMode::Ask,
            sessions: false,
            queue_compact: AtomicBool::new(false),
            compact_after_run: false,
            local_text: None,
            wait: false,
            active: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
        }
    }
}
impl ToolHost for Inner {
    fn sessions_enabled(&self) -> bool { self.sessions }
    fn subagents(&self) -> bool {
        true
    } // ChildHost must mask even a supporting inner host.
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.run_from(call, None)
    }
    fn run_from(&self, call: ToolCall, source: Option<SubagentRef>) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((call, source));
            struct Guard<'a>(&'a AtomicUsize);
            impl Drop for Guard<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            let _guard = Guard(&self.active);
            self.entered.notify_one();
            if self.wait {
                std::future::pending::<()>().await;
            }
            if self.compact_after_run {
                self.queue_compact.store(true, Ordering::SeqCst);
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| completed("CHILD-PRIVATE-OUTPUT"))
        })
    }
    fn local_tool(&self, tool: LocalTool, _: ExecLimits) -> BoxFuture<'_, LocalResult> {
        Box::pin(async move {
            self.locals.lock().unwrap().push(tool);
            LocalResult::text(self.local_text.as_deref().unwrap_or("local result"))
        })
    }
    fn permission_mode(&self) -> PermissionMode {
        self.mode
    }
    fn finish_subagent(&self, source: &SubagentRef) {
        assert_eq!(source.id, "agent-test");
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
    fn take_compaction(&self) -> Option<Option<String>> {
        self.queue_compact
            .swap(false, Ordering::SeqCst)
            .then_some(None)
    }
}
struct Parent {
    support: bool,
    child: RigClient,
    inner: Inner,
    calls: Mutex<Vec<(SubagentRequest, String)>>,
}
impl ToolHost for Parent {
    fn subagents(&self) -> bool {
        self.support
    }
    fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        panic!("parent must route agent through subagent()")
    }
    fn subagent(&self, request: SubagentRequest, call_id: &str) -> BoxFuture<'_, LocalResult> {
        self.calls
            .lock()
            .unwrap()
            .push((request.clone(), call_id.into()));
        Box::pin(async move {
            let out = subagent::run(
                &self.child,
                request,
                context(),
                snapshot(),
                &self.inner,
                fast_limits(),
                |_| {},
            )
            .await;
            let mut result =
                subagent::subagent_result(&out, ExecLimits::default(), Some("report-1".into()));
            result.subagent.as_mut().unwrap().transcript_id = Some("private-transcript-id".into());
            result
        })
    }
}
#[test]
fn schema_and_validation_cover_provider_options_and_chinese_errors() {
    for a in [false, true] {
        let cfg = config(a);
        let definition = subagent::agent_tool(&cfg);
        assert!(definition.description.contains("不要擅自添加"));
        assert!(definition.description.contains(&cfg.model));
        assert!(definition.parameters["properties"].get("type").is_none());
        assert_eq!(
            definition.parameters["required"],
            json!(["description", "prompt"])
        );
        assert_eq!(
            SubagentRequest::parse(delegation(), &cfg.provider)
                .unwrap()
                .kind,
            SubagentKind::General
        );
        for legacy in ["explore", "general"] {
            let mut args = delegation();
            args["type"] = json!(legacy);
            assert_eq!(
                SubagentRequest::parse(args, &cfg.provider).unwrap().kind,
                SubagentKind::General
            );
        }

        let ids = ReasoningLevel::options(&cfg.provider)
            .iter()
            .map(|r| json!(r.id()))
            .collect::<Vec<_>>();
        assert_eq!(
            definition.parameters["properties"]["reasoning"]["enum"],
            json!(ids)
        );
        for level in ReasoningLevel::options(&cfg.provider) {
            assert!(definition.description.contains(level.id()));
        }
        for (key, value) in [
            ("description", json!("  ")),
            ("prompt", json!("\n ")),
            ("type", json!("other")),
            ("model", json!("")),
            ("model", json!("x".repeat(257))),
            ("model", json!("bad\nmodel")),
            ("reasoning", json!("unknown")),
            ("run_in_background", json!("true")),
        ] {
            let mut args = delegation();
            args[key] = value;
            let error = SubagentRequest::parse(args, &cfg.provider).unwrap_err();
            assert!(
                error.contains("子任务") || error.contains("服务商"),
                "{error}"
            );
        }
        for id in ["minimal", "xhigh"] {
            let mut args = delegation();
            args["reasoning"] = json!(id);
            assert_eq!(SubagentRequest::parse(args, &cfg.provider).is_ok(), !a);
        }
        let unicode_model = "模".repeat(256);
        let mut unicode_args = delegation();
        unicode_args["model"] = json!(unicode_model);
        assert!(SubagentRequest::parse(unicode_args, &cfg.provider).is_ok());
        assert!(
            cfg.with_override(Some(&unicode_model), None)
                .validate()
                .is_ok()
        );
        assert!(
            cfg.with_override(Some(&"模".repeat(257)), None)
                .validate()
                .is_err()
        );
        let mut args = delegation();
        args["description"] = json!(format!("  {}  ", "长".repeat(35)));
        let parsed = SubagentRequest::parse(args, &cfg.provider).unwrap();
        assert_eq!(parsed.description.chars().count(), 30);
        assert!(parsed.description.ends_with('…'));
        let mut args = delegation();
        args["description"] = json!("  简短标题  ");
        args["run_in_background"] = json!(true);
        args["model"] = json!("  custom-child  ");
        let parsed = SubagentRequest::parse(args, &cfg.provider).unwrap();
        assert_eq!(parsed.description, "简短标题");
        assert!(parsed.background);
        assert_eq!(parsed.model.as_deref(), Some("custom-child"));
    }
}
#[tokio::test]
async fn availability_requires_background_main_and_host_opt_in() {
    for a in [false, true] {
        for support in [false, true] {
            for background in [false, true] {
                let http = SequencedHttpClient::new([response(a, &[], "done", false)]);
                let child = RigClient::with_http(&config(a), SequencedHttpClient::new([])).unwrap();
                let host = Parent {
                    support,
                    child,
                    inner: Inner::new([]),
                    calls: Mutex::new(vec![]),
                };
                let client = RigClient::with_http(&config(a), http.clone()).unwrap();
                let mut log = conversation();
                let caps = Capabilities {
                    terminal: true,
                    background,
                    ..Default::default()
                };
                let events = agent::run(&client, &mut log, caps.clone(), &host, fast_limits())
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await;
                assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                let offered = support && background;
                assert_eq!(tool(&bodies(&http)[0], a, "agent").is_some(), offered);
                assert_eq!(
                    log.breakdown(&client, &caps).unwrap().tool_count,
                    if offered { 8 } else { 6 }
                );
                assert!(host.calls.lock().unwrap().is_empty());
            }
        }
    }
}
#[tokio::test]
async fn isolation_tools_snapshot_and_auto_memory_on_both_wires() {
    for a in [false, true] {
        for enabled in [false, true] {
            let http = SequencedHttpClient::new([usage_response(
                a,
                &[],
                "结论：完成\n关键证据：ok\n执行过的变更：无\n未完成或风险：无",
                123,
                45,
            )]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let inner = Inner::new([]);
            let mut events = vec![];
            let out = subagent::run(
                &client,
                request(SubagentKind::Explore),
                context(),
                snapshot(),
                &inner,
                Limits {
                    auto_memory: enabled,
                    ..fast_limits()
                },
                |event| {
                    events.push(match event {
                        SubagentEvent::Started { .. } => "start",
                        SubagentEvent::Agent(AgentEvent::TextDelta(_)) => "text",
                        SubagentEvent::Finished(_) => "finish",
                        _ => "event",
                    })
                },
            )
            .await;
            assert_eq!(out.status, SubagentStatus::Completed);
            assert_eq!(out.steps, 0);
            assert_eq!(
                out.usage,
                Usage {
                    input_tokens: Some(123),
                    output_tokens: Some(45)
                }
            );
            assert!(out.report.starts_with("结论"));
            assert_eq!(events.first(), Some(&"start"));
            assert_eq!(events.last(), Some(&"finish"));
            assert!(events.contains(&"text"));
            assert_eq!(inner.finished.load(Ordering::SeqCst), 1);
            let body = &bodies(&http)[0];
            let sys = system(body, a);
            for expected in [
                "子任务助手",
                "prod-web-01",
                "SSH",
                "/srv/app",
                "交回主代理协调",
                "结论 / 关键证据 / 执行过的变更 / 未完成或风险",
                "GLOBAL-INSTRUCTIONS",
                "HOST-INSTRUCTIONS",
            ] {
                assert!(sys.contains(expected), "{expected}: {sys}");
            }
            assert_eq!(sys.contains("GLOBAL-INDEX"), enabled);
            assert_eq!(sys.contains("HOST-INDEX"), enabled);
            let names: Vec<_> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    if a {
                        t["name"].as_str().unwrap()
                    } else {
                        t["function"]["name"].as_str().unwrap()
                    }
                })
                .collect();
            assert_eq!(names.len(), if enabled { 6 } else { 5 });
            assert!(!names.contains(&"agent") && !names.contains(&"propose_command"));
            assert_eq!(
                schema(tool(body, a, "run_command").unwrap(), a)["properties"]["target"]["enum"],
                json!(["background"])
            );
            assert!(tool(body, a, "read_output").is_some() && tool(body, a, "stop_task").is_some());
            if enabled {
                assert_eq!(
                    schema(tool(body, a, "memory").unwrap(), a)["properties"]["command"]["enum"],
                    json!([
                        "view",
                        "create",
                        "str_replace",
                        "insert",
                        "delete",
                        "rename"
                    ])
                );
            } else {
                assert!(tool(body, a, "memory").is_none());
            }
            assert!(body["messages"].to_string().contains("检查 nginx"));
            assert!(!body.to_string().contains("MAIN-HISTORY-SECRET"));
            assert_eq!(out.transcript.turns().len(), 1);
        }
    }
}
#[tokio::test]
async fn all_child_commands_reach_the_common_host_policy_in_every_mode() {
    for a in [false, true] {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::Full,
        ] {
            for kind in [SubagentKind::Explore, SubagentKind::General] {
                let commands = [
                    "pwd",
                    "/bin/pwd",
                    "/usr/bin/pwd",
                    "/bin/ls -la",
                    "bash -lc 'pwd; ls -la'",
                    "touch /tmp/fixture",
                    "reboot",
                ];
                let calls: Vec<_> = commands
                    .iter()
                    .enumerate()
                    .map(|(i, command)| (format!("call-{i}"), json!({"command":command})))
                    .collect();
                let wire_calls: Vec<_> = calls
                    .iter()
                    .map(|(id, args)| (id.as_str(), "run_command", args.clone()))
                    .collect();
                let http = SequencedHttpClient::new([
                    response(a, &wire_calls, "", false),
                    response(a, &[], "report", false),
                ]);
                let client = RigClient::with_http(&config(a), http.clone()).unwrap();
                let mut inner = Inner::new([]);
                inner.mode = mode;
                let out = subagent::run(
                    &client,
                    request(kind),
                    context(),
                    snapshot(),
                    &inner,
                    fast_limits(),
                    |_| {},
                )
                .await;
                assert_eq!(out.status, SubagentStatus::Completed);
                let seen = inner.seen.lock().unwrap();
                assert_eq!(seen.len(), commands.len());
                for ((call, source), command) in seen.iter().zip(commands) {
                    assert_eq!(call.command, command);
                    assert_eq!(call.class, shellmate_ai::policy::classify(command));
                    assert_eq!(call.target, Target::Background);
                    assert_eq!(source.as_ref().unwrap().kind, SubagentKind::General);
                }
                assert!(!bodies(&http)[1].to_string().contains("只读子任务不能执行"));
            }
        }
    }
    // A caller cannot mislabel a command as ReadOnly to bypass the common approval policy.
    let inner = Inner::new([]);
    let child = ChildHost::new(
        &inner,
        SubagentRef {
            id: "test".into(),
            description: "legacy".into(),
            kind: SubagentKind::Explore,
        },
        snapshot(),
    );
    let call = ToolCall {
        service: None,
        session_id: None,
        cwd: None, destination: None,
        id: "fake".into(),
        command: "rm -rf /".into(),
        purpose: String::new(),
        target: Target::Terminal,
        timeout: Duration::from_secs(1),
        run_in_background: false,
        exec_limits: ExecLimits::default(),
        class: Class::ReadOnly,
    };
    child.run(call).await;
    assert_eq!(inner.seen.lock().unwrap()[0].0.class, Class::RedLine);
}
#[tokio::test]
async fn legacy_host_passes_source_denial_and_background_capability() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("write", "run_command", json!({"command":"touch /tmp/a"}))],
                "",
                false,
            ),
            response(a, &[], "报告：用户拒绝，未变更", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let inner = Inner::new([ToolOutcome::Denied {
            reason: Some("maintenance freeze".into()),
        }]);
        let out = subagent::run(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |_| {},
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Completed);
        let calls = inner.seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.target, Target::Background);
        assert_eq!(calls[0].1.as_ref().unwrap().description, "检查入口服务");
        assert!(tool_result(&bodies(&http)[1], a, "write").contains("maintenance freeze"));
    }
}
#[tokio::test]
async fn memory_writes_are_inherited_while_outputs_and_execution_targets_remain_scoped() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    (
                        "write",
                        "memory",
                        json!({"command":"create","path":"/memories/global/no.md","file_text":"bad"}),
                    ),
                    ("nested", "agent", delegation()),
                ],
                "",
                false,
            ),
            response(
                a,
                &[
                    (
                        "view",
                        "memory",
                        json!({"command":"view","path":"/memories"}),
                    ),
                    (
                        "terminal",
                        "run_command",
                        json!({"command":"pwd","target":"terminal"}),
                    ),
                ],
                "",
                false,
            ),
            response(
                a,
                &[("other", "read_output", json!({"id":"parent-output"}))],
                "",
                false,
            ),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let inner = Inner::new([]);
        let out = subagent::run(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |_| {},
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Completed);
        assert!(inner.seen.lock().unwrap().is_empty());
        assert_eq!(inner.locals.lock().unwrap().len(), 2);
        assert!(tool_result(&bodies(&http)[1], a, "write").contains("local result"));
        assert!(
            bodies(&http)[1]["messages"]
                .to_string()
                .contains("不支持子任务")
        );
        assert!(tool_result(&bodies(&http)[3], a, "other").contains("自己的输出"));
    }
    let mut output = output::spooled(
        "child-output",
        "pwd",
        b"ok",
        2,
        Some(0),
        Duration::ZERO,
        ExecLimits::default(),
    );
    output.task_id = Some("child-task".into());
    let inner = Inner::new([ToolOutcome::Completed(output)]);
    let child = ChildHost::new(
        &inner,
        SubagentRef {
            id: "test".into(),
            description: "child".into(),
            kind: SubagentKind::General,
        },
        snapshot(),
    );
    child
        .run(ToolCall {
            service: None,
            session_id: None,
            cwd: None, destination: None,
            id: "cmd".into(),
            command: "pwd".into(),
            purpose: String::new(),
            target: Target::Terminal,
            timeout: Duration::from_secs(1),
            run_in_background: false,
            exec_limits: ExecLimits::default(),
            class: Class::Other,
        })
        .await;
    assert_eq!(inner.seen.lock().unwrap()[0].0.target, Target::Terminal);
    for id in ["child-output", "child-task"] {
        assert!(
            !child
                .local_tool(
                    LocalTool::ReadOutput(agent::ReadOutput {
                        id: id.into(),
                        offset_line: 0,
                        limit_lines: None,
                        grep: None
                    }),
                    ExecLimits::default()
                )
                .await
                .is_error
        );
    }
    assert!(
        !child
            .local_tool(
                LocalTool::StopTask {
                    id: "child-task".into()
                },
                ExecLimits::default()
            )
            .await
            .is_error
    );
    assert!(
        child
            .local_tool(
                LocalTool::StopTask {
                    id: "parent-task".into()
                },
                ExecLimits::default()
            )
            .await
            .is_error
    );
}

#[tokio::test]
async fn main_routes_logs_replays_and_accounts_for_only_the_returned_report() {
    for a in [false, true] {
        let child_http = SequencedHttpClient::new([
            usage_response(
                a,
                &[("inspect", "run_command", json!({"command":"uname -s"}))],
                "child earlier analysis",
                900000,
                111,
            ),
            usage_response(a, &[], "CHILD-FINAL-REPORT", 800000, 222),
        ]);
        let mut child_config = config(a);
        child_config.context_window = ContextWindow::Tokens(10_000_000);
        let child = RigClient::with_http(&child_config, child_http.clone()).unwrap();
        let host = Parent {
            support: true,
            child,
            inner: Inner::new([]),
            calls: Mutex::new(vec![]),
        };
        let http = SequencedHttpClient::new([
            usage_response(
                a,
                &[("delegate", "agent", delegation())],
                "MAIN-HISTORY-SECRET",
                100,
                10,
            ),
            usage_response(a, &[], "main done", 200, 20),
            response(a, &[], "history replay", false),
            response(a, &[], "tool disabled", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut log = conversation();
        log.push_text("MAIN-HISTORY-SECRET");
        log.push_user(UserTurn::new("delegate", ContextSnapshot::default()));
        let mut ui = log.clone();
        let mut stream =
            agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
        while let Some(event) = stream.next().await {
            assert!(!matches!(
                event,
                AgentEvent::Error(_) | AgentEvent::Paused { .. }
            ));
            for update in stream.take_updates() {
                ui.apply(update);
            }
        }
        drop(stream);
        assert!(log == ui);
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        assert_eq!(host.calls.lock().unwrap()[0].1, "delegate");
        assert!(
            child_http
                .requests()
                .iter()
                .all(|r| !String::from_utf8_lossy(&r.body).contains("MAIN-HISTORY-SECRET"))
        );
        let child_text = child_http.requests()[0].body.clone();
        assert!(!String::from_utf8_lossy(&child_text).contains("main done"));
        let step = log
            .entries()
            .iter()
            .find_map(|e| {
                if let Entry::Step(s) = e {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(step.call.name, "agent");
        let display = step.result.subagent.as_ref().unwrap();
        assert_eq!(display.status, SubagentStatus::Completed);
        assert_eq!(display.steps, 1);
        assert_eq!(
            display.usage,
            Usage {
                input_tokens: Some(1700000),
                output_tokens: Some(333)
            }
        );
        assert_eq!(display.output_id.as_deref(), Some("report-1"));
        assert_eq!(
            display.transcript_id.as_deref(),
            Some("private-transcript-id")
        );
        assert!(
            log.turns()
                .last()
                .unwrap()
                .answer
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|i| matches!(i,AnswerItem::Subagent{display,..} if display.steps==1))
        );
        let main_second = &bodies(&http)[1];
        let result = tool_result(main_second, a, "delegate");
        assert!(result.contains("CHILD-FINAL-REPORT"));
        assert!(
            !result.contains("child earlier analysis") && !result.contains("CHILD-PRIVATE-OUTPUT")
        );
        assert!(!main_second.to_string().contains("private-transcript-id"));
        let breakdown = log.breakdown(&client, &capabilities()).unwrap();
        assert_eq!(breakdown.total, 220);
        assert_eq!(breakdown.tool_count, 8);
        assert!(breakdown.tool_results > 0);
        let ui_breakdown = ui.breakdown(&client, &capabilities()).unwrap();
        assert_eq!(breakdown, ui_breakdown);
        let json = serde_json::to_string(log.entries()).unwrap();
        let mut reopened = Conversation::from_entries(serde_json::from_str(&json).unwrap());
        reopened.push_user(UserTurn::new("replay", ContextSnapshot::default()));
        let events = agent::run(&client, &mut reopened, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(tool_result(&bodies(&http)[2], a, "delegate").contains("CHILD-FINAL-REPORT"));
        super::context_tests::assert_pairs(&bodies(&http)[2], a);
        let no_host = Host::new([]);
        reopened.push_user(UserTurn::new(
            "tool no longer offered",
            ContextSnapshot::default(),
        ));
        let events = agent::run(
            &client,
            &mut reopened,
            capabilities(),
            &no_host,
            fast_limits(),
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(tool(&bodies(&http)[3], a, "agent").is_none());
        assert!(
            bodies(&http)[3]["messages"]
                .to_string()
                .contains("CHILD-FINAL-REPORT")
        );
        let mut cleared = step.clone();
        cleared.result.cleared = true;
        assert!(cleared.wire_text().contains("read_output(id=\"report-1\")"));
        assert!(!cleared.wire_text().contains("private-transcript-id"));
    }
}
#[test]
fn old_steps_without_subagent_data_deserialize_and_projection_roundtrips() {
    let value = json!({"entry":"step","message_id":1,"call":{"id":"old","name":"read_output","args_json":{"id":"output"}},"result":{"inline":"old report","output_id":null,"cleared":false,"is_error":false},"display":null});
    let entry: Entry = serde_json::from_value(value).unwrap();
    let Entry::Step(step) = &entry else {
        panic!("step")
    };
    assert!(step.result.subagent.is_none());
    assert!(
        serde_json::to_value(&entry).unwrap()["result"]
            .get("subagent")
            .is_none()
    );
    let mut log = conversation();
    log.append(entry);
    assert!(matches!(
        &log.turns()[0].answer.as_ref().unwrap().items[0],
        AnswerItem::Notice(_)
    ));
}
#[tokio::test]
async fn invalid_agent_arguments_are_tool_errors_and_do_not_call_the_host() {
    for a in [false, true] {
        let mut invalid = delegation();
        invalid["prompt"] = json!("");
        let http = SequencedHttpClient::new([
            response(a, &[("bad", "agent", invalid)], "", false),
            response(a, &[], "corrected", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let child = RigClient::with_http(&config(a), SequencedHttpClient::new([])).unwrap();
        let host = Parent {
            support: true,
            child,
            inner: Inner::new([]),
            calls: Mutex::new(vec![]),
        };
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(host.calls.lock().unwrap().is_empty());
        assert!(tool_result(&bodies(&http)[1], a, "bad").contains("prompt 不能为空"));
        assert!(
            c.entries()
                .iter()
                .any(|e| matches!(e,Entry::Step(s) if s.result.is_error))
        );
    }
    let host = Host::new([]);
    let result = host.subagent(request(SubagentKind::Explore), "id").await;
    assert!(result.is_error);
    assert_eq!(result.inline, "当前会话不支持子任务");
}
#[tokio::test]
async fn step_limit_failure_retry_partial_report_and_footer_truncation() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([usage_response(
            a,
            &[("one", "run_command", json!({"command":"uname -s"}))],
            "",
            11,
            7,
        )]);
        let client = RigClient::with_http(&config(a), http).unwrap();
        let inner = Inner::new([]);
        let mut out = subagent::run(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            Limits {
                max_steps: Some(1),
                ..fast_limits()
            },
            |_| {},
        )
        .await;
        assert_eq!(out.status, SubagentStatus::StepLimit);
        assert_eq!(out.steps, 1);
        assert!(out.report.contains("uname -s"));
        assert!(out.report.ends_with("（达到步数上限，结果不完整）"));
        out.duration = Duration::from_secs(12);
        out.reasoning = ReasoningLevel::High;
        let result = subagent::subagent_result(&out, ExecLimits::default(), Some("report".into()));
        assert!(
            result
                .inline
                .contains("步数 1 · 耗时 12s · 输入/输出 token 11/7 · 模型 ")
        );
        assert!(result.inline.ends_with(" · 思考程度 high"));
        assert_eq!(result.output_id.as_deref(), Some("report"));
        out.status = SubagentStatus::Completed;
        out.report = format!("HEAD{}TAIL", "中".repeat(10000));
        let limits = ExecLimits {
            tool_output_chars: 1000,
            ..Default::default()
        };
        let result = subagent::subagent_result(&out, limits, Some("report".into()));
        let expected = output::spooled(
            "report",
            "subagent",
            out.report.as_bytes(),
            out.report.len() as u64,
            Some(0),
            out.duration,
            limits,
        );
        assert!(result.inline.starts_with(expected.stdout()));
        assert!(result.inline.contains("完整输出已保存"));
        out.status = SubagentStatus::Failed(AiError::Authentication);
        let result = subagent::subagent_result(&out, limits, None);
        assert!(result.is_error);
        assert!(result.inline.starts_with("子任务失败：Key 无效或无权限"));
        assert!(result.inline.contains("省略中间输出"));
        assert!(result.inline.contains("HEAD") && result.inline.contains("TAIL"));
        let http = SequencedHttpClient::new([
            http_error(503, "service unavailable"),
            usage_response(
                a,
                &[("one", "run_command", json!({"command":"uname -s"}))],
                "PARTIAL-FINDING",
                17,
                3,
            ),
            http_error(401, "bad key"),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let inner = Inner::new([]);
        let mut retries = 0;
        let out = subagent::run(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |event| {
                if matches!(event, SubagentEvent::Agent(AgentEvent::Retrying { .. })) {
                    retries += 1;
                }
            },
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Failed(AiError::Authentication));
        assert_eq!(out.report, "PARTIAL-FINDING");
        assert_eq!(out.steps, 1);
        assert_eq!(retries, 1);
        assert_eq!(http.requests().len(), 3);
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("one", "run_command", json!({"command":"uname -s"}))],
                "",
                false,
            ),
            http_error(401, "bad key"),
        ]);
        let client = RigClient::with_http(&config(a), http).unwrap();
        let inner = Inner::new([]);
        let out = subagent::run(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |_| {},
        )
        .await;
        assert!(out.report.contains("已执行命令：") && out.report.contains("uname -s"));
    }
}
#[tokio::test]
async fn cancellation_and_dropping_future_release_host_work() {
    for drop_future in [false, true] {
        let http = SequencedHttpClient::new([usage_response(
            false,
            &[("wait", "run_command", json!({"command":"sleep 30"}))],
            "PARTIAL-BEFORE-CANCEL",
            15,
            5,
        )]);
        let client = RigClient::with_http(&config(false), http).unwrap();
        let mut inner = Inner::new([]);
        inner.wait = true;
        let cancel = Cancellation::default();
        let mut run = Box::pin(subagent::run_with_cancel(
            &client,
            request(SubagentKind::General),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            &cancel,
            |_| {},
        ));
        tokio::select! {out=&mut run=>panic!("ended unexpectedly: {:?}",out.status),()=inner.entered.notified()=>{}}
        assert_eq!(inner.active.load(Ordering::SeqCst), 1);
        if drop_future {
            drop(run);
        } else {
            cancel.cancel();
            let out = run.await;
            assert_eq!(out.status, SubagentStatus::Stopped);
            assert_eq!(out.report, "PARTIAL-BEFORE-CANCEL");
            assert_eq!(
                out.usage,
                Usage {
                    input_tokens: Some(15),
                    output_tokens: Some(5)
                }
            );
            assert!(
                subagent::subagent_result(&out, ExecLimits::default(), None)
                    .inline
                    .starts_with("子任务已停止")
            );
        }
        assert_eq!(inner.active.load(Ordering::SeqCst), 0);
        assert_eq!(inner.finished.load(Ordering::SeqCst), 1);
    }
    let cancel = Cancellation::default();
    cancel.cancel();
    let http = SequencedHttpClient::new([]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let inner = Inner::new([]);
    let out = subagent::run_with_cancel(
        &client,
        request(SubagentKind::Explore),
        context(),
        snapshot(),
        &inner,
        fast_limits(),
        &cancel,
        |_| {},
    )
    .await;
    assert_eq!(out.status, SubagentStatus::Stopped);
    assert!(http.requests().is_empty());
}
#[tokio::test]
async fn config_override_is_provider_local_and_child_wire_uses_effective_model_reasoning() {
    for a in [false, true] {
        let mut main = config(a);
        main.reasoning = ReasoningLevel::Low;
        let inherited = main.with_override(None, None);
        assert_eq!(inherited.model, main.model);
        assert_eq!(inherited.reasoning, main.reasoning);
        let mapped = main.with_override(None, Some(ReasoningLevel::XHigh));
        assert_eq!(
            mapped.reasoning,
            if a {
                ReasoningLevel::Default
            } else {
                ReasoningLevel::XHigh
            }
        );
        for override_requested in [false, true] {
            let mut req = request(SubagentKind::General);
            if override_requested {
                req.model = Some(if a { "claude-opus-4-6" } else { "gpt-5" }.into());
                req.reasoning = Some(ReasoningLevel::High);
            }
            let effective = main.with_override(req.model.as_deref(), req.reasoning);
            assert_eq!(effective.provider, main.provider);
            assert_eq!(effective.api_key.expose(), main.api_key.expose());
            assert_eq!(effective.context_window, main.context_window);
            let http = SequencedHttpClient::new([response(a, &[], "report", false)]);
            let client = RigClient::with_http(&effective, http.clone()).unwrap();
            let inner = Inner::new([]);
            let out = subagent::run(
                &client,
                req,
                context(),
                snapshot(),
                &inner,
                fast_limits(),
                |_| {},
            )
            .await;
            assert_eq!(out.model, effective.model);
            assert_eq!(out.reasoning, effective.reasoning);
            let body = &bodies(&http)[0];
            assert_eq!(body["model"], effective.model);
            if a {
                assert_eq!(body["output_config"]["effort"], effective.reasoning.id());
            } else {
                assert_eq!(body["reasoning_effort"], effective.reasoning.id());
            }
            let display = subagent::subagent_result(&out, ExecLimits::default(), None)
                .subagent
                .unwrap();
            assert_eq!(display.model_overridden, override_requested);
            assert_eq!(display.reasoning_overridden, override_requested);
        }
    }
}
#[tokio::test]
async fn child_compaction_rebuilds_its_prompt_and_sums_summary_usage() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            usage_response(
                a,
                &[("one", "run_command", json!({"command":"uname -s"}))],
                "before",
                50,
                5,
            ),
            usage_response(a, &[], "<summary>CHILD-HANDOFF</summary>", 100, 10),
            usage_response(a, &[], "FINAL-AFTER-COMPACTION", 200, 20),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut inner = Inner::new([]);
        inner.compact_after_run = true;
        let mut compacted = false;
        let out = subagent::run(
            &client,
            request(SubagentKind::Explore),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |event| {
                if matches!(
                    event,
                    SubagentEvent::Agent(AgentEvent::Compacted {
                        summary: Some(_),
                        ..
                    })
                ) {
                    compacted = true;
                }
            },
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Completed);
        assert!(compacted);
        assert_eq!(out.report, "FINAL-AFTER-COMPACTION");
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: Some(350),
                output_tokens: Some(35)
            }
        );
        assert!(
            out.transcript
                .entries()
                .iter()
                .any(|e| matches!(e, Entry::Compaction(_)))
        );
        let body = &bodies(&http)[2];
        assert!(body.to_string().contains("CHILD-HANDOFF"));
        assert!(system(body, a).contains("子任务助手"));
        assert!(system(body, a).contains("GLOBAL-INSTRUCTIONS"));
        assert!(tool(body, a, "agent").is_none());
    }
}

#[tokio::test]
async fn child_tier_one_clears_older_results_without_changing_tools_or_memory() {
    for a in [false, true] {
        let ids: Vec<_> = (0..12).map(|n| format!("read{n}")).collect();
        let calls: Vec<_> = ids
            .iter()
            .map(|id| {
                (
                    id.as_str(),
                    "memory",
                    json!({"command":"view","path":"/memories/global/MEMORY.md"}),
                )
            })
            .collect();
        let http = SequencedHttpClient::new([
            usage_response(a, &calls, "", 100, 10),
            usage_response(a, &[], "after tier one", 200, 20),
        ]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(128000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut inner = Inner::new([]);
        // After clearing seven results, five must fit with the current tool schema.
        inner.local_text = Some("line\n".repeat(7500));
        let mut tier_one = false;
        let out = subagent::run(
            &client,
            request(SubagentKind::Explore),
            context(),
            snapshot(),
            &inner,
            fast_limits(),
            |event| {
                if matches!(
                    event,
                    SubagentEvent::Agent(AgentEvent::Compacted { summary: None, .. })
                ) {
                    tier_one = true;
                }
            },
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Completed);
        assert!(tier_one);
        assert_eq!(out.steps, 12);
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: Some(300),
                output_tokens: Some(30)
            }
        );
        assert_eq!(
            out.transcript
                .entries()
                .iter()
                .filter(|e| matches!(e,Entry::Step(s) if s.result.cleared))
                .count(),
            7
        );
        assert!(tool(&bodies(&http)[1], a, "agent").is_none());
        assert!(system(&bodies(&http)[1], a).contains("GLOBAL-INDEX"));
    }
}

#[tokio::test]
async fn main_agent_steps_are_cleared_by_tier_one_and_background_is_host_scheduled() {
    struct Reports;
    impl ToolHost for Reports {
        fn subagents(&self) -> bool {
            true
        }
        fn run(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            panic!("not a command")
        }
        fn subagent(&self, request: SubagentRequest, _: &str) -> BoxFuture<'_, LocalResult> {
            assert!(request.background);
            Box::pin(async {
                LocalResult { attachments: Vec::new(),
                    // Leave room for system/tool guidance after retaining the newest five reports.
                    inline: "report\n".repeat(5000),
                    output_id: Some("saved-report".into()),
                    is_error: false,
                    subagent: None,
                }
            })
        }
    }
    for a in [false, true] {
        let ids: Vec<_> = (0..12).map(|n| format!("delegate{n}")).collect();
        let calls: Vec<_> = ids
            .iter()
            .map(|id| {
                let mut args = delegation();
                args["run_in_background"] = json!(true);
                (id.as_str(), "agent", args)
            })
            .collect();
        let http = SequencedHttpClient::new([
            usage_response(a, &calls, "", 100, 10),
            usage_response(a, &[], "finished", 200, 20),
        ]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(128000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut c = conversation();
        let events = agent::run(&client, &mut c, capabilities(), &Reports, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compacted { summary: None, .. }))
        );
        let cleared: Vec<_> = c
            .entries()
            .iter()
            .filter_map(|e| match e {
                Entry::Step(s) if s.result.cleared => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(cleared.len(), 7);
        assert!(
            cleared
                .iter()
                .all(|s| s.result.subagent.as_ref().unwrap().status == SubagentStatus::Running)
        );
        let cleared_result = tool_result(&bodies(&http)[1], a, "delegate0");
        assert!(cleared_result.contains("read_output") && cleared_result.contains("saved-report"));
    }
}
#[tokio::test]
async fn unoffered_agent_calls_and_disabled_child_memory_never_execute() {
    for a in [false, true] {
        for support in [false, true] {
            let http = SequencedHttpClient::new([
                response(a, &[("bad", "agent", delegation())], "", false),
                response(a, &[], "done", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let child = RigClient::with_http(&config(a), SequencedHttpClient::new([])).unwrap();
            let host = Parent {
                support,
                child,
                inner: Inner::new([]),
                calls: Mutex::new(vec![]),
            };
            let mut c = conversation();
            let caps = Capabilities {
                terminal: true,
                background: !support,
                ..Default::default()
            };
            let events = agent::run(&client, &mut c, caps, &host, fast_limits())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            assert!(host.calls.lock().unwrap().is_empty());
            assert!(
                bodies(&http)[1]["messages"]
                    .to_string()
                    .contains("不支持子任务")
            );
        }
        let http = SequencedHttpClient::new([
            response(
                a,
                &[(
                    "bad",
                    "memory",
                    json!({"command":"view","path":"/memories"}),
                )],
                "",
                false,
            ),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let inner = Inner::new([]);
        let out = subagent::run(
            &client,
            request(SubagentKind::Explore),
            context(),
            snapshot(),
            &inner,
            Limits {
                auto_memory: false,
                ..fast_limits()
            },
            |_| {},
        )
        .await;
        assert_eq!(out.status, SubagentStatus::Completed);
        assert!(inner.locals.lock().unwrap().is_empty());
        assert!(
            bodies(&http)[1]["messages"]
                .to_string()
                .contains("自动记忆已关闭")
        );
    }
}

#[tokio::test]
async fn child_can_choose_both_channels_with_async_task_on_both_providers() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(a, &[
                ("visible", "run_command", json!({"command":"ls","session_id":"first","target":"terminal","run_in_background":true})),
                ("separate", "run_command", json!({"command":"ls","session_id":"second","target":"background","run_in_background":false})),
            ], "", false),
            response(a, &[], "done", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut inner = Inner::new([]);
        inner.sessions = true;
        let mut delegated = request(SubagentKind::General);
        delegated.background = true;
        delegated.prompt = "在第1个终端执行 ls 让我切过去看，再通过独立通道检查第2个目录".into();
        let out = subagent::run(&client, delegated, context(), snapshot(), &inner, fast_limits(), |_| {}).await;
        assert_eq!(out.status, SubagentStatus::Completed);
        let calls = inner.seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0.target, calls[0].0.run_in_background), (Target::Terminal, true));
        assert_eq!((calls[1].0.target, calls[1].0.run_in_background), (Target::Background, false));
        assert_eq!(calls[0].0.session_id.as_deref(), Some("first"));
        assert!(calls.iter().all(|(_, source)| source.is_some()));
        let requests = bodies(&http);
        assert!(requests[0].to_string().contains("普通检查、日期计算、ping、sleep 等辅助命令优先 background"));
        assert!(requests[0].to_string().contains("在第1个终端执行 ls 让我切过去看"));
        assert!(!requests[0]["tools"].to_string().contains("compare_directories"));
        assert!(requests[0]["tools"].to_string().contains("list_sessions"));
        assert_eq!(schema(tool(&requests[0], a, "run_command").unwrap(), a)["properties"]["target"]["enum"], json!(["terminal", "background"]));
        let steps = out.transcript.entries().iter().filter_map(|entry| if let Entry::Step(s)=entry {Some(s)}else{None}).filter(|s| s.call.name == "run_command").collect::<Vec<_>>();
        assert_eq!(steps[0].display.as_ref().unwrap().target, Target::Terminal);
        assert_eq!(steps[1].display.as_ref().unwrap().target, Target::Background);
    }
}

#[tokio::test]
async fn responses_child_inherits_protocol_and_replays_its_own_tools() {
    use super::responses_tests::{responses_config, wire};
    let http=SequencedHttpClient::new([
        wire::reply(vec![wire::reasoning("child"),wire::call("child-call","run_command",json!({"command":"pwd","target":"background"}))]),
        wire::reply(vec![wire::message("child-final","子任务完成","final_answer")]),
    ]);
    let client=RigClient::with_http(&responses_config(),http.clone()).unwrap();
    let mut inner=Inner::new([]);inner.sessions=true;
    let out=subagent::run(&client,request(SubagentKind::General),context(),snapshot(),&inner,fast_limits(),|_|{}).await;
    assert_eq!(out.status,SubagentStatus::Completed);
    assert_eq!(inner.seen.lock().unwrap().len(),1);
    let requests=bodies(&http);
    assert!(http.requests().iter().all(|r|r.uri.ends_with("/responses")));
    assert!(requests.iter().all(|r|r["store"]==false));
    assert!(requests[1].to_string().contains("CIPHER_child"));
    assert!(!requests[0]["tools"].to_string().contains("compare_directories"));
    assert!(requests[0]["tools"].to_string().contains("list_sessions"));
    assert!(requests[1].to_string().contains("function_call_output"));
    assert!(serde_json::to_string(out.transcript.entries()).unwrap().contains("PRIVATE_REASONING"));
}
