//! Window resolution, wire-safe replay and deterministic context accounting.
use crate::{
    AiError, CompactTrigger, Conversation, Entry, PermissionMode, Provider, RetainedState,
    agent::Capabilities, catalog_spec, estimate_tokens,
};
use rig_core::{
    completion::CompletionRequest,
    message::{
        AssistantContent, AssistantMessage, Message, ToolCall, ToolFunction, ToolName,
        ToolResultContent, UserContent,
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextWindow {
    #[default]
    Auto,
    Tokens(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowSource {
    Setting,
    Catalog,
    Fallback,
}
pub fn resolve_window(
    provider: &Provider,
    model: &str,
    setting: ContextWindow,
) -> (u32, WindowSource) {
    if let ContextWindow::Tokens(n) = setting
        && n > 0
    {
        return (n, WindowSource::Setting);
    }
    if let Some(n) = catalog_spec(provider, model)
        .and_then(|s| s.max_input_tokens.or(s.context_window))
        .filter(|n| *n > 0)
    {
        return (n, WindowSource::Catalog);
    }
    (128_000, WindowSource::Fallback)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextBudget {
    pub window: u32,
    pub auto: u32,
    pub reserve: u32,
    pub trigger: u32,
}
impl ContextBudget {
    pub fn new(window: u32, auto: ContextWindow, output: u32) -> Self {
        let a = match auto {
            ContextWindow::Auto => window,
            ContextWindow::Tokens(n) => n.clamp(window.min(100_000), window),
        };
        let reserve = output.min(a / 4).saturating_add(1000);
        Self {
            window,
            auto: a,
            reserve,
            trigger: a.saturating_sub(reserve),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub system_prompt: usize,
    pub tool_definitions: usize,
    pub tool_count: usize,
    pub terminal_context: usize,
    pub summary: usize,
    pub messages: usize,
    pub tool_results: usize,
    /// Provider usage + appended estimates when available; components are scaled to this total.
    pub total: usize,
    pub provider_usage: bool,
    pub budget: ContextBudget,
}
/// Stable semantic categories; presentation can add future categories without
/// relying on display labels or on the ordering of Conversation entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextCategory {
    System,
    Tools,
    Terminal,
    Messages,
    Results,
    Summary,
}
impl ContextBreakdown {
    pub fn categories(&self) -> [(ContextCategory, usize); 6] {
        [
            (ContextCategory::System, self.system_prompt),
            (ContextCategory::Tools, self.tool_definitions),
            (ContextCategory::Terminal, self.terminal_context),
            (ContextCategory::Messages, self.messages),
            (ContextCategory::Results, self.tool_results),
            (ContextCategory::Summary, self.summary),
        ]
    }
    /// Integer cumulative apportionment preserves zero categories and makes the
    /// sum exact, including the rounding remainder, without floating-point loss.
    pub fn scale_to(&mut self, total: usize) {
        let values = self.categories().map(|(_, n)| n);
        let sum = values.iter().map(|n| *n as u128).sum::<u128>();
        let mut cumulative = 0u128;
        let mut allocated = 0usize;
        let scaled = values.map(|n| {
            cumulative += n as u128;
            let next = (cumulative * total as u128).checked_div(sum).unwrap_or(0) as usize;
            let value = next - allocated;
            allocated = next;
            value
        });
        [
            self.system_prompt,
            self.tool_definitions,
            self.terminal_context,
            self.messages,
            self.tool_results,
            self.summary,
        ] = scaled;
        self.total = total;
    }
    pub fn remaining(&self) -> usize {
        (self.budget.window as usize).saturating_sub(self.total)
    }
    /// Initial estimate before the first question, with no artificial user entry.
    pub fn for_session(
        budget: ContextBudget,
        caps: &Capabilities,
        context: &crate::ContextSnapshot,
        limits: crate::agent::ExecLimits,
    ) -> Self {
        let system = if caps.available() {
            crate::conversation::execution_prompt(context, caps)
        } else {
            crate::conversation::system_prompt(context)
        };
        let mut tools = if caps.available() {
            crate::conversation::output_tools()
        } else {
            Vec::new()
        };
        tools.push(if caps.available() {
            crate::conversation::run_command_tool_with_limits(caps, limits)
        } else {
            crate::conversation::command_tool()
        });
        let system_prompt = estimate_tokens(&system) + 8;
        let tool_definitions = tools
            .iter()
            .map(|t| estimate_tokens(&serde_json::to_string(t).unwrap()))
            .sum::<usize>();
        Self {
            system_prompt,
            tool_definitions,
            tool_count: tools.len(),
            terminal_context: 0,
            summary: 0,
            messages: 0,
            tool_results: 0,
            total: system_prompt + tool_definitions,
            provider_usage: false,
            budget,
        }
    }
}
pub const SUMMARY_PREFIX: &str = "本会话延续自之前的对话，此前的上下文已压缩。以下是之前对话的摘要。你仍可看到工具的当前状态；请在已完成的工作基础上继续，避免重复。";
pub const SUMMARY_PROMPT: &str = "你正在执行上下文检查点压缩：为将接手这项任务的另一个模型写一份交接摘要，使它能无缝继续工作。\n先在 <analysis> 中按时间顺序梳理整段对话：用户的每个请求、你采取的做法、关键决定、遇到的错误及如何解决、用户的纠正与反馈。\n然后在 <summary> 中按以下部分输出：\n1. 主要请求与意图：用户明确提出的所有要求，尽量保留原意\n2. 关键概念与背景：涉及的技术、环境与事实\n3. 涉及的文件与内容：查看、修改或新建过的文件，及必要的关键片段\n4. 错误与修复：遇到的错误、处理方式，以及用户的纠正\n5. 解决过程：已解决的问题与仍在进行的排查\n6. 用户消息：按顺序列出用户的全部消息要点（不含工具结果）\n7. 待办事项：明确尚未完成的工作\n8. 当前工作：压缩前正在做的事，尽量具体\n9. 下一步：与最近请求直接相关的下一步；如有必要引用原话，避免偏离用户意图\n要求：简洁、结构化，保留继续工作所需的关键数据和引用。";
pub fn extract_summary(text: &str) -> String {
    let text = if let Some((_, rest)) = text.split_once("<summary>") {
        rest.split_once("</summary>").map_or(rest, |(s, _)| s)
    } else {
        text
    };
    let text = text.trim();
    if text.is_empty() {
        "（无可用摘要）".into()
    } else {
        text.into()
    }
}
/// Keep head and tail, measured with the same estimator, including the marker.
pub fn token_excerpt(text: &str, budget: usize) -> String {
    if estimate_tokens(text) <= budget {
        return text.into();
    }
    let chars: Vec<_> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let n = (lo + hi).div_ceil(2);
        let s = format!(
            "{}\n…\n{}",
            chars[..n / 2].iter().collect::<String>(),
            chars[chars.len() - (n - n / 2)..]
                .iter()
                .collect::<String>()
        );
        if estimate_tokens(&s) <= budget {
            lo = n;
        } else {
            hi = n - 1;
        }
    }
    if budget < 4 {
        return String::new();
    }
    format!(
        "{}\n…\n{}",
        chars[..lo / 2].iter().collect::<String>(),
        chars[chars.len() - (lo - lo / 2)..]
            .iter()
            .collect::<String>()
    )
}
pub(crate) fn char_excerpt(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.into();
    }
    let half = limit.saturating_sub(3) / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(half)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n…\n{tail}")
}
/// Only Resume owns these native assistant messages; they cannot reach persistence.
pub(crate) type NativeMessages = BTreeMap<usize, AssistantMessage>;

impl Conversation {
    pub fn prepare(&self, capabilities: &Capabilities) -> Result<CompletionRequest, AiError> {
        self.request(
            capabilities,
            &NativeMessages::new(),
            false,
            self.view_start(),
        )
    }
    pub(crate) fn request(
        &self,
        caps: &Capabilities,
        native: &NativeMessages,
        summarizing: bool,
        start: usize,
    ) -> Result<CompletionRequest, AiError> {
        let newest = self.latest_user().ok_or(AiError::InvalidConfig)?;
        if newest.question.trim().is_empty() {
            return Err(AiError::InvalidConfig);
        }
        let system = if caps.available() {
            crate::conversation::execution_prompt(&newest.context, caps)
        } else {
            crate::conversation::system_prompt(&newest.context)
        };
        let tool = if caps.available() {
            crate::conversation::run_command_tool(caps)
        } else {
            crate::conversation::command_tool()
        };
        let offered = if summarizing {
            &[][..]
        } else if caps.available() {
            &["run_command", "read_output", "stop_task"][..]
        } else {
            &["propose_command"][..]
        };
        let mut messages = Vec::new();
        let entries = &self.entries()[start..];
        let mut i = 0;
        while i < entries.len() {
            if let Some(group) = entries[i].message_id() {
                let end = i + entries[i..]
                    .iter()
                    .take_while(|e| e.message_id() == Some(group))
                    .count();
                let batch = &entries[i..end];
                let mut content = Vec::new();
                let mut results = Vec::new();
                let mut calls = BTreeMap::new();
                for e in batch {
                    match e {
                        Entry::Assistant { text, .. } if !text.is_empty() => {
                            if let Some(AssistantContent::Text(previous)) = content.last_mut() {
                                previous.text.push_str(text);
                            } else {
                                content.push(AssistantContent::text(text));
                            }
                        }
                        Entry::CommandCard { card, .. } => {
                            content.push(AssistantContent::text(AnswerCard(card).text()))
                        }
                        Entry::Step(s) => {
                            let mut result_text = s.result_text();
                            if summarizing {
                                result_text = char_excerpt(&result_text, 4000);
                            }
                            if offered.contains(&s.call.name.as_str()) {
                                let call = ToolCall::from_wire(
                                    &s.call.id,
                                    ToolFunction::new(
                                        ToolName::new(&s.call.name)
                                            .map_err(|_| AiError::InvalidResponse)?,
                                        s.call.args_json.clone(),
                                    ),
                                );
                                let mut result =
                                    call.result(vec![ToolResultContent::text(result_text)]);
                                result.is_error = s.result.is_error;
                                results.push(result);
                                calls.insert(s.call.id.clone(), call.clone());
                                content.push(AssistantContent::ToolCall(call));
                            } else {
                                let text = if summarizing {
                                    format!(
                                        "工具 {}（{}）\n{result_text}",
                                        s.call.name, s.call.args_json
                                    )
                                } else {
                                    s.plain_text()
                                };
                                content.push(AssistantContent::text(text));
                            }
                        }
                        _ => {}
                    }
                }
                let assistant = if !summarizing && let Some(original) = native.get(&group) {
                    // Keep reasoning's native payload (including signatures) verbatim.
                    // Completed calls only: a pause may split a multi-call response.
                    let mut a = original.clone();
                    a.content.retain(|c| !matches!(c, AssistantContent::ToolCall(call) if !calls.contains_key(&call.id.to_string())));
                    // If a tool is no longer offered, its pair is ordinary text.
                    for e in batch {
                        if let Entry::Step(s) = e
                            && !offered.contains(&s.call.name.as_str())
                        {
                            a.content.push(AssistantContent::text(s.plain_text()));
                        }
                    }
                    a
                } else {
                    AssistantMessage::new(content)
                };
                if !assistant.content.is_empty() {
                    push_message(&mut messages, Message::Assistant(assistant));
                }
                if !results.is_empty() {
                    push_message(&mut messages, Message::tool_results(results));
                }
                i = end;
                continue;
            }
            match &entries[i] {
                Entry::User { text, context } => push_message(
                    &mut messages,
                    Message::user(crate::UserTurn::new(text, context.clone()).wire_text()),
                ),
                Entry::Compaction(c) if c.summary.is_some() => {
                    push_message(
                        &mut messages,
                        Message::user(format!(
                            "{SUMMARY_PREFIX}\n{}",
                            c.summary.as_deref().unwrap()
                        )),
                    );
                    for text in &c.retained_user {
                        push_message(&mut messages, Message::user(text));
                    }
                    push_message(
                        &mut messages,
                        Message::user(format!(
                            "工具的当前状态：{}",
                            serde_json::to_string(&c.state).unwrap()
                        )),
                    );
                    push_message(
                        &mut messages,
                        Message::user(
                            crate::UserTurn::new("最新终端上下文", c.context.clone()).wire_text(),
                        ),
                    );
                }
                Entry::Notice { text, .. } => push_message(&mut messages, Message::user(text)),
                _ => {}
            }
            i += 1;
        }
        if summarizing && matches!(messages.first(), Some(Message::Assistant(_))) {
            messages.insert(0, Message::user("此前上下文已省略，请依据以下记录继续。"));
        }
        if messages.is_empty() {
            messages.push(Message::user("请继续。"));
        }
        let prompt = messages.pop().unwrap();
        let mut request = CompletionRequest::new(prompt)
            .messages(messages)
            .preamble(system);
        if !summarizing {
            request = request.tool(tool);
            if caps.available() {
                for tool in crate::conversation::output_tools() {
                    request = request.tool(tool);
                }
            }
        }
        Ok(request)
    }
    pub fn breakdown(
        &self,
        client: &crate::RigClient,
        caps: &Capabilities,
    ) -> Result<ContextBreakdown, AiError> {
        self.breakdown_for_budget(client.context_budget(), caps)
    }
    pub fn breakdown_for_budget(
        &self,
        budget: ContextBudget,
        caps: &Capabilities,
    ) -> Result<ContextBreakdown, AiError> {
        self.breakdown_with_limits(budget, caps, crate::agent::ExecLimits::default())
    }
    pub fn breakdown_with_limits(
        &self,
        budget: ContextBudget,
        caps: &Capabilities,
        limits: crate::agent::ExecLimits,
    ) -> Result<ContextBreakdown, AiError> {
        let narrowed = caps
            .clone()
            .for_question(&self.latest_user().ok_or(AiError::InvalidConfig)?.question);
        let caps = &narrowed;
        let mut request = self.prepare(caps)?;
        if request
            .chat_history
            .last()
            .is_some_and(|m| matches!(&**m, Message::Assistant(_)))
        {
            add_prompt(&mut request, "请继续。".into());
        }
        if let Some(tool) = request
            .tools
            .iter_mut()
            .find(|t| t.name.as_str() == "run_command")
        {
            *tool = Arc::new(crate::conversation::run_command_tool_with_limits(
                caps, limits,
            ));
        }
        let tool_definitions = request
            .tools
            .iter()
            .map(|t| estimate_tokens(&serde_json::to_string(t).unwrap()))
            .sum::<usize>();
        let system_prompt = system_tokens(&request) - tool_definitions;
        let mut summary = 0;
        let mut terminal_context = 0;
        let mut tool_results = 0;
        for entry in &self.entries()[self.view_start()..] {
            match entry {
                Entry::User { text, context } => {
                    // Count this snapshot once. Assign the question and protocol
                    // message overhead to Messages, without re-counting the wire.
                    let wire = crate::UserTurn::new(text, context.clone()).wire_text();
                    terminal_context += estimate_tokens(&wire)
                        .saturating_sub(estimate_tokens(&format!("问题：{text}")));
                }
                Entry::Compaction(c) if c.summary.is_some() => {
                    summary += estimate_tokens(&format!(
                        "{SUMMARY_PREFIX}\n{}",
                        c.summary.as_deref().unwrap()
                    ));
                    summary += c
                        .retained_user
                        .iter()
                        .map(|text| estimate_tokens(text))
                        .sum::<usize>();
                    summary += estimate_tokens(&format!(
                        "工具的当前状态：{}",
                        serde_json::to_string(&c.state).unwrap()
                    ));
                    terminal_context += estimate_tokens(
                        &crate::UserTurn::new("最新终端上下文", c.context.clone()).wire_text(),
                    );
                }
                Entry::Step(s) => {
                    let offered = request.tools.iter().any(|t| t.name.as_str() == s.call.name);
                    tool_results += if offered {
                        estimate_tokens(&s.call.name)
                            + estimate_tokens(&s.call.args_json.to_string())
                            + estimate_tokens(&s.result_text())
                    } else {
                        estimate_tokens(&s.plain_text())
                    };
                }
                _ => {}
            }
        }
        let estimated = request_tokens(&request);
        let provider_usage = self
            .accounting
            .usage
            .is_some_and(|(_, pos)| pos >= self.view_start());
        let mut breakdown = ContextBreakdown {
            system_prompt,
            tool_definitions,
            tool_count: request.tools.len(),
            terminal_context,
            summary,
            tool_results,
            messages: estimated.saturating_sub(
                system_prompt + tool_definitions + terminal_context + summary + tool_results,
            ),
            total: estimated,
            provider_usage,
            budget,
        };
        if provider_usage {
            breakdown.scale_to(self.used(&request, caps));
        }
        Ok(breakdown)
    }
    pub(crate) fn used(&self, request: &CompletionRequest, caps: &Capabilities) -> usize {
        match self.accounting.usage {
            Some((tokens, pos)) if pos >= self.view_start() => {
                let estimate = if pos < self.entries().len() {
                    self.request(caps, &NativeMessages::new(), false, pos)
                        .map(|r| request_tokens(&r).saturating_sub(system_tokens(&r)))
                        .unwrap_or(0)
                } else {
                    0
                };
                tokens.saturating_add(estimate)
            }
            _ => request_tokens(request),
        }
    }
    pub(crate) fn retained_users(&self) -> Vec<String> {
        let mut remaining = 20_000;
        let mut messages = Vec::new();
        for entry in self.entries().iter().rev() {
            if let Entry::User { text, .. } = entry {
                if remaining <= 8 {
                    break;
                }
                let text = token_excerpt(text, remaining - 8);
                remaining = remaining.saturating_sub(estimate_tokens(&text) + 8);
                messages.push(text);
            }
        }
        messages
    }
    pub(crate) fn retained_state(&self, permission_mode: PermissionMode) -> RetainedState {
        let mut state = RetainedState {
            permission_mode,
            ..Default::default()
        };
        for entry in self.entries() {
            if let Entry::Step(s) = entry
                && let Some(d) = &s.display
                && let crate::StepStatus::Denied { reason } = &d.status
            {
                let denied = crate::DeniedCommand {
                    command: d.command.clone(),
                    reason: reason.clone(),
                };
                if !state.denied_commands.contains(&denied) {
                    state.denied_commands.push(denied);
                }
            }
        }
        state
    }
}
struct AnswerCard<'a>(&'a crate::CommandCard);
impl AnswerCard<'_> {
    fn text(&self) -> String {
        crate::AnswerItem::Command(self.0.clone()).wire_text()
    }
}
pub(crate) fn push_message(messages: &mut Vec<Message>, message: Message) {
    // Anthropic requires tool_result blocks first in the user message immediately
    // after their tool_use. Merge adjacent users without crossing assistant turns.
    if let Message::User { content } = &message
        && let Some(Message::User { content: previous }) = messages.last_mut()
    {
        previous.extend(content.iter().cloned());
        previous.sort_by_key(|c| !matches!(c, UserContent::ToolResult(_)));
        return;
    }
    if let Message::Assistant(current) = &message
        && let Some(Message::Assistant(previous)) = messages.last_mut()
        && current.origin.is_none()
        && previous.origin.is_none()
    {
        previous.content.extend(current.content.iter().cloned());
        return;
    }
    messages.push(message);
}
pub(crate) fn request_tokens(request: &CompletionRequest) -> usize {
    system_tokens(request)
        + request
            .chat_history
            .iter()
            .filter(|m| !matches!(***m, Message::System { .. }))
            .map(|m| message_tokens(m))
            .sum::<usize>()
}
fn message_tokens(message: &Message) -> usize {
    8 + match message {
        Message::System { content } => estimate_tokens(content),
        Message::User { content } => content
            .iter()
            .map(|c| match c {
                UserContent::Text(t) => estimate_tokens(&t.text),
                UserContent::ToolResult(r) => r
                    .content
                    .iter()
                    .map(|c| {
                        c.as_text().map_or_else(
                            || estimate_tokens(&serde_json::to_string(c).unwrap()),
                            estimate_tokens,
                        )
                    })
                    .sum(),
                _ => estimate_tokens(&serde_json::to_string(c).unwrap()),
            })
            .sum(),
        Message::Assistant(a) => a
            .content
            .iter()
            .map(|c| match c {
                AssistantContent::Text(t) => estimate_tokens(&t.text),
                AssistantContent::ToolCall(c) => {
                    estimate_tokens(c.function.name.as_str())
                        + estimate_tokens(&serde_json::to_string(&c.function.arguments).unwrap())
                }
                AssistantContent::Reasoning(r) => estimate_tokens(&r.text),
                _ => estimate_tokens(&serde_json::to_string(c).unwrap()),
            })
            .sum(),
    }
}
fn system_tokens(request: &CompletionRequest) -> usize {
    request
        .chat_history
        .iter()
        .filter_map(|m| {
            if let Message::System { content } = &**m {
                Some(estimate_tokens(content) + 8)
            } else {
                None
            }
        })
        .sum::<usize>()
        + request
            .tools
            .iter()
            .map(|t| estimate_tokens(&serde_json::to_string(t).unwrap()))
            .sum::<usize>()
}
pub(crate) fn add_prompt(request: &mut CompletionRequest, text: String) {
    let mut messages = request
        .chat_history
        .iter()
        .map(|m| (**m).clone())
        .collect::<Vec<_>>();
    push_message(&mut messages, Message::user(text));
    request.chat_history = messages.into_iter().map(Arc::new).collect();
}
/// Normalize provider/legacy ids once, before logging, so both wires share identity.
pub(crate) fn portable_id(id: &str, position: usize, used: &mut HashSet<String>) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && used.insert(id.into())
    {
        return id.into();
    }
    let mut n = 0;
    loop {
        let id = format!("call_{position}_{n}");
        if used.insert(id.clone()) {
            return id;
        }
        n += 1;
    }
}
#[derive(Default)]
pub(crate) struct ThrashGuard {
    last: Option<usize>,
    consecutive: usize,
}
impl ThrashGuard {
    pub fn record(&mut self, model_steps: usize, trigger: CompactTrigger) -> Result<(), AiError> {
        if trigger == CompactTrigger::Manual {
            return Ok(());
        }
        self.consecutive = if self
            .last
            .is_some_and(|last| model_steps.saturating_sub(last) < 2)
        {
            self.consecutive + 1
        } else {
            1
        };
        self.last = Some(model_steps);
        if self.consecutive >= 3 {
            Err(AiError::CompactionThrash)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApiKey, Config, ContextSnapshot, LogUpdate, ReasoningLevel, Timeouts, Usage, UserTurn,
    };
    fn client() -> crate::RigClient {
        crate::RigClient::with_http(
            &Config {
                provider: Provider::default(),
                model: "unknown".into(),
                api_key: ApiKey::new("fixture"),
                reasoning: ReasoningLevel::Default,
                max_output_tokens: Default::default(),
                context_window: ContextWindow::Auto,
                autocompact_window: ContextWindow::Auto,
                timeouts: Timeouts::default(),
            },
            rig_core::test_utils::SequencedHttpClient::new([]),
        )
        .unwrap()
    }
    #[test]
    fn window_resolution_and_exact_thresholds() {
        let p = Provider::default();
        assert_eq!(
            resolve_window(&p, "unknown", ContextWindow::Tokens(256_000)),
            (256_000, WindowSource::Setting)
        );
        assert_eq!(
            resolve_window(&p, "unknown", ContextWindow::Auto),
            (128_000, WindowSource::Fallback)
        );
        assert_eq!(
            resolve_window(&p, "unknown", ContextWindow::Tokens(0)),
            (128_000, WindowSource::Fallback)
        );
        for (provider, model) in [
            (p.clone(), "gpt-4o"),
            (
                Provider::Anthropic {
                    base_url: "https://a.test".into(),
                },
                "claude-sonnet-4-6",
            ),
            (p.clone(), "claude-sonnet-4-6"),
            (
                Provider::Anthropic {
                    base_url: "https://a.test".into(),
                },
                "gpt-4o",
            ),
        ] {
            let spec = catalog_spec(&provider, model).unwrap();
            assert_eq!(
                resolve_window(&provider, model, ContextWindow::Auto),
                (
                    spec.max_input_tokens.or(spec.context_window).unwrap(),
                    WindowSource::Catalog
                )
            );
        }
        for (w, t) in [(128_000, 95_000), (200_000, 167_000), (1_000_000, 967_000)] {
            assert_eq!(
                ContextBudget::new(w, ContextWindow::Auto, 32_000),
                ContextBudget {
                    window: w,
                    auto: w,
                    reserve: 33_000,
                    trigger: t
                }
            );
        }
        assert_eq!(
            ContextBudget::new(1_000_000, ContextWindow::Tokens(200_000), 32_000).trigger,
            167_000
        );
        assert_eq!(
            ContextBudget::new(200_000, ContextWindow::Tokens(1), 32_000).auto,
            100_000
        );
        assert_eq!(
            ContextBudget::new(50_000, ContextWindow::Tokens(1), 32_000).auto,
            50_000
        );
        assert_eq!(
            ContextBudget::new(128_000, ContextWindow::Tokens(u32::MAX), 32_000).auto,
            128_000
        );
        assert_eq!(
            ContextBudget::new(128_000, ContextWindow::Auto, 1024).trigger,
            125_976
        );
    }
    #[test]
    fn accounting_uses_last_usage_plus_append_cost_and_all_request_overheads() {
        let client = client();
        let caps = Capabilities::default();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("问题", ContextSnapshot::default()));
        let request = log.prepare(&caps).unwrap();
        let b = log.breakdown(&client, &caps).unwrap();
        assert_eq!(b.total, request_tokens(&request));
        assert_eq!(
            b.categories().iter().map(|(_, n)| n).sum::<usize>(),
            b.total
        );
        assert!(
            b.system_prompt > 100,
            "system text counts separately from tools"
        );
        log.apply(LogUpdate::Usage(Usage {
            input_tokens: Some(1000),
            output_tokens: Some(100),
        }));
        assert_eq!(log.breakdown(&client, &caps).unwrap().total, 1100);
        log.push_text("0123456789中");
        assert_eq!(log.breakdown(&client, &caps).unwrap().total, 1100 + 4 + 8);
        log.apply(LogUpdate::Usage(Usage::default()));
        assert_eq!(log.breakdown(&client, &caps).unwrap().total, 1112);
        log.apply(LogUpdate::Usage(Usage {
            input_tokens: Some(2000),
            output_tokens: None,
        }));
        assert_eq!(log.breakdown(&client, &caps).unwrap().total, 2000);
    }
    #[test]
    fn retained_users_are_newest_first_and_fit_20k_including_message_overhead() {
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("old", ContextSnapshot::default()));
        log.push_user(UserTurn::new(
            &format!("HEAD{}TAIL", "中".repeat(30_000)),
            ContextSnapshot::default(),
        ));
        let retained = log.retained_users();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].starts_with("HEAD") && retained[0].ends_with("TAIL"));
        assert!(
            retained
                .iter()
                .map(|s| estimate_tokens(s) + 8)
                .sum::<usize>()
                <= 20_000
        );
        log.push_user(UserTurn::new("newest", ContextSnapshot::default()));
        assert_eq!(log.retained_users()[0], "newest");
    }
    #[test]
    fn extraction_prefix_and_thrash_guard_follow_the_contract() {
        assert_eq!(
            extract_summary("<analysis>private</analysis><summary>  handoff  </summary>ignored"),
            "handoff"
        );
        assert_eq!(extract_summary("fallback"), "fallback");
        assert_eq!(extract_summary("<summary> </summary>"), "（无可用摘要）");
        assert_eq!(extract_summary("  "), "（无可用摘要）");
        assert_eq!(
            SUMMARY_PREFIX,
            "本会话延续自之前的对话，此前的上下文已压缩。以下是之前对话的摘要。你仍可看到工具的当前状态；请在已完成的工作基础上继续，避免重复。"
        );
        let mut g = ThrashGuard::default();
        assert_eq!(g.record(0, CompactTrigger::PreRequest), Ok(()));
        assert_eq!(g.record(1, CompactTrigger::Reactive), Ok(()));
        assert_eq!(g.record(1, CompactTrigger::Manual), Ok(()));
        assert_eq!(
            g.record(2, CompactTrigger::PreRequest),
            Err(AiError::CompactionThrash)
        );
        assert_eq!(g.record(4, CompactTrigger::PreRequest), Ok(()));
        assert_eq!(
            AiError::CompactionThrash.to_string(),
            "上下文刚压缩就又满了（可能是单条输出过大）。可以用 /clear 开始新对话，或调大上下文窗口"
        );
    }
    #[test]
    fn portable_ids_never_collide_even_when_providers_reuse_them() {
        let mut seen = HashSet::new();
        assert_eq!(portable_id("good-id_1", 0, &mut seen), "good-id_1");
        for id in ["good-id_1", "bad:id", "中文", ""] {
            let id = portable_id(id, 3, &mut seen);
            assert!(id.starts_with("call_3_"));
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
        }
        assert_eq!(seen.len(), 5);
    }
}

#[cfg(test)]
#[path = "context_breakdown_tests.rs"]
mod breakdown_tests;
