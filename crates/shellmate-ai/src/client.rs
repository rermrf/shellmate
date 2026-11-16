use crate::{AiError, CommandCard, Config, Conversation, DEFAULT_BUDGET, Provider, Risk, Timeouts};
use futures::{Stream, StreamExt};
use rig_core::{
    DynModel,
    completion::CompletionRequest,
    http_client::HttpClientExt,
    message::AssistantContent,
    operation::Completion,
    providers::{anthropic::AnthropicConfig, openai::OpenAIConfig},
    streaming::{CompletionStream, Item, StreamEvent},
};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
pub enum Event {
    TextDelta(String),
    Command(CommandCard),
    Done(Option<Usage>),
    Error(AiError),
}
pub trait ProviderClient: Send + Sync {
    fn start(&self, conversation: &Conversation) -> Result<ResponseStream, AiError>;
}
pub struct RigClient {
    model: DynModel<Completion>,
    timeouts: Timeouts,
    pub character_budget: usize,
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
            anthropic: config.provider == Provider::Anthropic,
        });
        let model = match &config.provider {
            Provider::OpenAiCompatible { base_url } => OpenAIConfig::new("shellmate-placeholder")
                .with_base_url(base_url.trim_end_matches('/'))
                .connect(http)
                .chat(&config.model)
                .erase(),
            Provider::Anthropic => AnthropicConfig::new("shellmate-placeholder")
                .connect(http)
                .completion(&config.model)
                .erase(),
        };
        Ok(Self {
            model,
            timeouts: config.timeouts.clone(),
            character_budget: DEFAULT_BUDGET,
        })
    }
    pub fn test_connection(&self) -> Result<ResponseStream, AiError> {
        let request = CompletionRequest::new("请仅回答：连接成功").max_tokens(16);
        self.open(request, 0)
    }
    fn open(&self, request: CompletionRequest, dropped: usize) -> Result<ResponseStream, AiError> {
        let stream = self.model.stream(request).map_err(map_error)?;
        Ok(ResponseStream::new(stream, self.timeouts.clone(), dropped))
    }
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
    fn start(&self, c: &Conversation) -> Result<ResponseStream, AiError> {
        let p = c.prepare(self.character_budget)?;
        self.open(p.request, p.dropped_turns)
    }
}
pub struct ResponseStream {
    inner: Pin<Box<dyn Stream<Item = Event> + Send>>,
    pub dropped_turns: usize,
}
impl Stream for ResponseStream {
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
    start: Instant,
    timeouts: Timeouts,
    queue: VecDeque<Event>,
}
impl ResponseStream {
    fn new(stream: CompletionStream, timeouts: Timeouts, dropped_turns: usize) -> Self {
        let state = Reading {
            stream: Some(stream),
            text: String::new(),
            cards: 0,
            first: true,
            start: Instant::now(),
            timeouts,
            queue: VecDeque::new(),
        };
        let stream = futures::stream::unfold(state, |mut s| async move {
            loop {
                if let Some(event) = s.queue.pop_front() {
                    return Some((event, s));
                }
                let stream = s.stream.as_mut()?;
                let remaining = s.timeouts.total.saturating_sub(s.start.elapsed());
                let wait = remaining.min(if s.first {
                    s.timeouts.first
                } else {
                    s.timeouts.idle
                });
                let next = tokio::time::timeout(wait, stream.next()).await;
                match next {
                    Err(_) => {
                        s.stream = None;
                        return Some((Event::Error(AiError::Timeout), s));
                    }
                    Ok(Some(Err(e))) => {
                        s.stream = None;
                        return Some((Event::Error(map_error(e)), s));
                    }
                    Ok(Some(Ok(Item::Event(StreamEvent::Text { text, .. })))) => {
                        s.first = false;
                        if s.text.len() + text.len() > 256 * 1024 {
                            s.stream = None;
                            return Some((Event::Error(AiError::ContextTooLong), s));
                        }
                        s.text.push_str(&text);
                        return Some((Event::TextDelta(text), s));
                    }
                    Ok(Some(Ok(Item::Event(StreamEvent::End {
                        content: AssistantContent::ToolCall(call),
                        ..
                    })))) => {
                        s.first = false;
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
                    Ok(Some(Ok(_))) => s.first = false,
                    Ok(None) => {
                        let stream = s.stream.take().unwrap();
                        let result = tokio::time::timeout(remaining, stream.finish()).await;
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
                            Ok(Err(e)) => s.queue.push_back(Event::Error(map_error(e))),
                            Err(_) => s.queue.push_back(Event::Error(AiError::Timeout)),
                        }
                    }
                }
            }
        });
        Self {
            inner: Box::pin(stream),
            dropped_turns,
        }
    }
    /// Deterministic UI fixtures use the same event boundary as live providers.
    pub fn fixture(events: Vec<Event>) -> Self {
        Self {
            inner: Box::pin(futures::stream::iter(events)),
            dropped_turns: 0,
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
fn map_error(error: rig_core::ProviderError) -> AiError {
    // Never format or forward the provider's diagnostic: it may echo the key or prompt.
    let report = error.report();
    let code = report.code.as_deref().unwrap_or("").to_ascii_lowercase();
    let body = report
        .provider_response_body()
        .unwrap_or("")
        .to_ascii_lowercase();
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
        _ => match report.kind {
            rig_core::ErrorKind::Timeout => AiError::Timeout,
            rig_core::ErrorKind::Http => AiError::Network,
            _ if matches!(error, rig_core::ProviderError::Stalled(_)) => AiError::Timeout,
            _ => AiError::InvalidResponse,
        },
    }
}
