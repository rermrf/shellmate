//! Lazy agent loop. The host alone approves and executes; dropping the stream drops
//! the in-flight model/host future. A host must cancel its operation on future drop.
use crate::{
    AiError, AnswerItem, CommandCard, CompactTrigger, Compaction, ContextSnapshot, Conversation,
    Entry, LogUpdate, PermissionMode, RetainedState, RigClient, StepStatus, ToolOutput, Usage,
    UserTurn,
    client::option_refusal,
    context::{NativeMessages, ThrashGuard, request_tokens},
    policy::{self, Class},
};
use futures::{Stream, StreamExt, future::BoxFuture};
use rig_core::{
    completion::{CompletionRequest, FinishReason},
    message::{AssistantContent, Message, ToolCall as RigCall},
    serde::{Deserialize, Serialize},
    streaming::{CompletionStream, Item, PartKind, StreamEvent},
};
use std::{
    collections::{HashSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, SystemTime},
};
#[path = "agent_interaction.rs"]
mod interaction;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "lowercase")]
pub enum Target {
    Terminal,
    Background,
}
impl Target {
    pub fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Background => "background",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "终端内",
            Self::Background => "独立通道",
        }
    }
    pub fn timeout(self, requested: Option<u64>) -> Duration {
        ExecLimits::default().timeout(requested)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub sessions: bool,
    pub web: bool,
    pub terminal: bool,
    pub background: bool,
    pub terminal_busy: bool,
    pub cwd: Option<String>,
}
impl Capabilities {
    pub fn available(&self) -> bool {
        self.terminal || self.background || self.sessions
    }
    pub fn targets(&self) -> Vec<Target> {
        [Target::Terminal, Target::Background]
            .into_iter()
            .filter(|t| self.offers(*t))
            .collect()
    }
    pub fn offers(&self, target: Target) -> bool {
        match target {
            Target::Terminal => self.terminal,
            Target::Background => self.background,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub service: Option<ServiceOptions>,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub destination: Option<crate::session_tools::Destination>,
    pub id: String,
    pub command: String,
    pub purpose: String,
    pub target: Target,
    pub timeout: Duration,
    pub run_in_background: bool,
    pub exec_limits: ExecLimits,
    pub class: Class,
}

/// A process intended to outlive the agent turn. Readiness is evidence from output,
/// not a claim that an HTTP endpoint has been verified.
#[derive(Clone, Debug, PartialEq, Eq, rig_core::serde::Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct ServiceOptions {
    pub ready_pattern: Option<String>,
    #[serde(default = "service_startup_timeout")]
    pub startup_timeout_secs: u64,
}
fn service_startup_timeout() -> u64 { 10 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed(ToolOutput),
    Denied { reason: Option<String> },
    Busy,
    Disconnected,
    Failed(String),
}
impl ToolOutcome {
    fn cap_stored(self) -> Self {
        match self {
            Self::Denied { reason } => Self::Denied {
                reason: reason.map(|s| crate::output::stored_text(&s)),
            },
            Self::Failed(s) => Self::Failed(crate::output::stored_text(&s)),
            other => other,
        }
    }
    pub(crate) fn wire_text(&self) -> String {
        match self {
            Self::Completed(out) => out.wire_text(),
            Self::Denied { reason } => format!(
                "用户拒绝执行{}。不得换一种形式重试该命令。",
                reason
                    .as_ref()
                    .map(|r| format!("（{r}）"))
                    .unwrap_or_default()
            ),
            Self::Busy => "终端正忙，未执行命令".into(),
            Self::Disconnected => "会话已断开，未执行命令".into(),
            Self::Failed(reason) => format!("执行失败：{reason}"),
        }
    }
    pub fn item(&self, call: &ToolCall) -> AnswerItem {
        let (status, exit_code, duration_ms, output) = match self {
            Self::Completed(out) => (
                if out.disconnected {
                    StepStatus::Disconnected
                } else if out.task_id.is_some() {
                    StepStatus::Background
                } else if out.timed_out {
                    StepStatus::TimedOut
                } else {
                    StepStatus::Completed
                },
                out.exit_code,
                out.duration_ms,
                out.stored(),
            ),
            Self::Denied { reason } => (
                StepStatus::Denied {
                    reason: reason.clone(),
                },
                None,
                0,
                String::new(),
            ),
            Self::Busy => (StepStatus::Busy, None, 0, String::new()),
            Self::Disconnected => (StepStatus::Disconnected, None, 0, String::new()),
            Self::Failed(s) => (StepStatus::Failed(s.clone()), None, 0, String::new()),
        };
        let mut item = AnswerItem::Step {
            command: call.command.clone(),
            target: call.target,
            purpose: call.purpose.clone(),
            class: call.class,
            status,
            exit_code,
            duration_ms,
            automatic: false,
            output,
        };
        item.cap_stored();
        item
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct ReadOutput {
    pub id: String,
    #[serde(default)]
    pub offset_line: usize,
    pub limit_lines: Option<usize>,
    pub grep: Option<String>,
}
#[derive(Clone, Debug)]
pub enum LocalTool {
    ViewImage { id: String },
    Sessions(crate::session_tools::Request),
    ScopedMemory {
        session_id: String,
        command: crate::memory::Command,
    },
    Web(crate::web::Request),
    Memory(crate::memory::Command),
    ReadOutput(ReadOutput),
    StopTask {
        id: String,
    },
    WaitTasks {
        ids: Vec<String>,
        all: bool,
    },
}
#[derive(Clone, Debug)]
pub struct LocalResult {
    pub attachments: Vec<crate::Attachment>,
    pub subagent: Option<Box<crate::SubagentDisplay>>,
    pub inline: String,
    pub output_id: Option<String>,
    pub is_error: bool,
}
impl LocalResult {
    pub fn error(inline: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(inline)
        }
    }

    pub fn text(inline: impl Into<String>) -> Self {
        Self {
            attachments: Vec::new(),
            subagent: None,
            inline: inline.into(),
            output_id: None,
            is_error: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecLimits {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub tool_output_chars: usize,
}
impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 600,
            tool_output_chars: 30_000,
        }
    }
}
impl ExecLimits {
    pub fn timeout(self, requested: Option<u64>) -> Duration {
        let max = if self.max_timeout_secs == 0 {
            600
        } else {
            self.max_timeout_secs.min(600)
        };
        let default = if self.default_timeout_secs == 0 {
            120
        } else {
            self.default_timeout_secs
        };
        Duration::from_secs(requested.unwrap_or(default).clamp(1, max))
    }
    pub fn output_chars(self) -> usize {
        if (1_000..=150_000).contains(&self.tool_output_chars) {
            self.tool_output_chars
        } else {
            30_000
        }
    }
}
pub trait ToolHost: Send + Sync {
    fn model_slots(&self) -> Option<Arc<tokio::sync::Semaphore>> { None }
    fn sessions_enabled(&self) -> bool {
        false
    }
    fn web_enabled(&self) -> bool {
        false
    }
    fn take_external_steps(&self) -> Vec<crate::Step> {
        vec![]
    }
    fn control(&self) -> Option<crate::interaction::Control> {
        None
    }
    /// Registry updates applied before the next request; never adds child usage to this context.
    fn subagent_results(&self) -> Vec<(String, crate::LoggedResult)> {
        Vec::new()
    }
    fn cancelled(&self) -> bool {
        false
    }

