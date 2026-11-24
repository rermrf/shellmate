use super::*;
use shellmate_ai::agent::{ExecLimits, LocalResult, LocalTool};

struct OutputHost {
    calls: Mutex<Vec<ToolCall>>,
    locals: Mutex<Vec<LocalTool>>,
    notices: Mutex<Vec<String>>,
}
impl OutputHost {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            locals: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
        }
    }
}
impl ToolHost for OutputHost {
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call.clone());
            self.notices
                .lock()
                .unwrap()
                .push("后台任务 #7 已结束 · 退出码 0 · 用 read_output 查看输出".into());
            ToolOutcome::Completed(output::spooled(
                "spool-fixture",
                &call.command,
                b"content",
                7,
                Some(0),
                Duration::ZERO,
                call.exec_limits,
            ))
        })
    }
    fn local_tool(&self, tool: LocalTool, _: ExecLimits) -> BoxFuture<'_, LocalResult> {
        Box::pin(async move {
            self.locals.lock().unwrap().push(tool);
            LocalResult {
                inline: "本机工具结果".into(),
                output_id: Some("spool-fixture".into()),
                is_error: false,
            }
        })
    }
    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }
    fn task_state(&self) -> (Vec<String>, Vec<String>) {
        (
            vec!["#8 · cargo build · 输出 id spool-eight".into()],
            vec!["spool-fixture · pwd".into()],
        )
    }
}
async fn drain(stream: &mut agent::AgentStream<'_>) {
    while let Some(event) = stream.next().await {
        assert!(!matches!(
            event,
            AgentEvent::Error(_) | AgentEvent::Paused { .. }
        ));
    }
}
#[tokio::test]
async fn output_tools_replay_across_questions_on_both_wires_and_never_call_run() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    (
                        "run",
                        "run_command",
                        json!({"command":"pwd", "run_in_background":true}),
                    ),
                    (
                        "read",
                        "read_output",
                        json!({"id":"spool-fixture","offset_line":1,"limit_lines":2,"grep":"content"}),
                    ),
                    ("stop", "stop_task", json!({"id":"7"})),
                ],
                "",
                false,
            ),
            response(a, &[], "finished", false),
            response(a, &[], "followup", false),
            response(a, &[], "<summary>handoff</summary>", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = OutputHost::new();
        let mut c = conversation();
        drain(&mut agent::run(&client, &mut c, capabilities(), &host, Limits::default()).unwrap())
            .await;
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        assert!(host.calls.lock().unwrap()[0].run_in_background);
        assert_eq!(
            host.calls.lock().unwrap()[0].timeout,
            Duration::from_secs(120)
        );
        assert_eq!(host.locals.lock().unwrap().len(), 2);
        assert!(c.entries().iter().any(|e| matches!(e, Entry::Step(s) if s.call.name == "run_command" && s.result.output_id.as_deref() == Some("spool-fixture"))));
        let second = bodies(&http)[1].clone();
        assert!(second.to_string().contains("后台任务 #7 已结束"));
        let mut reopened = c.clone();
        reopened.push_user(UserTurn::new("下一题", ContextSnapshot::default()));
        drain(
            &mut agent::run(
                &client,
                &mut reopened,
                capabilities(),
                &host,
                Limits::default(),
            )
            .unwrap(),
        )
        .await;
        for request in bodies(&http).iter().take(3) {
            super::context_tests::assert_pairs(request, a);
        }
        let replay = bodies(&http)[2].clone();
        for name in ["read_output", "stop_task"] {
            assert!(replay["messages"].to_string().contains(name));
        }
        // The output id powers the existing tier-1 stub without changing its wire shape.
        let mut step = c
            .entries()
            .iter()
            .find_map(|e| {
                if let Entry::Step(s) = e {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();
        step.result.cleared = true;
        assert!(
            step.wire_text()
                .contains("read_output(id=\"spool-fixture\")")
        );
        drain(&mut agent::compact(&client, &mut reopened, capabilities(), &host, None).unwrap())
            .await;
        let state = reopened
            .entries()
            .iter()
            .rev()
            .find_map(|e| {
                if let Entry::Compaction(c) = e {
                    Some(&c.state)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(state.running_tasks[0].contains("#8"));
        assert!(state.readable_output_ids[0].contains("spool-fixture"));
    }
}
#[tokio::test]
async fn suggestion_sessions_offer_no_output_tools_and_replay_them_as_text() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "suggestion", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = OutputHost::new();
        let mut c = conversation();
        c.append(Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: "old-read".into(),
                name: "read_output".into(),
                args_json: json!({"id":"old"}),
            },
            result: LoggedResult {
                inline: "old data".into(),
                output_id: Some("old".into()),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        c.push_user(UserTurn::new("next", ContextSnapshot::default()));
        drain(
            &mut agent::run(
                &client,
                &mut c,
                Capabilities::default(),
                &host,
                Limits::default(),
            )
            .unwrap(),
        )
        .await;
        let body = bodies(&http)[0].clone();
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(body["tools"].to_string().contains("propose_command"));
        super::context_tests::assert_pairs(&body, a);
        assert!(host.calls.lock().unwrap().is_empty() && host.locals.lock().unwrap().is_empty());
    }
}
#[test]
fn inline_success_failure_benign_exit_one_and_settings() {
    for command in [
        "grep x file",
        "rg x",
        "egrep x",
        "fgrep x",
        "find .",
        "diff a b",
        "test -f a",
        "[ -f a ]",
        "git diff",
        "git grep x",
        "A='space value' B=2 grep x",
        "export PAGER=cat GIT_PAGER=cat SYSTEMD_PAGER= SYSTEMD_COLORS=0 TERM=dumb LC_ALL=C.UTF-8; A=1 git diff",
    ] {
        assert!(output::successful(command, Some(1)), "{command}");
    }
    for command in [
        "false",
        "cargo test",
        "git status",
        "grepper",
        "echo x; grep y",
    ] {
        assert!(!output::successful(command, Some(1)), "{command}");
    }
    for count in [1000, 30000, 100000, 128000, 150000] {
        let limits = ExecLimits {
            tool_output_chars: count,
            ..Default::default()
        };
        let text = format!("HEAD{}TAIL", "中".repeat(count));
        let ok = output::spooled(
            "id",
            "echo",
            text.as_bytes(),
            text.len() as u64,
            Some(0),
            Duration::ZERO,
            limits,
        );
        assert!(ok.stdout().starts_with("HEAD") && ok.stdout().contains("完整输出已保存"));
        assert!(ok.stdout().chars().count() <= 2100);
        let fail = output::spooled(
            "id",
            "false",
            text.as_bytes(),
            text.len() as u64,
            Some(1),
            Duration::ZERO,
            limits,
        );
        assert!(fail.stdout().chars().count() <= count.min(10000));
        assert!(fail.stdout().starts_with("HEAD") && fail.stdout().ends_with("TAIL"));
        assert!(fail.wire_text().contains("输出 id id"));
        assert!(fail.stored().len() <= output::STORED_LIMIT);
    }
    let clean = output::spooled(
        "id",
        "echo",
        b"\x1b[31mred\x1b[0m\x1b]0;hidden\x07\rOK",
        0,
        Some(0),
        Duration::ZERO,
        ExecLimits::default(),
    );
    assert_eq!(clean.stdout(), "OKd");
    assert_eq!(
        ExecLimits::default().timeout(None),
        Duration::from_secs(120)
    );
    assert_eq!(
        ExecLimits::default().timeout(Some(900)),
        Duration::from_secs(600)
    );
    assert_eq!(
        ExecLimits {
            tool_output_chars: 999,
            ..Default::default()
        }
        .output_chars(),
        30000
    );
}
