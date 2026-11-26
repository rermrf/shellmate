//! Conversation-owned fields and decisions; resetting them needs no window or credentials.
use super::{MarkdownCache, agent_ui::ActiveStep};
use crate::ai::model::DrawerModel;
use shellmate_ai::{UserTurn, agent};
use shellmate_store::{HostStore, ai::SavedConversation};
use std::collections::HashSet;

// Resume is opaque. Keeping its ownership generic lets tests use a drop-tracked marker.
pub(super) struct ConversationState<R = Box<agent::Resume>> {
    pub(super) model: DrawerModel,
    pub(super) context_popover: crate::ai::context_view::Popover,
    pub(super) local_cards: Vec<super::slash_ui::LocalCard>,
    pub(super) run_limits: Option<agent::Limits>,
    pub(super) run_subagent_settings: Option<shellmate_store::ai::Settings>,
    pub(super) run_budget: Option<(shellmate_ai::ContextBudget, shellmate_ai::WindowSource)>,
    pub(super) id: String,
    pub(super) created: i64,
    pub(super) save_error: Option<String>,
    pub(super) pending: bool,
    pub(super) queued: Option<UserTurn>,
    pub(super) control: shellmate_ai::interaction::Control,
    pub(super) timing: super::timing::Timeline,
    pub(super) accepted_inputs: Vec<String>,
    pub(super) accepted_events: Vec<String>,
    pub(super) reenter: bool,
    pub(super) interaction_scope: Option<serde_json::Value>,
    pub(super) btw: super::btw_ui::SideQuestion,
    pub(super) message_target: Option<String>,
    pub(super) editing_message: Option<String>,
    pub(super) queue_expanded: bool,
    pub(super) follow_bottom: bool,
    pub(super) active_steps: crate::ai::approvals::Queue<ActiveStep>,
    pub(super) subagents: std::collections::BTreeMap<String, crate::ai::subagent_view::View>,
    pub(super) active_tools: Vec<shellmate_ai::LoggedCall>,
    pub(super) child_page: super::thread_ui::Navigation,
    pub(super) tool_expanded: HashSet<String>,
    pub(super) tool_ids: crate::ai::tool_model::Identities,
    pub(super) tool_raw: HashSet<String>,
    pub(super) tool_collapsed: HashSet<String>,
    pub(super) output_panes:
        std::collections::BTreeMap<String, gpui::Entity<super::output_ui::OutputPane>>,
    pub(super) output_subscriptions: Vec<gpui::Subscription>,
    pub(super) output_lifetime: crate::ai::output_document::Lifetime,
    pub(super) markdown: MarkdownCache,
    pub(super) readonly: bool,
    pub(super) history_memory_host: Option<String>,
    pub(super) continuation: Option<R>,
    pub(super) resume_steps: usize,
    pub(super) expanded_summaries: HashSet<(usize, usize)>,
    pub(super) compaction_focus: Option<Option<String>>,
    pub(super) run_config: Option<shellmate_ai::Config>,
}
impl<R> Default for ConversationState<R> {
    fn default() -> Self {
        Self {
            model: Default::default(),
            context_popover: Default::default(),
            local_cards: Vec::new(),
            run_limits: None,
            run_subagent_settings: None,
            run_budget: None,
            id: shellmate_core::HostId::generate().to_string(),
            created: crate::ai::now(),
            save_error: None,
            pending: false,
            queued: None,
            control: Default::default(),
            timing: Default::default(),
            accepted_inputs: vec![],
            accepted_events: vec![],
            reenter: false,
            interaction_scope: None,
            btw: Default::default(),
            message_target: None,
            editing_message: None,
            queue_expanded: false,
            follow_bottom: true,
            active_steps: Default::default(),
            subagents: Default::default(),
            active_tools: vec![],
            child_page: Default::default(),
            tool_expanded: Default::default(),
            tool_ids: Default::default(),
            tool_raw: Default::default(),
            tool_collapsed: Default::default(),
            output_panes: Default::default(),
            output_subscriptions: Default::default(),
            output_lifetime: Default::default(),
            markdown: MarkdownCache::default(),
            readonly: false,
            history_memory_host: None,
            continuation: None,
            resume_steps: 0,
            run_config: None,
            expanded_summaries: HashSet::new(),
            compaction_focus: None,
        }
    }
}
impl<R> ConversationState<R> {
    pub fn toggle_summary(&mut self, key: (usize, usize)) {
        if !self.expanded_summaries.remove(&key) {
            self.expanded_summaries.insert(key);
        }
    }
    pub fn reset_conversation(&mut self) {
        *self = Self::default();
    }
    pub fn delete(&mut self, db: &mut HostStore, id: &str) -> shellmate_store::Result<bool> {
        db.delete_ai_conversation(id)?;
        let current = self.id == id;
        if current {
            self.reset_conversation();
        }
        Ok(current)
    }
    pub fn persist(
        &mut self,
        db: &mut HostStore,
        host_id: Option<String>,
        provider: &str,
        model: &str,
    ) {
        let snapshot = self.control.snapshot();
        let first = self
            .model
            .conversation
            .turns()
            .first()
            .map(|t| t.user.clone())
            .or_else(|| self.queued.clone())
            .or_else(|| {
                snapshot
                    .inputs
                    .front()
                    .map(|i| UserTurn::new(&i.text, i.context.clone()))
            });
        let Some(first) = first else {
            return;
        };
        let saved = SavedConversation {
            id: self.id.clone(),
            host_id,
            label: self
                .model
                .conversation
                .latest_user()
                .unwrap_or(&first)
                .context
                .label
                .clone(),
            title: first.question.chars().take(48).collect(),
            provider: provider.into(),
            model: model.into(),
            created_at: self.created,
            updated_at: crate::ai::now(),
            conversation: self.model.conversation.clone(),
        };
        let runtime = serde_json::json!({"queue":self.control.delivery_snapshot(&self.accepted_inputs,&self.accepted_events),"scope":self.interaction_scope,"timing":self.timing.snapshot(super::timing::Moment::now()),"preparing":self.queued.as_ref().map(|u|serde_json::json!({"text":u.question,"context":u.context}))});
        match db.save_ai_conversation_with_runtime(&saved, Some(&runtime)) {
            Ok(true) => {
                self.save_error = None;
                self.control
                    .acknowledge(&std::mem::take(&mut self.accepted_inputs));
                self.control
                    .acknowledge_completions(&std::mem::take(&mut self.accepted_events));
            }
            // Saving being disabled is not evidence that a failed save recovered.
            Ok(false) => {
                self.control
                    .acknowledge(&std::mem::take(&mut self.accepted_inputs));
                self.control
                    .acknowledge_completions(&std::mem::take(&mut self.accepted_events));
            }
            Err(_) => self.save_error = Some("对话未能保存到本机，请检查主机库".into()),
        }
    }
    pub fn new_conversation_action(&self, history_open: bool) -> NewConversation {
        let has_content = !self.model.conversation.turns().is_empty()
            || !self.control.snapshot().inputs.is_empty()
            || self.queued.is_some()
            || self.model.error.is_some()
            || self.save_error.is_some();
        let running = self.model.busy
            || self.pending
            || !self.active_steps.is_empty()
            || self.continuation.is_some();
        new_conversation_action(has_content, running, history_open)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NewConversation {
    Disabled,
    Start,
    Confirm,
}
pub(super) fn new_conversation_action(
    has_content: bool,
    running: bool,
    history_open: bool,
) -> NewConversation {
    if running {
        NewConversation::Confirm
    } else if has_content || history_open {
        NewConversation::Start
    } else {
        NewConversation::Disabled
    }
}
pub(super) fn new_conversation_tooltip(save_conversations: bool, save_error: bool) -> &'static str {
    if save_error {
        "开始新对话（当前对话未能保存）"
    } else if save_conversations {
        "开始新对话（当前对话已保存在历史对话里）"
    } else {
        "开始新对话（当前对话不会保存）"
    }
}
pub(super) fn new_conversation_confirmation(save_conversations: bool) -> &'static str {
    if save_conversations {
        "AI 正在执行，开始新对话会停止当前操作。已完成的内容会保存在历史对话里。"
    } else {
        "AI 正在执行，开始新对话会停止当前操作，当前对话不会保存。"
    }
}