    /// M1 is opt-in; the live app stays false until M2 provides a scoped registry.
    fn subagents(&self) -> bool {
        false
    }
    fn subagent(
        &self,
        _request: crate::SubagentRequest,
        _call_id: &str,
    ) -> BoxFuture<'_, LocalResult> {
        Box::pin(async { LocalResult::error("当前会话不支持子任务") })
    }
    fn send_agent_message(
        &self,
        _id: &str,
        _text: &str,
        _immediate: bool,
        _resume: bool,
        _image_ids: Vec<String>,
        _call_id: &str,
    ) -> BoxFuture<'_, LocalResult> {
        Box::pin(async { LocalResult::error("当前主机不支持追加子任务消息") })
    }
    /// Source labels approvals; child commands follow the same permission policy.
    fn run_from(
        &self,
        call: ToolCall,
        _source: Option<crate::SubagentRef>,
    ) -> BoxFuture<'_, ToolOutcome> {
        self.run(call)
    }
    /// Called once when a child finishes or its runner is dropped; M2 cancels its own tasks/approvals here.
    fn finish_subagent(&self, _source: &crate::SubagentRef) {}

    fn memory_snapshot(&self, auto_memory: bool) -> crate::memory::Snapshot {
        crate::memory::Snapshot {
            auto_memory,
            ..Default::default()
        }
    }
    /// Consume a manual request only at a boundary with no tool/model future in flight.
    fn take_compaction(&self) -> Option<Option<String>> {
        None
    }

    fn local_tool_from(&self,tool:LocalTool,limits:ExecLimits,_source:Option<crate::SubagentRef>,_call_id:&str)->BoxFuture<'_,LocalResult>{self.local_tool(tool,limits)}
    fn local_tool(&self, _tool: LocalTool, _limits: ExecLimits) -> BoxFuture<'_, LocalResult> {
        Box::pin(async {
            LocalResult {
                attachments: Vec::new(), subagent: None,
                inline: "此主机不支持输出工具".into(),
                output_id: None,
                is_error: true,
            }
        })
    }
    /// Atomically drain completion notices at each request boundary (also used by idle UI).
    fn take_notices(&self) -> Vec<String> {
        Vec::new()
    }
    fn task_state(&self) -> (Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// Approval and execution belong to the host. Dropping a foreground future
    /// cancels it; explicitly returned background tasks belong to the host registry.
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome>;
    /// The approved command may have been edited or retargeted by the user.
    /// Fresh host snapshot at a compaction boundary. None uses the last sent snapshot.
    fn context_snapshot(&self) -> BoxFuture<'_, Option<ContextSnapshot>> {
        Box::pin(async { None })
    }
    fn permission_mode(&self) -> PermissionMode {
        PermissionMode::Ask
    }
    fn output_id(&self, _call: &ToolCall) -> Option<String> {
        None
    }
    fn automatic(&self, _call: &ToolCall) -> bool {
        false
    }
    fn resolved_call(&self, call: &ToolCall) -> ToolCall {
        call.clone()
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub auto_memory: bool,
    /// Tool steps per run/resume; None means unlimited.
    pub max_steps: Option<usize>,
    pub exec: ExecLimits,
    /// Delay after the nth failed attempt (1-based); injectable for offline tests.
    pub backoff: fn(u32) -> Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            auto_memory: true,
            max_steps: None,
            exec: ExecLimits::default(),
            backoff: retry_backoff,
        }
    }
}
pub fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.saturating_sub(1).min(6)).min(60))
}
const MAX_ATTEMPTS: u32 = 10;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    StepLimit,
    Error(AiError),
    InvalidCalls,
}

pub enum AgentEvent {
    ToolStarted(crate::LoggedCall),
    ToolFinished(String),
    InputsAccepted(Vec<String>),
    CompletionsAccepted(Vec<String>),
    Context {
        used: usize,
        window: u32,
        trigger: u32,
    },
    Compacting {
        tier: u8,
    },
    Compacted {
        before: usize,
        after: usize,
        summary: Option<String>,
    },
    Thinking,
    Reasoning {message_id: usize, block: crate::ThinkingBlock},
    TextDelta(String),
    Command(CommandCard),
    StepProposed(ToolCall),
    StepFinished {
        call: ToolCall,
        outcome: ToolOutcome,
    },
    Paused {
        reason: PauseReason,
        steps: usize,
        resume: Box<Resume>,
    },
    Retrying {
        attempt: u32,
        max: u32,
        delay: Duration,
        error: AiError,
    },
    Done(Option<Usage>),
    Released(ReleaseReason),
    Error(AiError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    Detached,
    Stopped,
}
#[derive(Clone)]
enum Pending {
    Question(Box<RigCall>, crate::questions::Request),
    AgentMessage(Box<RigCall>, String, String, bool, bool),
    Subagent(Box<RigCall>, crate::SubagentRequest),
    Local(Box<RigCall>, LocalTool),
    Call(Box<RigCall>, ToolCall),
    Invalid(Box<RigCall>, String),
    Text(String),
    Reasoning(crate::ThinkingBlock),
    Truncated(u32),
}
/// Opaque, memory-only continuation. Deliberately neither Debug nor serializable:
/// it owns signed reasoning and real tool messages for this turn only.
pub struct Resume {
    native: NativeMessages,
    model_steps: usize,
    guard: ThrashGuard,
    pending: VecDeque<Pending>,
    capabilities: Capabilities,
    base_capabilities: Capabilities,
    steps: usize,
    invalid_calls: usize,
    turn: UserTurn,
    turns: usize,
    parallel_disabled: bool,
    message_id: usize,
}
pub struct AgentStream<'a> {
    inner: Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>,
    updates: Arc<Mutex<Vec<LogUpdate>>>,
}
impl Stream for AgentStream<'_> {
    type Item = AgentEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
