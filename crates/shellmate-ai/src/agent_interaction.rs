use super::*;
use crate::interaction::{Control, Intent, Phase};

pub(super) async fn urgent(control: Option<Control>) {
    match control {
        Some(control) => control.urgent().await,
        None => std::future::pending().await,
    }
}
impl State<'_> {
    pub(super) fn phase(&self, phase: Phase) {
        if let Some(c) = self.host.control() {
            c.set_phase(phase);
        }
    }
    fn close_unexecuted(&mut self, raw: RigCall, call: Option<ToolCall>) {
        let reason = "用户更新了指令，本步骤未执行";
        let display = call.as_ref().and_then(|call| {
            crate::StepDisplay::from_item(
                ToolOutcome::Denied {
                    reason: Some(reason.into()),
                }
                .item(call),
            )
        });
        self.entry(Entry::Step(crate::Step {
            message_id: self.message_id,
            call: crate::LoggedCall {
                id: raw.id.to_string(),
                name: raw.function.name.to_string(),
                args_json: serde_json::Value::Object(raw.function.arguments),
            },
            result: crate::LoggedResult {
                inline: reason.into(),
                output_id: None,
                is_error: true,
                cleared: false,
                subagent: None,
            },
            display,
        }));
        self.steps += 1;
        self.turn.as_mut().unwrap().steps += 1;
    }
    fn close_pending(&mut self) {
        if let Some((raw, call)) = self.active.take() {
            self.close_unexecuted(raw, Some(call));
        }
        let pending = std::mem::take(&mut self.turn.as_mut().unwrap().pending);
        for item in pending {
            match item {
                Pending::Call(raw, call) => self.close_unexecuted(*raw, Some(call)),
                Pending::Subagent(raw, _) | Pending::Local(raw, _) | Pending::Invalid(raw, _) => {
                    self.close_unexecuted(*raw, None)
                }
                Pending::AgentMessage(raw, ..) => self.close_unexecuted(*raw, None),
                Pending::Text(text) => self.text(&text),
                Pending::Truncated(_) => {}
            }
        }
    }
    pub(super) fn receive_input(&mut self) -> Option<AgentEvent> {
        if self.turn.is_none() || self.model.is_some() || self.compacting.is_some() {
            return None;
        }
        let control = self.host.control()?;
        let intent = control.intent();
        if intent == Intent::None && (!control.pending() || self.active.is_some()) {
            return None;
        }
        let intent = control.take_intent();
        self.close_pending();
        if intent == Intent::StopReply {
            self.entry(Entry::Notice {
                kind: "interaction".into(),
                text: "回复已停止；已经启动的异步任务继续在原通道运行".into(),
            });
            self.done = true;
            control.set_phase(Phase::Idle);
            return Some(AgentEvent::Released(ReleaseReason::Stopped));
        }
        let inputs = control.claim();
        if inputs.is_empty() {
            if intent == Intent::Detach {
                self.entry(Entry::Notice {
                    kind: "interaction".into(),
                    text: "已结束等待，任务在原执行通道继续运行".into(),
                });
                self.done = true;
                control.set_phase(Phase::Idle);
                return Some(AgentEvent::Released(ReleaseReason::Detached));
            }
            return None;
        }
        let ids = inputs.iter().map(|i| i.id.clone()).collect();
        for input in inputs {
            self.entry(Entry::User {
                sent_at_ms: input.sent_at_ms,
                origin: input.origin,
                text: input.text,
                context: input.context,
            });
        }
        let turn = self.turn.as_mut().unwrap();
        turn.turn = self.conversation.latest_user().unwrap().clone();
        turn.turns = self.conversation.turns().len();
        turn.capabilities = turn.base_capabilities.clone();
        turn.invalid_calls = 0;
        self.done = false;
        self.manual = false;
        self.failures = 0;
        self.retry_wait = None;
        self.error = None;
        Some(AgentEvent::InputsAccepted(ids))
    }
    pub(super) fn interrupt_stream(&mut self) {
        self.model = None;
        self.retry_wait = None;
        self.entry(Entry::Notice {
            kind: "interaction".into(),
            text: "已中断当前回复，正在处理新指令；未完成的工具调用没有执行".into(),
        });
    }
}
