//! Durable semantic log. A Step atomically owns its call and result: no persisted
//! or replayed half-pairs. Turn/Answer are read-only projections for the drawer.
//! Provider-native reasoning deliberately lives in agent::Resume, never here.
use crate::{
    Answer, AnswerItem, CommandCard, ContextSnapshot, PermissionMode, StepStatus, Turn, Usage,
    UserTurn, agent::Target, policy::Class,
};
use rig_core::serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct LoggedCall {
    pub id: String,
    pub name: String,
    pub args_json: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct LoggedResult {
    pub inline: String,
    /// Local spool id; absent for calls that never executed.
    pub output_id: Option<String>,
    pub cleared: bool,
    pub is_error: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct StepDisplay {
    pub command: String,
    pub target: Target,
    pub purpose: String,
    pub class: Class,
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub automatic: bool,
    pub output: String,
}
impl StepDisplay {
    pub fn from_item(item: AnswerItem) -> Option<Self> {
        if let AnswerItem::Step {
            command,
            target,
            purpose,
            class,
            status,
            exit_code,
            duration_ms,
            automatic,
            output,
        } = item
        {
            Some(Self {
                command,
                target,
                purpose,
                class,
                status,
                exit_code,
                duration_ms,
                automatic,
                output,
            })
        } else {
            None
        }
    }
    pub fn item(&self) -> AnswerItem {
        AnswerItem::Step {
            command: self.command.clone(),
            target: self.target,
            purpose: self.purpose.clone(),
            class: self.class,
            status: self.status.clone(),
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            automatic: self.automatic,
            output: self.output.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Step {
    pub message_id: usize,
    pub call: LoggedCall,
    pub result: LoggedResult,
    /// Invalid/unknown tool requests still have paired results, but no step card.
    pub display: Option<StepDisplay>,
}
impl Step {
    pub fn wire_text(&self) -> String {
        if !self.result.cleared {
            return self.result.inline.clone();
        }
        let command = self
            .display
            .as_ref()
            .map(|d| d.command.as_str())
            .unwrap_or("");
        let exit = self
            .display
            .as_ref()
            .and_then(|d| d.exit_code)
            .map_or_else(|| "未知".into(), |n| n.to_string());
        let lines: Vec<_> = self.result.inline.lines().collect();
        let head = lines.iter().take(3).copied().collect::<Vec<_>>().join("\n");
        let tail = lines
            .iter()
            .skip(lines.len().saturating_sub(3))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let mut text = format!(
            "[较早的输出已移出上下文] 命令：{command} · 退出码 {exit} · 原长 {}\n{head} … {tail}",
            self.result.inline.chars().count()
        );
        if let Some(id) = &self.result.output_id {
            text.push_str(&format!("\n完整输出：read_output(id=\"{id}\")"));
        }
        text
    }
    /// Denials/failures remain explicit even for v9's empty stored output excerpt.
    pub(crate) fn result_text(&self) -> String {
        let text = self.wire_text();
        if self.result.cleared
            && let Some(display) = &self.display
            && !matches!(
                display.status,
                StepStatus::Completed | StepStatus::TimedOut | StepStatus::Background
            )
        {
            return format!("{}\n{text}", display.item().wire_text());
        }
        text
    }
    pub fn plain_text(&self) -> String {
        let display = self
            .display
            .as_ref()
            .map(|d| d.item().wire_text())
            .unwrap_or_else(|| format!("工具 {}（{}）", self.call.name, self.call.args_json));
        format!("{display}\n{}", self.wire_text())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "snake_case")]
pub enum CompactTrigger {
    PreRequest,
    Reactive,
    Manual,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct DeniedCommand {
    pub command: String,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct RetainedState {
    pub denied_commands: Vec<DeniedCommand>,
    pub permission_mode: PermissionMode,
    /// M3 fills these slots from the task registry and spool inventory.
    pub running_tasks: Vec<String>,
    pub readable_output_ids: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Compaction {
    /// None marks tier 1 only, which does not replace the preceding context.
    pub summary: Option<String>,
    pub retained_user: Vec<String>,
    pub state: RetainedState,
    pub context: ContextSnapshot,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub trigger: CompactTrigger,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    User {
        text: String,
        context: ContextSnapshot,
    },
    Assistant {
        text: String,
        message_id: usize,
    },
    Step(Step),
    CommandCard {
        card: CommandCard,
        message_id: usize,
    },
    Compaction(Compaction),
    Notice {
        kind: String,
        text: String,
    },
}
impl Entry {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::Step(_) => "step",
            Self::CommandCard { .. } => "command_card",
            Self::Compaction(_) => "compaction",
            Self::Notice { .. } => "notice",
        }
    }
    pub(crate) fn message_id(&self) -> Option<usize> {
        match self {
            Self::Assistant { message_id, .. } | Self::CommandCard { message_id, .. } => {
                Some(*message_id)
            }
            Self::Step(s) => Some(s.message_id),
            _ => None,
        }
    }
}
#[derive(Clone, Debug)]
pub enum LogUpdate {
    Remote(crate::remote::State),
    BeforeUser(Vec<Entry>),
    Memory(crate::memory::Snapshot),
    Append(Box<Entry>),
    ClearResults(Vec<usize>),
    Usage(Usage),
}
#[derive(Clone, Debug, Default)]
pub(crate) struct Accounting {
    pub usage: Option<(usize, usize)>,
}
#[derive(Clone, Default)]
pub struct Conversation {
    pub(crate) remote: Option<crate::remote::State>,
    pub(crate) memory: Option<crate::memory::Snapshot>,
    entries: Vec<Entry>,
    turns: Vec<Turn>,
    pub(crate) accounting: Accounting,
}
impl PartialEq for Conversation {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}
impl Eq for Conversation {}
impl Conversation {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        let mut c = Self::default();
        for entry in entries {
            c.append(entry);
        }
        c
    }
    /// Adapter for v9 and demo scenes. Legacy calls get portable, unique ids.
    pub fn from_turns(turns: Vec<Turn>) -> Self {
        let mut c = Self::default();
        for turn in turns {
            c.push_turn(turn);
        }
        c
    }
    pub fn push_turn(&mut self, turn: Turn) {
        self.push_user(turn.user);
        if let Some(answer) = turn.answer {
            for item in answer.items {
                self.push_item(item);
            }
        }
    }
    pub fn push_answer(&mut self, answer: Answer) {
        for item in answer.items {
            self.push_item(item);
        }
    }
    pub fn push_user(&mut self, user: UserTurn) {
        self.append(Entry::User {
            text: user.question,
            context: user.context,
        });
    }
    pub fn push_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.append(Entry::Assistant {
                text: text.into(),
                message_id: self.entries.len(),
            });
        }
    }
    pub fn push_item(&mut self, item: AnswerItem) {
        let message_id = self.entries.len();
        let entry = match item {
            AnswerItem::Text(text) => Entry::Assistant { text, message_id },
            AnswerItem::Command(card) => Entry::CommandCard { card, message_id },
            AnswerItem::Compaction(c) => Entry::Compaction(c),
            AnswerItem::Notice(text) => Entry::Notice {
                kind: "notice".into(),
                text,
            },
            mut item @ AnswerItem::Step { .. } => {
                item.cap_stored();
                let display = StepDisplay::from_item(item).unwrap();
                let inline = display.output.clone();
                Entry::Step(Step {
                    message_id,
                    call: LoggedCall {
                        id: format!(
                            "legacy_{}_{}",
                            self.turns.len().saturating_sub(1),
                            message_id
                        ),
                        name: "run_command".into(),
                        args_json: serde_json::json!({"command":display.command,"purpose":display.purpose,"target":display.target.id()}),
                    },
                    result: LoggedResult {
                        inline,
                        output_id: None,
                        cleared: true,
                        is_error: !matches!(
                            display.status,
                            StepStatus::Completed | StepStatus::Background
                        ),
                    },
                    display: Some(display),
                })
            }
        };
        self.append(entry);
    }
    pub fn apply(&mut self, update: LogUpdate) {
        match update {
            LogUpdate::Remote(state) => self.remote = Some(state),
            LogUpdate::BeforeUser(context) => {
                let position = self
                    .entries
                    .iter()
                    .rposition(|e| matches!(e, Entry::User { .. }))
                    .unwrap_or(self.entries.len());
                let count = context.len();
                let mut entries = std::mem::take(&mut self.entries);
                for entry in &mut entries[position..] {
                    let id = match entry {
                        Entry::Assistant { message_id, .. }
                        | Entry::CommandCard { message_id, .. } => Some(message_id),
                        Entry::Step(step) => Some(&mut step.message_id),
                        _ => None,
                    };
                    if let Some(id) = id
                        && *id >= position
                    {
                        *id += count;
                    }
                }
                entries.splice(position..position, context);
                self.turns.clear();
                self.accounting.usage = None;
                for entry in entries {
                    self.append(entry);
                }
            }
            LogUpdate::Memory(snapshot) => self.memory = Some(snapshot),
            LogUpdate::Append(e) => self.append(*e),
            LogUpdate::ClearResults(positions) => {
                for pos in positions {
                    if let Some(Entry::Step(s)) = self.entries.get_mut(pos) {
                        s.result.cleared = true;
                    }
                }
                self.accounting.usage = None;
            }
            LogUpdate::Usage(u) => {
                if u.input_tokens.is_some() || u.output_tokens.is_some() {
                    self.accounting.usage = Some((
                        u.input_tokens
                            .unwrap_or(0)
                            .saturating_add(u.output_tokens.unwrap_or(0))
                            as usize,
                        self.entries.len(),
                    ));
                }
            }
        }
    }
    pub fn append(&mut self, mut entry: Entry) {
        if let Entry::Step(step) = &mut entry {
            let mut used = self
                .entries
                .iter()
                .filter_map(|e| {
                    if let Entry::Step(s) = e {
                        Some(s.call.id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            step.call.id =
                crate::context::portable_id(&step.call.id, self.entries.len(), &mut used);
        }
        match &entry {
            Entry::User { text, context } => self.turns.push(Turn {
                user: UserTurn::new(text, context.clone()),
                answer: None,
            }),
            _ => {
                if let Some(turn) = self.turns.last_mut() {
                    let answer = turn.answer.get_or_insert_with(Answer::default);
                    match &entry {
                        Entry::Assistant { text, .. } => answer.push_text(text),
                        Entry::CommandCard { card, .. } => {
                            answer.items.push(AnswerItem::Command(card.clone()))
                        }
                        Entry::Step(s) => {
                            if let Some(display) = &s.display {
                                answer.items.push(display.item());
                            } else if s.call.name == "memory" {
                                answer.items.push(AnswerItem::Notice(crate::memory::notice(
                                    &s.call.args_json,
                                    s.result.is_error,
                                )));
                            } else if matches!(s.call.name.as_str(), "read_output" | "stop_task") {
                                let label = if s.call.name == "read_output" {
                                    "读取输出"
                                } else {
                                    "已停止任务"
                                };
                                let id = s
                                    .call
                                    .args_json
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let status = if s.result.is_error {
                                    " · 未完成"
                                } else {
                                    ""
                                };
                                answer
                                    .items
                                    .push(AnswerItem::Notice(format!("{label} · {id}{status}")));
                            }
                        }
                        Entry::Compaction(c) => {
                            answer.items.push(AnswerItem::Compaction(c.clone()))
                        }
                        Entry::Notice { kind, .. } if kind == "remote_cwd" => {}
                        Entry::Notice { kind, text } => {
                            let text = if kind == shellmate_core::remote_instructions::CONTEXT_KIND
                            {
                                shellmate_core::remote_instructions::notice(text)
                            } else {
                                text.clone()
                            };
                            answer.items.push(AnswerItem::Notice(text))
                        }
                        Entry::User { .. } => unreachable!(),
                    }
                }
            }
        }
        if matches!(&entry, Entry::Compaction(c) if c.summary.is_some()) {
            self.accounting.usage = None;
        }
        self.entries.push(entry);
    }
    pub fn latest_user(&self) -> Option<&UserTurn> {
        self.turns.last().map(|t| &t.user)
    }
    pub(crate) fn view_start(&self) -> usize {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction(c) if c.summary.is_some()))
            .unwrap_or(0)
    }
    pub(crate) fn clear_candidates(&self) -> Vec<usize> {
        let mut positions: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .skip(self.view_start())
            .filter_map(|(i, e)| matches!(e, Entry::Step(_)).then_some(i))
            .collect();
        positions.truncate(positions.len().saturating_sub(5));
        positions.retain(|i| matches!(&self.entries[*i], Entry::Step(s) if !s.result.cleared));
        positions
    }
}