impl AgentStream<'_> {
    pub(crate) fn updates_handle(&self) -> Arc<Mutex<Vec<LogUpdate>>> {
        self.updates.clone()
    }
    /// Drain durable mutations after each event; the app applies these before saving.
    pub fn take_updates(&mut self) -> Vec<LogUpdate> {
        std::mem::take(&mut *self.updates.lock().unwrap())
    }
}
pub fn run<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    mut capabilities: Capabilities,
    host: &'a dyn ToolHost,
    limits: Limits,
) -> Result<AgentStream<'a>, AiError> {
    capabilities.web = host.web_enabled();
    capabilities.sessions = host.sessions_enabled();
    if capabilities.sessions {
        capabilities.background=true;
        capabilities.terminal=true;
    }
    let user = conversation
        .latest_user()
        .ok_or(AiError::InvalidConfig)?
        .clone();
    let base_capabilities = capabilities.clone();
    if let Some(control)=host.control(){control.restore_questions(conversation.entries());}
    conversation.prepare(&capabilities)?;
    let state = Resume {
        native: NativeMessages::new(),
        model_steps: 0,
        guard: ThrashGuard::default(),
        pending: VecDeque::new(),
        capabilities,
        base_capabilities,
        steps: 0,
        invalid_calls: 0,
        turn: user,
        turns: conversation.turns().len(),
        parallel_disabled: false,
        message_id: 0,
    };
    Ok(stream(client, conversation, host, limits, state, None))
}
pub fn resume<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    continuation: Resume,
    limits: Limits,
) -> Result<AgentStream<'a>, AiError> {
    if conversation.turns().len() != continuation.turns
        || conversation.latest_user() != Some(&continuation.turn)
    {
        return Err(AiError::InvalidConfig);
    }
    Ok(stream(
        client,
        conversation,
        host,
        limits,
        continuation,
        None,
    ))
}
/// Manual compaction also works on an idle/paused log. The caller retains Resume;
/// resuming rebuilds from the new log view and then processes any pending calls.
pub fn compact<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    capabilities: Capabilities,
    host: &'a dyn ToolHost,
    focus: Option<String>,
) -> Result<AgentStream<'a>, AiError> {
    let limits = Limits {
        auto_memory: conversation.memory.as_ref().is_none_or(|m| m.auto_memory),
        ..Default::default()
    };
    compact_with_limits(client, conversation, capabilities, host, focus, limits)
}
/// Manual compaction of a restored conversation uses the drawer's current settings too.
pub fn compact_with_limits<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    capabilities: Capabilities,
    host: &'a dyn ToolHost,
    focus: Option<String>,
    limits: Limits,
) -> Result<AgentStream<'a>, AiError> {
    let user = conversation
        .latest_user()
        .ok_or(AiError::InvalidConfig)?
        .clone();
    let state = Resume {
        native: NativeMessages::new(),
        model_steps: 0,
        guard: ThrashGuard::default(),
        pending: VecDeque::new(),
        base_capabilities: capabilities.clone(),
        capabilities,
        steps: 0,
        invalid_calls: 0,
        turn: user,
        turns: conversation.turns().len(),
        parallel_disabled: false,
        message_id: 0,
    };
    Ok(stream(
        client,
        conversation,
        host,
        limits,
        state,
        Some(focus),
    ))
}
struct State<'a> {
    started_tool: Option<String>,
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    limits: Limits,
    turn: Option<Resume>,
    model: Option<CompletionStream>,
    model_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    active: Option<(RigCall, ToolCall)>,
    first: bool,
    retry_wait: Option<Duration>,
    failures: u32,
    received: bool,
    defer_text: bool,
    thinking: bool,
    reasoning: crate::thinking::Tracker,
    retry_notice: bool,
    steps: usize,
    done: bool,
    error: Option<AiError>,
    updates: Arc<Mutex<Vec<LogUpdate>>>,
    queue: VecDeque<AgentEvent>,
    message_id: usize,
    reactive_retry: bool,
    compacting: Option<CompactWork>,
    manual: bool,
    skip_precheck: bool,
}
impl State<'_> {
    fn record(&mut self, update: LogUpdate) {
        self.conversation.apply(update.clone());
        self.updates.lock().unwrap().push(update);
    }
    fn entry(&mut self, entry: Entry) {
        if matches!(entry, Entry::Step(_))
            && let Some(control) = self.host.control()
        {
            control.tool_finished();
        }
        self.record(LogUpdate::Append(Box::new(entry)));
    }
    fn reasoning(&mut self, block: crate::ThinkingBlock) -> AgentEvent {
        self.entry(Entry::Thinking {message_id:self.message_id,block:block.clone()});
        AgentEvent::Reasoning {message_id:self.message_id,block}
    }
    fn text(&mut self, text: &str) {
        if !text.is_empty() {
            self.entry(Entry::Assistant {
                replay: None,
                text: text.into(),
                message_id: self.message_id,
            });
        }
    }
    fn request(&self) -> Result<CompletionRequest, AiError> {
        let turn = self.turn.as_ref().unwrap();
        let mut capabilities = turn.capabilities.clone();
        capabilities.web = self.host.web_enabled();
        capabilities.sessions = self.host.sessions_enabled();
        let responses = matches!(self.client.config.provider, crate::Provider::OpenAiResponses { .. });
        let empty = NativeMessages::new();
        let mut request = self.conversation.request(
            &capabilities,
            if responses { &empty } else { &turn.native },
            false,
            self.conversation.view_start(),
        )?;
        if let Some(tool) = request
            .tools
            .iter_mut()
            .find(|tool| tool.name.as_str() == "run_command")
        {
            *tool = Arc::new(crate::conversation::run_command_tool_with_limits(
                &turn.capabilities,
                self.limits.exec,
            ));
        }
        if responses {
            let replay = self.conversation.responses_native(
                &self.client.config, &self.conversation.system(&capabilities)?,
                rig_core::message::Fingerprint::of(&request.tools),
            );
            if !replay.is_empty() {
                request.chat_history = self.conversation.request(
                    &capabilities, &replay, false, self.conversation.view_start(),
                )?.chat_history;
            }
        }
        if request
            .chat_history
            .last()
            .is_some_and(|m| matches!(&**m, Message::Assistant(_)))
        {
            crate::context::add_prompt(&mut request, "请继续。".into());
        }
        if turn.capabilities.available() && !turn.parallel_disabled {
            request = request.parallel_tool_calls(false);
        }
        Ok(request)
    }
    fn fail(&mut self, error: AiError) -> AgentEvent {
        self.done = true;
        self.model = None;
        self.model_permit = None;
        self.turn = None;
        AgentEvent::Error(error)
    }
    fn pause(&mut self, reason: PauseReason) -> AgentEvent {
        self.done = true;
        self.model = None;
        self.model_permit = None;
        let mut resume = self.turn.take().unwrap();
        resume.message_id = self.message_id;
        if reason == PauseReason::InvalidCalls {
            resume.invalid_calls = 0;
        }
        AgentEvent::Paused {
            reason,
            steps: resume.steps,
            resume: Box::new(resume),
        }
    }
    fn failure(&mut self, error: AiError, retry_after: Option<Duration>) -> AgentEvent {
        self.model = None;
        self.model_permit = None;
        let transient = matches!(
            error,
            AiError::RateLimited | AiError::Unavailable | AiError::Network | AiError::Timeout
        );
        if error == AiError::ContextTooLong && !self.received && !self.reactive_retry {
            self.reactive_retry = true;
            match self.begin_compaction(CompactTrigger::Reactive, None) {
                Ok(()) => return AgentEvent::Compacting { tier: 1 },
                Err(e) => return self.fail(e),
            }
        }
        self.failures += 1;
        if transient
            && self.turn.as_ref().unwrap().capabilities.available()
            && !self.received
            && (error != AiError::Timeout || self.first)
            && self.failures < MAX_ATTEMPTS
        {
            let delay = retry_after
                .unwrap_or_else(|| (self.limits.backoff)(self.failures))
                .min(Duration::from_secs(60));
            self.retry_wait = Some(delay);
            self.retry_notice = true;
            return AgentEvent::Retrying {
                attempt: self.failures,
                max: MAX_ATTEMPTS,
                delay,
                error,
            };
        }
        if transient {
            if self.received {
                self.turn
                    .as_mut()
                    .unwrap()
                    .native
                    .insert(self.message_id, Default::default());
            }
            self.pause(PauseReason::Error(error))
        } else {
            self.fail(error)
        }
    }
    fn provider_failure(&mut self, error: rig_core::ProviderError) -> AgentEvent {
        let retry_after = error.report().retry_after(SystemTime::now());
        self.failure(self.client.map_error(error), retry_after)
    }
    fn retry_option(&mut self, error: &rig_core::ProviderError) -> bool {
        if self.received {
            return false;
        }
        let turn = self.turn.as_mut().unwrap();
        if !turn.parallel_disabled && parallel_refusal(error) {
            turn.parallel_disabled = true;
        } else if !self.client.option_fallback(error) {
            return false;
        }
        self.model = None;
        self.model_permit = None;
        true
    }
}
struct CompactWork {
    before: usize,
    trigger: CompactTrigger,
    focus: Option<String>,
    tier: u8,
}
impl State<'_> {
    fn begin_compaction(
        &mut self,
        trigger: CompactTrigger,
        focus: Option<String>,
    ) -> Result<(), AiError> {
        let request = self.request()?;
        let turn = self.turn.as_mut().unwrap();
        turn.guard.record(turn.model_steps, trigger)?;
        let before = self.conversation.used(&request, &turn.capabilities);
        self.compacting = Some(CompactWork {
            before,
            trigger,
            focus,
            tier: 1,
        });
        Ok(())
    }
    async fn compact_work(&mut self, mut work: CompactWork) -> Result<(), AiError> {
        if work.tier == 1 {
            let positions = self.conversation.clear_candidates();
            let changed = !positions.is_empty();
            if changed {
                self.record(LogUpdate::ClearResults(positions));
            }
            let after = request_tokens(&self.request()?);
            if work.trigger != CompactTrigger::Manual
                && changed
                && after <= self.client.context_budget().auto as usize / 2
            {
                self.record(LogUpdate::Memory(
                    self.host.memory_snapshot(self.limits.auto_memory),
                ));
                let after = request_tokens(&self.request()?);
                let c = Compaction {
                    summary: None,
                    retained_user: vec![],
                    state: RetainedState::default(),
                    context: ContextSnapshot::default(),
                    before_tokens: work.before,
                    after_tokens: after,
                    trigger: work.trigger,
                };
                self.entry(Entry::Compaction(c));
                self.queue.push_back(AgentEvent::Compacted {
                    before: work.before,
                    after,
                    summary: None,
                });
                let budget = self.client.context_budget();
                self.queue.push_back(AgentEvent::Context {
                    used: after,
                    window: budget.window,
                    trigger: budget.trigger,
                });
                self.skip_precheck = true;
            } else {
                work.tier = 2;
                self.compacting = Some(work);
                self.queue.push_back(AgentEvent::Compacting { tier: 2 });
            }
            return Ok(());
        }
        let caps = self.turn.as_ref().unwrap().capabilities.clone();
        let mut start = self.conversation.view_start();
        let summary = loop {
            let mut request =
                self.conversation
                    .request(&caps, &NativeMessages::new(), true, start)?;
            let mut prompt = crate::context::SUMMARY_PROMPT.to_owned();
            if let Some(focus) = &work.focus {
                prompt.push_str(&format!("\n额外要求：{focus}"));
            }
            crate::context::add_prompt(&mut request, prompt);
            let mut usage = Usage::default();
            let _permit=if let Some(slots)=self.host.model_slots(){slots.acquire_owned().await.ok()}else{None};
            let summary = self.client.summarize(request, |u| usage.add(u)).await;
            if self.conversation.child.is_some() {
                self.record(LogUpdate::RequestUsage(usage));
            }
            match summary {
                Ok(text) => break crate::context::extract_summary(&text),
                Err(AiError::ContextTooLong)
                    if self.conversation.entries().len().saturating_sub(start) > 1 =>
                {
                    start += 1
                }
                Err(e) => return Err(e),
            }
        };
        self.record(LogUpdate::Memory(
            self.host.memory_snapshot(self.limits.auto_memory),
        ));
        let mut snapshot = self
            .host
            .context_snapshot()
            .await
            .unwrap_or_else(|| self.conversation.latest_user().unwrap().context.clone());
        snapshot.text = crate::recent_context(&snapshot.text, 40);
        let mut c = Compaction {
            summary: Some(summary.clone()),
            retained_user: self.conversation.retained_users(),
            state: self
                .conversation
                .retained_state(self.host.permission_mode()),
            context: snapshot,
            before_tokens: work.before,
            after_tokens: 0,
            trigger: work.trigger,
        };
        (c.state.running_tasks, c.state.readable_output_ids) = self.host.task_state();
        let mut preview = self.conversation.clone();
        preview.append(Entry::Compaction(c.clone()));
        c.after_tokens = request_tokens(&preview.prepare(&caps)?);
        let after = c.after_tokens;
        self.entry(Entry::Compaction(c));
        self.queue.push_back(AgentEvent::Compacted {
            before: work.before,
            after,
            summary: Some(summary),
        });
        let budget = self.client.context_budget();
        self.queue.push_back(AgentEvent::Context {
            used: after,
            window: budget.window,
            trigger: budget.trigger,
        });
        self.skip_precheck = true;
        Ok(())
    }
}

