//! Live and reopened subagent cards share this model; history files load only on demand.
use shellmate_ai::{
    AgentEvent, Entry, SubagentDisplay, SubagentRequest, SubagentStatus, Usage,
    subagent::SubagentEvent,
};
use std::time::{Duration, Instant};
#[derive(Clone, Debug)]
pub struct StepView {
    pub id: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_id: Option<String>,
    pub status: shellmate_ai::StepStatus,
    pub running: bool,
    pub output: String,
}
#[derive(Clone, Debug)]
pub struct View {
    pub id: String,
    pub call_id: String,
    pub description: String,
    pub kind: shellmate_ai::SubagentKind,
    pub background: bool,
    pub model: String,
    pub reasoning: shellmate_ai::ReasoningLevel,
    pub model_overridden: bool,
    pub reasoning_overridden: bool,
    pub status: SubagentStatus,
    pub started: Instant,
    pub duration_ms: u64,
    pub steps: usize,
    pub step_list: Vec<StepView>,
    pub latest_activity: String,
    pub usage: Usage,
    pub report: String,
    pub output_id: Option<String>,
    pub transcript_id: Option<String>,
    pub transcript: Option<Vec<Entry>>,
    pub history: bool,
    transcript_loaded: bool,
    report_loaded: bool,
}
impl View {
    pub fn new(id: String, request: &SubagentRequest, config: &shellmate_ai::Config) -> Self {
        Self {
            id,
            call_id: String::new(),
            description: request.description.clone(),
            kind: request.kind,
            background: request.background,
            model: config.model.clone(),
            reasoning: config.reasoning,
            model_overridden: request.model.is_some(),
            reasoning_overridden: request.reasoning.is_some(),
            status: SubagentStatus::Running,
            started: Instant::now(),
            duration_ms: 0,
            steps: 0,
            step_list: vec![],
            latest_activity: "正在启动".into(),
            usage: Usage::default(),
            report: String::new(),
            output_id: None,
            transcript_id: None,
            transcript: None,
            history: false,
            transcript_loaded: false,
            report_loaded: false,
        }
    }
    pub fn event(&mut self, event: SubagentEvent<'_>) {
        match event {
            SubagentEvent::Started {
                model, reasoning, ..
            } => {
                self.model = model.into();
                self.reasoning = reasoning;
            }
            SubagentEvent::Agent(AgentEvent::StepProposed(call)) => {
                self.latest_activity = call.command.clone();
                self.step_list.push(StepView {
                    id: call.id.clone(),
                    command: call.command.clone(),
                    exit_code: None,
                    duration_ms: 0,
                    output_id: None,
                    status: shellmate_ai::StepStatus::Background,
                    running: true,
                    output: String::new(),
                });
            }
            SubagentEvent::Agent(AgentEvent::StepFinished { call, outcome }) => {
                let display = shellmate_ai::StepDisplay::from_item(outcome.item(call)).unwrap();
                let output_id = if let shellmate_ai::ToolOutcome::Completed(out) = outcome {
                    out.output_id.clone()
                } else {
                    None
                };
                let step = StepView {
                    id: call.id.clone(),
                    command: display.command,
                    exit_code: display.exit_code,
                    duration_ms: display.duration_ms,
                    output_id,
                    status: display.status,
                    running: false,
                    output: display.output,
                };
                if let Some(old) = self.step_list.iter_mut().find(|s| s.id == call.id) {
                    *old = step;
                } else {
                    self.step_list.push(step);
                }
                self.steps += 1;
            }
            SubagentEvent::Agent(AgentEvent::TextDelta(text)) => {
                self.report.push_str(text);
                self.latest_activity = text.chars().take(160).collect();
            }
            SubagentEvent::Agent(AgentEvent::Compacting { .. }) => {
                self.latest_activity = "正在压缩上下文".into()
            }
            SubagentEvent::Agent(AgentEvent::Retrying { .. }) => {
                self.latest_activity = "正在重试".into()
            }
            SubagentEvent::Progress { steps, usage } => {
                self.steps = steps;
                self.usage = usage;
            }
            SubagentEvent::Finished(out) => {
                self.duration_ms = out.duration.as_millis().min(u64::MAX as u128) as u64;
                self.status = out.status.clone();
                self.steps = out.steps;
                self.usage = out.usage;
                self.report = out.report.clone();
                self.latest_activity = status_label(&self.status).into();
                self.read_steps(out.transcript.entries());
                self.transcript = Some(out.transcript.entries().to_vec());
                self.transcript_loaded = true;
            }
            _ => {}
        }
    }
    fn read_steps(&mut self, entries: &[Entry]) {
        self.step_list = entries
            .iter()
            .filter_map(|e| {
                if let Entry::Step(s) = e {
                    s.display.as_ref().map(|d| StepView {
                        id: s.call.id.clone(),
                        command: d.command.clone(),
                        exit_code: d.exit_code,
                        duration_ms: d.duration_ms,
                        output_id: s.result.output_id.clone(),
                        status: d.status.clone(),
                        running: false,
                        output: d.output.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
    }
    pub fn display(&self) -> SubagentDisplay {
        SubagentDisplay {
            description: self.description.clone(),
            kind: self.kind,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_overridden: self.model_overridden,
            reasoning_overridden: self.reasoning_overridden,
            status: self.status.clone(),
            steps: self.steps,
            duration_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            usage: self.usage,
            output_id: self.output_id.clone(),
            transcript_id: self.transcript_id.clone(),
        }
    }
    pub fn reopened(
        id: String,
        display: &SubagentDisplay,
        background: bool,
        report: String,
    ) -> Self {
        Self {
            id,
            call_id: String::new(),
            description: display.description.clone(),
            kind: display.kind,
            background,
            model: display.model.clone(),
            reasoning: display.reasoning,
            model_overridden: display.model_overridden,
            reasoning_overridden: display.reasoning_overridden,
            status: display.status.clone(),
            duration_ms: display.duration_ms,
            started: Instant::now()
                .checked_sub(Duration::from_millis(display.duration_ms))
                .unwrap_or_else(Instant::now),
            steps: display.steps,
            step_list: vec![],
            latest_activity: status_label(&display.status).into(),
            usage: display.usage,
            report,
            output_id: display.output_id.clone(),
            transcript_id: display.transcript_id.clone(),
            transcript: None,
            history: true,
            transcript_loaded: false,
            report_loaded: false,
        }
    }
    /// Load a historical full report once, keeping the inline fallback if its spool expired.
    pub fn load_report(&mut self, root: &std::path::Path, conversation: &str) {
        if !self.history || self.report_loaded {
            return;
        }
        self.report_loaded = true;
        if let Some(text) = self
            .output_id
            .as_deref()
            .and_then(|id| saved_output(root, conversation, id))
        {
            self.report = text;
        }
    }
    pub fn load_transcript(&mut self, root: &std::path::Path, conversation: &str) {
        if self.transcript_loaded || self.transcript.is_some() {
            return;
        }
        self.transcript_loaded = true;
        if let Some(entries) = self
            .transcript_id
            .as_deref()
            .and_then(|id| super::subagent_files::read(root, conversation, id))
        {
            self.read_steps(&entries);
            self.transcript = Some(entries);
        }
    }
    /// Continuing a reopened conversation registers historical tasks too. Their
    /// one-second snapshots must not discard drawer-owned lazy reads (including misses).
    pub fn refresh(&mut self, mut next: Self) {
        if self.history && next.history {
            if self.report_loaded {
                next.report = std::mem::take(&mut self.report);
                next.report_loaded = true;
            }
            if self.transcript_loaded {
                next.transcript = self.transcript.take();
                next.step_list = std::mem::take(&mut self.step_list);
                next.transcript_loaded = true;
            }
        }
        *self = next;
    }
    pub fn progress(&self) -> String {
        let detail = self
            .step_list
            .last()
            .map(|s| {
                format!(
                    "{} · {:?} · 退出码 {} · {} ms{}",
                    s.command,
                    s.status,
                    s.exit_code.map_or_else(|| "未知".into(), |c| c.to_string()),
                    s.duration_ms,
                    s.output_id
                        .as_ref()
                        .map(|id| format!(" · 输出 id {id}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| self.latest_activity.clone());
        format!(
            "{} · {} 步 · {detail}",
            status_label(&self.status),
            self.steps
        )
    }
}
pub fn status_label(status: &SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Running => "运行中",
        SubagentStatus::Completed => "已完成",
        SubagentStatus::Failed(_) => "失败",
        SubagentStatus::Stopped => "已停止",
        SubagentStatus::StepLimit => "达到步数上限",
    }
}

/// Output ids are validated by the shared spool API; never interpret arbitrary paths.
pub fn saved_output(root: &std::path::Path, conversation: &str, id: &str) -> Option<String> {
    use std::io::Read;
    let path = shellmate_core::ai_output::path(root, conversation, id).ok()?;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(shellmate_core::ai_output::KEEP_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= shellmate_core::ai_output::KEEP_BYTES).then_some(text)
}
