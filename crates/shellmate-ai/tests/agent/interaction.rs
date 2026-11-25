use super::*;
use shellmate_ai::interaction::{Control, Intent};
struct Interactive {
    inner: Host,
    control: Control,
    enqueue_on_run: bool,
}
impl ToolHost for Interactive {
    fn control(&self) -> Option<Control> {
        Some(self.control.clone())
    }
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            let out = self.inner.run(call).await;
            if self.enqueue_on_run {
                self.control.enqueue(
                    "followup".into(),
                    "new instructions, do not run the second command".into(),
                    Default::default(),
                );
            }
            out
        })
    }
}

#[tokio::test]
async fn child_input_ack_waits_for_checkpoint_and_cancellation_keeps_it_unacknowledged() {
    use shellmate_ai::subagent::{self, Cancellation, SubagentContext, SubagentEvent};
    for stop in [false, true] {
        let http = SequencedHttpClient::new([response(false, &[], "answer", false)]);
        let client = RigClient::with_http(&config(false), http.clone()).unwrap();
        let control = Control::default();
        control.resume();
        control.enqueue(
            "followup".into(),
            "durable instruction".into(),
            Default::default(),
        );
        let host = Interactive {
            inner: Host::new([]),
            control: control.clone(),
            enqueue_on_run: false,
        };
        let cancel = Cancellation::default();
        let mut accepted = false;
        let mut failed_once = false;
        let outcome = subagent::run_with_cancel(
            &client,
            shellmate_ai::SubagentRequest {
                description: "test".into(),
                prompt: "initial".into(),
                kind: shellmate_ai::SubagentKind::General,
                background: true,
                model: None,
                reasoning: None,
            },
            SubagentContext {
                id: "agent-test".into(),
                snapshot: Default::default(),
                cwd: None,
            },
            Default::default(),
            &host,
            fast_limits(),
            &cancel,
            |event| match event {
                SubagentEvent::Agent(AgentEvent::InputsAccepted(_)) => accepted = true,
                SubagentEvent::Checkpoint(saved) if accepted && !failed_once => {
                    assert_eq!(control.snapshot().inputs.len(), 1);
                    assert!(control.snapshot().inputs[0].claimed);
                    failed_once = true;
                    *saved = Err(AiError::Persistence);
                }
                SubagentEvent::Saving(true) => {
                    assert_eq!(control.snapshot().inputs.len(), 1);
                    if stop {
                        cancel.cancel();
                    }
                }
                _ => {}
            },
        )
        .await;
        assert!(failed_once);
        if stop {
            assert_eq!(outcome.status, shellmate_ai::SubagentStatus::Stopped);
            assert_eq!(control.snapshot().inputs.len(), 1);
        } else {
            assert_eq!(outcome.status, shellmate_ai::SubagentStatus::Completed);
            assert!(control.snapshot().inputs.is_empty());
            assert_eq!(http.requests().len(), 1);
        }
        assert!(host.inner.calls.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn queued_input_closes_unexecuted_calls_before_the_next_request_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    ("first", "run_command", args("pwd")),
                    ("second", "run_command", args("touch untouched")),
                ],
                "checking",
                true,
            ),
            response(a, &[], "updated plan", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let control = Control::default();
        control.resume();
        let host = Interactive {
            inner: Host::new([completed("directory")]),
            control: control.clone(),
            enqueue_on_run: true,
        };
        let mut log = conversation();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(host.inner.calls.lock().unwrap().len(), 1);
        assert_eq!(log.turns().len(), 2);
        assert_eq!(
            log.latest_user().unwrap().question,
            "new instructions, do not run the second command"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e,AgentEvent::InputsAccepted(ids)if ids==&["followup"]))
        );
        let requests = bodies(&http);
        assert_eq!(requests.len(), 2);
        context_tests::assert_pairs(&requests[1], a);
        assert!(tool_result(&requests[1], a, "second").contains("未执行"));
        assert!(control.snapshot().inputs[0].claimed);
        control.acknowledge(&["followup".into()]);
        assert!(control.snapshot().inputs.is_empty());
    }
}
#[tokio::test]
async fn immediate_input_interrupts_streaming_without_executing_unfinished_calls() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[("never", "run_command", args("touch never"))],
                "partial text",
                false,
            ),
            response(a, &[], "new answer", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let control = Control::default();
        control.resume();
        let host = Interactive {
            inner: Host::new([]),
            control: control.clone(),
            enqueue_on_run: false,
        };
        let mut log = conversation();
        let mut stream =
            agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
        while !matches!(stream.next().await, Some(AgentEvent::TextDelta(_))) {}
        control.enqueue("now".into(), "reply now".into(), Default::default());
        control.request(Intent::SendNow);
        let events = tokio::time::timeout(Duration::from_secs(1), stream.collect::<Vec<_>>())
            .await
            .unwrap();
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert!(host.inner.calls.lock().unwrap().is_empty());
        let requests = bodies(&http);
        assert_eq!(requests.len(), 2);
        context_tests::assert_pairs(&requests[1], a);
        assert!(!requests[1]["messages"].to_string().contains("touch never"));
        assert!(requests[1]["messages"].to_string().contains("reply now"));
    }
}
#[tokio::test]
async fn stop_reply_holds_queued_messages_and_does_not_request_another_response() {
    let http = SequencedHttpClient::new([response(false, &[], "partial", false)]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let c = Control::default();
    c.resume();
    let host = Interactive {
        inner: Host::new([]),
        control: c.clone(),
        enqueue_on_run: false,
    };
    let mut log = conversation();
    let mut stream = agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
    while !matches!(stream.next().await, Some(AgentEvent::TextDelta(_))) {}
    c.enqueue("later".into(), "later".into(), Default::default());
    c.request(Intent::StopReply);
    let events = stream.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Released(agent::ReleaseReason::Stopped))
    ));
    assert_eq!(http.requests().len(), 1);
    assert!(!c.pending());
    assert!(!c.auto_continue());
    assert_eq!(c.snapshot().inputs.len(), 1);
}

