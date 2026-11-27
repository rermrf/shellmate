//! Local AI history; content is stored as sent, with bounded tool-output excerpts.
use crate::{HostStore, Result, StoreError};
use rusqlite::{OptionalExtension, params};
use shellmate_ai::{
    Answer, AnswerItem, CommandCard, ContextSnapshot, Conversation, PermissionMode, Provider,
    ApiProtocol, ReasoningLevel, UserTurn,
};
#[path = "ai_settings.rs"]
mod settings;
pub const SAVE_CONVERSATIONS: &str = "ai_save_conversations";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub provider: Provider,
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub save_conversations: bool,
    pub auto_memory: bool,
    pub subagent_limit: usize,
    pub context_window: shellmate_ai::ContextWindow,
    pub autocompact_window: shellmate_ai::ContextWindow,
    pub max_output_tokens: shellmate_ai::OutputTokens,
    pub max_steps: Option<usize>,
    pub exec: shellmate_ai::agent::ExecLimits,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            model: String::new(),
            reasoning: ReasoningLevel::Default,
            save_conversations: true,
            auto_memory: true,
            subagent_limit: 3,
            context_window: Default::default(),
            autocompact_window: Default::default(),
            max_output_tokens: Default::default(),
            max_steps: None,
            exec: Default::default(),
        }
    }
}
impl Settings {
    pub fn key_account(&self) -> String {
        format!("ai:{}:api_key", self.provider.id())
    }
    pub fn configured(&self) -> bool {
        !self.model.trim().is_empty() && self.provider.validate_endpoint().is_ok()
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
        answer.cap_stored();
        let items = serde_json::to_string(&answer.items)
            .map_err(|_| StoreError::Corrupt("无法编码 AI 回答".into()))?;
        tx.execute(
            "UPDATE ai_messages SET items_json=?1,content='',commands_json='[]' WHERE id=?2",
            params![items, id],
        )?;
    }
    Ok(())
}
pub(crate) fn migrate_v10(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let rows = tx.prepare("SELECT conversation_id,position,role,content,context,items_json,created_at FROM ai_messages ORDER BY conversation_id,position")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, i64>(6)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut logs = std::collections::HashMap::<String, Conversation>::new();
    for (id, old_position, role, content, context, items, timestamp) in rows {
        let log = logs.entry(id.clone()).or_default();
        let start = log.entries().len();
        if role == "user" {
            let value: serde_json::Value = serde_json::from_str(&context).map_err(|_| StoreError::Corrupt("AI 上下文损坏".into()))?;
            let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
            log.push_user(UserTurn::new(&content, ContextSnapshot { label: field("label"), protocol: field("protocol"), text: field("text") }));
        } else {
            let items: Vec<AnswerItem> = serde_json::from_str(&items).map_err(|_| StoreError::Corrupt("AI 回答损坏".into()))?;
            for (n, item) in items.into_iter().enumerate() {
                let mut part = Conversation::default();
                part.push_item(item);
                let mut entry = part.entries()[0].clone();
                match &mut entry {
                    shellmate_ai::Entry::Step(s) => { s.call.id = format!("legacy_{old_position}_{n}"); s.message_id = log.entries().len(); }
                    shellmate_ai::Entry::Assistant { message_id, .. } | shellmate_ai::Entry::CommandCard { message_id, .. } => *message_id = log.entries().len(),
                    _ => {}
                }
                log.append(entry);
            }
        }
        for (position, entry) in log.entries().iter().enumerate().skip(start) {
            let json = serde_json::to_string(entry).map_err(|_| StoreError::Corrupt("无法编码 AI 日志".into()))?;
            tx.execute("INSERT INTO ai_log VALUES(?1,?2,?3,?4,?5)", params![id, position as i64, entry.kind(), json, timestamp])?;
        }
    }
    Ok(())
}
fn protocol_keys(protocol: ApiProtocol) -> (&'static str, &'static str) {
    match protocol {
        ApiProtocol::ChatCompletions => ("ai_base_url", "ai_openai_reasoning"),
        ApiProtocol::Responses => ("ai_responses_base_url", "ai_responses_reasoning"),
        ApiProtocol::Anthropic => ("ai_anthropic_base_url", "ai_anthropic_reasoning"),
    }
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
        let id = self.setting("ai_provider")?.unwrap_or_else(|| "openai".into());
        let protocol = ApiProtocol::from_id(&id)
            .ok_or(StoreError::Corrupt("未知的 AI 协议".into()))?;
        self.ai_settings_for_provider(protocol)
    }
    /// Read a protocol's saved form without changing the active selection.
    pub fn ai_settings_for_provider(&self, protocol: ApiProtocol) -> Result<Settings> {
        let (base_key, reasoning_key) = protocol_keys(protocol);
        let provider = protocol.provider(self.setting(base_key)?.unwrap_or_else(|| protocol.default_url().into()));
        let reasoning = ReasoningLevel::from_id(&self.setting(reasoning_key)?.unwrap_or_default()).for_provider(&provider);
        let model = self.setting(&format!("ai_model:{}", protocol.id()))?
            .or(if protocol != ApiProtocol::Responses && self.setting("ai_provider")?.as_deref() != Some("openai_responses") { self.setting("ai_model")? } else { None })
            .unwrap_or_default();
        let mut settings = Settings {
            provider, reasoning, model,
            save_conversations: self.setting(SAVE_CONVERSATIONS)?.as_deref() != Some("0"),
            ..Settings::default()
        };
        self.read_ai_limits(&mut settings)?;
        Ok(settings)
    }
    pub fn save_ai_settings(&mut self, s: &Settings) -> Result<()> {
        s.provider
            .validate_endpoint()
            .map_err(|_| StoreError::Corrupt("AI Base URL 无效".into()))?;
        let tx = self.conn.transaction()?;
        // Preserve the legacy shared model before saving a different protocol.
        for id in ["openai", "anthropic"] {
            tx.execute(
                "INSERT OR IGNORE INTO settings(key,value) SELECT ?1,value FROM settings WHERE key='ai_model' AND COALESCE((SELECT value FROM settings WHERE key='ai_provider'),'openai')<>'openai_responses'",
                [format!("ai_model:{id}")],
            )?;
        }
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
        let (base_key, reasoning_key) = protocol_keys(s.provider.protocol());
        tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![format!("ai_model:{}", s.provider.id()), s.model.trim()])?;
        for (key, value) in [
            (base_key, s.provider.base_url()),
            (reasoning_key, s.reasoning.for_provider(&s.provider).id()),
        ] {
            tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        }
        settings::save_limits(&tx, s)?;
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
    /// The caller invokes this only once the request is dispatched.
    /// The preference is rechecked at each write, including streamed answers.
    pub fn save_ai_conversation(&mut self, c: &SavedConversation) -> Result<bool> {
        self.save_ai_conversation_with_runtime(c,None)
    }
    pub fn ai_runtime(&self,id:&str)->Result<Option<serde_json::Value>>{
        let json:Option<String>=self.conn.query_row("SELECT json FROM ai_runtime WHERE conversation_id=?1",[id],|r|r.get(0)).optional()?;
        json.map(|j|serde_json::from_str(&j).map_err(|_|StoreError::Corrupt("无法读取待处理消息".into()))).transpose()
    }
    pub fn save_ai_task_event(&mut self,conversation:&str,id:&str,task:&str,text:&str)->Result<()> {
        if self.setting(SAVE_CONVERSATIONS)?.as_deref()==Some("0"){return Ok(());}
        if self.ai_runtime(conversation)?.is_some_and(|runtime|runtime["queue"]["delivered"].as_array().is_some_and(|ids|ids.iter().any(|v|v.as_str()==Some(id)))){return Ok(());}
        self.conn.execute("INSERT OR IGNORE INTO ai_task_events SELECT ?1,?2,?3,?4 WHERE EXISTS(SELECT 1 FROM ai_conversations WHERE id=?1)",params![conversation,id,task,text])?;Ok(())
    }
    pub fn ai_conversation_exists(&self,id:&str)->Result<bool>{
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM ai_conversations WHERE id=?1)",[id],|r|r.get(0))?)
    }
    /// Timing heartbeats must not change the semantic log, queue receipts or history sort time.
    pub fn update_ai_run_timing(&mut self,id:&str,timing:&serde_json::Value)->Result<bool>{
        if self.setting(SAVE_CONVERSATIONS)?.as_deref()==Some("0"){return Ok(false);}
        let tx=self.conn.transaction()?;
        let json:Option<String>=tx.query_row("SELECT json FROM ai_runtime WHERE conversation_id=?1",[id],|r|r.get(0)).optional()?;
        let Some(json)=json else{return Ok(false);};
        let mut runtime:serde_json::Value=serde_json::from_str(&json).map_err(|_|StoreError::Corrupt("运行记录损坏".into()))?;
        runtime.as_object_mut().ok_or_else(||StoreError::Corrupt("运行记录损坏".into()))?.insert("timing".into(),timing.clone());
        tx.execute("UPDATE ai_runtime SET json=?2 WHERE conversation_id=?1",params![id,runtime.to_string()])?;
        tx.commit()?;Ok(true)
    }
    pub fn ai_task_events(&self,conversation:&str)->Result<Vec<(String,String,String)>>{
        Ok(self.conn.prepare("SELECT id,task_id,text FROM ai_task_events WHERE conversation_id=?1 ORDER BY rowid")?.query_map([conversation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn save_ai_external_step(&mut self,conversation:&str,step:&shellmate_ai::Step)->Result<()>{
        if self.setting(SAVE_CONVERSATIONS)?.as_deref()==Some("0"){return Ok(());}
        self.conn.execute("INSERT OR IGNORE INTO ai_external_steps SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM ai_conversations WHERE id=?1)",params![conversation,step.call.id,serde_json::to_string(step).map_err(|_|StoreError::Corrupt("无法保存追加消息".into()))?])?;Ok(())
    }
    pub fn ai_external_steps(&self,conversation:&str)->Result<Vec<shellmate_ai::Step>>{
        let rows=self.conn.prepare("SELECT json FROM ai_external_steps WHERE conversation_id=?1 ORDER BY rowid")?.query_map([conversation],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|json|serde_json::from_str(&json).map_err(|_|StoreError::Corrupt("无法读取追加消息".into()))).collect()
    }
    pub fn save_ai_conversation_with_runtime(&mut self,c:&SavedConversation,runtime:Option<&serde_json::Value>)->Result<bool>{
        if self.setting(SAVE_CONVERSATIONS)?.as_deref() == Some("0") {
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO ai_conversations VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET host_id=excluded.host_id,title=excluded.title,session_label=excluded.session_label,provider=excluded.provider,model=excluded.model,updated_at=excluded.updated_at",params![c.id,c.host_id,c.label,c.title,c.provider,c.model,c.created_at,c.updated_at])?;
        for (position, entry) in c.conversation.entries().iter().enumerate() {
            // A worker can finish between the drawer's clone and this save. Never replace
            // its terminal result with an older Running result from that clone.
            if matches!(entry,shellmate_ai::Entry::Step(step) if step.result.subagent.as_ref().is_some_and(|d|d.status==shellmate_ai::SubagentStatus::Running)) {
                let old:Option<String>=tx.query_row("SELECT json FROM ai_log WHERE conversation_id=?1 AND position=?2",params![c.id,position as i64],|r|r.get(0)).optional()?;
                if old.and_then(|json|serde_json::from_str::<shellmate_ai::Entry>(&json).ok()).is_some_and(|old|matches!(old,shellmate_ai::Entry::Step(step) if step.result.subagent.as_ref().is_some_and(|d|d.status!=shellmate_ai::SubagentStatus::Running))) {continue;}
            }
            let json = serde_json::to_string(entry).map_err(|_| StoreError::Corrupt("无法编码 AI 日志".into()))?;
            tx.execute("INSERT INTO ai_log(conversation_id,position,kind,json,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(conversation_id,position) DO UPDATE SET kind=excluded.kind,json=excluded.json WHERE ai_log.kind<>excluded.kind OR ai_log.json<>excluded.json", params![c.id, position as i64, entry.kind(), json, c.updated_at])?;
            if let shellmate_ai::Entry::Step(step)=entry{tx.execute("DELETE FROM ai_external_steps WHERE conversation_id=?1 AND call_id=?2",params![c.id,step.call.id])?;}
        }
        if let Some(runtime)=runtime {
            if let Some(targets)=runtime["targets"]["targets"].as_object(){
                tx.execute("DELETE FROM ai_conversation_targets WHERE conversation_id=?1",[&c.id])?;
                for(id,target)in targets{let role=if runtime["targets"]["default"].as_str()==Some(id){"default"}else{"participant"};tx.execute("INSERT INTO ai_conversation_targets VALUES(?1,?2,?3,?4,?5)",params![c.id,id,target["host_id"].as_str(),role,target.to_string()])?;}
            }
            tx.execute("INSERT INTO ai_runtime VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET json=excluded.json",params![c.id,runtime.to_string()])?;
            if let Some(ids)=runtime["queue"]["delivered"].as_array(){for id in ids.iter().filter_map(|id|id.as_str()){tx.execute("DELETE FROM ai_task_events WHERE conversation_id=?1 AND id=?2",params![c.id,id])?;}}
        }
        tx.commit()?;
        Ok(true)
    }
    /// Background child completion may arrive after its drawer has switched conversations.
    /// Update only the original agent Step; never recreate a deleted conversation.
    pub fn finish_ai_subagent(&mut self, conversation: &str, call_id: &str, result: &shellmate_ai::LoggedResult) -> Result<()> {
        let pending:Option<String>=self.conn.query_row("SELECT json FROM ai_external_steps WHERE conversation_id=?1 AND call_id=?2",params![conversation,call_id],|r|r.get(0)).optional()?;
        if let Some(pending)=pending {
            let mut step:shellmate_ai::Step=serde_json::from_str(&pending).map_err(|_|StoreError::Corrupt("追加消息记录损坏".into()))?;step.result=result.clone();
            self.conn.execute("UPDATE ai_external_steps SET json=?3 WHERE conversation_id=?1 AND call_id=?2",params![conversation,call_id,serde_json::to_string(&step).map_err(|_|StoreError::Corrupt("无法保存子任务结果".into()))?])?;
        }
        let rows = self.conn.prepare("SELECT position,json FROM ai_log WHERE conversation_id=?1 AND kind='step'")?
            .query_map([conversation], |row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (position,json) in rows {
            let mut entry: shellmate_ai::Entry=serde_json::from_str(&json).map_err(|_|StoreError::Corrupt("AI 日志损坏".into()))?;
            if let shellmate_ai::Entry::Step(step)=&mut entry && matches!(step.call.name.as_str(),"agent"|"send_agent_message") && step.call.id==call_id {
                step.result=result.clone();
                let json=serde_json::to_string(&entry).map_err(|_|StoreError::Corrupt("无法编码 AI 日志".into()))?;
                self.conn.execute("UPDATE ai_log SET json=?3 WHERE conversation_id=?1 AND position=?2",params![conversation,position,json])?;
                break;
            }
        }
        Ok(())
    }
    pub fn ai_conversations(&self) -> Result<Vec<ConversationSummary>> {
        Ok(self.conn.prepare("SELECT id,session_label,title,updated_at FROM ai_conversations ORDER BY updated_at DESC,id")?.query_map([],|r|Ok(ConversationSummary{id:r.get(0)?,label:r.get(1)?,title:r.get(2)?,updated_at:r.get(3)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn ai_conversation(&self, id: &str) -> Result<Option<SavedConversation>> {
        let Some(mut c)=self.conn.query_row("SELECT id,host_id,session_label,title,provider,model,created_at,updated_at FROM ai_conversations WHERE id=?1",[id],|r|Ok(SavedConversation{id:r.get(0)?,host_id:r.get(1)?,label:r.get(2)?,title:r.get(3)?,provider:r.get(4)?,model:r.get(5)?,created_at:r.get(6)?,updated_at:r.get(7)?,conversation:Conversation::default()})).optional()?else{return Ok(None)};
        let jsons = self.conn.prepare("SELECT json FROM ai_log WHERE conversation_id=?1 ORDER BY position")?.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let entries = jsons.iter().map(|s| serde_json::from_str::<shellmate_ai::Entry>(s).map_err(|_| StoreError::Corrupt("AI 日志损坏".into()))).collect::<Result<Vec<_>>>()?;
        c.conversation = Conversation::from_entries(entries);
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
    use shellmate_ai::Turn;
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
        assert_eq!(db.schema_version().unwrap(), 12);
        let c = db.ai_conversation("c1").unwrap().unwrap();
        assert!(
            matches!(&c.conversation.turns()[0].answer.as_ref().unwrap().items[..], [AnswerItem::Text(text),AnswerItem::Command(card)] if text=="回答" && card.command=="ls")
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
        assert_eq!(HostStore::open(path).unwrap().schema_version().unwrap(), 12);
    }
    #[test]
    fn ordered_steps_round_trip_preserves_content_and_caps_output() {
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
        c.conversation = Conversation::from_turns(vec![Turn { user: c.conversation.turns()[0].user.clone(), answer: Some(Answer { items }) }]);
        db.save_ai_conversation(&c).unwrap();
        let raw: String = db
            .conn
            .query_row(
                "SELECT group_concat(json) FROM ai_log WHERE kind<>'user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw.contains("private-value"));
        assert!(raw.contains("denied-secret"));
        assert!(!raw.contains("\\u001b"));
        let loaded = db.ai_conversation("c1").unwrap().unwrap();
        assert!(loaded.conversation == c.conversation);
        let answer = loaded.conversation.turns()[0].answer.as_ref().unwrap();
        assert!(matches!(answer.items[0], AnswerItem::Text(_)));
        assert!(matches!(answer.items[1], AnswerItem::Command(_)));
        for item in &answer.items {
            if let AnswerItem::Step { output, .. } = item {
                assert!(output.len() <= STORED_LIMIT);
                assert!(!output.contains('\x1b'));
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
    fn responses_settings_are_independent_and_legacy_chat_settings_survive() {
        let mut db = HostStore::open_in_memory().unwrap();
        db.set_setting("ai_provider", "openai").unwrap();
        db.set_setting("ai_base_url", "https://legacy.test/v1").unwrap();
        db.set_setting("ai_model", "legacy-model").unwrap();
        let legacy = db.ai_settings().unwrap();
        assert_eq!(legacy.provider.protocol(), ApiProtocol::ChatCompletions);
        assert_eq!(legacy.model, "legacy-model");
        assert_eq!(legacy.key_account(), "ai:openai:api_key");
        let fresh = db.ai_settings_for_provider(ApiProtocol::Responses).unwrap();
        assert_eq!(fresh.provider.base_url(), "https://api.openai.com/v1");
        assert!(fresh.model.is_empty());
        let responses = Settings {
            provider: ApiProtocol::Responses.provider("https://responses.test/custom/v1".into()),
            model: "gpt-5.4".into(), reasoning: ReasoningLevel::High,
            max_output_tokens: shellmate_ai::OutputTokens::Tokens(8192),
            ..Default::default()
        };
        db.save_ai_settings(&responses).unwrap();
        assert_eq!(db.ai_settings().unwrap(),responses);
        assert_eq!(db.ai_settings_for_provider(ApiProtocol::ChatCompletions).unwrap(),legacy);
        assert_eq!(responses.key_account(),"ai:openai_responses:api_key");
        db.save_ai_settings(&legacy).unwrap();
        assert_eq!(db.ai_settings_for_provider(ApiProtocol::Responses).unwrap(),responses);
        db.set_setting("ai_provider","future-protocol").unwrap();
        assert!(db.ai_settings().is_err());
    }
    #[test]
    fn provider_urls_and_reasoning_round_trip_independently() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert_eq!(db.ai_settings().unwrap().reasoning, ReasoningLevel::Default);
        let default_anthropic = db.ai_settings_for_provider(shellmate_ai::ApiProtocol::Anthropic).unwrap();
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
        assert_eq!(db.ai_settings_for_provider(shellmate_ai::ApiProtocol::ChatCompletions).unwrap(), openai);
        // Loading another provider's form must not change saved selection.
        assert_eq!(db.ai_settings().unwrap(), anthropic);
        db.save_ai_settings(&openai).unwrap();
        assert_eq!(db.ai_settings_for_provider(shellmate_ai::ApiProtocol::Anthropic).unwrap(), anthropic);
        assert_eq!(anthropic.key_account(), "ai:anthropic:api_key");
        assert_eq!(default_anthropic.key_account(), anthropic.key_account());
    }
    #[test]
    fn unknown_and_unoffered_stored_reasoning_fall_back_to_default() {
        let mut db = HostStore::open_in_memory().unwrap();
        for (anthropic, key, ids) in [
            (false, "ai_openai_reasoning", &["unknown"][..]),
            (
                true,
                "ai_anthropic_reasoning",
                &["unknown", "minimal", "xhigh"][..],
            ),
        ] {
            for id in ids {
                db.set_setting(key, id).unwrap();
                assert_eq!(
                    db.ai_settings_for_provider(if anthropic { ApiProtocol::Anthropic } else { ApiProtocol::ChatCompletions }).unwrap().reasoning,
                    ReasoningLevel::Default
                );
            }
        }
        // Both providers offer the top level.
        db.set_setting("ai_openai_reasoning", "max").unwrap();
        assert_eq!(db.ai_settings_for_provider(shellmate_ai::ApiProtocol::ChatCompletions).unwrap().reasoning, ReasoningLevel::Max);
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
            conversation: Conversation::from_turns(vec![Turn {
                    user: UserTurn::new(
                        "TOKEN=hidden",
                        ContextSnapshot {
                            text: "PASSWORD=hidden".into(),
                            ..Default::default()
                        },
                    ),
                    answer: Some(Answer {
                        items: vec![AnswerItem::Text("回答".into())],
                    }),
                }]),
        }
    }
    #[test]
    fn content_is_stored_verbatim_and_legacy_redaction_count_is_ignored() {
        const TEXT: &str = "PASSWORD=hunter2 sk-123456789abcdef";
        let mut db = HostStore::open_in_memory().unwrap();
        let mut c = saved();
        c.conversation = Conversation::default();
        c.conversation.push_user(UserTurn::new(TEXT, ContextSnapshot {
            text: TEXT.into(), label: TEXT.into(), protocol: "SSH".into(),
        }));
        c.conversation.push_answer(Answer { items: vec![
            AnswerItem::Text(TEXT.into()),
            AnswerItem::Command(CommandCard { command: TEXT.into(), explanation: TEXT.into(), risk: shellmate_ai::Risk::Low }),
        ] });
        db.save_ai_conversation(&c).unwrap();
        let count: i64 = db.conn.query_row("SELECT count(*) FROM ai_messages", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        db.conn.execute("INSERT INTO ai_messages VALUES('legacy','c1','user','old','{}','[]',1,0,99,'[]')", []).unwrap();
        let loaded = db.ai_conversation(&c.id).unwrap().unwrap();
        assert!(loaded.conversation == c.conversation);
        db.save_ai_conversation(&loaded).unwrap();
        let nonzero: i64 = db.conn.query_row("SELECT count(*) FROM ai_messages WHERE redactions<>0", [], |r| r.get(0)).unwrap();
        assert_eq!(nonzero, 1, "v10 saves leave v9 rows untouched");
        assert_eq!(db.schema_version().unwrap(), 12);
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
                .query_row::<i64, _, _>("SELECT count(*) FROM ai_log", [], |r| r.get(0))
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
        assert_eq!(db.schema_version().unwrap(), 12);
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
    fn message_timestamps_survive_followups_and_raw_inputs_are_preserved() {
        let mut db = HostStore::open_in_memory().unwrap();
        let mut c = saved();
        db.save_ai_conversation(&c).unwrap();
        c.updated_at = 100;
        c.conversation.push_turn(Turn {
            user: UserTurn::new("next", ContextSnapshot::default()),
            answer: None,
        });
        db.save_ai_conversation(&c).unwrap();
        let timestamps = db
            .conn
            .prepare("SELECT created_at FROM ai_log ORDER BY position")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(timestamps, [2, 2, 100]);
        c.conversation.push_user(UserTurn::new("TOKEN=unsafe", ContextSnapshot::default()));
        assert!(db.save_ai_conversation(&c).unwrap());
        assert_eq!(db.ai_conversation(&c.id).unwrap().unwrap().conversation.turns().last().unwrap().user.question, "TOKEN=unsafe");
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
    }
    #[test]
    fn user_send_time_is_independent_of_save_time_and_legacy_history_stays_unknown() {
        let mut db = HostStore::open_in_memory().unwrap();
        let mut saved = saved();
        let sent = 1_700_000_000_123;
        saved.conversation.push_user(UserTurn::new("sent", Default::default()).sent_at(Some(sent)));
        db.save_ai_conversation(&saved).unwrap();
        saved.updated_at += 600;
        saved.conversation.push_text("later reply");
        db.save_ai_conversation(&saved).unwrap();
        let loaded = db.ai_conversation(&saved.id).unwrap().unwrap();
        assert_eq!(loaded.conversation.turns()[1].user.sent_at_ms, Some(sent));
        assert!(loaded.conversation.turns()[0].user.sent_at_ms.is_none());
        let original = loaded.conversation.turns()[1].user.wire_text();
        let without_time = loaded.conversation.turns()[1].user.clone().sent_at(None).wire_text();
        assert_eq!(original, without_time);
    }
}

#[cfg(test)]
#[path = "ai_log_tests.rs"]
mod log_tests;