#[cfg(test)]
mod tests {
    fn approval_fixture(
        step: Option<super::ActiveStep>,
    ) -> crate::ai::approvals::Queue<super::ActiveStep> {
        let mut queue = crate::ai::approvals::Queue::default();
        if let Some(step) = step {
            queue.insert(step.call.id.clone(), None, step);
        }
        queue
    }

    use super::*;
    use shellmate_ai::{
        AiError, ContextSnapshot, Event,
        agent::{Target, ToolCall},
        policy::Class,
    };
    use std::{cell::Cell, rc::Rc, time::Duration};

    fn step() -> ActiveStep {
        ActiveStep::new(ToolCall {
            service: None,
            session_id: None,
            cwd: None, destination: None,
            id: "approval".into(),
            command: "pwd".into(),
            purpose: "查看目录".into(),
            target: Target::Background,
            timeout: Duration::from_secs(30),
            run_in_background: false,
            exec_limits: shellmate_ai::agent::ExecLimits::default(),
            class: Class::ReadOnly,
        })
    }

    #[test]
    fn new_conversation_action_all_combinations() {
        use NewConversation::*;
        for (content, running, history, expected) in [
            (false, false, false, Disabled),
            (false, false, true, Start),
            (false, true, false, Confirm),
            (false, true, true, Confirm),
            (true, false, false, Start),
            (true, false, true, Start),
            (true, true, false, Confirm),
            (true, true, true, Confirm),
        ] {
            assert_eq!(new_conversation_action(content, running, history), expected);
        }
    }

