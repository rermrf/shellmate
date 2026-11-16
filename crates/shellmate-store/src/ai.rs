//! Local history; never includes credentials, raw input, or terminal capture before redaction.
use crate::{HostStore, Result, StoreError};
use rusqlite::{OptionalExtension, params};
use shellmate_ai::{
    Answer, CommandCard, ContextSnapshot, Conversation, Provider, Turn, UserTurn, redact::redact,
};
pub const SAVE_CONVERSATIONS: &str = "ai_save_conversations";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub provider: Provider,
    pub model: String,
    pub save_conversations: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            model: String::new(),
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
impl HostStore {
    pub fn ai_settings(&self) -> Result<Settings> {
        Ok(Settings {
            provider: if self.setting("ai_provider")?.as_deref() == Some("anthropic") {
                Provider::Anthropic
            } else {
                Provider::OpenAiCompatible {
                    base_url: self
                        .setting("ai_base_url")?
                        .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                }
            },
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
        if let Provider::OpenAiCompatible { base_url } = &s.provider {
            tx.execute("INSERT INTO settings VALUES('ai_base_url',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[base_url])?;
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
                "INSERT INTO ai_messages VALUES(?1,?2,'user',?3,?4,'[]',?5,?6,?7) ON CONFLICT(id) DO UPDATE SET content=excluded.content,context=excluded.context,redactions=excluded.redactions",
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
                let commands = serde_json::to_string(&a.commands)
                    .map_err(|_| StoreError::Corrupt("无法编码命令卡片".into()))?;
                tx.execute(
                    "INSERT INTO ai_messages VALUES(?1,?2,'assistant',?3,'',?4,?5,?6,0) ON CONFLICT(id) DO UPDATE SET content=excluded.content,commands_json=excluded.commands_json",
                    params![
                        format!("{}:{index}:assistant", c.id),
                        c.id,
                        a.text,
                        commands,
                        c.updated_at,
                        (index * 2 + 1) as i64
                    ],
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
        let rows=self.conn.prepare("SELECT role,content,context,commands_json,redactions FROM ai_messages WHERE conversation_id=?1 ORDER BY position")?.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?.max(0) as usize)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (role, content, context, commands, count) in rows {
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
                let cards: Vec<CommandCard> = serde_json::from_str(&commands)
                    .map_err(|_| StoreError::Corrupt("AI 命令卡片损坏".into()))?;
                if let Some(t) = c.conversation.turns.last_mut() {
                    t.answer = Some(Answer {
                        text: content,
                        commands: cards,
                    });
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
                        text: "回答".into(),
                        commands: vec![],
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
    fn migration_v7_to_v8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 7);
        conn.execute("INSERT INTO settings VALUES('fixture','kept')", [])
            .unwrap();
        drop(conn);
        let db = HostStore::open(path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 8);
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
