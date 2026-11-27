//! Replay data for Responses, separate from user-visible text and tool results.
//! Only ciphertext and supported item identities survive; never raw reasoning text.
use crate::{Config, Conversation, Entry, Provider, context::NativeMessages};
use rig_core::{
    message::{AssistantContent, AssistantMessage, Fingerprint, Reasoning},
    serde::{Deserialize, Serialize},
};
use serde_json::{Value, json};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct ResponseReplay {
    endpoint: String,
    model: String,
    prefix: Fingerprint,
    instructions: Fingerprint,
    tools: Fingerprint,
    assistant: Value,
}
impl std::fmt::Debug for ResponseReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponseReplay { .. }")
    }
}
impl ResponseReplay {
    pub(crate) fn capture(
        config: &Config,
        prefix: Fingerprint,
        instructions: &str,
        tools: Fingerprint,
        message: &AssistantMessage,
    ) -> Option<Self> {
        if !matches!(config.provider, Provider::OpenAiResponses { .. }) {
            return None;
        }
        let mut saved = message.clone();
        saved.content = message.content.iter().filter_map(sanitized).collect();
        let assistant = serde_json::to_value(saved).ok()?;
        // A huge opaque item must not make checkpoints unbounded. The semantic
        // transcript remains sufficient for a fresh reasoning context.
        if serde_json::to_vec(&assistant).ok()?.len() > 2 * 1024 * 1024 {
            return None;
        }
        Some(Self {
            endpoint: config.provider.responses_base_url().ok()?,
            model: config.model.clone(),
            prefix,
            instructions: Fingerprint::of(&instructions),
            tools,
            assistant,
        })
    }
    fn restore(
        &self,
        config: &Config,
        prefix: Fingerprint,
        instructions: &str,
        tools: Fingerprint,
    ) -> Option<AssistantMessage> {
        if self.tools != tools
            || self.prefix != prefix
            || self.instructions != Fingerprint::of(&instructions)
            || self.model != config.model
            || self.endpoint != config.provider.responses_base_url().ok()?
        {
            return None;
        }
        let message: AssistantMessage = serde_json::from_value(self.assistant.clone()).ok()?;
        let origin = message.origin.as_ref()?;
        if origin.api.as_str() != "openai.responses" || origin.model != config.model {
            return None;
        }
        Some(message)
    }
}
fn sanitized(block: &AssistantContent) -> Option<AssistantContent> {
    let item = block.native_item();
    match block {
        AssistantContent::Reasoning(_) => {
            let item = item?;
            let cipher = item["encrypted_content"]
                .as_str()
                .filter(|s| !s.is_empty())?;
            let mut kept = json!({"type":"reasoning","summary":[],"encrypted_content":cipher});
            if let Some(id) = item["id"].as_str() {
                kept["id"] = id.into();
            }
            Some(AssistantContent::Reasoning(Reasoning::new("")).with_native(kept))
        }
        AssistantContent::Text(text) => {
            let mut kept = json!({"type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":text.text,"annotations":[]}]});
            if let Some(item) = item {
                if let Some(id) = item["id"].as_str() {
                    kept["id"] = id.into();
                }
                if let Some(phase @ ("commentary" | "final_answer")) = item["phase"].as_str() {
                    kept["phase"] = phase.into();
                }
            }
            Some(AssistantContent::text(&text.text).with_native(kept))
        }
        AssistantContent::ToolCall(call) => {
            let mut call = call.clone();
            call.native = None;
            let mut kept = json!({"type":"function_call","name":call.function.name.as_str(),
                "call_id":call.id.to_string(),"arguments":serde_json::to_string(&call.function.arguments).ok()?,"status":"completed"});
            if let Some(id) = item.and_then(|i| i["id"].as_str()) {
                kept["id"] = id.into();
            }
            Some(AssistantContent::ToolCall(call).with_native(kept))
        }
        _ => None,
    }
}
/// Hash the semantic prefix, excluding replay data and presentation-only fields.
/// Output compaction or edited tool results invalidate subsequent reasoning.
pub(crate) fn prefix_start(start: usize) -> Fingerprint {
    Fingerprint::of(&("responses-prefix-v1", start))
}
pub(crate) fn prefix_next(prefix: Fingerprint, entry: &Entry) -> Fingerprint {
    let value = match entry {
        Entry::User { .. } => {
            let mut value = serde_json::to_value(entry).expect("semantic entry serializes");
            value.as_object_mut().unwrap().remove("sent_at_ms");
            value
        }
        Entry::Assistant {
            text, message_id, ..
        } => json!(["assistant", message_id, text]),
        Entry::Step(s) => json!(["step", s.message_id, s.call, s.result_text()]),
        _ => serde_json::to_value(entry).expect("semantic entry serializes"),
    };
    Fingerprint::of(&(prefix, value))
}
impl Conversation {
    pub(crate) fn response_prefix(&self, end: usize) -> Fingerprint {
        let start = self.view_start();
        self.entries()[start..end]
            .iter()
            .fold(prefix_start(start), prefix_next)
    }
    pub(crate) fn responses_native(
        &self,
        config: &Config,
        instructions: &str,
        tools: Fingerprint,
    ) -> NativeMessages {
        let start = self.view_start();
        let mut prefix = prefix_start(start);
        let mut native = NativeMessages::new();
        for entry in &self.entries()[start..] {
            if let Entry::Assistant {
                message_id,
                replay: Some(replay),
                ..
            } = entry
                && let Some(message) = replay.restore(config, prefix, instructions, tools)
            {
                native.insert(*message_id, message);
            }
            prefix = prefix_next(prefix, entry);
        }
        native
    }
}
