use crate::AiError;
use std::{fmt, time::Duration};
use zeroize::Zeroizing;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompatible { base_url: String },
    Anthropic,
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
            Self::Anthropic => "anthropic",
        }
    }
    pub fn base_url(&self) -> &str {
        match self {
            Self::OpenAiCompatible { base_url } => base_url,
            Self::Anthropic => "https://api.anthropic.com",
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
#[derive(Clone, Debug)]
pub struct Timeouts {
    pub first: Duration,
    pub idle: Duration,
    pub total: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(45),
            idle: Duration::from_secs(30),
            total: Duration::from_secs(180),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub api_key: ApiKey,
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
