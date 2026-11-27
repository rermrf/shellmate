//! Provider-exposed reasoning for presentation. Never reconstructed from ciphertext,
//! mixed into assistant answers, or replayed as ordinary model instructions.
use rig_core::{
    message::AssistantContent,
    serde::{Deserialize, Serialize},
    streaming::StreamEvent,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "snake_case")]
pub enum ThinkingKind {
    Process,
    Summary,
}
impl ThinkingKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Process => "思考过程",
            Self::Summary => "思考摘要",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct ThinkingBlock {
    /// Stable across fragments and history reconstruction, scoped to one transcript.
    pub id: String,
    pub kind: ThinkingKind,
    pub text: String,
    pub complete: bool,
}
impl ThinkingBlock {
    pub fn merge(&mut self, fragment: &Self) {
        self.text.push_str(&fragment.text);
        self.complete |= fragment.complete;
        self.kind = fragment.kind;
    }
}
#[derive(Default)]
pub(crate) struct Tracker(BTreeMap<usize, Tracked>);
#[derive(Default)]
struct Tracked {
    text: String,
    emitted: usize,
}
impl Tracker {
    pub fn event(
        &mut self,
        event: &StreamEvent,
        message: usize,
        summary: bool,
    ) -> Option<ThinkingBlock> {
        let part = event.part().index();
        let (text, complete, kind) = match event {
            StreamEvent::Reasoning { text, .. } => (
                text.clone(),
                false,
                if summary {
                    ThinkingKind::Summary
                } else {
                    ThinkingKind::Process
                },
            ),
            StreamEvent::End {
                content: AssistantContent::Reasoning(r),
                ..
            } if !r.redacted => {
                // End carries the full block on some adapters. Emit only its missing suffix.
                let prior = self
                    .0
                    .get(&part)
                    .map(|t| t.text.as_str())
                    .unwrap_or_default();
                let text = r.text.strip_prefix(prior).unwrap_or_default().to_owned();
                (text, true, kind(r, summary))
            }
            _ => return None,
        };
        let prior = self.0.entry(part).or_default();
        if text.is_empty() && (!complete || prior.text.is_empty()) {
            return None;
        }
        prior.text.push_str(&text);
        if prior.text.trim().is_empty() {
            return None;
        }
        let text = prior.text[prior.emitted..].to_owned();
        prior.emitted = prior.text.len();
        Some(ThinkingBlock {
            id: format!("{message}:{part}"),
            kind,
            text,
            complete,
        })
    }
}
pub(crate) fn kind(reasoning: &rig_core::message::Reasoning, summary: bool) -> ThinkingKind {
    if summary
        && !reasoning.native.as_ref().is_some_and(|n| {
            n.item["content"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["type"] == "reasoning_text"))
        })
    {
        ThinkingKind::Summary
    } else {
        ThinkingKind::Process
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnswerItem, ContextSnapshot, Conversation, Entry, UserTurn};
    use rig_core::{message::Reasoning, streaming::Part};
    fn end(part: u32, text: &str) -> StreamEvent {
        StreamEvent::End {
            part: Part::new(part),
            content: AssistantContent::Reasoning(Reasoning::new(text)),
        }
    }
    #[test]
    fn end_payload_deduplicates_streamed_text_and_supplies_missing_suffix() {
        let mut t = Tracker::default();
        let first = t
            .event(
                &StreamEvent::Reasoning {
                    part: Part::new(2),
                    text: "先检查".into(),
                },
                9,
                false,
            )
            .unwrap();
        let last = t.event(&end(2, "先检查配置"), 9, false).unwrap();
        assert_eq!(last.text, "配置");
        assert!(last.complete);
        assert_eq!(first.id, last.id);
        let once = t.event(&end(3, "完整摘要"), 9, true).unwrap();
        assert_eq!(once.text, "完整摘要");
        assert_eq!(once.kind, ThinkingKind::Summary);
        assert!(t.event(&end(4, ""), 9, true).is_none());
        let mut redacted = Reasoning::new("WITHHELD");
        redacted.redacted = true;
        assert!(
            t.event(
                &StreamEvent::End {
                    part: Part::new(5),
                    content: AssistantContent::Reasoning(redacted)
                },
                9,
                false
            )
            .is_none()
        );
    }
    #[test]
    fn projection_preserves_order_copy_separation_and_restore_without_wire_injection() {
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("检查", ContextSnapshot::default()));
        let fragment = |id: &str, text: &str, complete| ThinkingBlock {
            id: id.into(),
            kind: ThinkingKind::Process,
            text: text.into(),
            complete,
        };
        log.append(Entry::Thinking {
            message_id: 1,
            block: fragment("1:0", "思考一", false),
        });
        log.append(Entry::Thinking {
            message_id: 1,
            block: fragment("1:0", "后半", true),
        });
        log.append(Entry::Assistant {
            message_id: 1,
            text: "回复一".into(),
            replay: None,
        });
        log.append(Entry::Thinking {
            message_id: 1,
            block: fragment("1:2", "思考二", true),
        });
        log.append(Entry::Assistant {
            message_id: 1,
            text: "回复二".into(),
            replay: None,
        });
        let restored = Conversation::from_entries(
            serde_json::from_str(&serde_json::to_string(log.entries()).unwrap()).unwrap(),
        );
        let answer = restored.turns()[0].answer.as_ref().unwrap();
        assert_eq!(answer.text(), "回复一回复二");
        assert!(
            matches!(&answer.items[..],[AnswerItem::Thinking(b),AnswerItem::Text(_),AnswerItem::Thinking(_),AnswerItem::Text(_)]if b.text=="思考一后半" && b.complete)
        );
        let wire =
            serde_json::to_string(&restored.prepare(&Default::default()).unwrap().chat_history)
                .unwrap();
        assert!(!wire.contains("思考一"));
        assert!(!wire.contains("思考二"));
        assert!(!answer.wire_text().contains("思考一"));
    }
}
