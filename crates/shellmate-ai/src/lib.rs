//! AI model and agent boundary; command execution is delegated to the host.
pub mod agent;
pub mod interaction;
mod client;
mod config;
mod conversation;
mod log;
pub mod context;
pub use log::{MessageOrigin, Conversation, Entry, LoggedCall, LoggedResult, Step, StepDisplay, Compaction, CompactTrigger, RetainedState, DeniedCommand, LogUpdate};
pub use context::{ContextWindow, WindowSource, resolve_window, ContextBudget, ContextBreakdown};
pub mod output;
pub mod policy;
pub use client::{Event, ProviderClient, ResponseStream, RigClient, Usage};
pub use config::{ApiKey, Config, Provider, ReasoningLevel, Timeouts, OutputTokens, catalog_spec};
pub use conversation::{
    Answer, AnswerItem, ContextSnapshot, estimate_tokens, recent_context, StepStatus, Turn,
    UserTurn,
};
use rig_core::serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}
impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "低风险",
            Self::Medium => "中风险",
            Self::High => "高风险",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct CommandCard {
    pub command: String,
    pub explanation: String,
    pub risk: Risk,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub enum AiError {
    #[error("尚未配置 AI 助手，请先设置服务商、模型和 API Key")]
    NotConfigured,
    #[error("AI 配置无效，请检查 Base URL 和模型名称")]
    InvalidConfig,
    #[error("当前模型不支持所选思考等级，请在设置中改为「默认」或其他等级")]
    UnsupportedReasoning,
    #[error("Key 无效或无权限")]
    Authentication,
    #[error("模型名称不存在")]
    ModelNotFound,
    #[error("请求过于频繁 / 限流，请稍后重试")]
    RateLimited,
    #[error("余额不足，请检查服务商账户")]
    Quota,
    #[error("上下文过长，请开始新对话或缩短问题")]
    ContextTooLong,
    #[error("上下文刚压缩就又满了（可能是单条输出过大）。可以用 /clear 开始新对话，或调大上下文窗口")]
    CompactionThrash,
    #[error("请求超时，请稍后重试")]
    Timeout,
    #[error("网络或 DNS 连接失败，请检查网络与 Base URL")]
    Network,
    #[error("服务暂时不可用，请稍后重试")]
    Unavailable,
    #[error("执行记录未能保存到本机，请检查存储空间后重试")]
    Persistence,
    #[error("服务返回了无法识别的回答，请重试")]
    InvalidResponse,
    #[error("会话已断开")]
    Disconnected,
    #[error("已停止生成")]
    Cancelled,
}

pub use agent::{
    AgentEvent, PauseReason, AgentStream, Capabilities, Limits, Resume, Target, ToolCall, ToolHost, ToolOutcome,
};
pub use output::{ToolOutput, shape};
pub use policy::PermissionMode;

pub mod memory;

pub mod subagent;
pub use subagent::{SubagentRequest, SubagentKind, SubagentRef, SubagentDisplay, SubagentStatus, SubagentOutcome};