    #[test]
    fn new_conversation_running_includes_paused_continuation_and_approval() {
        for flags in 0..16 {
            let state = ConversationState {
                model: DrawerModel {
                    busy: flags & 1 != 0,
                    ..Default::default()
                },
                pending: flags & 2 != 0,
                active_steps: approval_fixture((flags & 4 != 0).then(step)),
                continuation: (flags & 8 != 0).then_some(()),
                ..Default::default()
            };
            for history in [false, true] {
                let expected = if flags != 0 {
                    NewConversation::Confirm
                } else if history {
                    NewConversation::Start
                } else {
                    NewConversation::Disabled
                };
                assert_eq!(state.new_conversation_action(history), expected);
            }
        }
    }

    #[test]
    fn new_conversation_content_includes_queued_and_errors() {
        for flags in 0..16 {
            let mut state = ConversationState::<()> {
                queued: (flags & 2 != 0)
                    .then(|| UserTurn::new("queued", ContextSnapshot::default())),
                save_error: (flags & 8 != 0).then(|| "save failed".into()),
                ..Default::default()
            };
            if flags & 1 != 0 {
                state.model.start("question", ContextSnapshot::default());
                state.model.event(Event::Done(None));
            }
            state.model.error = (flags & 4 != 0).then_some(AiError::NotConfigured);
            assert_eq!(
                state.new_conversation_action(false),
                if flags == 0 {
                    NewConversation::Disabled
                } else {
                    NewConversation::Start
                }
            );
        }
    }

    #[test]
    fn new_conversation_tooltip_save_on_off_and_error() {
        assert_eq!(
            new_conversation_tooltip(true, false),
            "开始新对话（当前对话已保存在历史对话里）"
        );
        assert_eq!(
            new_conversation_tooltip(false, false),
            "开始新对话（当前对话不会保存）"
        );
        for saving in [false, true] {
            assert_eq!(
                new_conversation_tooltip(saving, true),
                "开始新对话（当前对话未能保存）"
            );
        }
    }

    #[test]
    fn new_conversation_confirmation_tracks_saving() {
        assert_eq!(
            new_conversation_confirmation(true),
            "AI 正在执行，开始新对话会停止当前操作。已完成的内容会保存在历史对话里。"
        );
        assert_eq!(
            new_conversation_confirmation(false),
            "AI 正在执行，开始新对话会停止当前操作，当前对话不会保存。"
        );
    }

