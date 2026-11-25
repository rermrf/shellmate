use crate::AiError;
use rig_core::serde::{Serialize, Deserialize};
use std::{fmt, time::Duration};
use zeroize::Zeroizing;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompatible { base_url: String },
    Anthropic { base_url: String },
}
impl Default for Provider {
    fn default() -> Self {
        Self::OpenAiCompatible {
            base_url: "https://api.openai.com/v1".into(),
        }
    }
}
impl Provider {
    pub fn id(&self) -> &'static str {
        match self {
            Self::OpenAiCompatible { .. } => "openai",
            Self::Anthropic { .. } => "anthropic",
        }
    }
    pub fn base_url(&self) -> &str {
        match self {
            Self::OpenAiCompatible { base_url } | Self::Anthropic { base_url } => base_url,
        }
    }
    pub fn destination(&self) -> Result<String, AiError> {
        let uri: Uri = self
            .base_url()
            .trim_end_matches('/')
            .parse()
            .map_err(|_| AiError::InvalidConfig)?;
        if !matches!(uri.scheme_str(), Some("https" | "http"))
            || uri.query().is_some()
            || uri.host().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || self.base_url().contains('#')
        {
            return Err(AiError::InvalidConfig);
        }
        Ok(format!(
            "{}://{}",
            uri.scheme_str().unwrap(),
            uri.authority().unwrap().as_str().to_ascii_lowercase()
        ))
    }
}
type Uri = rig_core::http_client::Uri;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "lowercase")]
pub enum ReasoningLevel {
    #[default]
    Default,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
impl ReasoningLevel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
    pub fn from_id(id: &str) -> Self {
        match id {
            "off" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => Self::Default,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "默认",
            Self::Off => "关闭",
            Self::Minimal => "最低",
            Self::Low => "低",
            Self::Medium => "中",
            Self::High => "高",
            Self::XHigh => "超高",
            Self::Max => "最高",
        }
    }
    pub fn options(provider: &Provider) -> &'static [Self] {
        use ReasoningLevel::*;
        match provider {
            Provider::OpenAiCompatible { .. } => &[Default, Off, Minimal, Low, Medium, High, XHigh, Max],
            Provider::Anthropic { .. } => &[Default, Off, Low, Medium, High, Max],
        }
    }
    pub fn for_provider(self, provider: &Provider) -> Self {
        if Self::options(provider).contains(&self) {
            self
        } else {
            Self::Default
        }
    }
    pub fn timeouts(self) -> Timeouts {
        Timeouts::default()
    }
}
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);
impl ApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}
impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub first: Duration,
    pub idle: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(600),
            idle: Duration::from_secs(300),
        }
    }
}
/// Output allowance, independent of reasoning effort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputTokens {
    #[default]
    Auto,
    Tokens(u32),
}
impl OutputTokens {
    pub fn resolve(self, provider: &Provider, model: &str) -> u32 {
        let maximum = catalog_spec(provider, model).and_then(|spec| spec.max_output_tokens);
        let requested = match self {
            Self::Auto => 32_000,
            Self::Tokens(n) => n.clamp(1024, 128_000),
        };
        maximum.map_or(requested, |max| requested.min(max))
    }
}
/// Vendor lookup (including dated aliases), then exact cross-vendor ids for proxies.
pub fn catalog_spec(provider: &Provider, model: &str) -> Option<&'static rig_core::catalog::ModelSpec> {
    static FACTS: std::sync::OnceLock<rig_core::catalog::ModelFacts> = std::sync::OnceLock::new();
    let facts = FACTS.get_or_init(Default::default);
    facts.for_model(provider.id(), model).or_else(|| {
        rig_core::catalog::Catalog::builtin().iter().find(|spec| spec.id == model)
    })
}
#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub api_key: ApiKey,
    pub reasoning: ReasoningLevel,
    pub max_output_tokens: OutputTokens,
    pub context_window: crate::ContextWindow,
    pub autocompact_window: crate::ContextWindow,
    pub timeouts: Timeouts,
}
impl Config {
    /// Overrides identity/effort only. The caller must resolve the effective model's
    /// context/output settings before constructing its RigClient (M2 store integration).
    pub fn with_override(&self, model: Option<&str>, reasoning: Option<ReasoningLevel>) -> Self {
        let mut config = self.clone();
        if let Some(model) = model { config.model = model.trim().into(); }
        config.reasoning = reasoning.unwrap_or(self.reasoning).for_provider(&config.provider);
        config
    }

    pub fn validate(&self) -> Result<(), AiError> {
        if self.model.trim().is_empty() || self.api_key.is_empty() {
            return Err(AiError::NotConfigured);
        }
        self.provider.destination()?;
        if self.model.chars().count() > 256
            || self.model.chars().any(char::is_control)
            || self.timeouts.first.is_zero()
            || self.timeouts.idle.is_zero()
        {
            return Err(AiError::InvalidConfig);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasoning_ids_options_and_timeouts_are_stable() {
        use ReasoningLevel::*;
        let openai = Provider::default();
        let anthropic = Provider::Anthropic {
            base_url: "https://api.anthropic.com".into(),
        };
        assert_eq!(
            ReasoningLevel::options(&openai),
            &[Default, Off, Minimal, Low, Medium, High, XHigh, Max]
        );
        assert_eq!(
            ReasoningLevel::options(&anthropic),
            &[Default, Off, Low, Medium, High, Max]
        );
        assert_eq!(XHigh.for_provider(&anthropic), Default);
        assert_eq!(Minimal.for_provider(&anthropic), Default);
        assert_eq!(Max.for_provider(&openai), Max);
        assert_eq!(ReasoningLevel::from_id("future-level"), Default);
        for (level, id, label) in [
            (Default, "default", "默认"),
            (Off, "off", "关闭"),
            (Minimal, "minimal", "最低"),
            (Low, "low", "低"),
            (Medium, "medium", "中"),
            (High, "high", "高"),
            (XHigh, "xhigh", "超高"),
            (Max, "max", "最高"),
        ] {
            assert_eq!(level.id(), id);
            assert_eq!(ReasoningLevel::from_id(id), level);
            assert_eq!(level.label(), label);
            let timeouts = level.timeouts();
            assert_eq!(timeouts.first, Duration::from_secs(600));
            assert_eq!(timeouts.idle, Duration::from_secs(300));
        }
    }
    #[test]
    fn destination_validation_is_shared_by_both_protocols() {
        for base in [
            "not-a-url",
            "ftp://host.test",
            "https://user@host.test",
            "https://host.test?key=hidden",
            "https://host.test/#fragment",
        ] {
            for provider in [
                Provider::OpenAiCompatible {
                    base_url: base.into(),
                },
                Provider::Anthropic {
                    base_url: base.into(),
                },
            ] {
                assert_eq!(provider.destination(), Err(AiError::InvalidConfig));
            }
        }
        let provider = Provider::Anthropic {
            base_url: "https://PROXY.example:8443/api/v1/messages/".into(),
        };
        assert_eq!(
            provider.destination().unwrap(),
            "https://proxy.example:8443"
        );
    }
}
