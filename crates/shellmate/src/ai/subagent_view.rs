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
    pub tool_ids: super::tool_model::Identities,
    finishing_tool: Option<String>,
    pub questions: Vec<shellmate_ai::questions::Pending>,
    pub pending_inputs: Vec<shellmate_ai::interaction::Input>,
    pub prompt: String,
    pub cwd: Option<String>,
    pub active_tools: Vec<shellmate_ai::LoggedCall>,
    pub revision: u64,
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
    pub saving: bool,
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
            tool_ids: Default::default(),
            finishing_tool: None,
            questions: vec![],
            pending_inputs: vec![],
            prompt: request.prompt.clone(),
            cwd: None,
            active_tools: vec![],
            revision: 0,
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
            saving: false,
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
        self.revision = self.revision.wrapping_add(1);
        match event {
            SubagentEvent::Snapshot(entries) => {
                self.transcript = Some(entries.to_vec());
                self.transcript_loaded = true;
                self.read_steps(entries);
            }
            SubagentEvent::Updates(updates) => {
                let entries = self.transcript.get_or_insert_with(Vec::new);
                for update in updates {
                    match update {
                        shellmate_ai::LogUpdate::Append(e) => {
                            let mut entry = *e.clone();
                            if matches!(entry, Entry::Step(_)) {
                                let mut log =
                                    shellmate_ai::Conversation::from_entries(entries.clone());
                                log.append(entry);
                                entry = log.entries().last().unwrap().clone();
                            }
                            if let Entry::Step(step) = &**e {
                                let raw = self
                                    .finishing_tool
                                    .take()
                                    .unwrap_or_else(|| step.call.id.clone());
                                if let Entry::Step(saved) = &entry {
                                    self.tool_ids.finish(&raw, &saved.call.id);
                                }
                                self.active_tools.retain(|c| c.id != step.call.id);
                            }
                            entries.push(entry);
                        }
                        shellmate_ai::LogUpdate::ClearResults(indices) => {
                            for index in indices {
                                if let Some(Entry::Step(step)) = entries.get_mut(*index) {
                                    step.result.cleared = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            SubagentEvent::Agent(AgentEvent::ToolStarted(call)) => {
                self.tool_ids.start(&call.id);
                self.latest_activity = call.name.clone();
                self.active_tools.push(call.clone());
            }
            SubagentEvent::Agent(AgentEvent::ToolFinished(id)) => {
                self.finishing_tool = Some(id.clone());
                self.active_tools.retain(|c| c.id != *id);
            }
            SubagentEvent::Agent(AgentEvent::InputsAccepted(_)) => {
                self.active_tools.clear();
            }
            SubagentEvent::Started {
                model, reasoning, ..
            } => {
                self.model = model.into();
                self.reasoning = reasoning;
            }
            SubagentEvent::Agent(AgentEvent::StepProposed(call)) => {
                self.tool_ids.start(&call.id);
                self.active_tools.push(shellmate_ai::LoggedCall{id:call.id.clone(),name:"run_command".into(),args_json:serde_json::json!({"command":call.command,"purpose":call.purpose,"target":call.target.id(),"cwd":self.cwd})});
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
                self.finishing_tool = Some(call.id.clone());
                self.active_tools.retain(|c| c.id != call.id);
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
            SubagentEvent::Agent(AgentEvent::Reasoning {block,..}) => {self.latest_activity=if block.complete {block.kind.label().into()}else{"正在思考…".into()};}
            SubagentEvent::Agent(AgentEvent::Thinking) => self.latest_activity="正在思考…".into(),
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
            SubagentEvent::Saving(waiting) => {
                self.saving = waiting;
                self.latest_activity = if waiting {
                    "执行记录保存失败，等待重试"
                } else {
                    "执行记录已保存，继续处理"
                }
                .into();
            }
            SubagentEvent::Progress { steps, usage } => {
                self.steps = steps;
                self.usage = usage;
            }
            SubagentEvent::Finished(out) => {
                self.active_tools.clear();
                self.saving = false;
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
        // A saved in-flight snapshot has no worker after reopening history.
        // Normalize here so both the drawer and the task registry agree.
        let status = if display.status == SubagentStatus::Running {
            SubagentStatus::Stopped
        } else {
            display.status.clone()
        };
        Self {
            tool_ids: Default::default(),
            finishing_tool: None,
            questions: vec![],
            pending_inputs: vec![],
            prompt: String::new(),
            cwd: None,
            active_tools: vec![],
            revision: 0,
            id,
            call_id: String::new(),
            description: display.description.clone(),
            kind: display.kind,
            background,
            model: display.model.clone(),
            reasoning: display.reasoning,
            model_overridden: display.model_overridden,
            reasoning_overridden: display.reasoning_overridden,
            status: status.clone(),
            duration_ms: display.duration_ms,
            started: Instant::now()
                .checked_sub(Duration::from_millis(display.duration_ms))
                .unwrap_or_else(Instant::now),
            steps: display.steps,
            step_list: vec![],
            latest_activity: status_label(&status).into(),
            saving: false,
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
        self.cwd = super::subagents::saved_cwd(root, conversation, &self.id);
        self.questions = super::subagent_files::read_checkpoint(root, conversation, &self.id).map(|c|c.inbox.questions).unwrap_or_default();
        self.pending_inputs = super::subagent_files::read_checkpoint(root, conversation, &self.id)
            .map(|c| c.inbox.inputs.into_iter().filter(|i| !i.claimed).collect())
            .unwrap_or_default();
        if let Some(entries) = self
            .transcript_id
            .as_deref()
            .and_then(|id| super::subagent_files::read(root, conversation, id))
            .or_else(|| {
                super::subagent_files::read_checkpoint(root, conversation, &self.id)
                    .map(|c| c.entries)
            })
        {
            self.read_steps(&entries);
            if self.prompt.is_empty() {
                self.prompt = entries
                    .iter()
                    .find_map(|e| {
                        if let Entry::User { text, .. } = e {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
            }
            self.transcript = Some(entries);
        }
    }
    /// Continuing a reopened conversation registers historical tasks too. Their
    /// one-second snapshots must not discard drawer-owned lazy reads (including misses).
    pub fn refresh(&mut self, mut next: Self) {
        if self.history && next.history {
            if next.cwd.is_none() {
                next.cwd = self.cwd.clone();
            }
            if next.prompt.is_empty() {
                next.prompt = self.prompt.clone();
            }
            if self.report_loaded {
                next.report = std::mem::take(&mut self.report);
                next.report_loaded = true;
            }
            if self.transcript_loaded {
                next.pending_inputs = self.pending_inputs.clone();
                next.questions = self.questions.clone();
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