    struct ResumeMarker(Rc<Cell<bool>>);
    impl Drop for ResumeMarker {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn delete_current_resets_created_readonly_continuation_and_resume_steps() {
        let mut db = HostStore::open_in_memory().unwrap();
        let dropped = Rc::new(Cell::new(false));
        let mut state = ConversationState {
            created: 1,
            readonly: true,
            continuation: Some(ResumeMarker(dropped.clone())),
            resume_steps: 20,
            queued: Some(UserTurn::new("queued", ContextSnapshot::default())),
            active_steps: approval_fixture(Some(step())),
            pending: true,
            follow_bottom: false,
            ..Default::default()
        };
        state.model.start("old", ContextSnapshot::default());
        state.model.event(Event::Thinking);
        state.model.meter = Some(crate::ai::model::ContextMeter {
            used: 37_000,
            window: 128_000,
            trigger: 95_000,
        });
        state.persist(&mut db, None, "openai", "fixture");
        state.save_error = Some("previous error".into());
        state.tool_expanded.insert("main:call".into());
        state.tool_collapsed.insert("child:call".into());
        state.tool_raw.insert("main:call".into());
        state.tool_ids.start("call");
        let old_id = state.id.clone();
        let before = crate::ai::now();

        assert!(state.delete(&mut db, &old_id).unwrap());

        assert!(db.ai_conversation(&old_id).unwrap().is_none());
        assert_ne!(state.id, old_id);
        assert!((before..=crate::ai::now()).contains(&state.created));
        assert!(!state.readonly);
        assert!(state.continuation.is_none());
        assert!(dropped.get());
        assert_eq!(state.resume_steps, 0);
        assert!(state.model.conversation.turns().is_empty());
        assert!(!state.model.busy && !state.model.thinking && !state.pending);
        assert!(state.model.meter.is_none());
        assert!(state.model.error.is_none() && state.save_error.is_none());
        assert!(
            state.queued.is_none() && state.active_steps.is_empty() && state.run_config.is_none()
        );
        assert!(state.follow_bottom);
        assert!(
            state.tool_expanded.is_empty()
                && state.tool_collapsed.is_empty()
                && state.tool_raw.is_empty()
        );
        state.persist(&mut db, None, "openai", "fixture");
        assert!(db.ai_conversations().unwrap().is_empty());
        state.model.start("new", ContextSnapshot::default());
        state.persist(&mut db, None, "openai", "fixture");
        assert_eq!(
            db.ai_conversation(&state.id).unwrap().unwrap().created_at,
            state.created
        );
    }

    #[test]
    fn delete_other_preserves_current_conversation() {
        let mut db = HostStore::open_in_memory().unwrap();
        let mut other = ConversationState::<()>::default();
        other.model.start("other", ContextSnapshot::default());
        other.persist(&mut db, None, "openai", "fixture");
        let mut state = ConversationState {
            created: 42,
            readonly: true,
            continuation: Some(()),
            resume_steps: 7,
            ..Default::default()
        };
        let id = state.id.clone();
        assert!(!state.delete(&mut db, &other.id).unwrap());
        assert_eq!(state.id, id);
        assert_eq!(state.created, 42);
        assert!(state.readonly && state.continuation.is_some());
        assert_eq!(state.resume_steps, 7);
    }

    #[test]
    fn new_conversation_keeps_history_without_empty_rows_and_load_continues_id() {
        let mut db = HostStore::open_in_memory().unwrap();
        let mut state = ConversationState::<()>::default();
        state.persist(&mut db, None, "openai", "fixture");
        assert!(db.ai_conversations().unwrap().is_empty());
        state.model.start("first", ContextSnapshot::default());
        state.persist(&mut db, None, "openai", "fixture");
        state.model.event(Event::TextDelta("partial answer".into()));
        state.model.event(Event::Error(AiError::Cancelled));
        state.persist(&mut db, None, "openai", "fixture");
        let old_id = state.id.clone();
        state.reset_conversation();
        state.persist(&mut db, None, "openai", "fixture");
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
        let saved = db.ai_conversation(&old_id).unwrap().unwrap();
        assert_eq!(
            saved.conversation.turns()[0]
                .answer
                .as_ref()
                .unwrap()
                .text(),
            "partial answer"
        );
        state.id = saved.id;
        state.created = saved.created_at;
        state.model.conversation = saved.conversation;
        state.model.start("follow-up", ContextSnapshot::default());
        state.persist(&mut db, None, "openai", "fixture");
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
        assert_eq!(
            db.ai_conversation(&old_id)
                .unwrap()
                .unwrap()
                .conversation
                .turns()
                .len(),
            2
        );
    }

