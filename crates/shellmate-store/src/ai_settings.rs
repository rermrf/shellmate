//! Validation and storage shared by the form and run-start snapshot.
use super::Settings;
use crate::{HostStore, Result};
use rusqlite::{Transaction, params};
use shellmate_ai::{ContextBudget, ContextWindow, OutputTokens, resolve_window};

fn number(value: Option<String>, min: u64, max: u64) -> Option<u64> {
    value?.parse().ok().filter(|n| (min..=max).contains(n))
}
fn window(value: Option<String>) -> ContextWindow {
    number(value, 8_000, 10_000_000)
        .map_or(ContextWindow::Auto, |n| ContextWindow::Tokens(n as u32))
}
fn auto_window(value: Option<String>) -> ContextWindow {
    number(value, 1, u32::MAX.into())
        .map_or(ContextWindow::Auto, |n| ContextWindow::Tokens(n as u32))
}
fn window_text(value: ContextWindow) -> String {
    match value {
        ContextWindow::Auto => "auto".into(),
        ContextWindow::Tokens(n) => n.to_string(),
    }
}
fn key(prefix: &str, s: &Settings) -> String {
    format!("{prefix}:{}:{}", s.provider.id(), s.model.trim())
}
impl Settings {
    pub fn normalized(&self) -> Self {
        let mut s = self.clone();
        s.subagent_limit = s.subagent_limit.clamp(1, 8);
        s.context_window = window(Some(window_text(s.context_window)));
        let w = resolve_window(&s.provider, &s.model, s.context_window).0;
        s.autocompact_window = match auto_window(Some(window_text(s.autocompact_window))) {
            ContextWindow::Auto => ContextWindow::Auto,
            ContextWindow::Tokens(n) => ContextWindow::Tokens(n.clamp(w.min(100_000), w)),
        };
        if matches!(s.max_output_tokens, OutputTokens::Tokens(n) if !(1024..=128_000).contains(&n))
        {
            s.max_output_tokens = OutputTokens::Auto;
        }
        s.max_steps = s.max_steps.filter(|n| (1..=10_000).contains(n));
        s.exec.tool_output_chars = s.exec.output_chars();
        if !(1..=600).contains(&s.exec.max_timeout_secs) {
            s.exec.max_timeout_secs = 600;
        }
        if !(1..=600).contains(&s.exec.default_timeout_secs) {
            s.exec.default_timeout_secs = 120;
        }
        s.exec.default_timeout_secs = s.exec.default_timeout_secs.min(s.exec.max_timeout_secs);
        s
    }
    pub fn context_budget(&self) -> ContextBudget {
        let s = self.normalized();
        ContextBudget::new(
            resolve_window(&s.provider, &s.model, s.context_window).0,
            s.autocompact_window,
            s.max_output_tokens.resolve(&s.provider, &s.model),
        )
    }
    pub fn limits(&self) -> shellmate_ai::agent::Limits {
        let s = self.normalized();
        shellmate_ai::agent::Limits {
            auto_memory: s.auto_memory,
            max_steps: s.max_steps,
            exec: s.exec,
            ..Default::default()
        }
    }
}
impl HostStore {
    /// Snapshot effective per-model budgets without changing provider, URL, key or global limits.
    pub fn subagent_config(
        &self,
        main: &shellmate_ai::Config,
        request: &shellmate_ai::SubagentRequest,
    ) -> Result<shellmate_ai::Config> {
        let mut config = main.with_override(request.model.as_deref(), request.reasoning);
        let s = self.ai_model_settings(&Settings {
            provider: config.provider.clone(),
            model: config.model.clone(),
            ..Settings::default()
        })?;
        config.context_window = s.context_window;
        config.autocompact_window = s.autocompact_window;
        config.max_output_tokens = s.max_output_tokens;
        Ok(config)
    }

    pub fn set_auto_memory(&mut self, enabled: bool) -> Result<()> {
        self.set_setting("ai_auto_memory", if enabled { "true" } else { "false" })
    }

