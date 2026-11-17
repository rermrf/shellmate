//! Local history; never includes credentials, raw input, or terminal capture before redaction.
use crate::{HostStore, Result, StoreError};
use rusqlite::{OptionalExtension, params};
use shellmate_ai::{
    Answer, AnswerItem, CommandCard, ContextSnapshot, Conversation, PermissionMode, Provider,
    ReasoningLevel, Turn, UserTurn, redact::redact,
};
pub const SAVE_CONVERSATIONS: &str = "ai_save_conversations";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub provider: Provider,
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub save_conversations: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            model: String::new(),
            reasoning: ReasoningLevel::Default,
            save_conversations: true,
        }
    }
}
impl Settings {
    pub fn key_account(&self) -> String {
        format!("ai:{}:api_key", self.provider.id())
    }
    pub fn configured(&self) -> bool {
        !self.model.trim().is_empty() && self.provider.destination().is_ok()
    }
}
#[derive(Clone)]
pub struct SavedConversation {
    pub id: String,
    pub host_id: Option<String>,
    pub label: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub conversation: Conversation,
}
#[derive(Clone)]
pub struct ConversationSummary {
    pub id: String,
    pub label: String,
    pub title: String,
    pub updated_at: i64,
}
pub(crate) fn migrate_v9(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let rows = tx
        .prepare("SELECT id,content,commands_json FROM ai_messages WHERE role='assistant'")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, text, commands) in rows {
        let cards: Vec<CommandCard> = serde_json::from_str(&commands)
            .map_err(|_| StoreError::Corrupt("AI 命令卡片损坏".into()))?;
        let mut answer = Answer::default();
        answer.push_text(&text);
        answer
            .items
            .extend(cards.into_iter().map(AnswerItem::Command));
        answer.sanitize();
        let items = serde_json::to_string(&answer.items)
            .map_err(|_| StoreError::Corrupt("无法编码 AI 回答".into()))?;
        tx.execute(
            "UPDATE ai_messages SET items_json=?1,content='',commands_json='[]' WHERE id=?2",
            params![items, id],
        )?;
    }
    Ok(())
}
impl HostStore {
    pub fn ai_permission(&self, host_key: &str) -> Result<PermissionMode> {
        Ok(PermissionMode::from_id(
            &self
                .setting(&format!("ai_permission:{host_key}"))?
                .unwrap_or_default(),
        ))
    }
    pub fn set_ai_permission(&mut self, host_key: &str, mode: PermissionMode) -> Result<()> {
        self.set_setting(&format!("ai_permission:{host_key}"), mode.id())
    }

