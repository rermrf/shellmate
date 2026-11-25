//! Run control is independent of task cancellation. Inputs are claimed, then acknowledged
//! only after their semantic log updates reach the durable conversation owner.
use crate::ContextSnapshot;
use rig_core::serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "snake_case")]
pub enum Intent {
    #[default]
    None,
    SendNow,
    Detach,
    StopReply,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Generating,
    Executing,
    Approval,
    Waiting(Vec<String>),
    Compacting,
    Retrying,
}
impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "空闲",
            Self::Generating => "正在回复",
            Self::Executing => "正在执行",
            Self::Approval => "等待批准",
            Self::Waiting(_) => "等待任务",
            Self::Compacting => "正在压缩上下文",
            Self::Retrying => "等待重试",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Input {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::MessageOrigin>,
    pub id: String,
    pub text: String,
    pub context: ContextSnapshot,
    pub claimed: bool,
    #[serde(default)]
    pub editing: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Snapshot {
    pub inputs: VecDeque<Input>,
    pub held: bool,
    pub auto_continue: bool,
    #[serde(default)]
    pub completions: VecDeque<Completion>,
    #[serde(default)]
    pub delivered: Vec<String>,
    #[serde(default)]
    pub tool_steps: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Completion {
    pub id: String,
    pub task_id: String,
    pub text: String,
    pub claimed: bool,
}
#[derive(Default)]
struct State {
    saved: Snapshot,
    intent: Intent,
    phase: Phase,
    sealed: bool,
}
#[derive(Clone)]
pub struct Control {
    state: Arc<Mutex<State>>,
    changed: watch::Sender<u64>,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            state: Default::default(),
            changed: watch::channel(0).0,
        }
    }
}
impl Control {
    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().saved.clone()
    }
    pub fn restore(&self, mut snapshot: Snapshot) {
        for input in &mut snapshot.inputs {
            input.claimed = false;
            input.editing = false;
        }
        for completion in &mut snapshot.completions {
            completion.claimed = false;
        }
        snapshot.held = true;
        snapshot.auto_continue = false;
        *self.state.lock().unwrap() = State {
            saved: snapshot,
            ..Default::default()
        };
        self.notify();
    }
    pub fn enqueue(&self, id: String, text: String, context: ContextSnapshot) -> bool {
        self.enqueue_with(id, text, context, |_| Ok::<_, std::convert::Infallible>(()))
            .unwrap()
    }
    /// Publish an input only after its owner has saved it. The callback must not
    /// access this control: the lock prevents a worker claiming it before commit.
    pub fn enqueue_with<E>(
        &self,
        id: String,
        text: String,
        context: ContextSnapshot,
        save: impl FnOnce(&Input) -> Result<(), E>,
    ) -> Result<bool, E> {
        self.enqueue_from(id, text, context, None, save)
    }
    pub fn enqueue_from<E>(
        &self,
        id: String,
        text: String,
        context: ContextSnapshot,
        origin: Option<crate::MessageOrigin>,
        save: impl FnOnce(&Input) -> Result<(), E>,
    ) -> Result<bool, E> {
        if text.trim().is_empty() {
            return Ok(false);
        }
        let mut state = self.state.lock().unwrap();
        if state.sealed {
            return Ok(false);
        }
        if state.saved.inputs.iter().any(|i| i.id == id) {
            return Ok(false);
        }
        let input = Input {
            origin,
            id,
            text,
            context,
            claimed: false,
            editing: false,
        };
        save(&input)?;
        state.saved.inputs.push_back(input);
        drop(state);
        self.notify();
        Ok(true)
    }
    pub fn withdraw(&self, id: &str) -> Option<Input> {
        let mut state = self.state.lock().unwrap();
        let index = state
            .saved
            .inputs
            .iter()
            .position(|i| i.id == id && !i.claimed)?;
        let input = state.saved.inputs.remove(index);
        drop(state);
        self.notify();
        input
    }
    pub fn edit(&self, id: &str, text: Option<String>) -> Option<String> {
        let mut s = self.state.lock().unwrap();
        let i = s
            .saved
            .inputs
            .iter_mut()
            .find(|i| i.id == id && !i.claimed)?;
        if let Some(text) = text {
            if text.trim().is_empty() {
                return None;
            }
            i.text = text;
            i.editing = false;
        } else {
            i.editing = true;
        }
        Some(i.text.clone())
    }
    pub fn pending(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.saved.held
            && state
                .saved
                .inputs
                .iter()
                .find(|i| !i.claimed)
                .is_some_and(|i| !i.editing)
    }
    pub fn claim_first(&self) -> Option<Input> {
        let mut s = self.state.lock().unwrap();
        if s.saved.held {
            return None;
        }
        let i = s.saved.inputs.iter_mut().find(|i| !i.claimed)?;
        if i.editing {
            return None;
        }
        i.claimed = true;
        Some(i.clone())
    }
    pub fn claim(&self) -> Vec<Input> {
        let mut state = self.state.lock().unwrap();
        if state.saved.held {
            return vec![];
        }
        state
            .saved
            .inputs
            .iter_mut()
            .filter(|i| !i.claimed)
            .take_while(|i| !i.editing)
            .map(|i| {
                i.claimed = true;
                i.clone()
            })
            .collect()
    }
    pub fn acknowledge(&self, ids: &[String]) {
        self.state
            .lock()
            .unwrap()
            .saved
            .inputs
            .retain(|i| !ids.contains(&i.id));
        self.notify();
    }
    pub fn release_claims(&self) {
        for i in &mut self.state.lock().unwrap().saved.inputs {
            i.claimed = false;
        }
        self.notify();
    }
    pub fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        state.saved.held = false;
        state.saved.auto_continue = true;
        state.intent = Intent::None;
        drop(state);
        self.notify();
    }
    pub fn finish_if_empty(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.saved.inputs.iter().any(|i| !i.claimed) {
            false
        } else {
            s.sealed = true;
            true
        }
    }
    pub fn seal(&self) {
        self.state.lock().unwrap().sealed = true;
    }
    pub fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.saved.held = true;
        state.saved.auto_continue = false;
        drop(state);
        self.notify();
    }
    pub fn delivery_snapshot(&self, inputs: &[String], events: &[String]) -> Snapshot {
        let mut snapshot = self.snapshot();
        snapshot.inputs.retain(|i| !inputs.contains(&i.id));
        snapshot.completions.retain(|e| !events.contains(&e.id));
        for id in events {
            if !snapshot.delivered.contains(id) {
                snapshot.delivered.push(id.clone());
            }
        }
        snapshot
    }
    pub fn auto_continue(&self) -> bool {
        self.state.lock().unwrap().saved.auto_continue
    }
    pub fn tool_steps(&self) -> usize {
        self.state.lock().unwrap().saved.tool_steps
    }
    pub fn reset_budget(&self) {
        self.state.lock().unwrap().saved.tool_steps = 0;
    }
    pub fn tool_finished(&self) {
        let mut s = self.state.lock().unwrap();
        s.saved.tool_steps = s.saved.tool_steps.saturating_add(1);
    }
    pub fn publish(&self, id: String, task_id: String, text: String) {
        let mut s = self.state.lock().unwrap();
        if s.saved.delivered.contains(&id) || s.saved.completions.iter().any(|e| e.id == id) {
            return;
        }
        s.saved.completions.push_back(Completion {
            id,
            task_id,
            text,
            claimed: false,
        });
        drop(s);
        self.notify();
    }
    pub fn pending_completions(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .saved
            .completions
            .iter()
            .any(|e| !e.claimed)
    }
    pub fn claim_completions(&self) -> Vec<Completion> {
        self.state
            .lock()
            .unwrap()
            .saved
            .completions
            .iter_mut()
            .filter(|e| !e.claimed)
            .map(|e| {
                e.claimed = true;
                e.clone()
            })
            .collect()
    }
    pub fn acknowledge_completions(&self, ids: &[String]) {
        let mut s = self.state.lock().unwrap();
        s.saved.completions.retain(|e| !ids.contains(&e.id));
        for id in ids {
            if !s.saved.delivered.contains(id) {
                s.saved.delivered.push(id.clone());
            }
        }
    }
    pub fn request(&self, intent: Intent) {
        let mut state = self.state.lock().unwrap();
        if intent == Intent::StopReply {
            state.saved.held = true;
            state.saved.auto_continue = false;
        } else {
            state.saved.held = false;
            state.saved.auto_continue = true;
        }
        // Explicit stop wins over late delivery until the user resumes the conversation.
        if state.intent != Intent::StopReply || intent == Intent::StopReply {
            state.intent = intent;
        }
        drop(state);
        self.notify();
    }
    pub fn intent(&self) -> Intent {
        self.state.lock().unwrap().intent
    }
    pub fn take_intent(&self) -> Intent {
        std::mem::take(&mut self.state.lock().unwrap().intent)
    }
    pub fn phase(&self) -> Phase {
        self.state.lock().unwrap().phase.clone()
    }
    pub fn set_phase(&self, phase: Phase) {
        let mut state = self.state.lock().unwrap();
        if state.phase == phase {
            return;
        }
        state.phase = phase;
        drop(state);
        self.notify();
    }
    pub async fn urgent(&self) {
        let mut changes = self.changed.subscribe();
        loop {
            if self.intent() != Intent::None {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_input_save_never_exposes_a_message_to_the_worker() {
        let c = Control::default();
        c.resume();
        let failed = c.enqueue_with(
            "a".into(),
            "new instruction".into(),
            Default::default(),
            |_| Err("disk full"),
        );
        assert_eq!(failed, Err("disk full"));
        assert!(c.claim().is_empty());
        assert!(c.enqueue("a".into(), "retry".into(), Default::default()));
        assert_eq!(c.claim()[0].text, "retry");
    }
    #[test]
    fn editing_reserves_fifo_position_and_ack_snapshot_does_not_mutate_live_queue() {
        let c = Control::default();
        c.resume();
        c.enqueue("a".into(), "first".into(), Default::default());
        c.enqueue("b".into(), "second".into(), Default::default());
        assert_eq!(c.edit("a", None).as_deref(), Some("first"));
        assert!(!c.pending());
        assert!(c.claim().is_empty());
        c.edit("a", Some("edited".into()));
        let claimed = c.claim();
        assert_eq!(
            claimed.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["edited", "second"]
        );
        assert_eq!(c.delivery_snapshot(&["a".into()], &[]).inputs[0].id, "b");
        assert_eq!(c.snapshot().inputs.len(), 2);
        c.acknowledge(&["a".into()]);
        assert_eq!(c.snapshot().inputs.len(), 1);
    }
    #[test]
    fn queued_input_is_fifo_claimed_once_and_acknowledged_after_delivery() {
        let c = Control::default();
        c.resume();
        assert!(c.enqueue("a".into(), "one".into(), Default::default()));
        assert!(!c.enqueue("a".into(), "duplicate".into(), Default::default()));
        c.enqueue("b".into(), "two".into(), Default::default());
        let inputs = c.claim();
        assert_eq!(
            inputs.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(c.withdraw("a").is_none());
        assert!(c.claim().is_empty());
        assert_eq!(c.snapshot().inputs.len(), 2);
        c.acknowledge(&["a".into(), "b".into()]);
        assert!(c.snapshot().inputs.is_empty());
    }
    #[test]
    fn stopping_and_restoring_hold_inputs_without_losing_or_auto_sending_them() {
        let c = Control::default();
        c.resume();
        c.enqueue("a".into(), "draft".into(), Default::default());
        c.claim();
        let restored = Control::default();
        restored.restore(c.snapshot());
        assert!(!restored.pending());
        assert!(!restored.auto_continue());
        restored.resume();
        assert_eq!(restored.claim().len(), 1);
        restored.request(Intent::StopReply);
        assert!(!restored.auto_continue());
        assert_eq!(restored.take_intent(), Intent::StopReply);
        assert_eq!(restored.snapshot().inputs.len(), 1);
    }
    #[tokio::test]
    async fn urgent_notifications_do_not_cancel_work_and_cannot_be_missed() {
        let c = Control::default();
        c.request(Intent::Detach);
        c.urgent().await;
        assert_eq!(c.take_intent(), Intent::Detach);
        let other = c.clone();
        let waiting = tokio::spawn(async move {
            other.urgent().await;
        });
        c.request(Intent::SendNow);
        waiting.await.unwrap();
    }
}
