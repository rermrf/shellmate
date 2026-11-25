//! M2 scenes use ordinary approval/task surfaces; the main agent is idle.
use crate::ai::tasks::{Kind, Status, TaskInfo};
use shellmate_ai::{SubagentKind, SubagentRef, Target, ToolCall};
use std::time::{Duration, Instant};
pub const SCENES: &[&str] = &["ai-subagent-approval", "ai-tasks-subagents"];
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
#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(rows[0].target_label(), "后台");
        assert_eq!(rows[1].target_label(), "子任务");
        assert_eq!(rows[1].bar_id(), "1");
    }
}
