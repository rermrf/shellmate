//! gpui-free policies for the drawer, disclosure, retries and settings.
use shellmate_ai::{
    AiError, Answer, AnswerItem, ContextSnapshot, Conversation, Event, Turn, UserTurn,
};
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
    pub error: Option<AiError>,
    pub dropped: usize,
}
impl DrawerModel {
    pub fn start(&mut self, question: &str, context: ContextSnapshot) -> bool {
        if self.busy || question.trim().is_empty() {
            return false;
        }
        self.conversation.turns.push(Turn {
            user: UserTurn::redacted(question.trim(), context),
            answer: None,
        });
        self.busy = true;
        self.thinking = false;
        self.error = None;
        true
    }
    pub fn retry(&mut self) -> bool {
        if self.busy || self.conversation.turns.is_empty() {
            return false;
        }
        self.conversation.turns.last_mut().unwrap().answer = None;
        self.error = None;
        self.busy = true;
        self.thinking = false;
        true
    }
    pub fn event(&mut self, event: Event) {
        let Some(turn) = self.conversation.turns.last_mut() else {
            return;
        };
        match event {
            Event::Thinking => {
                self.thinking = turn.answer.as_ref().is_none_or(|a| a.text().is_empty());
            }
            Event::TextDelta(text) => {
                if !text.is_empty() {
                    self.thinking = false;
                }
                turn.answer
                    .get_or_insert_with(Answer::default)
                    .push_text(&text);
            }
            Event::Command(card) => turn
                .answer
                .get_or_insert_with(Answer::default)
                .items
                .push(AnswerItem::Command(card)),
            Event::Done(_) => {
                self.busy = false;
                self.thinking = false;
                self.error = None
            }
            Event::Error(e) => {
                self.busy = false;
                self.thinking = false;
                self.error = Some(e)
            }
        }
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}
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
    fn drawer_stream_retry_clear_and_redaction() {
        let mut d = DrawerModel::default();
        assert!(d.start("TOKEN=hidden", ContextSnapshot::default()));
        assert!(!d.start("duplicate", ContextSnapshot::default()));
        d.event(Event::Thinking);
        assert!(d.thinking);
        assert!(d.conversation.turns[0].answer.is_none());
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
        assert!(d.conversation.turns[0].answer.is_none());
        assert_eq!(d.conversation.turns[0].user.redactions, 1);
        d.clear();
        assert!(d.conversation.turns.is_empty());
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
