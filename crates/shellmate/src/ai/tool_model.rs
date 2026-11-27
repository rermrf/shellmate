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
    Read,
    Sent,
    ConditionMet,
    TimedOut,
    ScreenChanged,
    TakenOver,
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
            Self::Read => "已读取",
            Self::Sent => "已发送",
            Self::ConditionMet => "条件已满足",
            Self::TimedOut => "等待超时",
            Self::ScreenChanged => "画面已变化",
            Self::TakenOver => "已接管",
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
            Self::Approval | Self::Question | Self::Paused | Self::TimedOut | Self::ScreenChanged => p.warn,
            Self::Running | Self::Waiting => p.acc,
            Self::Completed | Self::Answered | Self::ConditionMet => p.ok,
            Self::Failed => p.err,
            _ => p.tx3,
        }
    }
    pub fn badge_colors(self, p: &crate::theme::Palette) -> (gpui::Hsla, gpui::Hsla) {
        let bg = match self {
            Self::Approval | Self::Question | Self::Paused | Self::TimedOut | Self::ScreenChanged => p.warns,
            Self::Running | Self::Waiting => p.accs,
            Self::Completed | Self::Answered | Self::ConditionMet => p.oks,
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
        matches!(self, Self::Question | Self::Approval | Self::Failed | Self::Denied | Self::TimedOut | Self::ScreenChanged | Self::TakenOver)
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
        "terminal_input" => "终端交互",
        "create_terminal" => "创建独立终端",
        "manage_terminal" => "管理独立终端",
        "wait_terminal" => "等待终端画面",
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
        "create_terminal" | "manage_terminal" | "terminal_input" => return call.args_json["purpose"].as_str().unwrap_or("操作终端交互程序").into(),
        "wait_terminal" => return call.args_json["text"].as_str().map(|s|format!("等待画面出现：{s}")).unwrap_or_else(||"等待终端画面变化".into()),
        "read_terminal" if call.args_json["screen"]==true => return "读取程序当前画面".into(),
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
/// Execution transport success is distinct from observing the requested result.
pub fn terminal_status(call:&LoggedCall,result:Option<&LoggedResult>)->Option<(Status,String)>{
    if !matches!(call.name.as_str(),"read_terminal"|"terminal_input"|"wait_terminal"){return None;}
    let result = result?;
    let v=serde_json::from_str::<serde_json::Value>(&result.inline).unwrap_or_default();
    if result.is_error{
        let status=match v["code"].as_str(){Some("screen_changed")=>Status::ScreenChanged,Some("user_takeover")=>Status::TakenOver,Some("stopped")=>Status::Stopped,_=>Status::Failed};
        let detail=v["message"].as_str().or(v["error"].as_str()).unwrap_or(&result.inline);
        return Some((status,compact(detail,160)));
    }
    let status=match call.name.as_str(){"read_terminal"=>Status::Read,"terminal_input"=>if call.args_json["release"]==true{Status::Completed}else{Status::Sent},_=>if v["timed_out"]==true{Status::TimedOut}else if v["matched"]==true{Status::ConditionMet}else if v["screen"]["ended"]==true{Status::Stopped}else{Status::Unknown}};
    let detail=match call.name.as_str(){
        "terminal_input" if call.args_json["release"]==true=>"已交还控制，程序继续运行".into(),
        "terminal_input"=>format!("{} 个动作已发送 · {}",v["actions_sent"].as_u64().unwrap_or(0),match v["observation_status"].as_str(){Some("condition_met")=>"观察条件已满足",Some("changed")=>"已观察到画面变化",Some("no_change")=>"尚未观察到变化",Some("ended")=>"程序已结束",Some("state_changed")=>"终端状态已变化，请重新观察",_=>"等待确认实际效果"}),
        "wait_terminal"=>match status{Status::TimedOut=>"到时未满足条件，已返回当前画面",Status::ConditionMet=>if call.args_json["stable_ms"].is_number()&&call.args_json["text"].is_null()&&call.args_json["screen_id"].is_null()&&call.args_json["fingerprint"].is_null(){"观察区域已稳定；不代表操作成功"}else{"指定观察条件已满足"},Status::Stopped=>"程序已结束，已返回最后画面",_=>"已返回观察结果"}.into(),
        _=>if v["screen"]["ended"]==true{"程序已结束，显示最后画面".into()}else if let(Some(c),Some(r))=(v["screen"]["columns"].as_u64(),v["screen"]["rows"].as_u64()){format!("{c} × {r} · 当前画面")}else{String::new()},
    };Some((status,detail))
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

#[cfg(test)]
mod terminal_status_tests{
    use super::*;
    #[test]
    fn old_and_new_terminal_results_distinguish_sending_from_matching(){
        let mut call=LoggedCall{id:"call".into(),name:"wait_terminal".into(),args_json:serde_json::json!({"session_id":"s"})};
        let mut result=LoggedResult{inline:r#"{"matched":false,"timed_out":true}"#.into(),is_error:false,cleared:false,output_id:None,attachments:vec![],subagent:None};
        assert_eq!(terminal_status(&call,Some(&result)).unwrap().0,Status::TimedOut);
        result.inline=r#"{"matched":true,"timed_out":false}"#.into();assert_eq!(terminal_status(&call,Some(&result)).unwrap().0,Status::ConditionMet);
        call.name="terminal_input".into();result.inline=r#"{"actions_sent":1,"observation_status":"no_change"}"#.into();let(s,d)=terminal_status(&call,Some(&result)).unwrap();assert_eq!(s,Status::Sent);assert!(d.contains("尚未观察"));
        result.is_error=true;result.inline=r#"{"code":"screen_changed","message":"未发送"}"#.into();assert_eq!(terminal_status(&call,Some(&result)).unwrap().0,Status::ScreenChanged);
    }
}
