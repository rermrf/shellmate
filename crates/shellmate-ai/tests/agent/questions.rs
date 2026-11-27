use super::*;
use shellmate_ai::{
    interaction::{Control, Intent, Phase},
    questions::{Answer, Response, Status},
};
struct QuestionHost {
    control: Control,
    inner: Host,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}
impl ToolHost for QuestionHost {
    fn control(&self) -> Option<Control> {
        Some(self.control.clone())
    }
    fn model_slots(&self) -> Option<std::sync::Arc<tokio::sync::Semaphore>> {
        Some(self.slots.clone())
    }
    fn run(&self, c: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.inner.run(c)
    }
}
fn question(mode: &str) -> Value {
    json!({"mode":mode,"questions":[{"id":"env","question":"使用哪个环境？","options":[{"id":"dev","label":"开发环境"},{"id":"prod","label":"生产环境"}]}]})
}
fn answer() -> Response {
    Response {
        status: Status::Answered,
        answers: std::collections::BTreeMap::from([(
            "env".into(),
            Answer {
                selected: vec!["dev".into()],
                text: "先检查配置".into(),
            },
        )]),
    }
}
fn setup(protocol: usize) -> Config {
    if protocol == 2 {
        super::responses_tests::responses_config()
    } else {
        config(protocol == 1)
    }
}
fn reply(protocol: usize, calls: &[(&str, &str, Value)], text: &str) -> MockHttpResponse {
    if protocol == 2 {
        let mut items = calls
            .iter()
            .map(|(id, name, args)| super::responses_tests::wire::call(id, name, args.clone()))
            .collect::<Vec<_>>();
        if !text.is_empty() {
            items.push(super::responses_tests::wire::message(
                "m",
                text,
                "final_answer",
            ));
        }
        super::responses_tests::wire::reply(items)
    } else {
        response(protocol == 1, calls, text, false)
    }
}
#[tokio::test]
async fn wait_question_releases_model_slot_and_delivers_answer_once_on_all_protocols() {
    for protocol in 0..3 {
        let http = SequencedHttpClient::new([
            reply(
                protocol,
                &[
                    ("ask", "ask_user", question("wait")),
                    ("dependent", "run_command", args("never-before-answer")),
                ],
                "",
            ),
            reply(protocol, &[], "根据开发环境继续"),
        ]);
        let cfg = setup(protocol);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let control = Control::default();
        control.resume();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let host = QuestionHost {
            control: control.clone(),
            inner: Host::new([]),
            slots: slots.clone(),
        };
        let mut log = conversation();
        let mut stream =
            agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
        loop {
            match stream.next().await {
                Some(AgentEvent::ToolFinished(id)) if id == "ask" => break,
                Some(AgentEvent::Error(e)) => panic!("{e}"),
                Some(_) => {}
                None => panic!("question missing"),
            }
        }
        let id = control.snapshot().questions[0].id.clone();
        assert_eq!(http.requests().len(), 1);
        assert_eq!(slots.available_permits(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        assert_eq!(control.phase(), Phase::UserInput);
        assert!(host.inner.calls.lock().unwrap().is_empty());
        assert!(
            control
                .answer_question(&id, answer(), |_| Err("disk full".into()))
                .is_err()
        );
        assert!(control.question(&id).unwrap().response.is_none());
        assert!(!control.pending());
        control.answer_question(&id, answer(), |_| Ok(())).unwrap();
        assert!(control.answer_question(&id, answer(), |_| Ok(())).is_err());
        let mut accepted = 0;
        while let Some(e) = stream.next().await {
            match e {
                AgentEvent::InputsAccepted(ids) => {
                    accepted += ids.len();
                    control.acknowledge(&ids);
                }
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        drop(stream);
        assert_eq!(accepted, 1);
        assert!(host.inner.calls.lock().unwrap().is_empty());
        assert_eq!(log.turns().len(), 1);
        assert_eq!(
            log.entries()
                .iter()
                .filter(|e| matches!(e, Entry::QuestionAnswer { .. }))
                .count(),
            1
        );
        assert!(bodies(&http)[1].to_string().contains("先检查配置"));
        assert!(bodies(&http)[1].to_string().contains("开发环境"));
    }
}
#[tokio::test]
async fn async_question_allows_unrelated_work_then_waits_without_ending_turn() {
    let http = SequencedHttpClient::new([
        reply(
            0,
            &[
                ("ask", "ask_user", question("async")),
                ("check", "run_command", args("pwd")),
            ],
            "",
        ),
        reply(0, &[], "已完成独立检查"),
        reply(0, &[], "收到选择"),
    ]);
    let cfg = setup(0);
    let client = RigClient::with_http(&cfg, http.clone()).unwrap();
    let control = Control::default();
    control.resume();
    let host = QuestionHost {
        control: control.clone(),
        inner: Host::new([completed("/project")]),
        slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
    };
    let mut log = conversation();
    let mut stream = agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
    loop {
        let next = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if next.is_err() {
            break;
        }
        assert!(!matches!(
            next,
            Ok(Some(AgentEvent::Done(_))) | Ok(None) | Ok(Some(AgentEvent::Error(_)))
        ));
    }
    assert_eq!(http.requests().len(), 2);
    assert_eq!(host.inner.calls.lock().unwrap().len(), 1);
    assert_eq!(control.phase(), Phase::UserInput);
    let id = control.snapshot().questions[0].id.clone();
    control.answer_question(&id, answer(), |_| Ok(())).unwrap();
    while let Some(e) = stream.next().await {
        if let AgentEvent::Error(e) = e {
            panic!("{e}");
        }
    }
    assert_eq!(http.requests().len(), 3);
}
#[tokio::test]
async fn stopping_or_redirecting_a_question_never_selects_an_option() {
    for stop in [false, true] {
        let http = SequencedHttpClient::new([
            reply(0, &[("ask", "ask_user", question("wait"))], ""),
            reply(0, &[], "已按新要求继续"),
        ]);
        let cfg = setup(0);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let control = Control::default();
        control.resume();
        let host = QuestionHost {
            control: control.clone(),
            inner: Host::new([]),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        };
        let mut log = conversation();
        let mut stream =
            agent::run(&client, &mut log, capabilities(), &host, fast_limits()).unwrap();
        while !matches!(stream.next().await, Some(AgentEvent::ToolFinished(_))) {}
        if stop {
            control.request(Intent::StopReply);
        } else {
            control.enqueue(
                "redirect".into(),
                "先不部署，只解释配置".into(),
                Default::default(),
            );
        }
        while let Some(e) = stream.next().await {
            if let AgentEvent::Error(e) = e {
                panic!("{e}");
            }
        }
        drop(stream);
        let q = control.snapshot().questions.remove(0);
        if stop {
            assert!(q.response.is_none());
            assert_eq!(http.requests().len(), 1);
        } else {
            assert_eq!(q.response.unwrap().status, Status::Superseded);
            assert_eq!(http.requests().len(), 2);
        }
        assert!(host.inner.calls.lock().unwrap().is_empty());
    }
}