fn parallel_refusal(e: &rig_core::ProviderError) -> bool {
    option_refusal(e, &["parallel_tool_calls", "disable_parallel_tool_use"])
}

fn stream<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    limits: Limits,
    turn: Resume,
    manual_focus: Option<Option<String>>,
) -> AgentStream<'a> {
    let mut initial_updates = Vec::new();
    let offered =
        (conversation.child.is_none() && turn.capabilities.background && host.subagents())
            .then(|| crate::subagent::agent_tool(&client.config));
    if conversation.agent_tool != offered {
        conversation.agent_tool = offered.clone();
        initial_updates.push(LogUpdate::AgentTool(offered));
    }
    if conversation
        .memory
        .as_ref()
        .is_none_or(|m| m.auto_memory != limits.auto_memory)
    {
        let snapshot = host.memory_snapshot(limits.auto_memory);
        conversation.memory = Some(snapshot.clone());
        initial_updates.push(LogUpdate::Memory(snapshot));
    }
    let updates = Arc::new(Mutex::new(initial_updates));
    let message_id = turn.message_id;
    let mut state = State {
        started_tool: None,
        client,
        conversation,
        host,
        limits,
        turn: Some(turn),
        model: None,
        model_permit: None,
        active: None,
        first: true,
        retry_wait: None,
        failures: 0,
        received: false,
        defer_text: false,
        thinking: false,
        reasoning: Default::default(),
        retry_notice: false,
        steps: 0,
        done: false,
        error: None,
        updates: updates.clone(),
        queue: VecDeque::new(),
        message_id,
        reactive_retry: false,
        compacting: None,
        manual: manual_focus.is_some(),
        skip_precheck: false,
    };
    if let Some(focus) = manual_focus {
        if let Err(e) = state.begin_compaction(CompactTrigger::Manual, focus) {
            state.error = Some(e);
        } else {
            state.queue.push_back(AgentEvent::Compacting { tier: 1 });
        }
    }
    let inner = futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(event) = s.queue.pop_front() {
                if matches!(event,AgentEvent::Done(_)) && let Some(c)=s.host.control() && c.has_questions(true) && c.intent()==crate::interaction::Intent::None && !c.pending() {
                    s.phase(crate::interaction::Phase::UserInput);
                    c.question_activity(true).await;s.queue.push_front(event);continue;
                }
                if matches!(event, AgentEvent::Done(_)) {
                    if let Some(input) = s.receive_input() {
                        return Some((input, s));
                    }
                    s.phase(crate::interaction::Phase::Idle);
                }
                if matches!(event, AgentEvent::Done(_))
                    && let Some(focus) = s.host.take_compaction()
                {
                    s.done = false;
                    s.manual = true;
                    match s.begin_compaction(CompactTrigger::Manual, focus) {
                        Ok(()) => return Some((AgentEvent::Compacting { tier: 1 }, s)),
                        Err(e) => return Some((s.fail(e), s)),
                    }
                }
                return Some((event, s));
            }
            if s.done {
                return None;
            }
            if let Some(event) = s.receive_input() {
                return Some((event, s));
            }
            if s.host.cancelled() {
                return Some((s.fail(AiError::Cancelled), s));
            }
            if s.model.is_none() && let Some(c)=s.host.control() && c.has_questions(false) && c.intent()==crate::interaction::Intent::None && !c.pending() {
                s.phase(crate::interaction::Phase::UserInput);c.question_activity(false).await;continue;
            }
            if let Some(delay) = s.retry_wait.take() {
                // Owned by this stream: dropping it cancels the backoff future.
                s.phase(crate::interaction::Phase::Retrying);
                tokio::select! {biased; ()=interaction::urgent(s.host.control())=>{continue;}, ()=tokio::time::sleep(delay)=>{}}
            }
            if let Some(e) = s.error.take() {
                return Some((s.fail(e), s));
            }
            if let Some(work) = s.compacting.take() {
                s.phase(crate::interaction::Phase::Compacting);
                if let Err(e) = s.compact_work(work).await {
                    return Some((s.fail(e), s));
                }
                continue;
            }
            if s.manual {
                s.done = true;
                return Some((AgentEvent::Done(None), s));
            }
            if let Some((raw, call)) = s.active.take() {
                s.phase(crate::interaction::Phase::Executing);
                let outcome = s.host.run(call.clone()).await.cap_stored();
                let resolved = s.host.resolved_call(&call);
                let output_id = match &outcome {
                    ToolOutcome::Completed(out) => out.output_id.clone(),
                    _ => None,
                }
                .or_else(|| s.host.output_id(&resolved));
                let mut wire = if resolved != call {
                    format!(
                        "用户批准的实际命令：{}\n执行目标：{}\n{}",
                        crate::output::stored_text(&resolved.command),
                        resolved.target.label(),
                        outcome.wire_text()
                    )
                } else {
                    outcome.wire_text()
                };
                if !matches!(&outcome, ToolOutcome::Completed(_))
                    && let Some(id) = &output_id
                {
                    wire.push_str(&format!("\n输出 id {id} · 用 read_output 查看输出"));
                }
                let call = resolved;
                let turn = s.turn.as_mut().unwrap();
                turn.steps += 1;
                s.steps += 1;
                let mut display = crate::StepDisplay::from_item(outcome.item(&call)).unwrap();
                display.automatic = s.host.automatic(&call);
                s.entry(Entry::Step(crate::Step {
                    message_id: s.message_id,
                    call: crate::LoggedCall {
                        id: raw.id.to_string(),
                        name: raw.function.name.to_string(),
                        args_json: {
                            let mut args = serde_json::Value::Object(raw.function.arguments);
                            if let Some(d) = &call.destination {
                                args["resolved_target"] = serde_json::to_value(d).unwrap();
                            }
                            args
                        },
                    },
                    result: crate::LoggedResult {
                        attachments: Vec::new(), subagent: None,
                        inline: wire,
                        output_id,
                        cleared: false,
                        is_error: match &outcome {
                            ToolOutcome::Completed(out) => !out.successful,
                            _ => true,
                        },
                    },
                    display: Some(display),
                }));
                if !s.host.sessions_enabled()
                    && (matches!(&outcome, ToolOutcome::Disconnected)
                        || matches!(&outcome,ToolOutcome::Completed(out)if out.disconnected))
                {
                    s.error = Some(AiError::Disconnected);
                }
                return Some((AgentEvent::StepFinished { call, outcome }, s));
            }
            if s.model.is_none()
                && let Some(focus) = s.host.take_compaction()
            {
                match s.begin_compaction(CompactTrigger::Manual, focus) {
                    Ok(()) => return Some((AgentEvent::Compacting { tier: 1 }, s)),
                    Err(e) => return Some((s.fail(e), s)),
                }
            }
            if s.limits.max_steps.is_some_and(|max| s.steps >= max) {
                return Some((s.pause(PauseReason::StepLimit), s));
            }
            if let Some(pending) = s.turn.as_mut().unwrap().pending.pop_front() {
                if let Pending::Local(raw, _) | Pending::AgentMessage(raw, ..) | Pending::Question(raw,_) = &pending {
                    let id = raw.id.to_string();
                    if s.started_tool.as_deref() != Some(&id) {
                        let call = crate::LoggedCall {
                            id: id.clone(),
                            name: raw.function.name.to_string(),
                            args_json: serde_json::Value::Object(raw.function.arguments.clone()),
                        };
                        s.started_tool = Some(id);
                        s.turn.as_mut().unwrap().pending.push_front(pending);
                        return Some((AgentEvent::ToolStarted(call), s));
                    }
                    s.started_tool = None;
                }
                match pending {
                    Pending::Question(raw,request) => {
                        let call=crate::LoggedCall{id:raw.id.to_string(),name:"ask_user".into(),args_json:serde_json::Value::Object(raw.function.arguments)};
                        let q=crate::questions::Pending::new(shellmate_core::HostId::generate().to_string(),call.clone(),request,s.conversation.latest_user().unwrap().context.clone());
                        let (inline,is_error)=match s.host.control().ok_or_else(||"当前环境不支持用户提问".to_owned()).and_then(|c|c.register_question(q.clone())) {Ok(())=>(q.receipt(),false),Err(e)=>(e,true)};
                        let id=call.id.clone();s.entry(Entry::Step(crate::Step{message_id:s.message_id,call,result:crate::LoggedResult{attachments:vec![],subagent:None,inline,output_id:None,cleared:false,is_error},display:None}));
                        s.steps+=1;s.turn.as_mut().unwrap().steps+=1;
                        return Some((AgentEvent::ToolFinished(id),s));
                    }
                    Pending::AgentMessage(raw, id, text, immediate, resume) => {
                        let tool_id = raw.id.to_string();
                        let result = s
                            .host
                            .send_agent_message(&id, &text, immediate, resume, crate::media::parse_ids(raw.function.arguments.get("image_ids")).unwrap_or_default(), &raw.id.to_string())
                            .await;
                        s.entry(Entry::Step(crate::Step {
                            message_id: s.message_id,
                            call: crate::LoggedCall {
                                id: raw.id.to_string(),
                                name: raw.function.name.to_string(),
                                args_json: serde_json::Value::Object(raw.function.arguments),
                            },
                            result: crate::LoggedResult {
                                attachments: result.attachments, inline: result.inline,
                                output_id: result.output_id,
                                subagent: result.subagent,
                                cleared: false,
                                is_error: result.is_error,
                            },
                            display: None,
                        }));
                        s.steps += 1;
                        s.turn.as_mut().unwrap().steps += 1;
                        return Some((AgentEvent::ToolFinished(tool_id), s));
                    }
                    Pending::Subagent(raw, request) => {
                        s.phase(crate::interaction::Phase::Executing);
                        let result = s.host.subagent(request.clone(), &raw.id.to_string()).await;
                        let display = result.subagent.or_else(|| {
                            Some(Box::new(crate::SubagentDisplay {
                                description: request.description,
                                kind: request.kind,
                                model: request
                                    .model
                                    .clone()
                                    .unwrap_or_else(|| s.client.config.model.clone()),
                                reasoning: request.reasoning.unwrap_or(s.client.config.reasoning),
                                model_overridden: request.model.is_some(),
                                reasoning_overridden: request.reasoning.is_some(),
                                status: if result.is_error {
                                    crate::SubagentStatus::Failed(AiError::Unavailable)
                                } else if request.background {
                                    crate::SubagentStatus::Running
                                } else {
                                    crate::SubagentStatus::Completed
                                },
                                steps: 0,
                                duration_ms: 0,
                                usage: Usage::default(),
                                output_id: result.output_id.clone(),
                                transcript_id: None,
                            }))
                        });
                        s.entry(Entry::Step(crate::Step {
                            message_id: s.message_id,
                            call: crate::LoggedCall {
                                id: raw.id.to_string(),
                                name: "agent".into(),
                                args_json: serde_json::Value::Object(raw.function.arguments),
                            },
                            result: crate::LoggedResult {
                                attachments: result.attachments, inline: result.inline,
                                output_id: result.output_id,
                                cleared: false,
                                is_error: result.is_error,
                                subagent: display,
                            },
                            display: None,
                        }));
                        s.steps += 1;
                        let turn = s.turn.as_mut().unwrap();
                        turn.steps += 1;
                        turn.invalid_calls = 0;
                        continue;
                    }
                    Pending::Local(raw, tool) => {
                        if matches!(&tool, LocalTool::Web(_) | LocalTool::Sessions(_)) {
                            s.phase(crate::interaction::Phase::Executing);
                        }
                        let tool_id = raw.id.to_string();
                        let result = if let LocalTool::ViewImage {id}=&tool {
                            if let Some(a)=s.conversation.attachments().into_iter().find(|a|a.id==*id) {
                                let mut r=LocalResult::text(a.label());r.attachments.push(a);r
                            } else {s.host.local_tool_from(tool,s.limits.exec,None,&tool_id).await}
                        } else {s.host.local_tool_from(tool,s.limits.exec,None,&tool_id).await};
                        s.entry(Entry::Step(crate::Step {
                            message_id: s.message_id,
                            call: crate::LoggedCall {
                                id: raw.id.to_string(),
                                name: raw.function.name.to_string(),
                                args_json: serde_json::Value::Object(raw.function.arguments),
                            },
                            result: crate::LoggedResult {
                                attachments: result.attachments, subagent: None,
                                inline: result.inline,
                                output_id: result.output_id,
                                cleared: false,
                                is_error: result.is_error,
                            },
                            display: None,
                        }));
                        s.steps += 1;
                        s.turn.as_mut().unwrap().steps += 1;
                        s.turn.as_mut().unwrap().invalid_calls = 0;
                        return Some((AgentEvent::ToolFinished(tool_id), s));
                    }
                    Pending::Call(raw, call) => {
                        s.turn.as_mut().unwrap().invalid_calls = 0;
                        s.active = Some((*raw, call.clone()));
                        return Some((AgentEvent::StepProposed(call), s));
                    }
                    Pending::Invalid(raw, error) => {
                        s.entry(Entry::Step(crate::Step {
                            message_id: s.message_id,
                            call: crate::LoggedCall {
                                id: raw.id.to_string(),
                                name: raw.function.name.to_string(),
                                args_json: serde_json::Value::Object(raw.function.arguments),
                            },
                            result: crate::LoggedResult {
                                attachments: Vec::new(), subagent: None,
                                inline: error,
                                output_id: None,
                                cleared: false,
                                is_error: true,
                            },
                            display: None,
                        }));
                        let turn = s.turn.as_mut().unwrap();
                        turn.steps += 1;
                        s.steps += 1;
                        turn.invalid_calls += 1;
                        if turn.invalid_calls >= 3 {
                            return Some((s.pause(PauseReason::InvalidCalls), s));
                        }
                        continue;
                    }
                    Pending::Truncated(output) => {
                        s.entry(Entry::Notice { kind: "truncated".into(), text: format!("上一次工具调用因达到单次输出上限（{output}）被截断，未执行。请拆成更小的步骤（例如分多次写入文件）或缩短命令后重试。") });
                        let turn = s.turn.as_mut().unwrap();
                        turn.steps += 1;
                        s.steps += 1;
                        turn.invalid_calls += 1;
                        if turn.invalid_calls >= 3 {
                            return Some((s.pause(PauseReason::InvalidCalls), s));
                        }
                        continue;
                    }
                    Pending::Reasoning(block) => {let event=s.reasoning(block);return Some((event,s));}
                    Pending::Text(text) => {
                        s.text(&text);
                        return Some((AgentEvent::TextDelta(text), s));
                    }
                }
            }
            if s.model.is_none() {
                for mut step in s.host.take_external_steps() {
                    step.message_id = s.conversation.entries().len();
                    s.entry(Entry::Step(step));
                }
                // Drain notices first: every completion notice observed here must have its
                // final report applied before the next request is assembled.
                let control = s.host.control();
                let completions = control
                    .as_ref()
                    .map(|c| c.claim_completions())
                    .unwrap_or_default();
                let notices = if control.is_some() {
                    let _ = s.host.take_notices();
                    completions.iter().map(|e| e.text.clone()).collect()
                } else {
                    s.host.take_notices()
                };
                for (call_id, result) in s.host.subagent_results() {
                    if s.conversation.entries().iter().any(|e| matches!(e, Entry::Step(step) if step.call.id==call_id && !step.result.same_content(&result))) {
                        s.record(LogUpdate::SubagentResult {call_id,result});
                    }
                }
                for text in notices {
                    s.entry(Entry::Notice {
                        kind: "task".into(),
                        text,
                    });
                }
                if !completions.is_empty() {
                    return Some((
                        AgentEvent::CompletionsAccepted(
                            completions.into_iter().map(|e| e.id).collect(),
                        ),
                        s,
                    ));
                }
                let request = match s.request() {
                    Ok(r) => r,
                    Err(e) => return Some((s.fail(e), s)),
                };
                let used = s
                    .conversation
                    .used(&request, &s.turn.as_ref().unwrap().capabilities);
                if !std::mem::take(&mut s.skip_precheck)
                    && used >= s.client.context_budget().trigger as usize
                {
                    match s.begin_compaction(CompactTrigger::PreRequest, None) {
                        Ok(()) => return Some((AgentEvent::Compacting { tier: 1 }, s)),
                        Err(e) => return Some((s.fail(e), s)),
                    }
                }
                s.first = true;
                s.received = false;
                s.defer_text = false;
                s.thinking = false;
                s.reasoning = Default::default();
                s.message_id = s.conversation.entries().len();
                if let Some(slots)=s.host.model_slots() {
                    s.phase(crate::interaction::Phase::Resource);
                    s.model_permit=tokio::select! {
                        biased;
                        ()=interaction::urgent(s.host.control())=>{s.interrupt_stream();continue;},
                        permit=slots.acquire_owned()=>permit.ok(),
                    };
                }
                s.phase(crate::interaction::Phase::Generating);
                let request = match s.client.materialize(request).await { Ok(r) => r, Err(e) => return Some((s.fail(e), s)) };
                match s.client.agent_open(request) {
                    Ok(model) => s.model = Some(model),
                    Err(e) => {
                        if s.retry_option(&e) {
                            continue;
                        }
                        return Some((s.provider_failure(*e), s));
                    }
                }
            }
            let timeouts = s.client.timeouts();
            let wait = if s.first {
                timeouts.first
            } else {
                timeouts.idle
            };
            let next = tokio::select! {biased;
                ()=interaction::urgent(s.host.control())=>{s.interrupt_stream();continue;},
                result=tokio::time::timeout(wait, s.model.as_mut().unwrap().next())=>result,
            };
            match next {
                Err(_) => return Some((s.failure(AiError::Timeout, None), s)),
                Ok(Some(Err(e))) => {
                    if s.retry_option(&e) {
                        continue;
                    }
                    return Some((s.provider_failure(e), s));
                }
                Ok(Some(Ok(item))) => {
                    s.first = false;
                    match item {
                        Item::Event(StreamEvent::Text { text, .. }) => {
                            s.received |= !text.is_empty();
                            s.retry_notice = false;
                            if s.defer_text {
                                continue;
                            }
                            s.text(&text);
                            return Some((AgentEvent::TextDelta(text), s));
                        }
                        Item::Event(StreamEvent::End {
                            content: AssistantContent::ToolCall(_),
                            ..
                        }) => {
                            s.received = true;
                            s.defer_text = true;
                            if std::mem::take(&mut s.retry_notice) {
                                return Some((AgentEvent::Thinking, s));
                            }
                        }
                        // Display only provider-exposed text; finish() retains native signatures separately.
                        Item::Event(
                            event @ (StreamEvent::Reasoning { .. }
                            | StreamEvent::Start {
                                kind: PartKind::Reasoning,
                                ..
                            }
                            | StreamEvent::End {
                                content: AssistantContent::Reasoning(_),
                                ..
                            }),
                        ) => {
                            s.received |= !matches!(event, StreamEvent::Start { .. });
                            if !s.defer_text && let Some(block)=s.reasoning.event(&event,s.message_id,matches!(s.client.config.provider,crate::Provider::OpenAiResponses{..})) {
                                s.retry_notice=false;
                                s.thinking = !block.complete;
                                let event=s.reasoning(block);
                                return Some((event,s));
                            }
                            if (!s.thinking || s.retry_notice) && (s.turn.as_ref().unwrap().capabilities.available() || !s.conversation.entries()[s.message_id..].iter().any(|e| matches!(e, Entry::Assistant { text, .. } if !text.is_empty()))) {
                                s.retry_notice = false;
                                s.thinking = true;
                                return Some((AgentEvent::Thinking, s));
                            }
                        }
                        Item::Event(
                            StreamEvent::Arguments { .. }
                            | StreamEvent::Start {
                                kind: PartKind::ToolCall,
                                ..
                            },
                        ) => {
                            s.received = true;
                            if std::mem::take(&mut s.retry_notice) {
                                return Some((AgentEvent::Thinking, s));
                            }
                        }
                        Item::Event(StreamEvent::Start { .. }) => {}
                        Item::Event(_) => s.received = true,
                        _ => {}
                    }
                }
                Ok(None) => {
                    let model = s.model.take().unwrap();
                    let finished = tokio::select! {biased;
                        ()=interaction::urgent(s.host.control())=>{s.interrupt_stream();continue;},
                        result=tokio::time::timeout(timeouts.idle,model.finish())=>result,
                    };
                    s.model_permit=None;
                    let mut response = match finished {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            if s.retry_option(&e) {
                                continue;
                            }
                            return Some((s.provider_failure(e), s));
                        }
                        Err(_) => return Some((s.failure(AiError::Timeout, None), s)),
                    };
                    if s.conversation.child.is_some() {
                        s.record(LogUpdate::RequestUsage(Usage {
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                        }));
                    }
                    if response.error.is_some() || response.aborted.is_some() {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    }
                    s.failures = 0;
                    s.reactive_retry = false;
                    s.turn.as_mut().unwrap().model_steps += 1;
                    let mut unfinished: Vec<_> = response
                        .unfinished_calls()
                        .map(|call| call.id.clone())
                        .collect();
                    // rig 0.46's Chat decoder explicitly closes cut calls; close_item()
                    // does not add them to unfinished_calls(), but retains invalid_arguments.
                    if response.finish_reason() == Some(FinishReason::Length) {
                        unfinished.extend(
                            response
                                .tool_calls()
                                .filter(|call| call.function.invalid_arguments.is_some())
                                .map(|call| call.id.clone()),
                        );
                    }
                    let mut used_ids: HashSet<_> = s
                        .conversation
                        .entries()
                        .iter()
                        .filter_map(|e| {
                            if let Entry::Step(step) = e {
                                Some(step.call.id.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    for content in &mut response.choice {
                        if let AssistantContent::ToolCall(call) = content {
                            let id = crate::context::portable_id(
                                &call.id.to_string(),
                                s.message_id,
                                &mut used_ids,
                            );
                            if id != call.id.to_string() {
                                let old_id = call.id.clone();
                                call.id = rig_core::message::CallId::from_wire(id);
                                for id in &mut unfinished {
                                    if *id == old_id {
                                        *id = call.id.clone();
                                    }
                                }
                                call.native = None;
                            }
                        }
                    }
                    let truncated = !unfinished.is_empty();
                    if truncated
                        && !s.turn.as_ref().unwrap().capabilities.available()
                        && !s.turn.as_ref().unwrap().capabilities.web
                        && !s.host.sessions_enabled()
                        && s.conversation.attachments().is_empty()
                    {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    }
                    if truncated && response.finish_reason() != Some(FinishReason::Length) {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    }
                    let mut pending = VecDeque::new();
                    let mut after_call = false;
                    for (part, content) in response.choice.iter().enumerate() {
                        match content {
                            AssistantContent::Reasoning(r) if after_call && !r.redacted && !r.text.is_empty() => {
                                pending.push_back(Pending::Reasoning(crate::ThinkingBlock{id:format!("{}:deferred:{part}",s.message_id),kind:crate::thinking::kind(r,matches!(s.client.config.provider,crate::Provider::OpenAiResponses{..})),text:r.text.clone(),complete:true}));
                            }
                            AssistantContent::ToolCall(raw) => {
                                after_call = true;
                                if unfinished.contains(&raw.id) {
                                    continue;
                                }
                                if !s.turn.as_ref().unwrap().capabilities.available()
                                    && !s.host.sessions_enabled()
                                    && !matches!(
                                        raw.function.name.as_str(),
                                        "web_search" | "web_fetch" | "view_image" | "ask_user"
                                    )
                                {
                                    if raw.function.name.as_str() == "propose_command" {
                                        let card = serde_json::from_value::<CommandCard>(
                                            serde_json::Value::Object(
                                                raw.function.arguments.clone(),
                                            ),
                                        )
                                        .ok()
                                        .filter(|c| {
                                            !c.command.trim().is_empty()
                                                && c.command.len() <= 16384
                                                && c.explanation.len() <= 8192
                                        });
                                        let Some(card) = card else {
                                            return Some((s.fail(AiError::InvalidResponse), s));
                                        };
                                        if s.conversation.entries()[s.message_id..]
                                            .iter()
                                            .filter(|e| matches!(e, Entry::CommandCard { .. }))
                                            .count()
                                            >= 20
                                        {
                                            return Some((s.fail(AiError::InvalidResponse), s));
                                        }
                                        s.entry(Entry::CommandCard {
                                            card: card.clone(),
                                            message_id: s.message_id,
                                        });
                                        s.queue.push_back(AgentEvent::Command(card));
                                    }
                                    continue;
                                }
                                let raw = Box::new(raw.clone());
                                if raw.function.name.as_str()=="ask_user" {
                                    let parsed=if raw.function.invalid_arguments.is_some(){Err("提问参数必须为完整 JSON".into())}else{crate::questions::Request::parse(serde_json::Value::Object(raw.function.arguments.clone()))};
                                    pending.push_back(match parsed {Ok(request)=>Pending::Question(raw,request),Err(e)=>Pending::Invalid(raw,e)});continue;
                                }
                                if raw.function.name.as_str() == "send_agent_message" {
                                    let args =
                                        serde_json::Value::Object(raw.function.arguments.clone());
                                    let parsed = args["id"].as_str().filter(|s| !s.is_empty()).zip(
                                        args["message"].as_str().filter(|s| !s.trim().is_empty()),
                                    );
                                    let message =
                                        parsed.map(|(id, text)| (id.to_owned(), text.to_owned()));
                                    pending.push_back(if s.conversation.agent_tool.is_none() {
                                        Pending::Invalid(raw, "当前会话不支持追加子任务消息".into())
                                    } else if let Err(e)=crate::media::parse_ids(args.get("image_ids")) {
                                        Pending::Invalid(raw,e)
                                    } else if let Some((id, text)) = message {
                                        Pending::AgentMessage(
                                            raw,
                                            id,
                                            text,
                                            args["immediate"].as_bool().unwrap_or(false),
                                            args["resume"].as_bool().unwrap_or(false),
                                        )
                                    } else {
                                        Pending::Invalid(
                                            raw,
                                            "send_agent_message 需要 id 和非空 message".into(),
                                        )
                                    });
                                    continue;
                                }
                                if raw.function.name.as_str() == "agent" {
                                    let request = if s.conversation.agent_tool.is_none() {
                                        Err("当前会话不支持子任务（需要主任务、后台通道和主机支持）".into())
                                    } else if raw.function.invalid_arguments.is_some() {
                                        Err("子任务参数无效：需要 JSON 对象".into())
                                    } else {
                                        crate::SubagentRequest::parse(
                                            serde_json::Value::Object(
                                                raw.function.arguments.clone(),
                                            ),
                                            &s.client.config.provider,
                                        )
                                    };
                                    pending.push_back(match request {
                                        Ok(request) => Pending::Subagent(raw, request),
                                        Err(error) => Pending::Invalid(raw, error),
                                    });
                                    continue;
                                }
                                pending.push_back(
                                    match parse_local(
                                        &raw,
                                        s.limits.auto_memory,
                                        s.host.web_enabled(),
                                        s.host.sessions_enabled(),
                                    ) {
                                        Ok(Some(tool)) => Pending::Local(raw, tool),
                                        Err(error) => Pending::Invalid(raw, error),
                                        Ok(None) => match parse_call(
                                            &raw,
                                            &s.turn.as_ref().unwrap().capabilities,
                                            s.limits.exec,
                                        ) {
                                            Ok(call) => Pending::Call(raw, call),
                                            Err(error) => Pending::Invalid(raw, error),
                                        },
                                    },
                                );
                            }
                            AssistantContent::Text(text) if after_call => {
                                pending.push_back(Pending::Text(text.text.clone()))
                            }
                            _ => {}
                        }
                    }
                    if truncated {
                        pending.push_back(Pending::Truncated(s.client.output_tokens()));
                    }
                    // Suggestion responses end after their cards and any following text.
                    // They never start another model call just to acknowledge a card.
                    if !s.turn.as_ref().unwrap().capabilities.available()
                        && !s.turn.as_ref().unwrap().capabilities.web
                        && !s.host.sessions_enabled()
                        && s.conversation.attachments().is_empty()
                        && !pending.iter().any(|p|matches!(p,Pending::Question(..)))
                    {
                        while let Some(pending) = pending.pop_front() {
                            match pending {
                                Pending::Text(text) => {s.text(&text);s.queue.push_back(AgentEvent::TextDelta(text));},
                                Pending::Reasoning(block) => {let event=s.reasoning(block);s.queue.push_back(event);},
                                _=>{}
                            }
                        }
                    }
                    // message() includes unfinished calls. Remove only those blocks, leaving
                    // native signed reasoning, text and complete calls byte-for-byte intact.
                    response.choice.retain(|content| {
                        !matches!(content,
                        AssistantContent::ToolCall(call) if unfinished.contains(&call.id))
                    });
                    let message = response.message();
                    if message.is_none() && !truncated {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    }
                    // Anchor reasoning-only or tool-only batches even before their result.
                    if !s.conversation.entries()[s.message_id..]
                        .iter()
                        .any(|e| matches!(e,Entry::Assistant{message_id,..} if *message_id==s.message_id))
                    {
                        s.entry(Entry::Assistant {
                            replay: None,
                            text: String::new(),
                            message_id: s.message_id,
                        });
                    }
                    if let Some(Message::Assistant(message)) = message {
                        if matches!(s.client.config.provider, crate::Provider::OpenAiResponses { .. }) {
                            let mut capabilities = s.turn.as_ref().unwrap().capabilities.clone();
                            capabilities.web = s.host.web_enabled();
                            capabilities.sessions = s.host.sessions_enabled();
                            if let (Ok(instructions), Ok(request)) = (s.conversation.system(&capabilities), s.request())
                                && let Some(replay) = crate::ResponseReplay::capture(
                                    &s.client.config, s.conversation.response_prefix(s.message_id),
                                    &instructions, rig_core::message::Fingerprint::of(&request.tools), &message,
                                ) {
                                s.record(LogUpdate::ResponseReplay { message_id: s.message_id, replay: Box::new(replay) });
                            }
                        } else if s.turn.as_ref().unwrap().capabilities.available() {
                            s.turn.as_mut().unwrap().native.insert(s.message_id, message);
                        }
                    }
                    let u = response.usage;
                    let usage =
                        (u.input_tokens.is_some() || u.output_tokens.is_some()).then_some(Usage {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                        });
                    if let Some(u) = usage {
                        s.record(LogUpdate::Usage(u));
                    }
                    let request = s.request().unwrap();
                    let budget = s.client.context_budget();
                    s.queue.push_back(AgentEvent::Context {
                        used: s
                            .conversation
                            .used(&request, &s.turn.as_ref().unwrap().capabilities),
                        window: budget.window,
                        trigger: budget.trigger,
                    });
                    if pending.is_empty() {
                        if !s.turn.as_ref().unwrap().capabilities.available()
                            && !s.conversation.entries()[s.message_id..]
                                .iter()
                                .any(|e| matches!(e, Entry::CommandCard { .. }))
                        {
                            let text: String = s.conversation.entries()[s.message_id..]
                                .iter()
                                .filter_map(|e| {
                                    if let Entry::Assistant { text, .. } = e {
                                        Some(text.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            for card in crate::client::fenced_commands(&text) {
                                s.entry(Entry::CommandCard {
                                    card: card.clone(),
                                    message_id: s.message_id,
                                });
                                s.queue.push_back(AgentEvent::Command(card));
                            }
                        }
                        s.done = true;
                        s.queue.push_back(AgentEvent::Done(usage));
                    } else {
                        s.turn.as_mut().unwrap().pending = pending;
                    }
                }
            }
        }
    });
    AgentStream {
        inner: Box::pin(inner),
        updates,
    }
}
fn parse_call(
    raw: &RigCall,
    capabilities: &Capabilities,
    limits: ExecLimits,
) -> Result<ToolCall, String> {
    #[derive(Deserialize)]
    #[serde(crate = "rig_core::serde")]
    struct Arguments {
        service: Option<ServiceOptions>,
        cwd: Option<String>,
        session_id: Option<String>,
        command: Option<String>,
        purpose: Option<String>,
        target: Option<String>,
        timeout_secs: Option<serde_json::Value>,
        #[serde(default)]
        run_in_background: bool,
    }
    if raw.function.name.as_str() != "run_command" {
        return Err("未知工具，请使用 run_command".into());
    }
    if raw.function.invalid_arguments.is_some() {
        return Err("工具参数无效：无法解析 JSON，请提供 JSON 对象".into());
    }
    let a: Arguments =
        serde_json::from_value(serde_json::Value::Object(raw.function.arguments.clone()))
            .map_err(|_| "工具参数无效：command、purpose 和 target 必须为字符串".to_owned())?;
    let command = a.command.ok_or("工具参数无效：缺少 command")?;
    if command.trim().is_empty() {
        return Err("工具参数无效：command 不能为空".into());
    }
    if command.len() > 16384 {
        return Err("工具参数无效：command 超过 16384 字节".into());
    }
    let purpose = a.purpose.as_deref().unwrap_or_default().trim();
    if purpose.len() > 8192 {
        return Err("工具参数无效：purpose 超过 8192 字节".into());
    }
    let default_target = if capabilities.background {
        Target::Background
    } else {
        Target::Terminal
    };
    let requested = a.target.as_deref().unwrap_or(default_target.id());
    let target = capabilities
        .targets()
        .into_iter()
        .find(|t| t.id() == requested)
        .ok_or_else(|| {
            format!(
                "执行目标 {} 当前不可用，可用：{}",
                crate::output::stored_text(requested),
                capabilities
                    .targets()
                    .iter()
                    .map(|t| t.id())
                    .collect::<Vec<_>>()
                    .join("、")
            )
        })?;
    if let Some(cwd)=&a.cwd && (!capabilities.sessions || target!=Target::Background || !cwd.starts_with('/') || cwd.contains('\0') || cwd.len()>8192) {
        return Err("cwd 仅用于多终端独立通道命令，必须为绝对目录".into());
    }
    let timeout = a
        .timeout_secs
        .as_ref()
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()));
    if let Some(service) = &a.service
        && (!(1..=60).contains(&service.startup_timeout_secs)
            || service.ready_pattern.as_ref().is_some_and(|p| p.trim().is_empty() || p.len() > 512)) {
        return Err("service 就绪文本需为 1–512 字节，启动检查时间需为 1–60 秒".into());
    }
    Ok(ToolCall {
        run_in_background: a.run_in_background || a.service.is_some(),
        service: a.service,
        session_id: a.session_id,
        cwd: a.cwd, destination: None,
        id: raw.id.to_string(),
        class: policy::classify(&command),
        command,
        purpose: if purpose.is_empty() {
            "（未说明用途）"
        } else {
            purpose
        }
        .into(),
        target,
        timeout: limits.timeout(timeout),
        exec_limits: limits,
    })
}

fn parse_local(
    raw: &RigCall,
    auto_memory: bool,
    web: bool,
    sessions: bool,
) -> Result<Option<LocalTool>, String> {
    let name = raw.function.name.as_str();
    if name == "view_image" {
        if raw.function.invalid_arguments.is_some() || raw.function.arguments.len()!=1 { return Err("图片参数无效".into()); }
        return raw.function.arguments.get("id").and_then(|v|v.as_str()).map(|id|Some(LocalTool::ViewImage{id:id.into()})).ok_or_else(||"需要图片引用".into());
    }
    if matches!(
        name,
        "list_sessions" | "read_terminal" | "terminal_input" | "wait_terminal"
    ) {
        if !sessions {
            return Err("终端目录不可用".into());
        }
        if raw.function.invalid_arguments.is_some() {
            return Err("终端工具参数必须为完整 JSON".into());
        }
        return crate::session_tools::parse(
            name,
            serde_json::Value::Object(raw.function.arguments.clone()),
        )
        .map(|r| Some(LocalTool::Sessions(r)));
    }
    if matches!(name, "web_search" | "web_fetch") {
        if !web {
            return Err("网页工具未启用，请在 AI 设置中开启".into());
        }
        if raw.function.invalid_arguments.is_some() {
            return Err("网页工具参数必须为完整 JSON".into());
        }
        return crate::web::Request::parse(
            name,
            serde_json::Value::Object(raw.function.arguments.clone()),
        )
        .map(|r| Some(LocalTool::Web(r)));
    }
    if !matches!(name, "read_output" | "stop_task" | "memory" | "wait_tasks") {
        return Ok(None);
    }
    if raw.function.invalid_arguments.is_some() {
        return Err("工具参数无效：需要 JSON 对象".into());
    }
    let args = serde_json::Value::Object(raw.function.arguments.clone());
    if name == "wait_tasks" {
        let ids = args["ids"]
            .as_array()
            .and_then(|ids| {
                ids.iter()
                    .map(|id| id.as_str().filter(|id| !id.is_empty()).map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|ids| !ids.is_empty() && ids.len() <= 64)
            .ok_or("wait_tasks：ids 必须是 1–64 个任务 ID")?;
        let all = match args.get("mode").and_then(|s| s.as_str()).unwrap_or("all") {
            "all" => true,
            "any" => false,
            _ => return Err("wait_tasks：mode 必须是 any 或 all".into()),
        };
        return Ok(Some(LocalTool::WaitTasks { ids, all }));
    }
    if name == "memory" {
        if !auto_memory {
            return Err("自动记忆已关闭，memory 工具不可用".into());
        }
        let mut args = args;
        let session_id = args
            .as_object_mut()
            .and_then(|o| o.remove("session_id"))
            .and_then(|v| v.as_str().map(str::to_owned));
        let command: crate::memory::Command =
            serde_json::from_value(args).map_err(|e| format!("memory 参数无效：{e}"))?;
        return Ok(Some(match session_id {
            Some(session_id) => LocalTool::ScopedMemory {
                session_id,
                command,
            },
            None => LocalTool::Memory(command),
        }));
    }
    if name == "read_output" {
        let a: ReadOutput = serde_json::from_value(args).map_err(|_| "read_output 参数无效")?;
        if a.id.is_empty()
            || a.id.len() > 160
            || a.limit_lines.is_some_and(|n| n == 0 || n > 2000)
            || a.grep.as_ref().is_some_and(|s| s.len() > 4096)
        {
            return Err(
                "read_output：id 不能为空，limit_lines 为 1–2000，grep 最长 4096 字节".into(),
            );
        }
        Ok(Some(LocalTool::ReadOutput(a)))
    } else {
        #[derive(Deserialize)]
        #[serde(crate = "rig_core::serde", deny_unknown_fields)]
        struct Stop {
            id: String,
        }
        let a: Stop = serde_json::from_value(args).map_err(|_| "stop_task 参数无效")?;
        if a.id.is_empty() || a.id.len() > 160 {
            return Err("stop_task：无效的任务 id".into());
        }
        Ok(Some(LocalTool::StopTask { id: a.id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_option_fallback_is_scoped_to_parallel_only() {
        for (field, expected) in [
            ("parallel_tool_calls", true),
            ("reasoning", false),
            ("tools", false),
        ] {
            let error = rig_core::ProviderError::UnsupportedOption(
                rig_core::completion::UnsupportedOption::new(
                    field,
                    "fixture",
                    "model",
                    "private diagnostic",
                ),
            );
            assert_eq!(parallel_refusal(&error), expected);
        }
    }
}
