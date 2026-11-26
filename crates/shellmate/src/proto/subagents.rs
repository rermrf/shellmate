//! Deterministic child cards rendered with the real drawer components.
use crate::ai::tasks::{Kind, Status, TaskInfo};
use shellmate_ai::{SubagentKind, SubagentRef, Target, ToolCall};
use std::time::{Duration, Instant};
pub const SCENES: &[&str] = &[
    "ai-subagent-approval",
    "ai-tasks-subagents",
    "ai-subagent-running",
    "ai-subagent-done",
    "ai-subagent-expanded",
    "ai-subagent-states",
    "ai-subagent-background",
];
pub fn approvals() -> Vec<(SubagentRef, ToolCall)> {
    [
        (
            "agent-1",
            "检查 nginx 错误日志",
            "tail -n 80 /var/log/nginx/error.log",
        ),
        (
            "agent-2",
            "清理 7 天前的日志",
            "rm -rf /var/log/app/archive/*",
        ),
    ]
    .into_iter()
    .map(|(id, description, command)| {
        let source = SubagentRef {
            id: id.into(),
            description: description.into(),
            kind: SubagentKind::General,
        };
        let call = ToolCall {
            service: None,
            session_id: None,
            cwd: None, destination: None,
            id: format!("{id}-call"),
            command: command.into(),
            purpose: description.into(),
            target: Target::Background,
            timeout: Duration::from_secs(120),
            run_in_background: false,
            exec_limits: Default::default(),
            class: if id == "agent-2" {
                shellmate_ai::policy::Class::RedLine
            } else {
                shellmate_ai::policy::classify(command)
            },
        };
        (source, call)
    })
    .collect()
}
pub fn tasks() -> Vec<TaskInfo> {
    [
        (
            "1",
            "cd /srv/app && cargo build --release",
            Kind::Command,
            147,
        ),
        ("agent-1", "检查 nginx 错误日志", Kind::Agent, 92),
        ("agent-2", "检查应用和数据库连接", Kind::Agent, 43),
    ]
    .into_iter()
    .map(|(id, description, kind, elapsed)| TaskInfo {
        service: None, destination: None, owner: None,
        kind,
        call_id: format!("call-{id}"),
        id: id.into(),
        command: description.into(),
        target: Target::Background,
        started: Instant::now() - Duration::from_secs(elapsed),
        status: Status::Running,
        output_id: format!("output-{id}"),
        background: true,
    })
    .collect()
}
pub struct Cards {
    pub conversation: shellmate_ai::Conversation,
    pub views: Vec<crate::ai::subagent_view::View>,
    pub expanded: Option<(String, String)>,
    pub tasks: Vec<TaskInfo>,
}
const REPORT: &str = "## 结论\nnginx 的 502 来自上游连接超时。\n\n- 关键证据：`127.0.0.1:8080` 的响应超过 30 秒。\n- 未修改服务配置，建议先检查应用连接池。\n\n```sh\ncurl -I http://127.0.0.1:8080/health\n```\n\n未完成或风险：尚未检查高峰期的数据库延迟。";
pub fn view(id: &str, description: &str) -> crate::ai::subagent_view::View {
    let config = shellmate_ai::Config {
        provider: Default::default(),
        model: "gpt-5".into(),
        api_key: shellmate_ai::ApiKey::new("fixture"),
        reasoning: Default::default(),
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: Default::default(),
    };
    let request = shellmate_ai::SubagentRequest {
        session_ids: vec![],
        default_session_id: None,
        description: description.into(),
        prompt: "fixture".into(),
        kind: SubagentKind::Explore,
        background: false,
        model: None,
        reasoning: None,
    };
    let mut view = crate::ai::subagent_view::View::new(id.into(), &request, &config);
    view.call_id = format!("call-{id}");
    view.output_id = Some(id.into());
    view.transcript_id = Some(id.into());
    view.started = Instant::now() - Duration::from_secs(18);
    view.duration_ms = 18_000;
    view
}
fn steps(view: &mut crate::ai::subagent_view::View) {
    let commands = [
        (
            "systemctl status nginx --no-pager",
            "Active: active (running)\nMain PID: 481 (nginx)",
            0,
        ),
        (
            "tail -n 80 /var/log/nginx/error.log",
            "2026/11/25 14:03:12 [error] upstream timed out\nupstream: http://127.0.0.1:8080/api/orders\nrequest: GET /api/orders HTTP/1.1",
            0,
        ),
        (
            "grep -c 'connection refused' /var/log/nginx/error.log",
            "0",
            1,
        ),
        (
            "journalctl -u nginx --since \"1 hour ago\" | tail -n 200",
            "Started nginx web server.\nNo configuration changes detected.",
            0,
        ),
    ];
    let mut log = shellmate_ai::Conversation::default();
    for (index, (command, output, exit)) in commands.into_iter().enumerate() {
        view.step_list.push(crate::ai::subagent_view::StepView {
            id: format!("step-{}", index + 1),
            command: command.into(),
            exit_code: Some(exit),
            duration_ms: 1300,
            output_id: None,
            status: shellmate_ai::StepStatus::Completed,
            running: false,
            output: output.into(),
        });
        log.push_item(shellmate_ai::AnswerItem::Step {
            command: command.into(),
            target: Target::Background,
            purpose: "检查服务".into(),
            class: shellmate_ai::policy::Class::ReadOnly,
            status: shellmate_ai::StepStatus::Completed,
            exit_code: Some(exit),
            duration_ms: 1300,
            automatic: true,
            output: output.into(),
        });
    }
    view.steps = 4;
    view.transcript = Some(log.entries().to_vec());
    view.latest_activity = view.step_list.last().unwrap().command.clone();
}
pub fn cards(scene: &str) -> Option<Cards> {
    use shellmate_ai::SubagentStatus as Status;
    if !SCENES.contains(&scene) || scene == "ai-tasks-subagents" {
        return None;
    }
    let mut first = view("agent-1", "检查 nginx 错误日志");
    steps(&mut first);
    first.report = REPORT.into();
    first.status = Status::Completed;
    first.duration_ms = 62_000;
    let mut expanded = None;
    let views = match scene {
        "ai-subagent-running" => {
            first.status = Status::Running;
            first.steps = 3;
            first.duration_ms = 18_000;
            first.report.clear();
            let step = first.step_list.last_mut().unwrap();
            step.running = true;
            step.exit_code = None;
            vec![first]
        }
        "ai-subagent-done" => {
            first.kind = SubagentKind::General;
            first.model = "gpt-5-mini".into();
            first.reasoning = shellmate_ai::ReasoningLevel::Low;
            first.model_overridden = true;
            first.reasoning_overridden = true;
            first.usage = shellmate_ai::Usage {
                input_tokens: Some(18_400),
                output_tokens: Some(2100),
            };
            vec![first]
        }
        "ai-subagent-expanded" => {
            expanded = Some((first.id.clone(), "step-2".into()));
            vec![first]
        }
        "ai-subagent-states" => {
            first.status = Status::Failed(shellmate_ai::AiError::Disconnected);
            first.report = "## 部分结论\nnginx 服务仍在运行，连接中断前尚未完成上游检查。".into();
            first.steps = 2;
            let mut stopped = view("agent-2", "检查应用连接池");
            stopped.status = Status::Stopped;
            stopped.steps = 1;
            stopped.report = "已读取应用配置，任务被停止，未作变更。".into();
            let mut limit = view("agent-3", "分析数据库慢查询");
            limit.status = Status::StepLimit;
            limit.steps = 8;
            limit.report = "已定位两条慢查询，尚未核对索引。\n（达到步数上限，结果不完整）".into();
            vec![first, stopped, limit]
        }
        "ai-subagent-background" => {
            first.status = Status::Running;
            first.background = true;
            first.duration_ms = 18_000;
            first.steps = 3;
            first.report.clear();
            let mut second = view("agent-2", "检查应用和数据库连接");
            steps(&mut second);
            second.background = true;
            second.status = Status::Completed;
            second.duration_ms = 43_000;
            second.report =
                "## 结论\n数据库连接正常。\n应用连接池已接近上限，建议关注等待队列。".into();
            vec![first, second]
        }
        "ai-subagent-approval" => approvals()
            .into_iter()
            .map(|(source, call)| {
                let mut v = view(&source.id, &source.description);
                v.kind = source.kind;
                v.background = true;
                v.step_list.push(crate::ai::subagent_view::StepView {
                    id: call.id,
                    command: call.command,
                    exit_code: None,
                    duration_ms: 0,
                    output_id: None,
                    status: shellmate_ai::StepStatus::Background,
                    running: true,
                    output: String::new(),
                });
                v
            })
            .collect(),
        _ => unreachable!(),
    };
    let mut conversation = shellmate_ai::Conversation::default();
    conversation.push_user(shellmate_ai::UserTurn::new(
        "检查 nginx 502 的原因",
        shellmate_ai::ContextSnapshot::default(),
    ));
    if scene == "ai-subagent-background" {
        conversation.push_text("已在后台启动两个子任务");
    }
    for v in &views {
        let mut display = v.display();
        display.duration_ms = v.duration_ms;
        conversation.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
            message_id: conversation.entries().len(),
            call: shellmate_ai::LoggedCall {
                id: v.call_id.clone(),
                name: "agent".into(),
                args_json: serde_json::json!({"run_in_background":v.background}),
            },
            result: shellmate_ai::LoggedResult {
                subagent: Some(Box::new(display)),
                inline: v.report.clone(),
                output_id: v.output_id.clone(),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
    }
    if scene == "ai-subagent-done" {
        conversation.push_text("上游应用响应慢是当前最直接的线索。我会继续检查连接池和数据库等待情况，再给出调整建议。");
    }
    let tasks = if scene == "ai-subagent-background" {
        vec![TaskInfo {
            service: None, destination: None, owner: None,
            kind: Kind::Agent,
            call_id: views[0].call_id.clone(),
            id: views[0].id.clone(),
            command: views[0].description.clone(),
            target: Target::Background,
            started: views[0].started,
            status: crate::ai::tasks::Status::Running,
            output_id: views[0].id.clone(),
            background: true,
        }]
    } else {
        vec![]
    };
    Some(Cards {
        conversation,
        views,
        expanded,
        tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn card_scenes_cover_live_reports_expansion_and_terminal_states() {
        use shellmate_ai::SubagentStatus;
        let running = cards("ai-subagent-running").unwrap();
        assert_eq!(running.views[0].status, SubagentStatus::Running);
        assert_eq!(running.views[0].steps, 3);
        assert_eq!(running.views[0].duration_ms, 18_000);
        assert!(!running.views[0].background);
        assert!(running.views[0].step_list.last().unwrap().running);
        assert_eq!(
            running.views[0].step_list.last().unwrap().command,
            "journalctl -u nginx --since \"1 hour ago\" | tail -n 200"
        );
        let done = cards("ai-subagent-done").unwrap();
        assert_eq!(
            crate::ai::subagent_card::meta(&done.views[0]).unwrap(),
            "gpt-5-mini · 思考 低 · 输入 18.4K · 输出 2.1K token"
        );
        assert!(matches!(
            done.conversation.turns()[0]
                .answer
                .as_ref()
                .unwrap()
                .items
                .last(),
            Some(shellmate_ai::AnswerItem::Text(_))
        ));
        let expanded = cards("ai-subagent-expanded").unwrap();
        assert_eq!(expanded.expanded, Some(("agent-1".into(), "step-2".into())));
        assert_eq!(expanded.views[0].step_list.len(), 4);
        assert_eq!(expanded.views[0].step_list[2].exit_code, Some(1));
        assert!(expanded.views[0].report.contains("```sh"));
        assert!(expanded.views[0].report.contains("`127.0.0.1:8080`"));
        let states = cards("ai-subagent-states").unwrap();
        assert_eq!(
            states
                .views
                .iter()
                .map(|v| v.status.clone())
                .collect::<Vec<_>>(),
            vec![
                SubagentStatus::Failed(shellmate_ai::AiError::Disconnected),
                SubagentStatus::Stopped,
                SubagentStatus::StepLimit
            ]
        );
        let background = cards("ai-subagent-background").unwrap();
        assert_eq!(background.views.len(), 2);
        assert_eq!(background.tasks.len(), 1);
        assert!(background.views.iter().all(|v| v.background));
        let approvals = cards("ai-subagent-approval").unwrap();
        assert_eq!(approvals.views.len(), 2);
        assert!(
            approvals
                .views
                .iter()
                .all(|v| v.status == SubagentStatus::Running && v.background)
        );
    }
    #[test]
    fn source_approvals_and_mixed_tasks_are_registered_in_both_themes() {
        for scene in SCENES {
            assert!(super::super::SCENES.contains(scene));
            assert_eq!(super::super::default_window_size(scene), (1440, 900));
            for dark in [false, true] {
                assert!(
                    super::super::readings::READINGS
                        .iter()
                        .any(|r| r.scene == *scene && r.dark == dark)
                );
            }
        }
        let cards = approvals();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].0.description, "检查 nginx 错误日志");
        assert_eq!(cards[1].1.class, shellmate_ai::policy::Class::RedLine);
        let rows = tasks();
        assert_eq!(rows.len(), 3);
        assert!(rows.windows(2).all(|r| r[0].started < r[1].started));
        assert_eq!(rows[0].target_label(), "独立通道");
        assert_eq!(rows[1].target_label(), "子任务");
    }
}
