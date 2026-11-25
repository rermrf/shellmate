use crate::{AiError, CommandCard, Config, Conversation, Provider, ReasoningLevel, Risk, Timeouts};
use futures::{Stream, StreamExt};
use rig_core::serde::{Deserialize, Serialize};
use rig_core::{
    DynModel,
    completion::{CacheRetention, CompletionRequest, Effort, Reasoning},
    http_client::HttpClientExt,
    message::AssistantContent,
    operation::Completion,
    providers::{anthropic::AnthropicConfig, openai::OpenAIConfig},
    streaming::{CompletionStream, Item, PartKind, StreamEvent},
};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::{Context, Poll},
};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
impl Usage {
    pub(crate) fn add(&mut self, other: Self) {
        fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            }
        }
        self.input_tokens = sum(self.input_tokens, other.input_tokens);
        self.output_tokens = sum(self.output_tokens, other.output_tokens);
    }
}
pub enum Event {
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
    TextDelta(String),
    Command(CommandCard),
    Done(Option<Usage>),
    Error(AiError),
}
pub trait ProviderClient: Send + Sync {
    fn start<'a>(
        &'a self,
        conversation: &'a mut Conversation,
    ) -> Result<ResponseStream<'a>, AiError>;
}
#[derive(Clone)]
pub struct RigClient {
    pub(crate) config: Config,
    model: Arc<DynModel<Completion>>,
    timeouts: Timeouts,
    reasoning: Option<Reasoning>,
    options: Arc<ClientOptions>,
    window: u32,
    auto_window: crate::ContextWindow,
}
struct ClientOptions {
    output_tokens: AtomicU32,
    cache: AtomicBool,
}
impl RigClient {
    pub fn new(config: &Config) -> Result<Self, AiError> {
        config.validate()?;
        // Rig's Secret is Debug-redacted but owns a non-zeroizing String.
        // Keep only a placeholder there; our zeroizing credential reaches the
        // auth header at the transport boundary and is never retained by a wire.
        let http = OpenAIConfig::new("shellmate-placeholder")
            .client()
            .chat("transport-only")
            .transport;
        Self::with_http(config, http)
    }
    pub fn with_http(config: &Config, http: impl HttpClientExt + 'static) -> Result<Self, AiError> {
        config.validate()?;
        let http = rig_core::http_client::DynHttpClient::new(http).with_middleware(AuthHeader {
            key: config.api_key.clone(),
            anthropic: matches!(config.provider, Provider::Anthropic { .. }),
        });
        let model = match &config.provider {
            Provider::OpenAiCompatible { base_url } => OpenAIConfig::new("shellmate-placeholder")
                .with_base_url(base_url.trim_end_matches('/'))
                .connect(http)
                .chat(&config.model)
                // Shellmate times first event and idle separately; rig otherwise
                // imposes a 300 s first-frame deadline as well.
                .with_idle_timeout(None)
                .erase(),
            Provider::Anthropic { base_url } => AnthropicConfig::new("shellmate-placeholder")
                .with_base_url(base_url)
                .connect(http)
                .completion(&config.model)
                .with_idle_timeout(None)
                .erase(),
        };
        let (reasoning, max_tokens) = reasoning_options(config)?;
        Ok(Self {
            config: config.clone(),
            model: Arc::new(model),
            timeouts: config.timeouts.clone(),
            reasoning,
            window: crate::resolve_window(&config.provider, &config.model, config.context_window).0,
            auto_window: config.autocompact_window,
            options: Arc::new(ClientOptions {
                output_tokens: AtomicU32::new(max_tokens),
                cache: AtomicBool::new(matches!(config.provider, Provider::Anthropic { .. })),
            }),
        })
    }
    pub(crate) fn agent_open(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, Box<rig_core::ProviderError>> {
        let output = self.output_tokens();
        let mut request = request.max_tokens(u64::from(output));
        request.options.reasoning = self.reasoning;
        request.options.cache = self
            .options
            .cache
            .load(Ordering::Relaxed)
            .then_some(CacheRetention::Short);
        self.model.stream(request).map_err(Box::new)
    }
    /// Effective per-call allowance, including budget thinking and learned fallbacks.
    pub fn output_tokens(&self) -> u32 {
        self.options.output_tokens.load(Ordering::Relaxed)
    }
    pub fn context_budget(&self) -> crate::ContextBudget {
        crate::ContextBudget::new(self.window, self.auto_window, self.output_tokens())
    }
    /// Before content, try specific option fixes before a one-shot cache-free 400 retry.
    pub(crate) fn option_fallback(&self, error: &rig_core::ProviderError) -> bool {
        if cache_refusal(error) && self.options.cache.swap(false, Ordering::Relaxed) {
            return true;
        }
        if option_refusal(
            error,
            &["max_tokens", "max_completion_tokens", "max_output_tokens"],
        ) {
            let current = self.output_tokens();
            if current > 4096 {
                let next = if current > 8192 { 8192 } else { 4096 };
                self.options
                    .output_tokens
                    .fetch_min(next, Ordering::Relaxed);
                return true;
            }
        }
        !is_context_error(error)
            && error.report().http_status == Some(400)
            && self.options.cache.swap(false, Ordering::Relaxed)
    }
    fn open_with_fallback(&self, request: &CompletionRequest) -> Result<CompletionStream, AiError> {
        loop {
            match self.agent_open(request.clone()) {
                Ok(stream) => return Ok(stream),
                Err(error) if self.option_fallback(&error) => {}
                Err(error) => return Err(map_error(*error)),
            }
        }
    }
    pub(crate) fn timeouts(&self) -> Timeouts {
        self.timeouts.clone()
    }
    pub fn test_connection(&self) -> Result<ResponseStream<'static>, AiError> {
        let request = CompletionRequest::new("请仅回答：连接成功");
        self.open(request)
    }
    fn open(&self, request: CompletionRequest) -> Result<ResponseStream<'static>, AiError> {
        let stream = self.open_with_fallback(&request)?;
        Ok(ResponseStream::new(
            stream,
            self.timeouts.clone(),
            Some((self.clone(), request)),
        ))
    }
}
impl ResponseStream<'_> {
    pub fn take_updates(&mut self) -> Vec<crate::LogUpdate> {
        std::mem::take(&mut *self.updates.lock().unwrap())
    }
}
impl RigClient {
    /// Independent context-only side question. No executable tools are sent and the
    /// supplied conversation is immutable, including its usage/accounting state.
    pub async fn side_question(
        &self,
        conversation: &Conversation,
        question: &str,
    ) -> Result<(String, Usage), AiError> {
        let mut request = conversation.prepare(&Default::default())?;
        request.tools.clear();
        crate::context::add_prompt(
            &mut request,
            format!(
                "这是独立侧问，只根据已有对话回答，不执行命令或调用工具，不改变主任务。问题：{question}"
            ),
        );
        let mut usage = Usage::default();
        let answer = self.summarize(request, |u| usage.add(u)).await?;
        Ok((answer, usage))
    }
    pub(crate) async fn summarize(
        &self,
        request: CompletionRequest,
        mut on_usage: impl FnMut(Usage),
    ) -> Result<String, AiError> {
        loop {
            let mut request = request
                .clone()
                .max_tokens(u64::from(self.output_tokens().min(16_000)));
            request.options.reasoning = None;
            request.options.cache = self
                .options
                .cache
                .load(Ordering::Relaxed)
                .then_some(CacheRetention::Short);
            let mut stream = match self.model.stream(request) {
                Ok(s) => s,
                Err(e) if self.option_fallback(&e) => continue,
                Err(e) => return Err(map_error(e)),
            };
            let mut first = true;
            let mut received = false;
            let mut retry = false;
            loop {
                match tokio::time::timeout(
                    if first {
                        self.timeouts.first
                    } else {
                        self.timeouts.idle
                    },
                    stream.next(),
                )
                .await
                {
                    Err(_) => return Err(AiError::Timeout),
                    Ok(Some(Err(e))) if !received && self.option_fallback(&e) => {
                        retry = true;
                        break;
                    }
                    Ok(Some(Err(e))) => return Err(map_error(e)),
                    Ok(Some(Ok(Item::Event(_)))) => {
                        first = false;
                        received = true;
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(None) => break,
                }
            }
            if retry {
                continue;
            }
            let response = match tokio::time::timeout(self.timeouts.idle, stream.finish()).await {
                Err(_) => return Err(AiError::Timeout),
                Ok(Err(e)) if !received && self.option_fallback(&e) => continue,
                Ok(Err(e)) => return Err(map_error(e)),
                Ok(Ok(r)) => r,
            };
            on_usage(Usage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            });
            if response.error.is_some() || response.aborted.is_some() {
                return Err(AiError::InvalidResponse);
            }
            return Ok(response
                .choice
                .iter()
                .filter_map(|c| {
                    if let AssistantContent::Text(t) = c {
                        Some(t.text.as_str())
                    } else {
                        None
                    }
                })
                .collect());
        }
    }
}
fn reasoning_options(config: &Config) -> Result<(Option<Reasoning>, u32), AiError> {
    let output = config
        .max_output_tokens
        .resolve(&config.provider, &config.model);
    let level = config.reasoning;
    let effort = match level {
        ReasoningLevel::Default => return Ok((None, output)),
        ReasoningLevel::Off => return Ok((Some(Reasoning::Off), output)),
        ReasoningLevel::Minimal => Effort::Minimal,
        ReasoningLevel::Low => Effort::Low,
        ReasoningLevel::Medium => Effort::Medium,
        ReasoningLevel::High => Effort::High,
        ReasoningLevel::XHigh => Effort::XHigh,
        ReasoningLevel::Max => Effort::Max,
    };
    // Use the same built-in facts and dated-model lookup as the Messages wire.
    if matches!(config.provider, Provider::Anthropic { .. })
        && let Some(spec) = crate::catalog_spec(&config.provider, &config.model)
        && spec.reasoning.levels().is_some_and(<[Effort]>::is_empty)
        && let Some(range) = spec.reasoning.budget()
    {
        let tokens = match level {
            ReasoningLevel::Low => 2048_u32,
            ReasoningLevel::Medium => 6144,
            ReasoningLevel::High => 14336,
            ReasoningLevel::Max => 30000,
            _ => return Err(AiError::UnsupportedReasoning),
        };
        let min = (*range.start()).max(1024);
        let max = *range.end();
        if min > max {
            return Err(AiError::UnsupportedReasoning);
        }
        let tokens = tokens.clamp(min, max);
        return Ok((
            Some(Reasoning::Budget { tokens }),
            output.max(tokens + 4096),
        ));
    }
    Ok((Some(Reasoning::Effort(effort)), output))
}
struct AuthHeader {
    key: crate::ApiKey,
    anthropic: bool,
}
impl rig_core::http_client::HttpMiddleware for AuthHeader {
    fn before_request_headers<'a>(
        &'a self,
        _: rig_core::http_client::Exchange<'a>,
        headers: &'a mut rig_core::http_client::HeaderMap,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, rig_core::http_client::Result<()>> {
        Box::pin(async move {
            let value = zeroize::Zeroizing::new(if self.anthropic {
                self.key.expose().to_owned()
            } else {
                format!("Bearer {}", self.key.expose())
            });
            let mut header = rig_core::http_client::HeaderValue::from_str(&value)?;
            header.set_sensitive(true);
            headers.insert(
                if self.anthropic {
                    "x-api-key"
                } else {
                    "authorization"
                },
                header,
            );
            Ok(())
        })
    }
}
impl ProviderClient for RigClient {
    fn start<'a>(&'a self, c: &'a mut Conversation) -> Result<ResponseStream<'a>, AiError> {
        // Validate provider encoding synchronously; streams are lazy and no I/O occurs.
        drop(self.open_with_fallback(&c.prepare(&crate::Capabilities::default())?)?);
        let mut stream = crate::agent::run(
            self,
            c,
            crate::Capabilities::default(),
            &SuggestionHost,
            crate::Limits::default(),
        )?;
        let updates = stream.updates_handle();
        let inner =
            futures::stream::poll_fn(move |cx| Pin::new(&mut stream).poll_next(cx)).map(|event| {
                match event {
                    crate::AgentEvent::ToolStarted(_) | crate::AgentEvent::ToolFinished(_) => {
                        Event::TextDelta(String::new())
                    }
                    crate::AgentEvent::InputsAccepted(_)
                    | crate::AgentEvent::CompletionsAccepted(_) => Event::TextDelta(String::new()),
                    crate::AgentEvent::Thinking => Event::Thinking,
                    crate::AgentEvent::TextDelta(t) => Event::TextDelta(t),
                    crate::AgentEvent::Command(c) => Event::Command(c),
                    crate::AgentEvent::Done(u) => Event::Done(u),
                    crate::AgentEvent::Released(_) => Event::Done(None),
                    crate::AgentEvent::Error(e) => Event::Error(e),
                    crate::AgentEvent::Context {
                        used,
                        window,
                        trigger,
                    } => Event::Context {
                        used,
                        window,
                        trigger,
                    },
                    crate::AgentEvent::Compacting { tier } => Event::Compacting { tier },
                    crate::AgentEvent::Compacted {
                        before,
                        after,
                        summary,
                    } => Event::Compacted {
                        before,
                        after,
                        summary,
                    },
                    crate::AgentEvent::Paused { reason, .. } => Event::Error(match reason {
                        crate::PauseReason::Error(e) => e,
                        _ => AiError::InvalidResponse,
                    }),
                    crate::AgentEvent::Retrying { .. } => Event::Thinking,
                    crate::AgentEvent::StepProposed(_) | crate::AgentEvent::StepFinished { .. } => {
                        unreachable!("suggestion host has no executable tools")
                    }
                }
            });
        Ok(ResponseStream {
            inner: Box::pin(inner),
            updates,
        })
    }
}
struct SuggestionHost;
impl crate::ToolHost for SuggestionHost {
    fn run(&self, _: crate::ToolCall) -> futures::future::BoxFuture<'_, crate::ToolOutcome> {
        Box::pin(async { crate::ToolOutcome::Failed("此会话不支持执行".into()) })
    }
}
pub struct ResponseStream<'a> {
    inner: Pin<Box<dyn Stream<Item = Event> + Send + 'a>>,
    updates: Arc<std::sync::Mutex<Vec<crate::LogUpdate>>>,
}
impl Stream for ResponseStream<'_> {
    type Item = Event;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.inner.as_mut().poll_next(cx)
    }
}
struct Reading {
    stream: Option<CompletionStream>,
    text: String,
    cards: usize,
    first: bool,
    thinking: bool,
    fallback: Option<(RigClient, CompletionRequest)>,
    received: bool,
    timeouts: Timeouts,
    queue: VecDeque<Event>,
}
impl Reading {
    fn retry_option(&mut self, error: &rig_core::ProviderError) -> bool {
        if self.received {
            return false;
        }
        let Some((client, request)) = &self.fallback else {
            return false;
        };
        if !client.option_fallback(error) {
            return false;
        }
        self.stream = None;
        self.first = true;
        match client.open_with_fallback(request) {
            Ok(stream) => self.stream = Some(stream),
            Err(error) => self.queue.push_back(Event::Error(error)),
        }
        true
    }
}
impl ResponseStream<'static> {
    fn new(
        stream: CompletionStream,
        timeouts: Timeouts,
        fallback: Option<(RigClient, CompletionRequest)>,
    ) -> Self {
        let state = Reading {
            stream: Some(stream),
            text: String::new(),
            cards: 0,
            first: true,
            thinking: false,
            fallback,
            received: false,
            timeouts,
            queue: VecDeque::new(),
        };
        let stream = futures::stream::unfold(state, |mut s| async move {
            loop {
                if let Some(event) = s.queue.pop_front() {
                    return Some((event, s));
                }
                let stream = s.stream.as_mut()?;
                let wait = if s.first {
                    s.timeouts.first
                } else {
                    s.timeouts.idle
                };
                let next = tokio::time::timeout(wait, stream.next()).await;
                match next {
                    Err(_) => {
                        s.stream = None;
                        return Some((Event::Error(AiError::Timeout), s));
                    }
                    Ok(Some(Err(e))) => {
                        if s.retry_option(&e) {
                            continue;
                        }
                        s.stream = None;
                        return Some((Event::Error(map_error(e)), s));
                    }
                    Ok(Some(Ok(Item::Event(
                        StreamEvent::Reasoning { .. }
                        | StreamEvent::Start {
                            kind: PartKind::Reasoning,
                            ..
                        }
                        | StreamEvent::End {
                            content: AssistantContent::Reasoning(_),
                            ..
                        },
                    )))) => {
                        s.first = false;
                        s.received = true;
                        // Discard all reasoning payloads; only expose progress before answer text.
                        if !s.thinking && s.text.is_empty() {
                            s.thinking = true;
                            return Some((Event::Thinking, s));
                        }
                    }
                    Ok(Some(Ok(Item::Event(StreamEvent::Text { text, .. })))) => {
                        s.first = false;
                        s.received = true;
                        s.text.push_str(&text);
                        return Some((Event::TextDelta(text), s));
                    }
                    Ok(Some(Ok(Item::Event(StreamEvent::End {
                        content: AssistantContent::ToolCall(call),
                        ..
                    })))) => {
                        s.first = false;
                        s.received = true;
                        if call.function.name.as_str() == "propose_command" {
                            let card = serde_json::from_value::<CommandCard>(
                                serde_json::Value::Object(call.function.arguments),
                            )
                            .ok()
                            .filter(|c| {
                                !c.command.trim().is_empty()
                                    && c.command.len() <= 16384
                                    && c.explanation.len() <= 8192
                            });
                            match card {
                                Some(card) if s.cards < 20 => {
                                    s.cards += 1;
                                    return Some((Event::Command(card), s));
                                }
                                _ => {
                                    s.stream = None;
                                    return Some((Event::Error(AiError::InvalidResponse), s));
                                }
                            }
                        }
                    }
                    Ok(Some(Ok(item))) => {
                        s.first = false;
                        s.received |= matches!(item, Item::Event(_));
                    }
                    Ok(None) => {
                        let stream = s.stream.take().unwrap();
                        let result = tokio::time::timeout(s.timeouts.idle, stream.finish()).await;
                        match result {
                            Ok(Ok(response)) => {
                                if s.cards == 0 {
                                    for card in fenced_commands(&s.text) {
                                        s.queue.push_back(Event::Command(card));
                                    }
                                }
                                let u = response.usage;
                                let usage = (u.input_tokens.is_some() || u.output_tokens.is_some())
                                    .then_some(Usage {
                                        input_tokens: u.input_tokens,
                                        output_tokens: u.output_tokens,
                                    });
                                s.queue.push_back(Event::Done(usage));
                            }
                            Ok(Err(e)) if s.retry_option(&e) => continue,
                            Ok(Err(e)) => s.queue.push_back(Event::Error(map_error(e))),
                            Err(_) => s.queue.push_back(Event::Error(AiError::Timeout)),
                        }
                    }
                }
            }
        });
        Self {
            inner: Box::pin(stream),
            updates: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
    /// Deterministic UI fixtures use the same event boundary as live providers.
    pub fn fixture(events: Vec<Event>) -> Self {
        Self {
            inner: Box::pin(futures::stream::iter(events)),
            updates: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}
pub fn fenced_commands(text: &str) -> Vec<CommandCard> {
    let mut cards = Vec::new();
    let mut block = None;
    let mut code = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(lang) = trimmed.strip_prefix("```") {
            if let Some(allowed) = block.take() {
                if allowed && !code.trim().is_empty() && code.len() <= 16384 && cards.len() < 20 {
                    cards.push(CommandCard {
                        command: code.trim().into(),
                        explanation: "来自回答中的代码块；请先检查再执行".into(),
                        risk: Risk::Medium,
                    });
                }
                code.clear();
            } else {
                block = Some(matches!(
                    lang.trim().to_ascii_lowercase().as_str(),
                    "" | "bash" | "sh" | "shell" | "zsh"
                ));
            }
        } else if block == Some(true) {
            code.push_str(line);
            code.push('\n');
        }
    }
    cards
}
fn cache_refusal(error: &rig_core::ProviderError) -> bool {
    match error {
        rig_core::ProviderError::UnsupportedOption(option) => {
            matches!(option.option.as_ref(), "cache" | "cache_control")
        }
        _ => option_refusal(error, &["cache_control"]),
    }
}
/// Match complete option names, never substrings of unrelated fields.
pub(crate) fn option_refusal(error: &rig_core::ProviderError, fields: &[&str]) -> bool {
    if let rig_core::ProviderError::UnsupportedOption(option) = error {
        return fields.contains(&option.option.as_ref());
    }
    let report = error.report();
    report.http_status == Some(400)
        && report.provider_response_body().is_some_and(|body| {
            body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|word| fields.contains(&word))
        })
}
fn is_context_error(error: &rig_core::ProviderError) -> bool {
    let report = error.report();
    let code = report.code.as_deref().unwrap_or("").to_ascii_lowercase();
    let body = report
        .provider_response_body()
        .unwrap_or("")
        .to_ascii_lowercase();
    code.contains("context_length")
        || [
            "context_length_exceeded",
            "prompt is too long",
            "maximum context length",
            "context window",
            "too many tokens",
            "input is too long",
        ]
        .iter()
        .any(|s| body.contains(s))
}
pub(crate) fn map_error(error: rig_core::ProviderError) -> AiError {
    // Never format or forward the provider's diagnostic: it may echo the key or prompt.
    if matches!(error, rig_core::ProviderError::UnsupportedOption(_)) {
        return AiError::UnsupportedReasoning;
    }
    let report = error.report();
    let code = report.code.as_deref().unwrap_or("").to_ascii_lowercase();
    let body = report
        .provider_response_body()
        .unwrap_or("")
        .to_ascii_lowercase();
    if is_context_error(&error) {
        return AiError::ContextTooLong;
    }
    if report.http_status == Some(400)
        && [
            "reasoning_effort",
            "thinking",
            "budget_tokens",
            "output_config",
        ]
        .iter()
        .any(|field| body.contains(field))
    {
        return AiError::UnsupportedReasoning;
    }
    if code.contains("insufficient_quota")
        || body.contains("insufficient_quota")
        || body.contains("insufficient balance")
        || body.contains("credit balance is too low")
        || report.http_status == Some(402)
    {
        return AiError::Quota;
    }
    if code.contains("context_length")
        || body.contains("context_length_exceeded")
        || body.contains("prompt is too long")
        || body.contains("maximum context length")
    {
        return AiError::ContextTooLong;
    }
    if code.contains("model_not_found") || body.contains("model_not_found") {
        return AiError::ModelNotFound;
    }
    match report.http_status {
        Some(401 | 403) => AiError::Authentication,
        Some(404) => AiError::ModelNotFound,
        Some(429) => AiError::RateLimited,
        Some(500..=599) => AiError::Unavailable,
        _ if report.retryable && code == "rate_limit_error" => AiError::RateLimited,
        _ if report.retryable && matches!(code.as_str(), "overloaded_error" | "api_error") => {
            AiError::Unavailable
        }
        _ => match report.kind {
            rig_core::ErrorKind::Timeout => AiError::Timeout,
            rig_core::ErrorKind::Http => AiError::Network,
            _ if matches!(error, rig_core::ProviderError::Stalled(_)) => AiError::Timeout,
            _ => AiError::InvalidResponse,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::{
        completion::UnsupportedOption,
        driver::{Exchange, Model, Opening, Transport},
        test_utils::{MockCompletionModel, MockFrame, MockScript, MockStreamEvent},
    };

    #[test]
    fn typed_option_rejections_are_scoped_and_cache_disables_once() {
        let config = Config {
            provider: Provider::Anthropic {
                base_url: "https://api.anthropic.com".into(),
            },
            model: "claude-sonnet-4-6".into(),
            api_key: crate::ApiKey::new("fixture"),
            reasoning: ReasoningLevel::Default,
            max_output_tokens: Default::default(),
            context_window: Default::default(),
            autocompact_window: Default::default(),
            timeouts: Timeouts::default(),
        };
        let client =
            RigClient::with_http(&config, rig_core::test_utils::SequencedHttpClient::new([]))
                .unwrap();
        let unsupported = |name: &'static str| {
            rig_core::ProviderError::UnsupportedOption(UnsupportedOption::new(
                name,
                "fixture",
                "model",
                "private diagnostic",
            ))
        };
        assert!(!client.option_fallback(&unsupported("reasoning")));
        assert!(!client.option_fallback(&unsupported("other_max_tokens")));
        assert!(client.option_fallback(&unsupported("cache")));
        assert!(!client.option_fallback(&unsupported("cache")));
        assert!(client.option_fallback(&unsupported("max_tokens")));
        assert_eq!(client.output_tokens(), 8192);
        assert!(client.option_fallback(&unsupported("max_output_tokens")));
        assert_eq!(client.output_tokens(), 4096);
        assert!(!client.option_fallback(&unsupported("max_completion_tokens")));
    }

    #[tokio::test]
    async fn streamed_unsupported_option_is_sanitized() {
        #[derive(Clone)]
        struct Refused;
        impl Transport<MockScript> for Refused {
            fn send(&self, _: CompletionRequest, _: Exchange) -> Opening<MockFrame> {
                Opening::failed(rig_core::ProviderError::UnsupportedOption(
                    UnsupportedOption::new("reasoning", "fixture", "model", "PRIVATE diagnostic"),
                ))
            }
        }
        let stream = Model::new(MockScript::new("fixture"), Refused)
            .stream(CompletionRequest::new("test"))
            .unwrap();
        let mut stream = ResponseStream::new(stream, Timeouts::default(), None);
        assert!(matches!(
            stream.next().await,
            Some(Event::Error(AiError::UnsupportedReasoning))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn empty_and_multiple_reasoning_blocks_signal_only_before_text() {
        for late in [false, true] {
            let mut events = vec![];
            if late {
                events.push(MockStreamEvent::text("answer"));
            }
            events.extend([
                MockStreamEvent::Reasoning {
                    id: "one".into(),
                    text: String::new(),
                },
                MockStreamEvent::Reasoning {
                    id: "two".into(),
                    text: "PRIVATE".into(),
                },
            ]);
            if !late {
                events.push(MockStreamEvent::text("answer"));
            }
            events.push(MockStreamEvent::FinalResponse(Default::default()));
            let model = MockCompletionModel::from_stream_turns([events]);
            let stream = model.stream(CompletionRequest::new("test")).unwrap();
            let events = ResponseStream::new(stream, Timeouts::default(), None)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, Event::Thinking))
                    .count(),
                usize::from(!late)
            );
            assert!(matches!(events.last(), Some(Event::Done(_))));
            let text: String = events
                .into_iter()
                .filter_map(|e| match e {
                    Event::TextDelta(text) => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "answer");
        }
    }
}
