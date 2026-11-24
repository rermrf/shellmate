//! gpui-free policies for the drawer, disclosure, retries and settings.
use shellmate_ai::{AiError, Conversation, Event, PauseReason};
#[cfg(any(test, debug_assertions))]
use shellmate_ai::{ContextSnapshot, UserTurn};
use shellmate_store::{SecretKey, SecretStore, ai::Settings};
pub const SUGGESTIONS: [&str; 4] = [
    "解释最近的报错",
    "找出占用 3000 端口的进程",
    "统计最近 1 小时的 5xx",
    "清理 7 天前的日志归档",
];
pub fn error_line(screen: &str) -> Option<String> {
    screen
        .lines()
        .rev()
        .find(|line| {
            let s = line.to_ascii_lowercase();
            [
                "[error]",
                "failed",
                "refused",
                "broken pipe",
                "fatal",
                "esp_err",
            ]
            .iter()
            .any(|p| s.contains(p))
        })
        .map(str::to_owned)
}
pub fn load_config(
    settings: &Settings,
    secrets: &dyn SecretStore,
) -> Result<shellmate_ai::Config, AiError> {
    if !settings.configured() {
        return Err(AiError::NotConfigured);
    }
    if secrets.is_locked() {
        return Err(AiError::Cancelled);
    }
    let secret = secrets
        .get(&SecretKey::Account(settings.key_account()))
        .map_err(|_| AiError::Authentication)?
        .ok_or(AiError::NotConfigured)?;
    let config = shellmate_ai::Config {
        provider: settings.provider.clone(),
        model: settings.model.clone(),
        api_key: shellmate_ai::ApiKey::new(secret.expose()),
        reasoning: settings.reasoning,
        max_output_tokens: Default::default(),
        context_window: Default::default(),
        autocompact_window: Default::default(),
        timeouts: settings.reasoning.timeouts(),
    };
    config.validate()?;
    Ok(config)
}
pub fn authorized_config(
    settings: &Settings,
    secrets: &dyn SecretStore,
    unlock_accepted: bool,
) -> Result<shellmate_ai::Config, AiError> {
    if !unlock_accepted {
        return Err(AiError::Cancelled);
    }
    load_config(settings, secrets)
}
pub fn needs_disclosure(destination: &str, accepted: &[String]) -> bool {
    !accepted.iter().any(|s| s == destination)
}
#[derive(Default)]
pub struct DrawerModel {
    pub conversation: Conversation,
    pub busy: bool,
    pub thinking: bool,
    pub retrying: Option<String>,
    pub pause_reason: Option<PauseReason>,
    pub error: Option<AiError>,
    pub meter: Option<ContextMeter>,
    pub compacting: bool,
    pub manual_compaction: bool,
}
impl DrawerModel {
    pub fn context_hint(&self, session_bound: bool) -> &'static str {
        if !session_bound { "无会话" }
        else if self.meter.is_none() { " · 最近 40 行输出" }
        else { " · 已用 " }
    }
    pub fn status(&self, pending: bool) -> &str {
        if self.compacting { "正在压缩上下文…" }
        else if pending { "正在准备请求…" }
        else if let Some(retrying) = &self.retrying { retrying }
        else if self.thinking { "正在思考…" }
        else { "正在分析当前会话输出…" }
    }
    pub fn pause(&mut self, reason: PauseReason) {
        self.busy = false;
        self.thinking = false;
        self.retrying = None;
        self.pause_reason = Some(reason);
        self.error = match reason {
            PauseReason::StepLimit => None,
            PauseReason::Error(error) => Some(error),
            PauseReason::InvalidCalls => Some(AiError::InvalidResponse),
        };
    }
    pub fn continuation_label(&self, steps: usize) -> String {
        match self.pause_reason {
            Some(PauseReason::Error(_)) => "重试".into(),
            Some(PauseReason::InvalidCalls) => "继续".into(),
            _ => format!("已执行 {steps} 步 · 继续"),
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn start(&mut self, question: &str, context: ContextSnapshot) -> bool {
        if self.busy || question.trim().is_empty() {
            return false;
        }
        self.conversation.push_user(UserTurn::new(question.trim(), context));
        self.busy = true;
        self.thinking = false;
        self.error = None;
        self.retrying = None;
        self.pause_reason = None;
        true
    }
    pub fn retry(&mut self) -> bool {
        if self.busy || self.conversation.turns().is_empty() {
            return false;
        }
        self.error = None;
        self.retrying = None;
        self.pause_reason = None;
        self.busy = true;
        self.thinking = false;
        true
    }
    pub fn event(&mut self, event: Event) {
        self.retrying = None;
        if !self.manual_compaction { self.pause_reason = None; }
        match event {
            Event::Context { used, window, trigger } => self.meter = Some(ContextMeter { used, window, trigger }),
            Event::Compacting { .. } => { self.compacting = true; self.busy = true; self.thinking = false; }
            Event::Compacted { .. } => self.compacting = false,
            Event::Thinking => self.thinking = self.conversation.turns().last().and_then(|t| t.answer.as_ref()).is_none_or(|a| a.text().is_empty()),
            Event::TextDelta(text) => { if !text.is_empty() { self.thinking = false; } self.conversation.push_text(&text); }
            Event::Command(card) => self.conversation.push_item(shellmate_ai::AnswerItem::Command(card)),
            Event::Done(_) => { self.busy = false; self.thinking = false; self.compacting = false; self.error = None; }
            Event::Error(e) => { self.busy = false; self.thinking = false; self.compacting = false; self.error = Some(e); }
        }
    }
    #[cfg(any(test, debug_assertions))]
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextMeter { pub used: usize, pub window: u32, pub trigger: u32 }
impl ContextMeter {
    pub fn amber(self) -> bool { self.used as u128 * 10 >= u128::from(self.trigger) * 9 }
    #[cfg(test)]
    pub fn label(self) -> String { format!("已用 {} / {}", tokens(self.used), tokens(self.window as usize)) }
}
pub fn tokens(n: usize) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) { format!("{}M", n / 1_000_000) }
    else if n >= 1000 { format!("{}K", n / 1000) } else { n.to_string() }
}
pub fn compaction_label(c: &shellmate_ai::Compaction) -> String { format!("已压缩上下文 · {} → {}", tokens(c.before_tokens), tokens(c.after_tokens)) }

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_store::{MemoryStore, SecretError};
    #[test]
    fn error_detection_matches_prototype() {
        for line in [
            "[error] broken",
            "failed to connect",
            "Connection REFUSED",
            "Broken pipe",
            "FATAL",
            "ESP_ERR_TIMEOUT",
        ] {
            assert_eq!(error_line(&format!("ok\n{line}\nnext")), Some(line.into()));
        }
        assert!(error_line("ls -la\nall good").is_none());
    }
    #[test]
    fn disclosure_per_host_and_port() {
        assert!(!needs_disclosure(
            "https://a.test",
            &["https://a.test".into()]
        ));
        assert!(needs_disclosure(
            "https://b.test",
            &["https://a.test".into()]
        ));
        assert!(needs_disclosure(
            "https://a.test:8443",
            &["https://a.test".into()]
        ));
    }
    #[test]
    fn unconfigured_and_locked_do_not_read_credentials() {
        struct Locked;
        impl SecretStore for Locked {
            fn is_locked(&self) -> bool {
                true
            }
            fn get(&self, _: &SecretKey) -> Result<Option<shellmate_core::Secret>, SecretError> {
                panic!("must not read a locked store")
            }
            fn set(&self, _: &SecretKey, _: &shellmate_core::Secret) -> Result<(), SecretError> {
                unreachable!()
            }
            fn delete(&self, _: &SecretKey) -> Result<(), SecretError> {
                unreachable!()
            }
        }
        assert!(matches!(
            load_config(&Settings::default(), &MemoryStore::new()),
            Err(AiError::NotConfigured)
        ));
        let s = Settings {
            model: "model".into(),
            ..Default::default()
        };
        assert!(matches!(load_config(&s, &Locked), Err(AiError::Cancelled)));
    }
    #[test]
    fn drawer_stream_retry_clear_and_verbatim_input() {
        let mut d = DrawerModel::default();
        assert!(d.start("TOKEN=hidden", ContextSnapshot::default()));
        assert!(!d.start("duplicate", ContextSnapshot::default()));
        d.event(Event::Thinking);
        assert!(d.thinking);
        assert!(d.conversation.turns()[0].answer.is_none());
        d.event(Event::TextDelta("answer".into()));
        assert!(!d.thinking);
        d.event(Event::Thinking);
        assert!(!d.thinking);
        d.event(Event::Error(AiError::RateLimited));
        assert!(d.retry());
        assert!(!d.thinking);
        d.event(Event::Thinking);
        d.event(Event::Error(AiError::Timeout));
        assert!(!d.thinking && !d.busy);
        assert_eq!(d.conversation.turns()[0].answer.as_ref().unwrap().text(), "answer");
        assert_eq!(d.conversation.turns()[0].user.question, "TOKEN=hidden");
        d.clear();
        assert!(d.conversation.turns().is_empty());
    }
    #[test]
    fn pause_notices_labels_and_retry_status_preserve_the_answer() {
        let mut model = DrawerModel::default();
        model.start("question", ContextSnapshot::default());
        model.event(Event::TextDelta("partial".into()));
        for (reason, error, label) in [
            (PauseReason::StepLimit, None, "已执行 50 步 · 继续"),
            (PauseReason::Error(AiError::Unavailable), Some(AiError::Unavailable), "重试"),
            (PauseReason::InvalidCalls, Some(AiError::InvalidResponse), "继续"),
        ] {
            model.busy = true;
            model.retrying = Some("retry status".into());
            model.pause(reason);
            assert!(!model.busy && !model.thinking && model.retrying.is_none());
            assert_eq!(model.error, error);
            assert_eq!(model.continuation_label(50), label);
            assert_eq!(model.conversation.turns()[0].answer.as_ref().unwrap().text(), "partial");
        }
        for event in [Event::Thinking, Event::TextDelta("more".into()), Event::Done(None), Event::Error(AiError::Authentication)] {
            model.retrying = Some("retry status".into());
            model.event(event);
            assert!(model.retrying.is_none());
        }
    }
    #[test]
    fn unsaved_settings_config_uses_reasoning_timeouts_and_provider_key() {
        use shellmate_ai::{Provider, ReasoningLevel};
        let secrets = MemoryStore::new();
        let settings = Settings {
            provider: Provider::Anthropic {
                base_url: "https://proxy.example/v1".into(),
            },
            model: "claude-sonnet-4-6".into(),
            reasoning: ReasoningLevel::High,
            ..Default::default()
        };
        secrets
            .set(
                &SecretKey::Account(settings.key_account()),
                &shellmate_core::Secret::new("fixture-key"),
            )
            .unwrap();
        let config = load_config(&settings, &secrets).unwrap();
        assert_eq!(config.provider, settings.provider);
        assert_eq!(config.reasoning, ReasoningLevel::High);
        assert_eq!(config.timeouts, ReasoningLevel::High.timeouts());
    }
    #[test]
    fn locked_vault_and_cancel_never_reach_request_factory() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let manager = crate::security::fixture_coordinator(shellmate_store::SharedStore::new(
            shellmate_store::HostStore::open_in_memory().unwrap(),
        ));
        let settings = Settings {
            model: "fixture-model".into(),
            ..Default::default()
        };
        manager
            .set(
                &SecretKey::Account(settings.key_account()),
                &shellmate_core::Secret::new("fixture-key"),
            )
            .unwrap();
        manager
            .switch_to_master(
                "Meadow9!Fixture",
                shellmate_vault::KdfParams {
                    memory_kib: 8192,
                    iterations: 1,
                    parallelism: 1,
                },
            )
            .unwrap();
        manager.lock();
        let requests = AtomicUsize::new(0);
        for accepted in [true, false] {
            let result = authorized_config(&settings, manager.as_ref(), accepted)
                .map(|_| requests.fetch_add(1, Ordering::SeqCst));
            assert!(result.is_err());
        }
        manager.unlock("Meadow9!Fixture").unwrap();
        assert!(authorized_config(&settings, manager.as_ref(), false).is_err());
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        authorized_config(&settings, manager.as_ref(), true)
            .map(|_| requests.fetch_add(1, Ordering::SeqCst))
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod permission_tests {
    #[test]
    fn chip_permissions_persist_per_host_and_local_key() {
        use shellmate_ai::policy::PermissionMode;
        use shellmate_store::HostStore;
        let root = std::env::temp_dir().join(format!("shellmate-agent-store-{}",shellmate_core::HostId::generate()));
        std::fs::create_dir(&root).unwrap();
        let path=root.join("hosts.db");
        {
            let mut db=HostStore::open(&path).unwrap();
            db.set_ai_permission("local",PermissionMode::Full).unwrap();
            db.set_ai_permission("prod-web-01",PermissionMode::Auto).unwrap();
        }
        {
            let db=HostStore::open(&path).unwrap();
            assert_eq!(db.ai_permission("local").unwrap(),PermissionMode::Full);
            assert_eq!(db.ai_permission("prod-web-01").unwrap(),PermissionMode::Auto);
            assert_eq!(db.ai_permission("other-host").unwrap(),PermissionMode::Ask);
            assert_eq!(db.setting("ai_permission:local").unwrap().as_deref(),Some("full"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Protocol, not transport implementation, determines the offered tools (Mosh also owns SSH).
pub fn execution_targets(protocol: Option<shellmate_core::Protocol>, connected: bool, ssh_exec: bool) -> (bool, bool) {
    match protocol {
        None => (true, connected),
        Some(shellmate_core::Protocol::Ssh) => (ssh_exec, connected),
        Some(shellmate_core::Protocol::Telnet) => (false, connected),
        Some(shellmate_core::Protocol::Mosh | shellmate_core::Protocol::Serial) => (false, false),
    }
}
#[cfg(test)]
mod capability_tests {
    use super::execution_targets;
    use shellmate_core::Protocol;
    #[test]
    fn session_capability_matrix_including_mosh_auxiliary_ssh() {
        for connected in [false,true] {for ssh in [false,true] {
            assert_eq!(execution_targets(None,connected,ssh),(true,connected));
            assert_eq!(execution_targets(Some(Protocol::Ssh),connected,ssh),(ssh,connected));
            assert_eq!(execution_targets(Some(Protocol::Telnet),connected,ssh),(false,connected));
            assert_eq!(execution_targets(Some(Protocol::Mosh),connected,ssh),(false,false));
            assert_eq!(execution_targets(Some(Protocol::Serial),connected,ssh),(false,false));
        }}
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    #[test]
    fn header_keeps_recent_output_until_usage_and_unbound_text_stays_unchanged() {
        let mut model = DrawerModel::default();
        assert_eq!(format!("上下文：prod-web-01{}", model.context_hint(true)), "上下文：prod-web-01 · 最近 40 行输出");
        assert_eq!(format!("上下文：{}", model.context_hint(false)), "上下文：无会话");
        model.event(Event::Context { used: 37_000, window: 1_000_000, trigger: 967_000 });
        let meter = model.meter.unwrap();
        assert_eq!(format!("上下文：prod-web-01{}{} / {}", model.context_hint(true), tokens(meter.used), tokens(meter.window as usize)), "上下文：prod-web-01 · 已用 37K / 1M");
        assert_eq!(model.context_hint(false), "无会话");
        assert!(!meter.amber());
        model.clear();
        assert_eq!(model.context_hint(true), " · 最近 40 行输出");
    }
    #[test]
    fn meter_is_hidden_until_usage_and_amber_at_ninety_percent_of_trigger() {
        let mut model=DrawerModel::default();
        assert!(model.meter.is_none());
        model.event(Event::Context { used:37_000,window:1_000_000,trigger:967_000 });
        assert_eq!(model.meter.unwrap().label(),"已用 37K / 1M");
        assert!(!model.meter.unwrap().amber());
        model.event(Event::Context {used:85_499,window:128_000,trigger:95_000});
        assert!(!model.meter.unwrap().amber());
        model.event(Event::Context {used:85_500,window:128_000,trigger:95_000});
        assert!(model.meter.unwrap().amber());
        model.clear();assert!(model.meter.is_none());
    }
    #[test]
    fn idle_and_paused_manual_compaction_share_busy_lock_and_status() {
        for paused in [false,true] {
            let mut model=DrawerModel::default();model.start("question",ContextSnapshot::default());
            model.event(Event::Done(None));
            if paused {model.pause(PauseReason::Error(AiError::Unavailable));}
            let old_pause=model.pause_reason;
            model.manual_compaction=true;
            model.event(Event::Compacting {tier:1});
            assert!(model.busy && model.compacting);
            assert_eq!(model.status(false),"正在压缩上下文…");
            assert!(!model.start("cannot send",ContextSnapshot::default()));
            model.event(Event::Compacting {tier:2});
            model.event(Event::Compacted {before:812_000,after:41_000,summary:Some("summary".into())});
            assert!(model.busy,"lock survives until Done");
            model.event(Event::Done(None));
            assert!(!model.busy && !model.compacting);
            assert_eq!(model.pause_reason,old_pause);
            if paused { assert_eq!(model.continuation_label(2),"重试"); }
        }
    }
}
