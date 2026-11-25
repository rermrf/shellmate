use super::*;
use shellmate_ai::{AiError, ReasoningLevel, SubagentDisplay, SubagentKind, Usage};

fn view() -> View {
    View::reopened(
        "agent-1".into(),
        &SubagentDisplay {
            description: "检查服务".into(),
            kind: SubagentKind::Explore,
            model: "gpt-5-mini".into(),
            reasoning: ReasoningLevel::Low,
            model_overridden: false,
            reasoning_overridden: false,
            status: SubagentStatus::Completed,
            steps: 4,
            duration_ms: 62_000,
            usage: Usage::default(),
            output_id: Some("agent-1".into()),
            transcript_id: Some("agent-1".into()),
        },
        false,
        "inline only".into(),
    )
}
#[test]
fn status_colors_and_duration_boundaries() {
    for (seconds, text) in [
        (0, "0s"),
        (59, "59s"),
        (60, "1m00s"),
        (62, "1m02s"),
        (3599, "59m59s"),
        (3600, "1h00m"),
        (3720, "1h02m"),
    ] {
        assert_eq!(duration(seconds), text);
    }
    for (s, text, color) in [
        (
            SubagentStatus::Running,
            "运行中 · 3 步 · 18s",
            Color::Accent,
        ),
        (SubagentStatus::Completed, "完成 · 3 步 · 18s", Color::Ok),
        (
            SubagentStatus::StepLimit,
            "达到步数上限 · 3 步",
            Color::Warning,
        ),
        (
            SubagentStatus::Failed(AiError::Disconnected),
            "失败 · 3 步",
            Color::Error,
        ),
        (SubagentStatus::Stopped, "已停止 · 3 步", Color::Quiet),
    ] {
        assert_eq!(status(&s, 3, 18), (text.into(), color));
    }
}
#[test]
fn meta_only_shows_overrides_and_known_usage_with_error_before_usage() {
    let mut v = view();
    assert_eq!(meta(&v), None);
    v.model_overridden = true;
    assert_eq!(meta(&v).as_deref(), Some("gpt-5-mini"));
    v.reasoning_overridden = true;
    v.usage = Usage {
        input_tokens: Some(18400),
        output_tokens: Some(2100),
    };
    assert_eq!(
        meta(&v).unwrap(),
        "gpt-5-mini · 思考 低 · 输入 18.4K · 输出 2.1K token"
    );
    v.model_overridden = false;
    v.reasoning_overridden = false;
    v.status = SubagentStatus::Failed(AiError::Disconnected);
    v.usage = Usage {
        input_tokens: Some(999),
        output_tokens: Some(0),
    };
    assert_eq!(meta(&v).unwrap(), "连接已断开 · 输入 999 · 输出 0 token");
    assert_eq!(tokens(Some(1000)), "1.0K");
    assert_eq!(tokens(None), "未知");
}
#[test]
fn preview_strips_markers_and_clamps_nonempty_lines() {
    assert_eq!(
        preview("\n## **结论**\n\n> - **证据**\n1. 已检查\n* 风险"),
        "结论\n证据\n已检查…"
    );
    assert_eq!(preview("# \n  \n> \n"), EMPTY_REPORT);
    assert_eq!(
        preview("1.2 秒\n- 已完成\n**没有变更**"),
        "1.2 秒\n已完成\n没有变更"
    );
}
#[test]
fn expansion_is_per_id_and_only_one_step_is_open() {
    let mut cards: BTreeMap<String, Expansion> = BTreeMap::new();
    let a = cards.entry("agent-1".into()).or_default();
    assert!(!a.open);
    a.open = true;
    a.toggle_step("one");
    a.toggle_step("two");
    assert_eq!(a.step.as_deref(), Some("two"));
    a.toggle_step("two");
    assert_eq!(a.step, None);
    assert!(!cards.entry("agent-2".into()).or_default().open);
    assert!(cards["agent-1"].open);
}
#[test]
fn missing_and_corrupt_transcripts_are_loaded_once_and_report_is_retained() {
    let root = tempfile::tempdir().unwrap();
    let mut v = view();
    v.load_transcript(root.path(), "conversation");
    assert_eq!(transcript_note(&v), Some(EXPIRED));
    assert_eq!(v.report, "inline only");
    crate::ai::subagent_files::write(root.path(), "conversation", "agent-1", &[]).unwrap();
    v.load_transcript(root.path(), "conversation");
    assert!(v.transcript.is_none(), "do not retry on each expand");
    let mut reopened = view();
    reopened.load_transcript(root.path(), "conversation");
    assert!(reopened.transcript.is_some());
    assert_eq!(transcript_note(&reopened), None);
    let path = shellmate_core::ai_output::path(root.path(), "conversation", "agent-1")
        .unwrap()
        .with_extension("json");
    std::fs::write(path, "broken").unwrap();
    let mut corrupt = view();
    corrupt.load_transcript(root.path(), "conversation");
    assert_eq!(transcript_note(&corrupt), Some(EXPIRED));
    let mut live = view();
    live.history = false;
    assert_eq!(transcript_note(&live), None);
}
#[test]
fn copy_and_expanded_report_use_full_spool_without_loading_transcript() {
    let root = tempfile::tempdir().unwrap();
    let mut v = view();
    let path = shellmate_core::ai_output::path(root.path(), "conversation", "agent-1").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let report = format!("## 结论\n{}\n最后证据", "完整报告\n".repeat(1000));
    std::fs::write(&path, &report).unwrap();
    v.load_report(root.path(), "conversation");
    assert_eq!(copy_report(&v), report);
    assert_eq!(v.report, report);
    assert!(v.transcript.is_none());
    assert_ne!(preview(&v.report), copy_report(&v));
    std::fs::remove_file(path).unwrap();
    v.load_report(root.path(), "conversation");
    assert_eq!(copy_report(&v), report);
}
#[test]
fn live_step_states_and_output_survive_projection() {
    assert_eq!(
        command_line("printf first\nprintf second"),
        "printf first printf second"
    );
    let mut step = StepView {
        id: "s".into(),
        command: "cat /etc/hosts".into(),
        exit_code: Some(0),
        duration_ms: 1300,
        output_id: None,
        status: StepStatus::Completed,
        running: false,
        output: "localhost".into(),
    };
    assert_eq!(step_status(&step), ("0 · 1.3s".into(), Color::Ok));
    step.exit_code = Some(1);
    assert_eq!(step_status(&step).1, Color::Error);
    step.running = true;
    assert_eq!(step_status(&step), ("运行中".into(), Color::Accent));
    step.running = false;
    step.status = StepStatus::Background;
    assert_eq!(step_status(&step), ("已转后台".into(), Color::Accent));
    step.status = StepStatus::Denied {
        reason: Some("不允许".into()),
    };
    assert_eq!(step_status(&step), ("已拒绝".into(), Color::Error));
}

