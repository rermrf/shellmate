//! One presentation vocabulary for every tool, independent of its owning thread.
use shellmate_ai::{LoggedCall, LoggedResult, StepDisplay, StepStatus, SubagentStatus};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Approval,
    Question,
    Answered,
    Skipped,
    Updated,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Stopped,
    Denied,
    Paused,
    Unknown,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "等待批准",
            Self::Question => "待回答",
            Self::Answered => "已回答",
            Self::Skipped => "已跳过",
            Self::Updated => "已更新要求",
            Self::Running => "运行中",
            Self::Waiting => "等待结果",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
            Self::Stopped => "已停止",
            Self::Denied => "已拒绝",
            Self::Paused => "已暂停",
            Self::Unknown => "状态未知",
        }
    }
    pub fn color(self, p: &crate::theme::Palette) -> gpui::Hsla {
        match self {
            Self::Approval | Self::Question | Self::Paused => p.warn,
            Self::Running | Self::Waiting => p.acc,
            Self::Completed | Self::Answered => p.ok,
            Self::Failed => p.err,
            _ => p.tx3,
        }
    }
    pub fn badge_colors(self, p: &crate::theme::Palette) -> (gpui::Hsla, gpui::Hsla) {
        let bg = match self {
            Self::Approval | Self::Question | Self::Paused => p.warns,
            Self::Running | Self::Waiting => p.accs,
            Self::Completed | Self::Answered => p.oks,
            Self::Failed => p.errs,
            _ => p.card3,
        };
        (bg, self.color(p))
    }
    pub fn has_dot(self) -> bool {
        matches!(
            self,
            Self::Approval | Self::Running | Self::Waiting | Self::Completed | Self::Answered | Self::Failed
        )
    }
    pub fn attention(self) -> bool {
        matches!(self, Self::Question | Self::Approval | Self::Failed | Self::Denied)
    }
}
pub fn elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{:.1} 秒", ms as f64 / 1000.)
    } else {
        let s = ms / 1000;
        if s < 60 {
            format!("{s} 秒")
        } else if s < 3600 {
            format!("{} 分 {} 秒", s / 60, s % 60)
        } else {
            format!("{} 小时 {} 分", s / 3600, s / 60 % 60)
        }
    }
}
pub fn label(call: &LoggedCall) -> String {
    match call.name.as_str() {
        "ask_user" => "向你提问",
        "run_command" => "执行命令",
        "list_sessions" => "查看终端列表",
        "read_terminal" => "读取终端",
        // Display-only compatibility for saved calls; this tool is no longer offered.
        "compare_directories" => "比较目录",
        "web_search" => "搜索网页",
        "web_fetch" => "读取网页",
        "agent" => "委派子任务",
        "send_agent_message" => "追加子任务消息",
        "read_output" => "读取输出",
        "view_image" => "查看图片",
        "wait_tasks" => "等待任务",
        "stop_task" => "停止任务",
        "memory" => "记忆操作",
        other => return format!("工具 · {other}"),
    }
    .into()
}
pub fn summary(call: &LoggedCall) -> String {
    match call.name.as_str() {
        "view_image" => return "读取会话中的图片".into(),
        "list_sessions" => return "查看已打开的终端与连接状态".into(),
        "read_terminal" => {
            return format!(
                "读取最近 {} 行输出",
                call.args_json["lines"].as_u64().unwrap_or(40)
            );
        }
        "compare_directories" => {
            return format!(
                "{} ↔ {}",
                call.args_json["left"]["path"]
                    .as_str()
                    .unwrap_or("左侧目录"),
                call.args_json["right"]["path"]
                    .as_str()
                    .unwrap_or("右侧目录")
            );
        }
        _ => {}
    }
    if call.name == "read_output" {
        return "读取已保存的执行输出".into();
    }
    if call.name == "stop_task" {
        return "停止异步任务".into();
    }
    let field = match call.name.as_str() {
        "run_command" => "command",
        "web_search" => "query",
        "web_fetch" => "url",
        "agent" => "description",
        "send_agent_message" => "message",
        "memory" => "path",
        _ => "id",
    };
    call.args_json[field]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if call.name == "wait_tasks" {
                format!(
                    "等待 {} 个任务",
                    call.args_json["ids"].as_array().map_or(0, Vec::len)
                )
            } else {
                "查看输入与执行结果".into()
            }
        })
}
pub fn command<'a>(call: &'a LoggedCall, display: Option<&'a StepDisplay>) -> &'a str {
    display
        .map(|d| d.command.as_str())
        .unwrap_or_else(|| call.args_json["command"].as_str().unwrap_or_default())
}
pub fn compact(text: &str, limit: usize) -> String {
    let s = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > limit {
        format!("{}…", s.chars().take(limit).collect::<String>())
    } else {
        s
    }
}
pub fn command_status(display: Option<&StepDisplay>, result: Option<&LoggedResult>) -> Status {
    if let Some(d) = display {
        return match &d.status {
            StepStatus::Completed => {
                if successful(d) {
                    Status::Completed
                } else {
                    Status::Failed
                }
            }
            StepStatus::Background => Status::Waiting,
            StepStatus::Denied { .. } => Status::Denied,
            StepStatus::TimedOut
            | StepStatus::Busy
            | StepStatus::Disconnected
            | StepStatus::Failed(_) => Status::Failed,
        };
    }
    if let Some(r) = result {
        if r.is_error {
            Status::Failed
        } else {
            Status::Completed
        }
    } else {
        Status::Running
    }
}
fn successful(d: &StepDisplay) -> bool {
    shellmate_ai::output::successful(&d.command, d.exit_code)
}
pub fn child_status(status: &SubagentStatus) -> Status {
    match status {
        SubagentStatus::Running => Status::Running,
        SubagentStatus::Completed => Status::Completed,
        SubagentStatus::Stopped => Status::Cancelled,
        SubagentStatus::StepLimit => Status::Paused,
        SubagentStatus::Failed(_) => Status::Failed,
    }
}
/// Startup evidence is current only while the service is still running.
pub fn service_detail(task: &super::tasks::TaskInfo) -> Option<String> {
    let service = task.service?;
    Some(if task.status == super::tasks::Status::Running {
        format!("持续服务 · {}", service.label())
    } else {
        "持续服务".into()
    })
}
pub fn service_history(task: &super::tasks::TaskInfo) -> Option<&'static str> {
    use super::tasks::{ServiceState, Status};
    if task.status == Status::Running { return None; }
    task.service.map(|service| match service {
        ServiceState::Ready => "启动记录：曾出现就绪信号；不代表服务当前仍可用。",
        ServiceState::Starting | ServiceState::Failed => "启动记录：结束前未完成启动检查。",
        ServiceState::Unverified => "启动记录：结束前未确认就绪。",
    })
}
/// Live IDs survive provider call-id reuse and the normalization used in saved logs.
#[derive(Clone, Debug, Default)]
pub struct Identities {
    next: u64,
    active: BTreeMap<String, String>,
    saved: BTreeMap<String, String>,
}
impl Identities {
    pub fn start(&mut self, raw: &str) {
        if !self.active.contains_key(raw) {
            self.next += 1;
            self.active
                .insert(raw.into(), format!("live-{}", self.next));
        }
    }
    pub fn finish(&mut self, raw: &str, saved: &str) {
        if let Some(id) = self.active.remove(raw) {
            self.saved.insert(saved.into(), id);
        }
    }
    pub fn active(&self, raw: &str) -> String {
        self.active
            .get(raw)
            .cloned()
            .unwrap_or_else(|| format!("pending-{raw}"))
    }
    pub fn saved(&self, id: &str) -> String {
        self.saved
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("saved-{id}"))
    }
    pub fn clear_active(&mut self) {
        self.active.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_survives_finish_and_reused_provider_id_does_not_change_old_card() {
        let mut ids = Identities::default();
        ids.start("call");
        let first = ids.active("call");
        ids.start("call");
        assert_eq!(ids.active("call"), first);
        ids.finish("call", "call");
        assert_eq!(ids.saved("call"), first);
        ids.start("call");
        let next = ids.active("call");
        assert_ne!(next, first);
        ids.finish("call", "call_42_0");
        assert_eq!(ids.saved("call_42_0"), next);
        assert_eq!(ids.saved("call"), first);
    }
    #[test]
    fn process_and_child_states_are_explicit() {
        assert_eq!(child_status(&SubagentStatus::Completed), Status::Completed);
        assert_eq!(child_status(&SubagentStatus::Stopped), Status::Cancelled);
        assert_eq!(child_status(&SubagentStatus::StepLimit), Status::Paused);
        assert_eq!(command_status(None, None), Status::Running);
        assert_eq!(compact("first\nsecond", 6), "first …");
    }
    #[test]
    fn command_exit_status_is_independent_from_tool_transport_success() {
        let mut d = StepDisplay {
            command: "false".into(),
            target: shellmate_ai::Target::Background,
            purpose: "check".into(),
            status: StepStatus::Completed,
            exit_code: Some(1),
            duration_ms: 10,
            automatic: true,
            output: String::new(),
            class: shellmate_ai::policy::Class::ReadOnly,
        };
        assert_eq!(command_status(Some(&d), None), Status::Failed);
        d.command = "grep pattern /tmp/file".into();
        assert_eq!(command_status(Some(&d), None), Status::Completed);
        d.status = StepStatus::Background;
        assert_eq!(command_status(Some(&d), None), Status::Waiting);
        d.status = StepStatus::Denied { reason: None };
        assert_eq!(command_status(Some(&d), None), Status::Denied);
        assert_eq!(elapsed(150), "0.1 秒");
        assert_eq!(elapsed(62_000), "1 分 2 秒");
        let call = LoggedCall {
            id: "requested".into(),
            name: "run_command".into(),
            args_json: serde_json::json!({"command":"old command"}),
        };
        assert_eq!(command(&call, Some(&d)), "grep pattern /tmp/file");
        assert_eq!(command(&call, None), "old command");
    }
}
