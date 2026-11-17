//! Lazy agent loop. The host alone approves and executes; dropping the stream drops
//! the in-flight model/host future. A host must cancel its operation on future drop.
use crate::{
    AiError, Answer, AnswerItem, CommandCard, Conversation, Event, ProviderClient, RigClient,
    StepStatus, ToolOutput, Usage, UserTurn,
    client::map_error,
    policy::{self, Class},
};
use futures::{Stream, StreamExt, future::BoxFuture};
use rig_core::{
    completion::CompletionRequest,
    message::{AssistantContent, Message, ToolCall as RigCall, ToolResultContent},
    serde::{Deserialize, Serialize},
    streaming::{CompletionStream, Item, StreamEvent},
};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

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
            Self::Terminal => "终端",
            Self::Background => "后台",
        }
    }
    pub fn timeout(self, requested: Option<u64>) -> Duration {
        Duration::from_secs(requested.unwrap_or(30).clamp(
            1,
            match self {
                Self::Terminal => 300,
                Self::Background => 120,
            },
        ))
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub terminal: bool,
    pub background: bool,
    pub terminal_busy: bool,
    pub cwd: Option<String>,
}
impl Capabilities {
    pub fn available(&self) -> bool {
        self.terminal || self.background
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
    /// When the user explicitly asks for the terminal or for a separate run, offer only
    /// that target for this turn, so the model cannot pick the other one.
    pub fn for_question(mut self, question: &str) -> Self {
        match explicit_target(question) {
            Some(Target::Terminal) if self.terminal => self.background = false,
            Some(Target::Background) if self.background => self.terminal = false,
            _ => {}
        }
        self
    }
}
/// The target a question names explicitly, if any.
pub fn explicit_target(question: &str) -> Option<Target> {
    let q: String = question.chars().filter(|c| !c.is_whitespace()).collect();
    let negated_terminal = [
        "不要在终端",
        "别在终端",
        "不在终端",
        "不要用终端",
        "别用终端",
    ]
    .iter()
    .any(|p| q.contains(p));
    let background = negated_terminal
        || [
            "后台",
            "单独执行",
            "单独运行",
            "独立执行",
            "独立运行",
            "独立通道",
            "单独跑",
        ]
        .iter()
        .any(|p| q.contains(p));
    let terminal = !negated_terminal
        && [
            "在终端",
            "当前终端",
            "终端里",
            "终端中",
            "终端上",
            "我的终端",
            "这个终端",
            "终端执行",
            "终端运行",
            "终端跑",
        ]
        .iter()
        .any(|p| q.contains(p));
    match (terminal, background) {
        (true, false) => Some(Target::Terminal),
        (false, true) => Some(Target::Background),
        _ => None,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub command: String,
    pub purpose: String,
    pub target: Target,
    pub timeout: Duration,
    pub class: Class,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed(ToolOutput),
    Denied { reason: Option<String> },
    Busy,
    Disconnected,
    Failed(String),
}
impl ToolOutcome {
    fn sanitize(self) -> Self {
        match self {
            Self::Denied { reason } => Self::Denied {
                reason: reason.map(|s| crate::output::stored_text(&s)),
            },
            Self::Failed(s) => Self::Failed(crate::output::stored_text(&s)),
            other => other,
        }
    }
    fn wire_text(&self) -> String {
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
                if out.timed_out {
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
        item.sanitize();
        item
    }
}
pub trait ToolHost: Send + Sync {
    /// Approval and execution belong to the host. Do not detach work: cancellation
    /// of this future must cancel the operation. The core never spawns host tasks.
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome>;
    /// The approved command may have been edited or retargeted by the user.
    fn resolved_call(&self, call: &ToolCall) -> ToolCall {
        call.clone()
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub steps: usize,
    pub output_chars: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            steps: 15,
            output_chars: 64_000,
        }
    }
}

pub enum AgentEvent {
    Thinking,
    TextDelta(String),
    Command(CommandCard),
    StepProposed(ToolCall),
    StepFinished {
        call: ToolCall,
        outcome: ToolOutcome,
    },
    StepLimit {
        steps: usize,
        resume: Box<Resume>,
    },
    Done(Option<Usage>),
    Error(AiError),
}
enum Pending {
    Call(Box<RigCall>, ToolCall),
    Invalid(Box<RigCall>, String),
    Text(String),
}
/// Opaque, memory-only continuation. Deliberately neither Debug nor serializable:
/// it owns signed reasoning and real tool messages for this turn only.
pub struct Resume {
    request: CompletionRequest,
    pending: VecDeque<Pending>,
    capabilities: Capabilities,
    output_chars: usize,
    steps: usize,
    invalid_calls: usize,
    /// The last command that completed in this turn, to refuse an immediate identical repeat.
    last_run: Option<(String, Target)>,
    turn: UserTurn,
    turns: usize,
    parallel_disabled: bool,
}
pub struct AgentStream<'a> {
    inner: Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>,
    pub dropped_turns: usize,
}
impl Stream for AgentStream<'_> {
    type Item = AgentEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
pub fn run<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    capabilities: Capabilities,
    host: &'a dyn ToolHost,
    limits: Limits,
) -> Result<AgentStream<'a>, AiError> {
    if !capabilities.available() {
        let stream = client.start(conversation)?;
        let dropped_turns = stream.dropped_turns;
        let inner = stream.map(move |event| {
            let answer = conversation
                .turns
                .last_mut()
                .unwrap()
                .answer
                .get_or_insert_with(Answer::default);
            match event {
                Event::Thinking => AgentEvent::Thinking,
                Event::TextDelta(text) => {
                    answer.push_text(&text);
                    AgentEvent::TextDelta(text)
                }
                Event::Command(card) => {
                    answer.items.push(AnswerItem::Command(card.clone()));
                    AgentEvent::Command(card)
                }
                Event::Done(u) => {
                    answer.sanitize();
                    AgentEvent::Done(u)
                }
                Event::Error(e) => {
                    answer.sanitize();
                    AgentEvent::Error(e)
                }
            }
        });
        return Ok(AgentStream {
            inner: Box::pin(inner),
            dropped_turns,
        });
    }
    let capabilities = capabilities.for_question(&conversation.turns.last().unwrap().user.question);
    let prepared =
        conversation.prepare_with_capabilities(client.character_budget, &capabilities)?;
    conversation.turns.last_mut().unwrap().answer = Some(Answer::default());
    let state = Resume {
        request: prepared.request.parallel_tool_calls(false),
        pending: VecDeque::new(),
        capabilities,
        output_chars: 0,
        steps: 0,
        invalid_calls: 0,
        last_run: None,
        turn: conversation.turns.last().unwrap().user.clone(),
        turns: conversation.turns.len(),
        parallel_disabled: false,
    };
    Ok(stream(
        client,
        conversation,
        host,
        limits,
        state,
        prepared.dropped_turns,
    ))
}
pub fn resume<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    continuation: Resume,
    limits: Limits,
) -> Result<AgentStream<'a>, AiError> {
    if conversation.turns.len() != continuation.turns
        || conversation
            .turns
            .last()
            .is_none_or(|t| t.user != continuation.turn)
    {
        return Err(AiError::InvalidConfig);
    }
    Ok(stream(client, conversation, host, limits, continuation, 0))
}
struct State<'a> {
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    limits: Limits,
    turn: Option<Resume>,
    model: Option<CompletionStream>,
    active: Option<(RigCall, ToolCall)>,
    first: bool,
    started: Instant,
    received: bool,
    defer_text: bool,
    thinking: bool,
    text_bytes: usize,
    steps: usize,
    done: bool,
    error: Option<AiError>,
}
impl State<'_> {
    fn answer(&mut self) -> &mut Answer {
        self.conversation
            .turns
            .last_mut()
            .unwrap()
            .answer
            .get_or_insert_with(Answer::default)
    }
    fn fail(&mut self, error: AiError) -> AgentEvent {
        self.done = true;
        self.model = None;
        self.turn = None;
        self.answer().sanitize();
        AgentEvent::Error(error)
    }
    fn retry(&mut self, error: &rig_core::ProviderError) -> bool {
        let turn = self.turn.as_mut().unwrap();
        if self.received || turn.parallel_disabled || !parallel_refusal(error) {
            return false;
        }
        turn.parallel_disabled = true;
        turn.request.options.parallel_tool_calls = None;
        self.model = None;
        true
    }
}
fn parallel_refusal(e: &rig_core::ProviderError) -> bool {
    if let rig_core::ProviderError::UnsupportedOption(o) = e {
        return o.option == "parallel_tool_calls";
    }
    let r = e.report();
    r.http_status == Some(400)
        && r.provider_response_body().is_some_and(|b| {
            b.contains("parallel_tool_calls") || b.contains("disable_parallel_tool_use")
        })
}
fn stream<'a>(
    client: &'a RigClient,
    conversation: &'a mut Conversation,
    host: &'a dyn ToolHost,
    limits: Limits,
    turn: Resume,
    dropped_turns: usize,
) -> AgentStream<'a> {
    let state = State {
        client,
        conversation,
        host,
        limits,
        turn: Some(turn),
        model: None,
        active: None,
        first: true,
        started: Instant::now(),
        received: false,
        defer_text: false,
        thinking: false,
        text_bytes: 0,
        steps: 0,
        done: false,
        error: None,
    };
    let inner = futures::stream::unfold(state, |mut s| async move {
        loop {
            if s.done {
                return None;
            }
            if let Some(e) = s.error.take() {
                return Some((s.fail(e), s));
            }
            if let Some((raw, call)) = s.active.take() {
                let outcome = s.host.run(call.clone()).await.sanitize();
                let resolved = s.host.resolved_call(&call);
                let wire = if resolved != call {
                    format!(
                        "用户批准的实际命令：{}\n执行目标：{}\n{}",
                        crate::output::stored_text(&resolved.command),
                        resolved.target.label(),
                        outcome.wire_text()
                    )
                } else {
                    outcome.wire_text()
                };
                let call = resolved;
                let turn = s.turn.as_mut().unwrap();
                turn.last_run = matches!(outcome, ToolOutcome::Completed(_))
                    .then(|| (normalized(&call.command), call.target));
                turn.output_chars = turn.output_chars.saturating_add(wire.chars().count());
                turn.steps += 1;
                s.steps += 1;
                append(
                    &mut turn.request,
                    Message::tool_results(vec![raw.result(vec![ToolResultContent::text(wire)])]),
                );
                if turn.output_chars >= s.limits.output_chars.min(64_000) {
                    s.error = Some(AiError::ToolOutputLimit);
                }
                if matches!(outcome, ToolOutcome::Disconnected) {
                    s.error = Some(AiError::Disconnected);
                }
                s.answer().items.push(outcome.item(&call));
                return Some((AgentEvent::StepFinished { call, outcome }, s));
            }
            if s.steps >= s.limits.steps.clamp(1, 15) {
                let resume = s.turn.take().unwrap();
                let steps = resume.steps;
                s.done = true;
                s.answer().sanitize();
                return Some((
                    AgentEvent::StepLimit {
                        steps,
                        resume: Box::new(resume),
                    },
                    s,
                ));
            }
            if let Some(pending) = s.turn.as_mut().unwrap().pending.pop_front() {
                match pending {
                    Pending::Call(raw, call)
                        if s.turn.as_ref().unwrap().last_run
                            == Some((normalized(&call.command), call.target)) =>
                    {
                        s.turn.as_mut().unwrap().pending.push_front(Pending::Invalid(
                            raw,
                            "这条命令刚刚已经执行过，结果就在上一条工具结果中，本次未重复执行。请直接根据已有结果回答；确需再次执行时，先说明原因并调整命令。".into(),
                        ));
                        continue;
                    }
                    Pending::Call(raw, call) => {
                        s.turn.as_mut().unwrap().invalid_calls = 0;
                        s.active = Some((*raw, call.clone()));
                        return Some((AgentEvent::StepProposed(call), s));
                    }
                    Pending::Invalid(raw, error) => {
                        let turn = s.turn.as_mut().unwrap();
                        let mut result = raw.result(vec![ToolResultContent::text(error)]);
                        result.is_error = true;
                        append(&mut turn.request, Message::tool_results(vec![result]));
                        turn.steps += 1;
                        s.steps += 1;
                        turn.invalid_calls += 1;
                        if turn.invalid_calls >= 3 {
                            return Some((s.fail(AiError::InvalidResponse), s));
                        }
                        continue;
                    }
                    Pending::Text(text) => {
                        s.answer().push_text(&text);
                        return Some((AgentEvent::TextDelta(text), s));
                    }
                }
            }
            if s.model.is_none() {
                match s
                    .client
                    .agent_open(s.turn.as_ref().unwrap().request.clone())
                {
                    Ok(model) => {
                        s.model = Some(model);
                        s.started = Instant::now();
                        s.first = true;
                        s.received = false;
                        s.defer_text = false;
                    }
                    Err(e) => {
                        if s.retry(&e) {
                            continue;
                        }
                        return Some((s.fail(map_error(*e)), s));
                    }
                }
            }
            let timeouts = s.client.timeouts();
            let remaining = timeouts.total.saturating_sub(s.started.elapsed());
            let wait = remaining.min(if s.first {
                timeouts.first
            } else {
                timeouts.idle
            });
            let next = tokio::time::timeout(wait, s.model.as_mut().unwrap().next()).await;
            match next {
                Err(_) => return Some((s.fail(AiError::Timeout), s)),
                Ok(Some(Err(e))) => {
                    if s.retry(&e) {
                        continue;
                    }
                    return Some((s.fail(map_error(e)), s));
                }
                Ok(Some(Ok(item))) => {
                    s.first = false;
                    match item {
                        Item::Event(StreamEvent::Text { text, .. }) => {
                            s.received = true;
                            s.text_bytes += text.len();
                            if s.text_bytes > 256 * 1024 {
                                return Some((s.fail(AiError::ContextTooLong), s));
                            }
                            if s.defer_text {
                                continue;
                            }
                            s.answer().push_text(&text);
                            return Some((AgentEvent::TextDelta(text), s));
                        }
                        Item::Event(StreamEvent::End {
                            content: AssistantContent::ToolCall(_),
                            ..
                        }) => {
                            s.received = true;
                            s.defer_text = true;
                        }
                        // Never expose reasoning text. finish() retains native signatures.
                        Item::Event(StreamEvent::Reasoning { .. }) => {
                            s.received = true;
                            if !s.thinking && s.text_bytes == 0 {
                                s.thinking = true;
                                return Some((AgentEvent::Thinking, s));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => {
                    let model = s.model.take().unwrap();
                    let response = match tokio::time::timeout(remaining, model.finish()).await {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            if s.retry(&e) {
                                continue;
                            }
                            return Some((s.fail(map_error(e)), s));
                        }
                        Err(_) => return Some((s.fail(AiError::Timeout), s)),
                    };
                    if response.error.is_some()
                        || response.aborted.is_some()
                        || response.unfinished_calls().next().is_some()
                    {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    }
                    let mut pending = VecDeque::new();
                    let mut after_call = false;
                    for content in &response.choice {
                        match content {
                            AssistantContent::ToolCall(raw) => {
                                after_call = true;
                                let raw = Box::new(raw.clone());
                                pending.push_back(
                                    match parse_call(&raw, &s.turn.as_ref().unwrap().capabilities) {
                                        Ok(call) => Pending::Call(raw, call),
                                        Err(error) => Pending::Invalid(raw, error),
                                    },
                                );
                            }
                            AssistantContent::Text(text) if after_call => {
                                pending.push_back(Pending::Text(text.text.clone()))
                            }
                            _ => {}
                        }
                    }
                    let Some(message) = response.message() else {
                        return Some((s.fail(AiError::InvalidResponse), s));
                    };
                    if pending.is_empty() {
                        s.done = true;
                        s.turn = None;
                        s.answer().sanitize();
                        let u = response.usage;
                        let usage = (u.input_tokens.is_some() || u.output_tokens.is_some())
                            .then_some(Usage {
                                input_tokens: u.input_tokens,
                                output_tokens: u.output_tokens,
                            });
                        return Some((AgentEvent::Done(usage), s));
                    }
                    let turn = s.turn.as_mut().unwrap();
                    append(&mut turn.request, message);
                    turn.pending = pending;
                }
            }
        }
    });
    AgentStream {
        inner: Box::pin(inner),
        dropped_turns,
    }
}
fn normalized(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// Append after the current question. `CompletionRequest::message` inserts *before* the
/// prompt (its last message), which would leave the question after the tool results and make
/// the model treat it as a new request and run the same command again.
fn append(request: &mut CompletionRequest, message: impl Into<Message>) {
    request
        .chat_history
        .push(std::sync::Arc::new(message.into()));
}
fn parse_call(raw: &RigCall, capabilities: &Capabilities) -> Result<ToolCall, String> {
    #[derive(Deserialize)]
    #[serde(crate = "rig_core::serde")]
    struct Arguments {
        command: Option<String>,
        purpose: Option<String>,
        target: Option<String>,
        timeout_secs: Option<serde_json::Value>,
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
    let timeout = a
        .timeout_secs
        .as_ref()
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()));
    Ok(ToolCall {
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
        timeout: target.timeout(timeout),
    })
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
