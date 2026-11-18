use crate::AiError;
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    pub fn max_tokens(self) -> u64 {
        match self {
            Self::Default | Self::Off => 2048,
            Self::Minimal | Self::Low => 4096,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::XHigh | Self::Max => 32000,
        }
    }
    pub fn timeouts(self) -> Timeouts {
        let first = match self {
            Self::Default | Self::Off | Self::Minimal | Self::Low => 45,
            Self::Medium => 90,
            Self::High => 150,
            Self::XHigh | Self::Max => 240,
        };
        Timeouts {
            first: Duration::from_secs(first),
            idle: Duration::from_secs(30),
            total: Duration::from_secs(180.max(first + 150)),
        }
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
    pub total: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        ReasoningLevel::Default.timeouts()
    }
}
#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub api_key: ApiKey,
    pub reasoning: ReasoningLevel,
    pub timeouts: Timeouts,
}
impl Config {
    pub fn validate(&self) -> Result<(), AiError> {
        if self.model.trim().is_empty() || self.api_key.is_empty() {
            return Err(AiError::NotConfigured);
        }
        self.provider.destination()?;
        if self.model.len() > 256
            || self.model.chars().any(char::is_control)
            || self.timeouts.first.is_zero()
            || self.timeouts.idle.is_zero()
            || self.timeouts.total.is_zero()
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
        for (level, id, label, first, total) in [
            (Default, "default", "默认", 45, 195),
            (Off, "off", "关闭", 45, 195),
            (Minimal, "minimal", "最低", 45, 195),
            (Low, "low", "低", 45, 195),
            (Medium, "medium", "中", 90, 240),
            (High, "high", "高", 150, 300),
            (XHigh, "xhigh", "超高", 240, 390),
            (Max, "max", "最高", 240, 390),
        ] {
            assert_eq!(level.id(), id);
            assert_eq!(ReasoningLevel::from_id(id), level);
            assert_eq!(level.label(), label);
            let timeouts = level.timeouts();
            assert_eq!(timeouts.first, Duration::from_secs(first));
            assert_eq!(timeouts.idle, Duration::from_secs(30));
            assert_eq!(timeouts.total, Duration::from_secs(total));
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