    pub fn ai_settings(&self) -> Result<Settings> {
        self.ai_settings_for_provider(self.setting("ai_provider")?.as_deref() == Some("anthropic"))
    }
    /// Read one provider's saved form without changing the active provider.
    pub fn ai_settings_for_provider(&self, anthropic: bool) -> Result<Settings> {
        let provider = if anthropic {
            Provider::Anthropic {
                base_url: self
                    .setting("ai_anthropic_base_url")?
                    .unwrap_or_else(|| "https://api.anthropic.com".into()),
            }
        } else {
            Provider::OpenAiCompatible {
                base_url: self
                    .setting("ai_base_url")?
                    .unwrap_or_else(|| "https://api.openai.com/v1".into()),
            }
        };
        let reasoning = ReasoningLevel::from_id(
            &self
                .setting(if anthropic {
                    "ai_anthropic_reasoning"
                } else {
                    "ai_openai_reasoning"
                })?
                .unwrap_or_default(),
        )
        .for_provider(&provider);
        Ok(Settings {
            provider,
            reasoning,
            model: self.setting("ai_model")?.unwrap_or_default(),
            save_conversations: self.setting(SAVE_CONVERSATIONS)?.as_deref() != Some("0"),
        })
    }
    pub fn save_ai_settings(&mut self, s: &Settings) -> Result<()> {
        s.provider
            .destination()
            .map_err(|_| StoreError::Corrupt("AI Base URL 无效".into()))?;
        let tx = self.conn.transaction()?;
        for (k, v) in [
            ("ai_provider", s.provider.id()),
            ("ai_model", s.model.trim()),
            (
                SAVE_CONVERSATIONS,
                if s.save_conversations { "1" } else { "0" },
            ),
        ] {
            tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![k,v])?;
        }
        let (base_key, reasoning_key) = match s.provider {
            Provider::OpenAiCompatible { .. } => ("ai_base_url", "ai_openai_reasoning"),
            Provider::Anthropic { .. } => ("ai_anthropic_base_url", "ai_anthropic_reasoning"),
        };
        for (key, value) in [
            (base_key, s.provider.base_url()),
            (reasoning_key, s.reasoning.for_provider(&s.provider).id()),
        ] {
            tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn ai_disclosed(&self, destination: &str) -> Result<bool> {
        Ok(self
            .setting(&format!("ai_disclosure:{destination}"))?
            .as_deref()
            == Some("1"))
    }
    pub fn accept_ai_disclosure(&mut self, destination: &str) -> Result<()> {
        self.set_setting(&format!("ai_disclosure:{destination}"), "1")
    }
    /// The caller invokes this only once the redacted request is dispatched.
    /// The preference is rechecked at each write, including streamed answers.
    pub fn save_ai_conversation(&mut self, c: &SavedConversation) -> Result<bool> {
        if self.setting(SAVE_CONVERSATIONS)?.as_deref() == Some("0") {
            return Ok(false);
        }
        for t in &c.conversation.turns {
            for text in [
                &t.user.question,
                &t.user.context.text,
                &t.user.context.label,
                &t.user.context.protocol,
            ] {
                if redact(text).text != *text {
                    return Err(StoreError::Corrupt("拒绝保存未脱敏的 AI 输入".into()));
                }
            }
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO ai_conversations VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET title=excluded.title,session_label=excluded.session_label,provider=excluded.provider,model=excluded.model,updated_at=excluded.updated_at",params![c.id,c.host_id,c.label,c.title,c.provider,c.model,c.created_at,c.updated_at])?;
        tx.execute(
            "DELETE FROM ai_messages WHERE conversation_id=?1 AND position>=?2",
            params![c.id, (c.conversation.turns.len() * 2) as i64],
        )?;
        for (index, t) in c.conversation.turns.iter().enumerate() {
            let context=serde_json::json!({"label":t.user.context.label,"protocol":t.user.context.protocol,"text":t.user.context.text}).to_string();
            tx.execute(
                "INSERT INTO ai_messages VALUES(?1,?2,'user',?3,?4,'[]',?5,?6,?7,'[]') ON CONFLICT(id) DO UPDATE SET content=excluded.content,context=excluded.context,redactions=excluded.redactions",
                params![
                    format!("{}:{index}:user", c.id),
                    c.id,
                    t.user.question,
                    context,
                    c.updated_at,
                    (index * 2) as i64,
                    t.user.redactions as i64
                ],
            )?;
            if let Some(a) = &t.answer {
                let mut answer = a.clone();
                answer.sanitize();
                let items = serde_json::to_string(&answer.items)
                    .map_err(|_| StoreError::Corrupt("无法编码 AI 回答".into()))?;
                tx.execute(
                    "INSERT INTO ai_messages VALUES(?1,?2,'assistant','','','[]',?3,?4,0,?5) ON CONFLICT(id) DO UPDATE SET content='',commands_json='[]',items_json=excluded.items_json",
                    params![format!("{}:{index}:assistant", c.id), c.id, c.updated_at, (index*2+1) as i64, items],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM ai_messages WHERE id=?1",
                    [format!("{}:{index}:assistant", c.id)],
                )?;
            }
        }
        tx.commit()?;
        Ok(true)
    }
    pub fn ai_conversations(&self) -> Result<Vec<ConversationSummary>> {
        Ok(self.conn.prepare("SELECT id,session_label,title,updated_at FROM ai_conversations ORDER BY updated_at DESC,id")?.query_map([],|r|Ok(ConversationSummary{id:r.get(0)?,label:r.get(1)?,title:r.get(2)?,updated_at:r.get(3)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn ai_conversation(&self, id: &str) -> Result<Option<SavedConversation>> {
        let Some(mut c)=self.conn.query_row("SELECT id,host_id,session_label,title,provider,model,created_at,updated_at FROM ai_conversations WHERE id=?1",[id],|r|Ok(SavedConversation{id:r.get(0)?,host_id:r.get(1)?,label:r.get(2)?,title:r.get(3)?,provider:r.get(4)?,model:r.get(5)?,created_at:r.get(6)?,updated_at:r.get(7)?,conversation:Conversation::default()})).optional()?else{return Ok(None)};
        let rows=self.conn.prepare("SELECT role,content,context,items_json,redactions FROM ai_messages WHERE conversation_id=?1 ORDER BY position")?.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?.max(0) as usize)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (role, content, context, items, count) in rows {
            if role == "user" {
                let value: serde_json::Value = serde_json::from_str(&context)
                    .map_err(|_| StoreError::Corrupt("AI 上下文损坏".into()))?;
                let s = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
                c.conversation.turns.push(Turn {
                    user: UserTurn {
                        question: content,
                        context: ContextSnapshot {
                            label: s("label"),
                            protocol: s("protocol"),
                            text: s("text"),
                        },
                        redactions: count,
                    },
                    answer: None,
                });
            } else {
                let items: Vec<AnswerItem> = serde_json::from_str(&items)
                    .map_err(|_| StoreError::Corrupt("AI 回答损坏".into()))?;
                if let Some(t) = c.conversation.turns.last_mut() {
                    let mut answer = Answer { items };
                    answer.sanitize();
                    t.answer = Some(answer);
                }
            }
        }
        Ok(Some(c))
    }
    pub fn delete_ai_conversation(&mut self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM ai_conversations WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn clear_ai_conversations(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM ai_conversations", [])?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_v8_to_v9_preserves_order_and_clears_legacy_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 8);
        conn.execute(
            "INSERT INTO ai_conversations VALUES('c1',NULL,'host','问题','openai','model',1,2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ai_messages VALUES('u','c1','user','问题','{}','[]',1,0,0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO ai_messages VALUES('a','c1','assistant','回答','','[{\"command\":\"ls\",\"explanation\":\"列表\",\"risk\":\"low\"}]',2,1,0)", []).unwrap();
        drop(conn);
        let db = HostStore::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 9);
        let c = db.ai_conversation("c1").unwrap().unwrap();
        assert!(
            matches!(&c.conversation.turns[0].answer.as_ref().unwrap().items[..], [AnswerItem::Text(text),AnswerItem::Command(card)] if text=="回答" && card.command=="ls")
        );
        let legacy: (String, String, i64) = db
            .conn
            .query_row(
                "SELECT content,commands_json,created_at FROM ai_messages WHERE id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(legacy, (String::new(), "[]".into(), 2));
        drop(db);
        assert_eq!(HostStore::open(path).unwrap().schema_version().unwrap(), 9);
    }
    #[test]
    fn ordered_steps_round_trip_and_only_sanitized_shaped_output_is_stored() {
        use shellmate_ai::{StepStatus, Target, output::STORED_LIMIT, policy::Class};
        let mut db = HostStore::open_in_memory().unwrap();
        let mut c = saved();
        let items = vec![
            AnswerItem::Text("检查".into()),
            AnswerItem::Command(CommandCard {
                command: "ls".into(),
                explanation: "列表".into(),
                risk: shellmate_ai::Risk::Low,
            }),
            AnswerItem::Step {
                automatic: true,
                command: "docker ps".into(),
                target: Target::Background,
                purpose: "检查容器".into(),
                class: Class::ReadOnly,
                status: StepStatus::Completed,
                exit_code: Some(0),
                duration_ms: 15,
                output: format!(
                    "\x1b[31mPASSWORD=private-value\x1b[0m\n{}",
                    "中".repeat(6000)
                ),
            },
            AnswerItem::Text("完成".into()),
            AnswerItem::Step {
                automatic: false,
                command: "rm x".into(),
                target: Target::Terminal,
                purpose: "删除".into(),
                class: Class::Other,
                status: StepStatus::Denied {
                    reason: Some("保留 PASSWORD=denied-secret".into()),
                },
                exit_code: None,
                duration_ms: 0,
                output: String::new(),
            },
        ];
        c.conversation.turns[0].answer = Some(Answer { items });
        db.save_ai_conversation(&c).unwrap();
        let raw: String = db
            .conn
            .query_row(
                "SELECT items_json FROM ai_messages WHERE role='assistant'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!raw.contains("private-value"));
        assert!(!raw.contains("denied-secret"));
        assert!(!raw.contains("\\u001b"));
        let loaded = db.ai_conversation("c1").unwrap().unwrap();
        c.conversation.turns[0].answer.as_mut().unwrap().sanitize();
        assert!(loaded.conversation == c.conversation);
        let answer = loaded.conversation.turns[0].answer.as_ref().unwrap();
        assert!(matches!(answer.items[0], AnswerItem::Text(_)));
        assert!(matches!(answer.items[1], AnswerItem::Command(_)));
        for item in &answer.items {
            if let AnswerItem::Step { output, .. } = item {
                assert!(output.len() <= STORED_LIMIT);
                assert_eq!(redact(output).text, *output);
            }
        }
    }
    #[test]
    fn permissions_are_per_host_default_ask_and_unknown_ids_are_ask() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert_eq!(db.ai_permission("local").unwrap(), PermissionMode::Ask);
        db.set_ai_permission("local", PermissionMode::Full).unwrap();
        db.set_ai_permission("host1", PermissionMode::Auto).unwrap();
        assert_eq!(db.ai_permission("local").unwrap(), PermissionMode::Full);
        assert_eq!(db.ai_permission("host1").unwrap(), PermissionMode::Auto);
        assert_eq!(db.ai_permission("host2").unwrap(), PermissionMode::Ask);
        db.set_setting("ai_permission:host1", "future").unwrap();
        assert_eq!(db.ai_permission("host1").unwrap(), PermissionMode::Ask);
        db.set_ai_permission("local", PermissionMode::Ask).unwrap();
        assert_eq!(
            db.setting("ai_permission:local").unwrap().as_deref(),
            Some("ask")
        );
    }
    #[test]
    fn provider_urls_and_reasoning_round_trip_independently() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert_eq!(db.ai_settings().unwrap().reasoning, ReasoningLevel::Default);
        let default_anthropic = db.ai_settings_for_provider(true).unwrap();
        assert_eq!(
            default_anthropic.provider.base_url(),
            "https://api.anthropic.com"
        );
        assert_eq!(default_anthropic.reasoning, ReasoningLevel::Default);
        let openai = Settings {
            provider: Provider::OpenAiCompatible {
                base_url: "https://openai-proxy.example/v1".into(),
            },
            model: "shared-model".into(),
            reasoning: ReasoningLevel::XHigh,
            ..Default::default()
        };
        let anthropic = Settings {
            provider: Provider::Anthropic {
                base_url: "https://anthropic-proxy.example/api/v1/messages".into(),
            },
            reasoning: ReasoningLevel::Max,
            ..openai.clone()
        };
        db.save_ai_settings(&openai).unwrap();
        assert_eq!(db.ai_settings().unwrap(), openai);
        db.save_ai_settings(&anthropic).unwrap();
        assert_eq!(db.ai_settings().unwrap(), anthropic);
        assert_eq!(db.ai_settings_for_provider(false).unwrap(), openai);
        // Loading another provider's form must not change saved selection.
        assert_eq!(db.ai_settings().unwrap(), anthropic);
        db.save_ai_settings(&openai).unwrap();
        assert_eq!(db.ai_settings_for_provider(true).unwrap(), anthropic);
        assert_eq!(anthropic.key_account(), "ai:anthropic:api_key");
        assert_eq!(default_anthropic.key_account(), anthropic.key_account());
    }
    #[test]
    fn unknown_and_unoffered_stored_reasoning_fall_back_to_default() {
        let mut db = HostStore::open_in_memory().unwrap();
        for (anthropic, key, ids) in [
            (false, "ai_openai_reasoning", &["unknown", "max"][..]),
            (
                true,
                "ai_anthropic_reasoning",
                &["unknown", "minimal", "xhigh"][..],
            ),
        ] {
            for id in ids {
                db.set_setting(key, id).unwrap();
                assert_eq!(
                    db.ai_settings_for_provider(anthropic).unwrap().reasoning,
                    ReasoningLevel::Default
                );
            }
        }
    }
    #[test]
    fn anthropic_proxy_disclosure_requires_each_destination() {
        let mut db = HostStore::open_in_memory().unwrap();
        let destination = |url: &str| {
            Provider::Anthropic {
                base_url: url.into(),
            }
            .destination()
            .unwrap()
        };
        let official = destination("https://api.anthropic.com/v1");
        db.accept_ai_disclosure(&official).unwrap();
        let proxy = destination("https://proxy.example/v1/messages");
        assert!(!db.ai_disclosed(&proxy).unwrap());
        db.accept_ai_disclosure(&proxy).unwrap();
        assert!(
            db.ai_disclosed(&destination("https://proxy.example/messages"))
                .unwrap()
        );
        assert!(db.ai_disclosed(&official).unwrap());
        assert!(
            !db.ai_disclosed(&destination("https://other.example/v1"))
                .unwrap()
        );
        assert!(
            !db.ai_disclosed(&destination("http://proxy.example/v1"))
                .unwrap()
        );
        assert!(
            !db.ai_disclosed(&destination("https://proxy.example:8443/v1"))
                .unwrap()
        );
    }
    fn saved() -> SavedConversation {
        SavedConversation {
            id: "c1".into(),
            host_id: Some("h1".into()),
            label: "host".into(),
            title: "问题".into(),
            provider: "openai".into(),
            model: "test-model".into(),
            created_at: 1,
            updated_at: 2,
            conversation: Conversation {
                turns: vec![Turn {
                    user: UserTurn::redacted(
                        "TOKEN=hidden",
                        ContextSnapshot {
                            text: "PASSWORD=hidden".into(),
                            ..Default::default()
                        },
                    ),
                    answer: Some(Answer {
                        items: vec![AnswerItem::Text("回答".into())],
                    }),
                }],
            },
        }
    }
    #[test]
    fn crud_cascade_and_save_off() {
        let mut db = HostStore::open_in_memory().unwrap();
        let c = saved();
        assert!(db.ai_settings().unwrap().save_conversations);
        assert!(db.save_ai_conversation(&c).unwrap());
        assert!(db.ai_conversation(&c.id).unwrap().unwrap().conversation == c.conversation);
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
        db.set_setting(SAVE_CONVERSATIONS, "0").unwrap();
        let mut edited = c.clone();
        edited.title = "changed".into();
        assert!(!db.save_ai_conversation(&edited).unwrap());
        assert_eq!(db.ai_conversations().unwrap()[0].title, "问题");
        db.delete_ai_conversation(&c.id).unwrap();
        assert_eq!(
            db.conn
                .query_row::<i64, _, _>("SELECT count(*) FROM ai_messages", [], |r| r.get(0))
                .unwrap(),
            0
        );
        assert!(!db.save_ai_conversation(&c).unwrap());
        assert!(db.ai_conversations().unwrap().is_empty());
        db.set_setting(SAVE_CONVERSATIONS, "1").unwrap();
        db.save_ai_conversation(&c).unwrap();
        db.clear_ai_conversations().unwrap();
        assert!(db.ai_conversations().unwrap().is_empty());
    }
    #[test]
    fn migration_v7_to_v9() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 7);
        conn.execute("INSERT INTO settings VALUES('fixture','kept')", [])
            .unwrap();
        drop(conn);
        let db = HostStore::open(path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 9);
        assert_eq!(db.setting("fixture").unwrap().as_deref(), Some("kept"));
        assert!(db.ai_conversations().unwrap().is_empty());
    }
    #[test]
    fn disclosure_is_scoped_to_destination_and_settings_contain_no_key() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert!(!db.ai_disclosed("https://a.example").unwrap());
        db.accept_ai_disclosure("https://a.example").unwrap();
        assert!(db.ai_disclosed("https://a.example").unwrap());
        assert!(!db.ai_disclosed("https://b.example").unwrap());
        let s = Settings {
            model: "model".into(),
            ..Default::default()
        };
        db.save_ai_settings(&s).unwrap();
        assert_eq!(db.ai_settings().unwrap(), s);
        assert_eq!(s.key_account(), "ai:openai:api_key");
    }
    #[test]
    fn message_timestamps_survive_followups_and_raw_inputs_are_rejected() {
        let mut db = HostStore::open_in_memory().unwrap();
        let mut c = saved();
        db.save_ai_conversation(&c).unwrap();
        c.updated_at = 100;
        c.conversation.turns.push(Turn {
            user: UserTurn::redacted("next", ContextSnapshot::default()),
            answer: None,
        });
        db.save_ai_conversation(&c).unwrap();
        let timestamps = db
            .conn
            .prepare("SELECT created_at FROM ai_messages ORDER BY position")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(timestamps, [2, 2, 100]);
        c.conversation.turns.last_mut().unwrap().user.question = "TOKEN=unsafe".into();
        assert!(db.save_ai_conversation(&c).is_err());
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
    }
}