#[tokio::test]
async fn releasing_a_wait_has_a_distinct_reason_from_natural_completion() {
    let http = SequencedHttpClient::new([response(false, &[], "partial", false)]);
    let client = RigClient::with_http(&config(false), http.clone()).unwrap();
    let c = Control::default();
    c.resume();
    let host = Interactive {
        inner: Host::new([]),
        control: c.clone(),
        enqueue_on_run: false,
    };
    let mut log = conversation();
    let mut stream = agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
    while !matches!(stream.next().await, Some(AgentEvent::TextDelta(_))) {}
    c.request(Intent::Detach);
    let events = stream.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Released(agent::ReleaseReason::Detached))
    ));
    assert!(c.auto_continue());
    assert_eq!(http.requests().len(), 1);
}

#[tokio::test]
async fn completion_delivery_and_new_input_share_one_request_without_duplicate_notifications() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([response(a, &[], "updated summary", false)]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let c = Control::default();
        c.resume();
        c.publish(
            "done-1".into(),
            "agent-1".into(),
            "child completed with evidence".into(),
        );
        c.publish("done-1".into(), "agent-1".into(), "duplicate".into());
        c.enqueue(
            "followup".into(),
            "include this new requirement".into(),
            Default::default(),
        );
        let host = Interactive {
            inner: Host::new([]),
            control: c.clone(),
            enqueue_on_run: false,
        };
        let mut log = conversation();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        assert_eq!(http.requests().len(), 1);
        let request = &bodies(&http)[0];
        assert!(request.to_string().contains("include this new requirement"));
        assert!(
            request
                .to_string()
                .contains("child completed with evidence")
        );
        assert!(!request.to_string().contains("duplicate"));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::CompletionsAccepted(_)))
                .count(),
            1
        );
    }
}