    #[test]
    fn save_disabled_and_failed_save_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.db");
        let moved = temp.path().join("moved.db");
        let mut db = HostStore::open(&path).unwrap();
        let mut state = ConversationState::<()>::default();
        state
            .model
            .start("TOKEN=fixture-sensitive", ContextSnapshot::default());
        db.set_setting(shellmate_store::ai::SAVE_CONVERSATIONS, "0")
            .unwrap();
        state.persist(&mut db, None, "openai", "fixture");
        assert!(db.ai_conversations().unwrap().is_empty());
        assert!(state.save_error.is_none());
        db.set_setting(shellmate_store::ai::SAVE_CONVERSATIONS, "1")
            .unwrap();
        // SQLite refuses writes after its backing file is moved. This exercises
        // a real save failure without relying on the removed redaction policy.
        std::fs::rename(&path, &moved).unwrap();
        state.persist(&mut db, None, "openai", "fixture");
        assert!(state.save_error.is_some());
        drop(db);
        std::fs::rename(&moved, &path).unwrap();
        let mut db = HostStore::open(&path).unwrap();
        state.persist(&mut db, None, "openai", "fixture");
        assert!(state.save_error.is_none());
        assert_eq!(db.ai_conversations().unwrap().len(), 1);
        assert_eq!(
            db.ai_conversation(&state.id)
                .unwrap()
                .unwrap()
                .conversation
                .turns()[0]
                .user
                .question,
            "TOKEN=fixture-sensitive"
        );
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use shellmate_ai::{CompactTrigger, Compaction, ContextSnapshot, Entry, Event, RetainedState};
    #[test]
    fn compaction_divider_summary_toggle_roundtrip_and_reset() {
        let mut state = ConversationState::<()>::default();
        state.model.start("question", ContextSnapshot::default());
        state
            .model
            .event(Event::TextDelta("original answer".into()));
        let c = Compaction {
            summary: Some("handoff summary".into()),
            retained_user: vec!["question".into()],
            state: RetainedState::default(),
            context: ContextSnapshot::default(),
            before_tokens: 812_000,
            after_tokens: 41_000,
            trigger: CompactTrigger::Manual,
        };
        assert_eq!(
            crate::ai::model::compaction_label(&c),
            "已压缩上下文 · 812K → 41K"
        );
        state
            .model
            .conversation
            .append(Entry::Compaction(c.clone()));
        state.model.event(Event::TextDelta("continued".into()));
        let items = &state.model.conversation.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .items;
        assert!(
            matches!(&items[..],[shellmate_ai::AnswerItem::Text(a),shellmate_ai::AnswerItem::Compaction(b),shellmate_ai::AnswerItem::Text(d)] if a=="original answer" && *b==c && d=="continued")
        );
        assert!(!state.expanded_summaries.contains(&(0, 1)));
        state.toggle_summary((0, 1));
        assert!(state.expanded_summaries.contains(&(0, 1)));
        state.toggle_summary((0, 1));
        assert!(!state.expanded_summaries.contains(&(0, 1)));
        let mut db = HostStore::open_in_memory().unwrap();
        state.persist(&mut db, None, "openai", "fixture");
        let loaded = db.ai_conversation(&state.id).unwrap().unwrap();
        assert!(loaded.conversation == state.model.conversation);
        state.toggle_summary((0, 1));
        state.reset_conversation();
        assert!(state.expanded_summaries.is_empty());
    }
}
