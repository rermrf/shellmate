use super::*;
#[path = "../support/responses.rs"]
pub(super) mod wire;
pub(super) fn responses_config() -> Config {
    let mut c = config(false);
    c.provider = Provider::OpenAiResponses {
        base_url: "https://responses.test/v1".into(),
    };
    c.model = "gpt-5.4".into();
    c
}
fn done() -> MockHttpResponse {
    wire::reply(vec![wire::message("done", "完成", "final_answer")])
}
#[tokio::test]
async fn tools_replay_ciphertext_and_phase_across_execution_and_durable_reopen() {
    let http = SequencedHttpClient::new([
        wire::reply(vec![
            wire::reasoning("r1"),
            wire::message("m1", "检查中", "commentary"),
            wire::call("call_1", "run_command", args("pwd")),
        ]),
        done(),
        done(),
    ]);
    let cfg = responses_config();
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    let host = Host::new([completed("/project")]);
    let mut log = conversation();
    let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    assert_eq!(host.calls.lock().unwrap().len(), 1);
    let requests = bodies(&http);
    let input = requests[1]["input"].as_array().unwrap();
    assert!(
        input
            .iter()
            .any(|i| i["type"] == "reasoning" && i["encrypted_content"] == "CIPHER_r1")
    );
    assert!(
        input
            .iter()
            .any(|i| i["type"] == "message" && i["phase"] == "commentary")
    );
    let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
    let result = input
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(call["call_id"], result["call_id"]);
    assert_eq!(call["id"], "fc_call_1");
    assert!(result["output"].as_str().unwrap().contains("/project"));
    let saved = serde_json::to_string(log.entries()).unwrap();
    assert!(saved.contains("PRIVATE_REASONING"));
    assert!(log.turns()[0].answer.as_ref().unwrap().items.iter().any(|i|matches!(i,AnswerItem::Thinking(b)if b.kind==shellmate_ai::ThinkingKind::Summary)));
    assert!(saved.contains("CIPHER_r1"));
    assert!(
        !log.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .text()
            .contains("CIPHER")
    );
    let mut reopened = Conversation::from_entries(serde_json::from_str(&saved).unwrap());
    reopened.push_user(UserTurn::new("继续", Default::default()));
    let events = agent::run(&client, &mut reopened, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    let last = &bodies(&http)[2];
    assert!(last.to_string().contains("CIPHER_r1"));
    assert!(
        last["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["phase"] == "final_answer")
    );
}
#[tokio::test]
async fn prefix_compaction_or_endpoint_change_drops_incompatible_reasoning() {
    for changed_endpoint in [false, true] {
        let http = SequencedHttpClient::new([
            wire::reply(vec![wire::call("first", "run_command", args("pwd"))]),
            wire::reply(vec![
                wire::reasoning("later"),
                wire::message("m", "报告", "final_answer"),
            ]),
        ]);
        let cfg = responses_config();
        let client = RigClient::with_http(&cfg, http).unwrap();
        let host = Host::new([completed("output")]);
        let mut log = conversation();
        agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let mut cfg = cfg.clone();
        if changed_endpoint {
            cfg.provider = Provider::OpenAiResponses {
                base_url: "https://other.test/v1".into(),
            };
        } else {
            let pos = log
                .entries()
                .iter()
                .position(|e| matches!(e, Entry::Step(_)))
                .unwrap();
            log.apply(LogUpdate::ClearResults(vec![pos]));
        }
        log.push_user(UserTurn::new("继续", Default::default()));
        let http = SequencedHttpClient::new([done()]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(!bodies(&http)[0].to_string().contains("CIPHER_later"));
        assert!(
            bodies(&http)[0]
                .to_string()
                .contains("function_call_output")
        );
    }
}
#[tokio::test]
async fn incomplete_tool_never_executes_and_recovers_on_next_response() {
    let partial = wire::event(
        json!({"type":"response.created","response":{"id":"resp_cut","status":"in_progress","output":[]}}),
    ) + &wire::event(
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_cut","call_id":"cut","name":"run_command","arguments":""}}),
    ) + &wire::event(
        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_cut","delta":"{\"command\":"}),
    ) + &wire::event(
        json!({"type":"response.incomplete","response":{"id":"resp_cut","status":"incomplete","output":[],"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}),
    );
    let http = SequencedHttpClient::new([
        MockHttpResponse::success_typed(partial, "text/event-stream"),
        done(),
    ]);
    let client = RigClient::with_http(&responses_config(), http).unwrap();
    let host = Host::new([]);
    let mut log = conversation();
    let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(host.calls.lock().unwrap().is_empty());
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
}
#[tokio::test]
async fn failed_response_with_complete_call_does_not_execute() {
    let failure = wire::event(
        json!({"type":"response.failed","response":{"id":"bad","status":"failed","error":{"code":"server_error","message":"SECRET"},"output":[wire::call("never","run_command",args("pwd"))]}}),
    );
    let http = SequencedHttpClient::new([MockHttpResponse::success_typed(
        failure,
        "text/event-stream",
    )]);
    let client = RigClient::with_http(&responses_config(), http).unwrap();
    let host = Host::new([]);
    let mut log = conversation();
    let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(host.calls.lock().unwrap().is_empty());
    assert!(!matches!(events.last(), Some(AgentEvent::Done(_))));
}

#[tokio::test]
async fn pause_resume_preserves_response_items_without_rerunning_finished_commands() {
    let http = SequencedHttpClient::new([
        wire::reply(vec![
            wire::reasoning("paused"),
            wire::call("one", "run_command", args("pwd")),
        ]),
        done(),
    ]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let host = Host::new([completed("ok")]);
    let mut log = conversation();
    let mut limits = fast_limits();
    limits.max_steps = Some(1);
    let mut stream = agent::run(&client, &mut log, capabilities(), &host, limits).unwrap();
    let resume = loop {
        match stream.next().await {
            Some(AgentEvent::Paused { resume, .. }) => break resume,
            Some(AgentEvent::Error(e)) => panic!("{e}"),
            Some(_) => {}
            None => panic!("expected pause"),
        }
    };
    drop(stream);
    let events = agent::resume(&client, &mut log, &host, *resume, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    assert_eq!(host.calls.lock().unwrap().len(), 1);
    assert!(bodies(&http)[1].to_string().contains("CIPHER_paused"));
}

#[tokio::test]
async fn immediate_input_interrupts_responses_without_executing_pending_tool() {
    use shellmate_ai::interaction::{Control, Intent};
    struct Interactive {
        host: Host,
        control: Control,
    }
    impl ToolHost for Interactive {
        fn control(&self) -> Option<Control> {
            Some(self.control.clone())
        }
        fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
            self.host.run(call)
        }
    }
    let http = SequencedHttpClient::new([
        wire::reply(vec![
            wire::message("m", "处理中", "commentary"),
            wire::call("never", "run_command", args("touch never")),
        ]),
        done(),
    ]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let control = Control::default();
    control.resume();
    let host = Interactive {
        host: Host::new([]),
        control: control.clone(),
    };
    let mut log = conversation();
    let mut stream = agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
    while !matches!(stream.next().await, Some(AgentEvent::TextDelta(_))) {}
    control.enqueue("now".into(), "新的指令".into(), Default::default());
    control.request(Intent::SendNow);
    let events = tokio::time::timeout(Duration::from_secs(3), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    assert!(host.host.calls.lock().unwrap().is_empty());
    let second = &bodies(&http)[1];
    assert!(second.to_string().contains("新的指令"));
    assert!(!second.to_string().contains("touch never"));
}

#[tokio::test]
async fn manual_compaction_uses_responses_and_replaces_old_replay_state() {
    let http = SequencedHttpClient::new([
        wire::reply(vec![
            wire::reasoning("before-compact"),
            wire::call("before", "run_command", args("pwd")),
        ]),
        done(),
        wire::reply(vec![wire::message(
            "summary",
            "项目目录已检查，接下来继续。",
            "final_answer",
        )]),
        done(),
    ]);
    let client = RigClient::with_http(&responses_config(), http.clone()).unwrap();
    let host = Host::new([completed("/project")]);
    let mut log = conversation();
    agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let events = agent::compact(&client, &mut log, capabilities(), &host, None)
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
    log.push_user(UserTurn::new("继续", Default::default()));
    let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
    let requests = bodies(&http);
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|r| r["store"] == false));
    assert!(requests[2]["tools"].as_array().is_none_or(|t| t.is_empty()));
    assert!(requests[3].to_string().contains("项目目录已检查"));
    assert!(!requests[3].to_string().contains("CIPHER_before-compact"));
    assert_eq!(host.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn exposed_thinking_and_tools_keep_transcript_order_and_do_not_inflate_answer() {
    let http=SequencedHttpClient::new([
        wire::reply(vec![wire::reasoning("before"),wire::call("inspect","run_command",args("pwd")),wire::reasoning("after"),wire::message("m","核对中","commentary")]),done(),
    ]);
    let cfg=responses_config();let client=RigClient::with_http(&cfg,http).unwrap();let host=Host::new([completed("/project")]);let mut log=conversation();
    let events=agent::run(&client,&mut log,capabilities(),&host,fast_limits()).unwrap().collect::<Vec<_>>().await;
    assert!(matches!(events.last(),Some(AgentEvent::Done(_))));
    let kinds=log.turns()[0].answer.as_ref().unwrap().items.iter().map(|i|match i{AnswerItem::Thinking(_)=>"thinking",AnswerItem::Step{..}=>"step",AnswerItem::Text(_)=>"text",_=>"other"}).collect::<Vec<_>>();
    assert_eq!(kinds,["thinking","step","thinking","text"]);
    assert_eq!(log.turns()[0].answer.as_ref().unwrap().text(),"核对中完成");
}
