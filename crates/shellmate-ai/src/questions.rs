//! Generic, durable user questions. Registration is a complete tool result;
//! an explicit user answer is delivered separately, exactly once, as user input.
use crate::{ContextSnapshot, Entry, LoggedCall, UserTurn};
use rig_core::{
    completion::ToolDefinition,
    message::ToolName,
    serde::{Deserialize, Serialize},
};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Wait,
    Async,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct OptionItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<OptionItem>,
    #[serde(default)]
    pub multi_select: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Request {
    pub questions: Vec<Question>,
    #[serde(default)]
    pub mode: Mode,
}
impl Request {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let request: Self = serde_json::from_value(value).map_err(|_| "提问参数无效")?;
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.questions.len()) {
            return Err("每次需要 1–3 个问题".into());
        }
        let mut ids = BTreeSet::new();
        for q in &self.questions {
            if !valid_id(&q.id)
                || !ids.insert(&q.id)
                || q.question.trim().is_empty()
                || q.question.chars().count() > 2000
                || q.options.len() > 4
            {
                return Err("问题需要唯一 id、非空正文，最多 4 个选项".into());
            }
            let mut options = BTreeSet::new();
            let mut labels = BTreeSet::new();
            for o in &q.options {
                if !valid_id(&o.id)
                    || !options.insert(&o.id)
                    || o.label.trim().is_empty()
                    || !labels.insert(o.label.trim())
                    || o.label.chars().count() > 120
                    || o.description.chars().count() > 500
                {
                    return Err("选项需要唯一 id 和名称，说明最多 500 字".into());
                }
            }
        }
        Ok(())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Answer {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub text: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "snake_case")]
pub enum Status {
    Answered,
    Skipped,
    Cancelled,
    Superseded,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Response {
    pub status: Status,
    #[serde(default)]
    pub answers: BTreeMap<String, Answer>,
}
impl Response {
    pub fn validate(&self, request: &Request) -> Result<(), String> {
        if self.status != Status::Answered {
            return if self.answers.is_empty() {
                Ok(())
            } else {
                Err("未回答状态不能携带答案".into())
            };
        }
        if self.answers.len() != request.questions.len() {
            return Err("请回答每个问题，也可以填写自定义答案".into());
        }
        for q in &request.questions {
            let a = self.answers.get(&q.id).ok_or("请回答每个问题")?;
            if a.text.chars().count() > 8000
                || a.selected.len() > 4
                || (!q.multi_select && a.selected.len() > 1)
                || a.selected.iter().collect::<BTreeSet<_>>().len() != a.selected.len()
                || a.selected
                    .iter()
                    .any(|id| !q.options.iter().any(|o| &o.id == id))
            {
                return Err("答案或选项无效".into());
            }
            if a.selected.is_empty() && a.text.trim().is_empty() {
                return Err("请选择选项或填写答案".into());
            }
        }
        Ok(())
    }
    pub fn label(&self) -> &'static str {
        match self.status {
            Status::Answered => "已回答",
            Status::Skipped => "已跳过",
            Status::Cancelled => "已取消",
            Status::Superseded => "已更新要求",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Pending {
    pub id: String,
    pub call: LoggedCall,
    pub request: Request,
    pub context: ContextSnapshot,
    pub created_at_ms: i64,
    #[serde(default)]
    pub draft: BTreeMap<String, Answer>,
    #[serde(default)]
    pub response: Option<Response>,
}
impl Pending {
    pub fn new(id: String, call: LoggedCall, request: Request, context: ContextSnapshot) -> Self {
        Self {
            id,
            call,
            request,
            context,
            created_at_ms: UserTurn::now_ms(),
            draft: Default::default(),
            response: None,
        }
    }
    pub fn receipt(&self) -> String {
        serde_json::json!({"request_id":self.id,"status":"pending","mode":self.request.mode,"message":"问题已发出，尚未收到答案。未回答不代表同意；等待实际用户回复。"}).to_string()
    }
    pub fn reply_text(&self, response: &Response) -> String {
        let answers=self.request.questions.iter().map(|q|{
            let a=response.answers.get(&q.id);serde_json::json!({"id":q.id,"question":q.question,"selected":a.map(|a|a.selected.iter().filter_map(|id|q.options.iter().find(|o|&o.id==id).map(|o|o.label.clone())).collect::<Vec<_>>()),"text":a.map(|a|&a.text)})
        }).collect::<Vec<_>>();
        format!(
            "用户对问题的反馈：{}\n跳过、取消或更新要求不是同意，不可据此执行依赖答案的操作。",
            serde_json::json!({"request_id":self.id,"status":response.status,"answers":answers})
        )
    }
}
/// Reconstruction also works when only the semantic transcript survives.
pub fn from_entries(entries: &[Entry]) -> Vec<Pending> {
    let mut questions = vec![];
    let mut context = ContextSnapshot::default();
    for e in entries {
        match e {
            Entry::User { context: c, .. } => context = c.clone(),
            Entry::Step(s) if s.call.name == "ask_user" && !s.result.is_error => {
                let Ok(request) = Request::parse(s.call.args_json.clone()) else {
                    continue;
                };
                let Ok(result) = serde_json::from_str::<serde_json::Value>(&s.result.inline) else {
                    continue;
                };
                let Some(id) = result["request_id"].as_str() else {
                    continue;
                };
                questions.push(Pending {
                    id: id.into(),
                    call: s.call.clone(),
                    request,
                    context: context.clone(),
                    created_at_ms: 0,
                    draft: Default::default(),
                    response: None,
                });
            }
            Entry::QuestionAnswer {
                request_id,
                response,
                ..
            } => {
                if let Some(q) = questions.iter_mut().find(|q| q.id == *request_id) {
                    q.response = Some(response.clone());
                }
            }
            _ => {}
        }
    }
    questions
}
pub const RULES: &str = "\n需要用户的偏好、缺失条件或有真实歧义时使用 ask_user。先利用已有上下文和可用工具查明信息，不重复询问用户已明确的要求。普通沟通可直接回复。每次优先一个问题，选项简洁且允许自定义答案。wait 暂停本 Agent 后续执行直到用户回答；async 允许继续无关工作，依赖答案的操作仍必须等待。返回 pending 只表示问题已送达，绝非用户已经回答或同意；若没有其他工作就结束当前生成，由应用等待答复。不能通过 ask_user 代替命令审批，也不收集密码/API Key。\n";
pub fn tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new("ask_user").unwrap(),
        "向用户询问缺失信息或选择；主/子代理均可使用。应用显示选项和自定义输入，用户提交后回复原任务。推荐选项不自动选中或提交。",
        serde_json::json!({"type":"object","properties":{"mode":{"type":"string","enum":["wait","async"],"default":"wait","description":"wait 等答案再继续；async 先做无关工作，答案随后作为用户反馈送达"},"questions":{"type":"array","minItems":1,"maxItems":3,"items":{"type":"object","properties":{"id":{"type":"string"},"question":{"type":"string"},"multi_select":{"type":"boolean","default":false},"options":{"type":"array","maxItems":4,"items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"},"description":{"type":"string"}},"required":["id","label"],"additionalProperties":false}}},"required":["id","question"],"additionalProperties":false}}},"required":["questions"],"additionalProperties":false}),
    )
}