#[test]
fn pending_card_becomes_the_same_projected_step_and_history_gets_its_scope() {
    let mut live = view();
    live.history = false;
    live.call_id = "delegate".into();
    let mut views = BTreeMap::from([(live.id.clone(), live.clone())]);
    let mut log = shellmate_ai::Conversation::default();
    log.push_user(shellmate_ai::UserTurn::new("检查", Default::default()));
    assert!(projected(&log, &mut views).is_empty());
    log.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
        message_id: 1,
        call: shellmate_ai::LoggedCall {
            id: "delegate".into(),
            name: "agent".into(),
            args_json: serde_json::json!({"run_in_background":true}),
        },
        result: shellmate_ai::LoggedResult {
            subagent: Some(Box::new(live.display())),
            inline: "inline".into(),
            output_id: live.output_id.clone(),
            cleared: false,
            is_error: false,
        },
        display: None,
    }));
    log.push_text("main follow-up");
    assert_eq!(projected(&log, &mut views)[&(0, 0)], "agent-1");
    assert_eq!(views.len(), 1);
    assert!(!views["agent-1"].history);
    let mut reopened = BTreeMap::new();
    assert_eq!(projected(&log, &mut reopened)[&(0, 0)], "agent-1");
    assert!(reopened["agent-1"].history);
    assert!(reopened["agent-1"].background);
    assert!(reopened["agent-1"].transcript.is_none());
}

#[test]
fn registry_refresh_preserves_historical_full_report_and_once_only_transcript_reads() {
    let root = tempfile::tempdir().unwrap();
    let path = shellmate_core::ai_output::path(root.path(), "conversation", "agent-1").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "full report").unwrap();
    let mut v = view();
    v.load_report(root.path(), "conversation");
    v.load_transcript(root.path(), "conversation");
    v.refresh(view());
    crate::ai::subagent_files::write(root.path(), "conversation", "agent-1", &[]).unwrap();
    v.load_transcript(root.path(), "conversation");
    assert_eq!(copy_report(&v), "full report");
    assert_eq!(
        transcript_note(&v),
        Some(EXPIRED),
        "a registry tick must not retry missing files"
    );
    let mut available = view();
    available.load_transcript(root.path(), "conversation");
    available.refresh(view());
    assert!(available.transcript.is_some());
    assert_eq!(transcript_note(&available), None);
    let mut live = view();
    live.history = false;
    live.report = "new live report".into();
    v.refresh(live);
    assert_eq!(copy_report(&v), "new live report");
    assert!(!v.history);
}
