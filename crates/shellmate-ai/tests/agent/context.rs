use super::*;
use shellmate_ai::context::SUMMARY_PREFIX;

pub(super) fn assert_pairs(body: &Value, anthropic: bool) {
    use std::collections::BTreeMap;
    let mut calls = BTreeMap::<String, usize>::new();
    let mut results = BTreeMap::<String, usize>::new();
    let messages = body["messages"].as_array().unwrap();
    for (i, message) in messages.iter().enumerate() {
        if anthropic {
            if i > 0 {
                assert_ne!(
                    messages[i - 1]["role"],
                    message["role"],
                    "adjacent roles: {body}"
                );
            }
            let blocks = message["content"].as_array().unwrap();
            let mut saw_text = false;
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap();
                        assert!(
                            id.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                        );
                        *calls.entry(id.into()).or_default() += 1;
                        assert!(
                            body["tools"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|t| t["name"] == block["name"])
                        );
                    }
                    Some("tool_result") => {
                        assert!(!saw_text, "tool results must lead user content");
                        let id = block["tool_use_id"].as_str().unwrap();
                        *results.entry(id.into()).or_default() += 1;
                        assert!(
                            i > 0
                                && messages[i - 1]["content"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .any(|b| b["type"] == "tool_use" && b["id"] == id)
                        );
                    }
                    _ => saw_text = true,
                }
            }
        } else {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"].as_str().unwrap();
                assert!(
                    id.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                );
                *calls.entry(id.into()).or_default() += 1;
                assert!(
                    body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["function"]["name"] == call["function"]["name"])
                );
            }
            if let Some(id) = message["tool_call_id"].as_str() {
                *results.entry(id.into()).or_default() += 1;
            }
        }
    }
    assert_eq!(calls, results, "{body}");
    assert!(
        calls.values().all(|n| *n == 1),
        "a call must have exactly one result"
    );
}
fn with_usage(a: bool, calls: &[(&str, &str, Value)], text: &str, input: u64) -> MockHttpResponse {
    let body = wire(a, calls, text, false)
        .split_inclusive('\n')
        .map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return line.to_owned();
            };
            let Ok(mut v) = serde_json::from_str::<Value>(data) else {
                return line.to_owned();
            };
            if a && v["type"] == "message_start" {
                v["message"]["usage"]["input_tokens"] = json!(input);
            }
            if !a && v["choices"][0]["finish_reason"].is_string() {
                v["usage"] = json!({"prompt_tokens":input,"completion_tokens":8});
            }
            format!("data: {v}\n")
        })
        .collect::<String>();
    MockHttpResponse::success_typed(body, "text/event-stream")
}
fn logged_step(n: usize, text: String, output_id: Option<String>) -> Entry {
    Entry::Step(Step {
        message_id: n + 1,
        call: LoggedCall {
            id: format!("call_{n}"),
            name: "run_command".into(),
            args_json: args(&format!("echo {n}")),
        },
        result: LoggedResult {
            subagent: None,
            inline: text.clone(),
            output_id,
            cleared: false,
            is_error: false,
        },
        display: Some(StepDisplay {
            command: format!("echo {n}"),
            target: Target::Background,
            purpose: "inspect".into(),
            class: Class::ReadOnly,
            status: StepStatus::Completed,
            exit_code: Some(0),
            duration_ms: 12,
            automatic: true,
            output: output::stored_text(&text),
        }),
    })
}
struct ContextHost {
    host: Host,
}
impl ToolHost for ContextHost {
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.host.run(call)
    }
    fn context_snapshot(&self) -> BoxFuture<'_, Option<ContextSnapshot>> {
        Box::pin(async {
            Some(ContextSnapshot {
                label: "fresh-host".into(),
                protocol: "SSH".into(),
                text: (0..80).map(|n| format!("fresh-{n}\n")).collect(),
            })
        })
    }
    fn permission_mode(&self) -> PermissionMode {
        PermissionMode::Full
    }
}
#[tokio::test]
async fn cross_question_replay_is_paired_on_both_wires_and_unoffered_tools_are_text() {
    for a in [false, true] {
        for executable in [false, true] {
            let http = SequencedHttpClient::new([
                response(
                    a,
                    &[
                        ("bad:id", "run_command", args("pwd")),
                        ("other", "run_command", args("ls")),
                    ],
                    "checking",
                    a,
                ),
                response(a, &[], "done", false),
                response(a, &[], "follow-up", false),
            ]);
            let client = RigClient::with_http(&config(a), http.clone()).unwrap();
            let host = Host::new([completed("/srv"), completed("file")]);
            let mut log = conversation();
            let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            assert_pairs(&bodies(&http)[1], a);
            let json = serde_json::to_string(log.entries()).unwrap();
            assert!(!json.contains("PRIVATE-THINKING") && !json.contains("signature-fixture"));
            log.push_user(UserTurn::new("follow-up", ContextSnapshot::default()));
            log.push_user(UserTurn::new("also this", ContextSnapshot::default()));
            let events = agent::run(
                &client,
                &mut log,
                if executable {
                    capabilities()
                } else {
                    Capabilities::default()
                },
                &host,
                fast_limits(),
            )
            .unwrap()
            .collect::<Vec<_>>()
            .await;
            assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
            let body = &bodies(&http)[2];
            assert_pairs(body, a);
            let text = body["messages"].to_string();
            assert!(text.contains("/srv") && text.contains("file") && text.contains("also this"));
            assert!(!text.contains("PRIVATE-THINKING") && !text.contains("signature-fixture"));
            assert_eq!(
                text.contains(if a { "tool_use" } else { "tool_calls" }),
                executable
            );
            if !executable {
                assert!(body.to_string().contains("propose_command"));
            }
        }
    }
}
#[tokio::test]
async fn tier_one_is_one_bulk_clear_preserves_five_and_persists_the_divider() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "done", false)]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(100_000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut log = conversation();
        for n in 0..8 {
            log.append(logged_step(
                n,
                if n < 3 {
                    "line\n".repeat(30_000)
                } else {
                    "new result".into()
                },
                (n == 0).then(|| "output-0".into()),
            ));
        }
        let host = Host::new([]);
        let mut stream =
            agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
        let mut clears = 0;
        let mut events = vec![];
        while let Some(event) = stream.next().await {
            for update in stream.take_updates() {
                if let LogUpdate::ClearResults(p) = update {
                    clears += 1;
                    assert_eq!(p, vec![1, 2, 3]);
                }
            }
            events.push(event);
        }
        drop(stream);
        assert_eq!(clears, 1);
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compacted { summary: None, .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compacting { tier: 2 }))
        );
        let steps = log
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
        assert_eq!(steps.iter().filter(|s| s.result.cleared).count(), 3);
        assert!(
            steps[0]
                .wire_text()
                .contains("完整输出：read_output(id=\"output-0\")")
        );
        assert!(!steps[1].wire_text().contains("read_output"));
        assert!(
            steps[0]
                .wire_text()
                .starts_with("[较早的输出已移出上下文] 命令：echo 0 · 退出码 0 · 原长 150000")
        );
        assert_pairs(&bodies(&http)[0], a);
        let restored = Conversation::from_entries(
            serde_json::from_str(&serde_json::to_string(log.entries()).unwrap()).unwrap(),
        );
        assert!(restored == log);
        assert_eq!(restored.turns()[0].answer.as_ref().unwrap().items.len(), 10);
    }
}
#[tokio::test]
async fn manual_focus_summary_state_fresh_snapshot_and_second_compaction_fold() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[],
                "<analysis>discard scratchpad</analysis><summary>first handoff</summary>",
                false,
            ),
            response(a, &[], "<summary>second handoff</summary>", false),
            response(a, &[], "after reload", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = ContextHost {
            host: Host::new([]),
        };
        let mut log = conversation();
        let mut denied = logged_step(0, "denied".into(), None);
        if let Entry::Step(s) = &mut denied {
            let d = s.display.as_mut().unwrap();
            d.command = "rm important".into();
            d.status = StepStatus::Denied {
                reason: Some("keep it".into()),
            };
        }
        log.append(denied);
        log.push_user(UserTurn::new("newer request", ContextSnapshot::default()));
        let events = agent::compact(
            &client,
            &mut log,
            capabilities(),
            &host,
            Some("重点保留文件和待办".into()),
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let Entry::Compaction(c) = log.entries().last().unwrap() else {
            panic!("compaction")
        };
        assert_eq!(c.summary.as_deref(), Some("first handoff"));
        assert_eq!(c.retained_user, vec!["newer request", "检查容器"]);
        assert_eq!(c.state.permission_mode, PermissionMode::Full);
        assert_eq!(
            c.state.denied_commands[0],
            DeniedCommand {
                command: "rm important".into(),
                reason: Some("keep it".into())
            }
        );
        assert!(c.state.running_tasks.is_empty() && c.state.readable_output_ids.is_empty());
        assert_eq!(c.context.text.lines().count(), 40);
        assert!(c.context.text.starts_with("fresh-40\n") && c.context.text.ends_with("fresh-79"));
        assert_eq!(c.context.label, "fresh-host");
        agent::compact(&client, &mut log, capabilities(), &host, None)
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let bodies = bodies(&http);
        for body in &bodies {
            assert!(body.get("tools").is_none());
            assert_eq!(body["max_tokens"], 16_000);
        }
        assert!(
            bodies[0]
                .to_string()
                .contains("额外要求：重点保留文件和待办")
        );
        assert!(bodies[1].to_string().contains("first handoff"));
        let mut restored = Conversation::from_entries(
            serde_json::from_str(&serde_json::to_string(log.entries()).unwrap()).unwrap(),
        );
        restored.push_user(UserTurn::new(
            "continue after reload",
            ContextSnapshot::default(),
        ));
        agent::run(&client, &mut restored, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let body = &super::bodies(&http)[2];
        let text = body.to_string();
        assert!(
            text.contains(SUMMARY_PREFIX)
                && text.contains("second handoff")
                && text.contains("rm important")
        );
        assert!(!text.contains("first handoff") && !text.contains("discard scratchpad"));
        assert_pairs(body, a);
    }
}
#[tokio::test]
async fn mid_run_compaction_then_pause_and_manual_compaction_then_resume() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            with_usage(a, &[("one", "run_command", args("pwd"))], "step one", 4500),
            response(a, &[], "<summary>checkpoint one</summary>", false),
            response(a, &[("two", "run_command", args("ls"))], "step two", a),
            response(a, &[], "<summary>manual while paused</summary>", false),
            response(a, &[], "finished", false),
        ]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(6000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let host = ContextHost {
            host: Host::new([completed("/srv"), completed("files")]),
        };
        let mut log = conversation();
        let events = agent::run(
            &client,
            &mut log,
            capabilities(),
            &host,
            Limits {
                max_steps: Some(2),
                ..fast_limits()
            },
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Compacted {
                summary: Some(_),
                ..
            }
        )));
        let AgentEvent::Paused {
            resume, steps: 2, ..
        } = events.into_iter().last().unwrap()
        else {
            panic!("expected pause")
        };
        agent::compact(
            &client,
            &mut log,
            capabilities(),
            &host,
            Some("next step".into()),
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        let events = agent::resume(&client, &mut log, &host, *resume, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.host.calls.lock().unwrap().len(), 2);
        assert_eq!(
            log.entries()
                .iter()
                .filter(|e| matches!(e, Entry::Step(_)))
                .count(),
            2
        );
        let bodies = bodies(&http);
        assert_eq!(bodies.len(), 5);
        assert!(bodies[2].to_string().contains("checkpoint one"));
        assert!(bodies[4].to_string().contains("manual while paused"));
        assert_pairs(&bodies[2], a);
        assert_pairs(&bodies[4], a);
    }
}
#[tokio::test]
async fn reactive_compaction_resends_once_and_second_too_long_is_terminal() {
    for a in [false, true] {
        for execute in [false, true] {
            for second_error in [false, true] {
                let http = SequencedHttpClient::new([
                    http_error(400, "prompt is too long"),
                    response(a, &[], "<summary>checkpoint</summary>", false),
                    if second_error {
                        http_error(400, "context_length_exceeded")
                    } else {
                        response(a, &[], "done", false)
                    },
                ]);
                let client = RigClient::with_http(&config(a), http.clone()).unwrap();
                let host = Host::new([]);
                let mut log = conversation();
                let terminal = if execute {
                    let events =
                        agent::run(&client, &mut log, capabilities(), &host, fast_limits())
                            .unwrap()
                            .collect::<Vec<_>>()
                            .await;
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| matches!(e, AgentEvent::Compacting { tier: 1 }))
                            .count(),
                        1
                    );
                    if let Some(AgentEvent::Error(e)) = events.last() {
                        Some(*e)
                    } else {
                        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
                        None
                    }
                } else {
                    let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| matches!(e, Event::Compacting { tier: 1 }))
                            .count(),
                        1
                    );
                    if let Some(Event::Error(e)) = events.last() {
                        Some(*e)
                    } else {
                        assert!(matches!(events.last(), Some(Event::Done(_))));
                        None
                    }
                };
                assert_eq!(terminal, second_error.then_some(AiError::ContextTooLong));
                assert_eq!(http.requests().len(), 3);
                let bodies = bodies(&http);
                assert!(bodies[1].get("tools").is_none());
                assert!(bodies[2].to_string().contains("checkpoint"));
                assert_pairs(&bodies[2], a);
            }
        }
    }
}
#[tokio::test]
async fn summary_drops_oldest_entries_on_too_long_and_fails_when_one_remains() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            http_error(400, "prompt is too long"),
            response(a, &[], "<summary>short</summary>", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([]);
        let mut log = conversation();
        log.push_text("middle answer");
        log.push_user(UserTurn::new("last request", ContextSnapshot::default()));
        let events = agent::compact(&client, &mut log, capabilities(), &host, None)
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let b = bodies(&http);
        assert!(b[0].to_string().contains("检查容器"));
        assert!(!b[1].to_string().contains("检查容器"));
        assert!(b[1].to_string().contains("middle answer"));
        let http = SequencedHttpClient::new([http_error(400, "prompt is too long")]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let mut log = conversation();
        let events = agent::compact(&client, &mut log, capabilities(), &host, None)
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::ContextTooLong))
        ));
        assert_eq!(http.requests().len(), 1);
    }
}
#[tokio::test]
async fn nonagent_pre_request_compaction_uses_same_log_and_4k_summary_excerpts() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(a, &[], "<summary>compressed</summary>", false),
            response(a, &[], "advice", false),
        ]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(6000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let mut log = conversation();
        log.append(logged_step(
            0,
            format!("HEAD{}TAIL", "中".repeat(20_000)),
            None,
        ));
        log.push_user(UserTurn::new("advice", ContextSnapshot::default()));
        let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Event::Done(_))));
        assert!(events.iter().any(|e| matches!(e, Event::Compacted { .. })));
        let b = bodies(&http);
        assert_eq!(b.len(), 2);
        assert!(b[0].get("tools").is_none());
        let text = b[0]["messages"].to_string();
        assert!(text.contains("HEAD") && text.contains("TAIL"));
        assert!(text.matches('中').count() < 4000);
        assert_pairs(&b[1], a);
        assert!(
            log.turns()
                .last()
                .unwrap()
                .answer
                .as_ref()
                .unwrap()
                .text()
                .contains("advice")
        );
    }
}