impl crate::interaction::Control {
    pub fn register_question(&self, question: Pending) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.saved.questions.iter().any(|q| q.id == question.id) {
            return Ok(());
        }
        if state
            .saved
            .questions
            .iter()
            .filter(|q| q.response.is_none())
            .count()
            >= 16
        {
            return Err("已有较多问题待回答，请等待用户回复".into());
        }
        state.saved.questions.push(question);
        drop(state);
        self.notify();
        Ok(())
    }
    pub fn restore_questions(&self, entries: &[Entry]) {
        let mut state = self.state.lock().unwrap();
        for q in from_entries(entries) {
            if let Some(old) = state.saved.questions.iter_mut().find(|old| old.id == q.id) {
                if q.response.is_some() {
                    old.response = q.response;
                }
            } else {
                state.saved.questions.push(q);
            }
        }
        state.saved.inputs.retain(|i| {
            i.question_id.as_ref().is_none_or(|id| {
                !entries
                    .iter()
                    .any(|e| matches!(e,Entry::QuestionAnswer{request_id,..}if request_id==id))
            })
        });
    }
    pub fn has_questions(&self, all: bool) -> bool {
        self.state
            .lock()
            .unwrap()
            .saved
            .questions
            .iter()
            .any(|q| q.response.is_none() && (all || q.request.mode == Mode::Wait))
    }
    pub fn update_question_draft(&self, id: &str, answers: BTreeMap<String, Answer>) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(q) = state
            .saved
            .questions
            .iter_mut()
            .find(|q| q.id == id && q.response.is_none())
        else {
            return false;
        };
        if q.draft == answers {
            return false;
        }
        q.draft = answers;
        true
    }
    pub fn question(&self, id: &str) -> Option<Pending> {
        self.state
            .lock()
            .unwrap()
            .saved
            .questions
            .iter()
            .find(|q| q.id == id)
            .cloned()
    }
    /// Commit the answer and its delivery together before exposing either to workers.
    /// The callback must not reenter this Control.
    pub fn answer_question(
        &self,
        id: &str,
        response: Response,
        save: impl FnOnce(&crate::interaction::Snapshot) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let mut snapshot = state.saved.clone();
        let q = snapshot
            .questions
            .iter_mut()
            .find(|q| q.id == id)
            .ok_or("问题已不可用")?;
        if q.response.is_some() {
            return Err("这个问题已处理，请勿重复提交".into());
        }
        response.validate(&q.request)?;
        let text = q.reply_text(&response);
        q.response = Some(response);
        snapshot.inputs.push_back(crate::interaction::Input {
            question_id: Some(id.into()),
            sent_at_ms: Some(UserTurn::now_ms()),
            origin: Some(crate::MessageOrigin::User),
            id: format!("answer:{id}"),
            text,
            context: q.context.clone(),
            attachments: vec![],
            claimed: false,
            editing: false,
        });
        save(&snapshot)?;
        state.saved = snapshot;
        drop(state);
        self.notify();
        Ok(())
    }
    pub fn input_entry(&self, input: &crate::interaction::Input) -> Entry {
        if let Some(id) = &input.question_id
            && let Some(q) = self.question(id)
            && let Some(response) = q.response
        {
            Entry::QuestionAnswer {
                request_id: id.clone(),
                response,
                text: input.text.clone(),
                sent_at_ms: input.sent_at_ms,
            }
        } else {
            Entry::User {
                attachments: input.attachments.clone(),
                sent_at_ms: input.sent_at_ms,
                origin: input.origin,
                text: input.text.clone(),
                context: input.context.clone(),
            }
        }
    }
    pub(crate) fn supersede_questions(&self) -> Vec<Entry> {
        let mut state = self.state.lock().unwrap();
        let mut entries = vec![];
        for q in &mut state.saved.questions {
            if q.response.is_none() && q.request.mode == Mode::Wait {
                let response = Response {
                    status: Status::Superseded,
                    answers: Default::default(),
                };
                entries.push(Entry::QuestionAnswer {
                    request_id: q.id.clone(),
                    text: q.reply_text(&response),
                    response: response.clone(),
                    sent_at_ms: Some(UserTurn::now_ms()),
                });
                q.response = Some(response);
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending() -> Pending {
        Pending::new(
            "request".into(),
            LoggedCall {
                id: "call".into(),
                name: "ask_user".into(),
                args_json: serde_json::json!({}),
            },
            Request {
                mode: Mode::Wait,
                questions: vec![Question {
                    id: "choice".into(),
                    question: "如何继续？".into(),
                    options: vec![
                        OptionItem {
                            id: "a".into(),
                            label: "方案 A".into(),
                            description: String::new(),
                        },
                        OptionItem {
                            id: "b".into(),
                            label: "方案 B".into(),
                            description: String::new(),
                        },
                    ],
                    multi_select: false,
                }],
            },
            Default::default(),
        )
    }
    #[test]
    fn validation_covers_multi_select_custom_text_and_skipping() {
        let q = pending();
        let mut response = Response {
            status: Status::Answered,
            answers: BTreeMap::from([(
                "choice".into(),
                Answer {
                    selected: vec!["a".into(), "b".into()],
                    text: String::new(),
                },
            )]),
        };
        assert!(response.validate(&q.request).is_err());
        let mut multi = q.request.clone();
        multi.questions[0].multi_select = true;
        assert!(response.validate(&multi).is_ok());
        response.answers.get_mut("choice").unwrap().selected = vec!["bad".into()];
        assert!(response.validate(&multi).is_err());
        response.answers.insert(
            "choice".into(),
            Answer {
                selected: vec![],
                text: "我有另一个方案".into(),
            },
        );
        assert!(response.validate(&q.request).is_ok());
        assert!(
            Response {
                status: Status::Skipped,
                answers: Default::default()
            }
            .validate(&q.request)
            .is_ok()
        );
        let mut invalid = q.request.clone();
        invalid.questions.push(invalid.questions[0].clone());
        assert!(invalid.validate().is_err());
    }
    #[test]
    fn durable_answer_survives_restore_without_duplicate_or_fifo_edits() {
        let c = crate::interaction::Control::default();
        c.register_question(pending()).unwrap();
        let draft = BTreeMap::from([(
            "choice".into(),
            Answer {
                selected: vec!["b".into()],
                text: "尚未提交".into(),
            },
        )]);
        assert!(c.update_question_draft("request", draft.clone()));
        assert!(!c.pending());
        assert!(c.question("request").unwrap().response.is_none());
        let draft_restore = crate::interaction::Control::default();
        draft_restore.restore(c.snapshot());
        assert_eq!(draft_restore.question("request").unwrap().draft, draft);
        let response = Response {
            status: Status::Answered,
            answers: BTreeMap::from([(
                "choice".into(),
                Answer {
                    selected: vec!["b".into()],
                    text: "保留原文件".into(),
                },
            )]),
        };
        c.answer_question("request", response.clone(), |_| Ok(()))
            .unwrap();
        let restored = crate::interaction::Control::default();
        restored
            .restore(serde_json::from_str(&serde_json::to_string(&c.snapshot()).unwrap()).unwrap());
        assert!(!restored.pending());
        assert!(
            restored
                .answer_question("request", response, |_| Ok(()))
                .is_err()
        );
        assert!(restored.withdraw("answer:request").is_none());
        assert!(
            restored
                .edit("answer:request", Some("wrong".into()))
                .is_none()
        );
        restored.resume();
        let input = restored.claim_first().unwrap();
        assert!(
            matches!(restored.input_entry(&input),Entry::QuestionAnswer{request_id,..}if request_id=="request")
        );
        restored.acknowledge(&[input.id]);
        assert!(!restored.pending());
    }
}