    /// Reload model-specific values after editing the model/provider; globals stay in the form.
    pub fn ai_model_settings(&self, settings: &Settings) -> Result<Settings> {
        let mut s = settings.clone();
        s.context_window = window(self.setting(&key("ai_context_window", &s))?);
        s.autocompact_window = auto_window(self.setting(&key("ai_autocompact_window", &s))?);
        s.max_output_tokens = number(self.setting(&key("ai_output_tokens", &s))?, 1024, 128_000)
            .map_or(OutputTokens::Auto, |n| OutputTokens::Tokens(n as u32));
        Ok(s.normalized())
    }
    pub(super) fn read_ai_limits(&self, s: &mut Settings) -> Result<()> {
        s.auto_memory = self.setting("ai_auto_memory")?.as_deref() != Some("false");
        s.subagent_limit = self
            .setting("ai_subagent_limit")?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(3)
            .clamp(1, 8) as usize;
        s.max_steps = number(self.setting("ai_max_steps")?, 1, 10_000).map(|n| n as usize);
        s.exec.tool_output_chars =
            number(self.setting("ai_tool_output_chars")?, 1000, 150_000).unwrap_or(30_000) as usize;
        s.exec.default_timeout_secs =
            number(self.setting("ai_cmd_timeout_default")?, 1, 600).unwrap_or(120);
        s.exec.max_timeout_secs =
            number(self.setting("ai_cmd_timeout_max")?, 1, 600).unwrap_or(600);
        *s = self.ai_model_settings(s)?;
        Ok(())
    }
}
pub(super) fn save_limits(tx: &Transaction<'_>, s: &Settings) -> Result<()> {
    let s = s.normalized();
    for (key, value) in [
        ("ai_auto_memory".into(), s.auto_memory.to_string()),
        ("ai_subagent_limit".into(), s.subagent_limit.to_string()),
        (key("ai_context_window", &s), window_text(s.context_window)),
        (
            key("ai_autocompact_window", &s),
            window_text(s.autocompact_window),
        ),
        (
            key("ai_output_tokens", &s),
            match s.max_output_tokens {
                OutputTokens::Auto => "auto".into(),
                OutputTokens::Tokens(n) => n.to_string(),
            },
        ),
        (
            "ai_max_steps".into(),
            s.max_steps
                .map_or_else(|| "unlimited".into(), |n| n.to_string()),
        ),
        (
            "ai_tool_output_chars".into(),
            s.exec.tool_output_chars.to_string(),
        ),
        (
            "ai_cmd_timeout_default".into(),
            s.exec.default_timeout_secs.to_string(),
        ),
        (
            "ai_cmd_timeout_max".into(),
            s.exec.max_timeout_secs.to_string(),
        ),
    ] {
        tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_models_providers_globals_and_auto() {
        let mut db = HostStore::open_in_memory().unwrap();
        let s = Settings {
            model: "fixture".into(),
            context_window: ContextWindow::Tokens(200_000),
            autocompact_window: ContextWindow::Tokens(150_000),
            max_output_tokens: OutputTokens::Tokens(8192),
            max_steps: Some(50),
            exec: shellmate_ai::agent::ExecLimits {
                tool_output_chars: 42000,
                default_timeout_secs: 60,
                max_timeout_secs: 300,
            },
            ..Default::default()
        };
        db.save_ai_settings(&s).unwrap();
        assert_eq!(db.ai_settings().unwrap(), s);
        let other = Settings {
            model: "other".into(),
            ..s.clone()
        };
        let defaults = db.ai_model_settings(&other).unwrap();
        assert_eq!(defaults.context_window, ContextWindow::Auto);
        db.save_ai_settings(&Settings {
            context_window: ContextWindow::Tokens(400000),
            ..other.clone()
        })
        .unwrap();
        assert_eq!(db.ai_model_settings(&s).unwrap(), s);
        assert_eq!(
            db.ai_model_settings(&other).unwrap().context_window,
            ContextWindow::Tokens(400000)
        );
        let anthropic = Settings {
            provider: shellmate_ai::Provider::Anthropic {
                base_url: "https://api.anthropic.com".into(),
            },
            ..s.clone()
        };
        db.save_ai_settings(&Settings {
            context_window: ContextWindow::Auto,
            ..anthropic.clone()
        })
        .unwrap();
        assert_eq!(db.ai_model_settings(&s).unwrap(), s);
        assert_eq!(
            db.ai_settings_for_provider(shellmate_ai::ApiProtocol::Anthropic).unwrap().context_window,
            ContextWindow::Auto
        );
        assert_eq!(db.ai_settings().unwrap().max_steps, Some(50));
        assert_eq!(
            db.setting("ai_context_window:anthropic:fixture")
                .unwrap()
                .as_deref(),
            Some("auto")
        );
    }
    #[test]
    fn invalid_values_fall_back_and_valid_values_clamp() {
        let mut db = HostStore::open_in_memory().unwrap();
        for (k, v) in [
            ("ai_context_window:openai:", "7999"),
            ("ai_autocompact_window:openai:", "unknown"),
            ("ai_output_tokens:openai:", "128001"),
            ("ai_max_steps", "0"),
            ("ai_tool_output_chars", "150001"),
            ("ai_cmd_timeout_default", "bad"),
            ("ai_cmd_timeout_max", "601"),
        ] {
            db.set_setting(k, v).unwrap();
        }
        assert_eq!(db.ai_settings().unwrap(), Settings::default());
        for (w, a, expected) in [
            (8000, 10000, 8000),
            (50000, 8000, 50000),
            (200000, 1000, 100000),
            (200000, 900000, 200000),
        ] {
            let s = Settings {
                context_window: ContextWindow::Tokens(w),
                autocompact_window: ContextWindow::Tokens(a),
                ..Default::default()
            }
            .normalized();
            assert_eq!(s.autocompact_window, ContextWindow::Tokens(expected));
        }
        let mut s = Settings::default();
        s.exec.default_timeout_secs = 500;
        s.exec.max_timeout_secs = 30;
        db.save_ai_settings(&s).unwrap();
        let s = db.ai_settings().unwrap();
        assert_eq!(s.exec.default_timeout_secs, 30);
        assert_eq!(s.exec.max_timeout_secs, 30);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    #[test]
    fn auto_memory_defaults_invalid_values_and_global_roundtrip() {
        let mut store = HostStore::open_in_memory().unwrap();
        assert!(store.ai_settings().unwrap().auto_memory);
        for value in ["garbage", "0", "", "TRUE", "true"] {
            store.set_setting("ai_auto_memory", value).unwrap();
            assert!(store.ai_settings().unwrap().auto_memory);
        }
        let settings = Settings {
            auto_memory: false,
            ..Default::default()
        };
        store.save_ai_settings(&settings).unwrap();
        assert!(!store.ai_settings().unwrap().auto_memory);
        assert!(!store.ai_settings().unwrap().limits().auto_memory);
        assert!(!store.ai_settings_for_provider(shellmate_ai::ApiProtocol::Anthropic).unwrap().auto_memory);
        store.save_ai_settings(&Settings::default()).unwrap();
        assert!(store.ai_settings().unwrap().auto_memory);
    }
}

#[cfg(test)]
mod subagent_tests {
    use super::*;
    #[test]
    fn global_limit_clamps_and_model_override_uses_its_stored_budgets() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert_eq!(db.ai_settings().unwrap().subagent_limit, 3);
        for (value, expected) in [("bad", 3), ("0", 1), ("-7", 1), ("99", 8), ("5", 5)] {
            db.set_setting("ai_subagent_limit", value).unwrap();
            assert_eq!(db.ai_settings().unwrap().subagent_limit, expected);
        }
        let settings = Settings {
            model: "child".into(),
            context_window: ContextWindow::Tokens(400000),
            autocompact_window: ContextWindow::Tokens(200000),
            max_output_tokens: OutputTokens::Tokens(8192),
            subagent_limit: 6,
            ..Default::default()
        };
        db.save_ai_settings(&settings).unwrap();
        assert_eq!(db.ai_settings().unwrap().subagent_limit, 6);
        let main = shellmate_ai::Config {
            provider: settings.provider.clone(),
            model: "parent".into(),
            api_key: shellmate_ai::ApiKey::new("fixture"),
            reasoning: Default::default(),
            max_output_tokens: Default::default(),
            context_window: ContextWindow::Tokens(128000),
            autocompact_window: Default::default(),
            timeouts: Default::default(),
        };
        let request = shellmate_ai::SubagentRequest {
            session_ids: vec![],
            default_session_id: None,
            description: "test".into(),
            prompt: "test".into(),
            kind: shellmate_ai::SubagentKind::Explore,
            background: false,
            model: Some("child".into()),
            reasoning: Some(shellmate_ai::ReasoningLevel::High),
        };
        let effective = db.subagent_config(&main, &request).unwrap();
        assert_eq!(effective.model, "child");
        assert_eq!(effective.context_window, settings.context_window);
        assert_eq!(effective.autocompact_window, settings.autocompact_window);
        assert_eq!(effective.max_output_tokens, settings.max_output_tokens);
        assert_eq!(effective.api_key.expose(), "fixture");
        assert_eq!(effective.provider, main.provider);
    }
}
