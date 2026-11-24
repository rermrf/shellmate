use super::*;
use crate::{
    CompactTrigger, Compaction, ContextSnapshot, LogUpdate, LoggedCall, LoggedResult,
    RetainedState, Step, Usage, UserTurn,
};
fn budget() -> ContextBudget {
    ContextBudget::new(200000, ContextWindow::Auto, 32000)
}
fn caps() -> Capabilities {
    Capabilities {
        background: true,
        ..Default::default()
    }
}
fn log() -> Conversation {
    let mut log = Conversation::default();
    log.push_user(UserTurn::new(
        "检查服务",
        ContextSnapshot {
            label: "fixture-host".into(),
            protocol: "SSH".into(),
            text: "ONLY-ONCE-IN-TERMINAL".into(),
        },
    ));
    log
}
fn step(cleared: bool) -> Entry {
    Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "call1".into(),
            name: "run_command".into(),
            args_json: serde_json::json!({"command":"uname -s","target":"background"}),
        },
        result: LoggedResult {
            inline: "line1\nline2\nline3\nline4\nline5\nline6\nline7".repeat(100),
            output_id: Some("output1".into()),
            cleared,
            is_error: false,
        },
        display: None,
    })
}
#[test]
fn system_and_tool_schemas_are_separate_even_before_first_message() {
    let context = ContextSnapshot::default();
    for caps in [Capabilities::default(), caps()] {
        let b = ContextBreakdown::for_session(
            budget(),
            &caps,
            &context,
            crate::agent::ExecLimits::default(),
        );
        let prompt = if caps.available() {
            crate::conversation::execution_prompt(&context, &caps)
        } else {
            crate::conversation::system_prompt(&context)
        };
        assert_eq!(b.system_prompt, estimate_tokens(&prompt) + 8);
        assert_eq!(b.tool_count, if caps.available() { 4 } else { 1 });
        assert!(b.tool_definitions > 0);
        assert_eq!(b.total, b.system_prompt + b.tool_definitions);
        assert_eq!(
            b.categories().iter().map(|(_, n)| n).sum::<usize>(),
            b.total
        );
        assert!(!b.provider_usage);
    }
}
#[test]
fn terminal_once_per_message_and_tool_arguments_and_stubs_count() {
    let mut log = log();
    let caps = caps();
    let first = log.breakdown_for_budget(budget(), &caps).unwrap();
    let user = log.latest_user().unwrap().clone();
    let expected =
        estimate_tokens(&user.wire_text()) - estimate_tokens(&format!("问题：{}", user.question));
    assert_eq!(first.terminal_context, expected);
    log.push_user(user);
    assert_eq!(
        log.breakdown_for_budget(budget(), &caps)
            .unwrap()
            .terminal_context,
        2 * expected
    );
    assert_eq!(first.summary, 0);
    log.append(step(false));
    let b = log.breakdown_for_budget(budget(), &caps).unwrap();
    let Entry::Step(step) = log.entries().last().unwrap() else {
        panic!("step")
    };
    assert_eq!(
        b.tool_results,
        estimate_tokens(&step.call.name)
            + estimate_tokens(&step.call.args_json.to_string())
            + estimate_tokens(&step.result_text())
    );
    assert_eq!(
        b.categories().iter().map(|(_, n)| n).sum::<usize>(),
        b.total
    );
    let mut entries = log.entries().to_vec();
    if let Entry::Step(step) = entries.last_mut().unwrap() {
        step.result.cleared = true;
    }
    let cleared = Conversation::from_entries(entries)
        .breakdown_for_budget(budget(), &caps)
        .unwrap();
    assert!(cleared.tool_results > 0 && cleared.tool_results < b.tool_results);
}
#[test]
fn summary_contains_retained_users_and_state_but_snapshot_is_terminal() {
    let mut log = log();
    let c = Compaction {
        summary: Some("服务正常".into()),
        retained_user: vec!["保留用户要求".into()],
        state: RetainedState::default(),
        context: ContextSnapshot {
            label: "fixture-host".into(),
            protocol: "SSH".into(),
            text: "FRESH-SNAPSHOT".into(),
        },
        before_tokens: 10000,
        after_tokens: 1000,
        trigger: CompactTrigger::Manual,
    };
    let expected = estimate_tokens(&format!(
        "{SUMMARY_PREFIX}\n{}",
        c.summary.as_deref().unwrap()
    )) + estimate_tokens(&c.retained_user[0])
        + estimate_tokens(&format!(
            "工具的当前状态：{}",
            serde_json::to_string(&c.state).unwrap()
        ));
    let terminal = estimate_tokens(&UserTurn::new("最新终端上下文", c.context.clone()).wire_text());
    log.append(Entry::Compaction(c));
    let b = log.breakdown_for_budget(budget(), &caps()).unwrap();
    assert_eq!(b.summary, expected);
    assert_eq!(b.terminal_context, terminal);
    assert_eq!(
        b.categories().iter().map(|(_, n)| n).sum::<usize>(),
        b.total
    );
}
#[test]
fn usage_scales_exactly_preserves_zero_categories_and_clamps_remainder() {
    let mut log = log();
    log.append(step(false));
    log.push_text("assistant response");
    let raw = log.breakdown_for_budget(budget(), &caps()).unwrap();
    for total in [0, 1, 101, 73600, 210000] {
        log.apply(LogUpdate::Usage(Usage {
            input_tokens: Some(total),
            output_tokens: Some(0),
        }));
        let b = log.breakdown_for_budget(budget(), &caps()).unwrap();
        assert!(b.provider_usage);
        assert_eq!(b.total, total as usize);
        assert_eq!(
            b.categories().iter().map(|(_, n)| n).sum::<usize>(),
            b.total
        );
        assert_eq!(b.summary, 0);
        assert_eq!(b.remaining(), 200000usize.saturating_sub(total as usize));
        for ((_, a), (_, n)) in raw.categories().into_iter().zip(b.categories()) {
            let proportion = a as f64 * total as f64 / raw.total as f64;
            assert!((n as f64 - proportion).abs() < 1.01);
        }
    }
}