#[tokio::test]
async fn manual_compaction_of_split_batch_keeps_pending_pair_and_signed_thinking() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    ("first", "run_command", args("pwd")),
                    ("pending", "run_command", args("ls")),
                ],
                "batch",
                a,
            ),
            response(
                a,
                &[],
                "<summary>first finished; second pending</summary>",
                false,
            ),
            response(a, &[], "finished", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("/srv"), completed("files")]);
        let mut log = conversation();
        let events = agent::run(
            &client,
            &mut log,
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
        let AgentEvent::Paused { resume, .. } = events.into_iter().last().unwrap() else {
            panic!("pause")
        };
        agent::compact(&client, &mut log, capabilities(), &host, None)
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let events = agent::resume(&client, &mut log, &host, *resume, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.calls.lock().unwrap().len(), 2);
        let b = bodies(&http);
        assert_pairs(&b[2], a);
        assert!(tool_result(&b[2], a, "pending").contains("files"));
        if a {
            assert!(
                b[2].to_string().contains("signature-fixture")
                    && b[2].to_string().contains("PRIVATE-THINKING")
            );
        }
        assert!(
            !serde_json::to_string(log.entries())
                .unwrap()
                .contains("signature-fixture")
        );
    }
}
#[tokio::test]
async fn repeated_immediate_pressure_ends_with_exact_thrash_error() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            with_usage(a, &[("a", "run_command", args("pwd"))], "", 4500),
            response(a, &[], "summary one", false),
            with_usage(a, &[("b", "run_command", args("ls"))], "", 4500),
            response(a, &[], "summary two", false),
            with_usage(a, &[("c", "run_command", args("date"))], "", 4500),
        ]);
        let mut cfg = config(a);
        cfg.context_window = ContextWindow::Tokens(6000);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let host = Host::new([completed("a"), completed("b"), completed("c")]);
        let mut log = conversation();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(AiError::CompactionThrash))
        ));
        assert_eq!(http.requests().len(), 5);
        assert_eq!(host.calls.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn unfinished_anthropic_call_keeps_truncation_identity_when_id_is_normalized() {
    let input = wire(
        true,
        &[("bad:id", "run_command", args("must never execute"))],
        "kept",
        true,
    );
    let mut blocks = vec![];
    for block in input.split("\n\n").filter(|s| !s.is_empty()) {
        let data = block
            .lines()
            .find_map(|s| s.strip_prefix("data: "))
            .unwrap();
        let mut value: Value = serde_json::from_str(data).unwrap();
        if value["type"] == "content_block_stop" && value["index"] == 2 {
            continue;
        }
        if value["type"] == "content_block_delta" && value["index"] == 2 {
            value["delta"]["partial_json"] = json!("{\"command\":\"partial");
        }
        if value["type"] == "message_delta" {
            value["delta"]["stop_reason"] = json!("max_tokens");
        }
        blocks.push(format!(
            "event: {}\ndata: {value}\n\n",
            value["type"].as_str().unwrap()
        ));
    }
    let http = SequencedHttpClient::new([
        MockHttpResponse::success_typed(blocks.concat(), "text/event-stream"),
        response(true, &[], "done", false),
    ]);
    let client = RigClient::with_http(&config(true), http.clone()).unwrap();
    let host = Host::new([]);
    let mut log = conversation();
    let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    assert!(host.calls.lock().unwrap().is_empty());
    let b = bodies(&http);
    assert!(b[1].to_string().contains("被截断，未执行"));
    assert!(!b[1].to_string().contains("must never execute"));
    assert!(b[1].to_string().contains("signature-fixture"));
    assert_pairs(&b[1], true);
}

#[tokio::test]
async fn suggestion_card_followed_by_text_finishes_without_an_extra_model_call() {
    let input = wire(
        true,
        &[(
            "proposal",
            "propose_command",
            json!({"command":"pwd","explanation":"directory","risk":"low"}),
        )],
        "after the card",
        false,
    );
    let mut values: Vec<Value> = input
        .lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let mut text_blocks = Vec::new();
    values.retain(|v| {
        if v["index"] == 0
            && v["type"]
                .as_str()
                .is_some_and(|s| s.starts_with("content_block_"))
        {
            text_blocks.push(v.clone());
            false
        } else {
            true
        }
    });
    for v in &mut values {
        if v["index"] == 1 {
            v["index"] = json!(0);
        }
    }
    for v in &mut text_blocks {
        v["index"] = json!(1);
    }
    values.splice(values.len() - 2..values.len() - 2, text_blocks);
    let body: String = values
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect();
    let http =
        SequencedHttpClient::new([MockHttpResponse::success_typed(body, "text/event-stream")]);
    let client = RigClient::with_http(&config(true), http.clone()).unwrap();
    let mut log = conversation();
    let events = client.start(&mut log).unwrap().collect::<Vec<_>>().await;
    assert!(matches!(events.last(), Some(Event::Done(_))));
    assert_eq!(http.requests().len(), 1);
    assert!(
        matches!(&log.turns()[0].answer.as_ref().unwrap().items[..],[AnswerItem::Command(c),AnswerItem::Text(t)] if c.command=="pwd" && t=="after the card")
    );
}
