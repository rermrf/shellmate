//! Pure AI boundary: redacted conversations in, text and proposed commands out.
//! There is no command execution facility in this crate.
pub mod agent;
mod client;
mod config;
mod conversation;
pub mod output;
pub mod policy;
pub mod redact;
pub use client::{Event, ProviderClient, ResponseStream, RigClient, Usage};
pub use config::{ApiKey, Config, Provider, ReasoningLevel, Timeouts};
pub use conversation::{
    Answer, AnswerItem, ContextSnapshot, Conversation, DEFAULT_BUDGET, Prepared, StepStatus, Turn,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
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
    #[error("上下文过长，请清空对话或缩短问题")]
    ContextTooLong,
    #[error("请求超时，请稍后重试")]
    Timeout,
    #[error("网络或 DNS 连接失败，请检查网络与 Base URL")]
    Network,
    #[error("服务暂时不可用，请稍后重试")]
    Unavailable,
    #[error("服务返回了无法识别的回答，请重试")]
    InvalidResponse,
    #[error("本轮工具输出已达到 64 K 字符上限，请开始新的对话轮次")]
    ToolOutputLimit,
    #[error("会话已断开")]
    Disconnected,
    #[error("已停止生成")]
    Cancelled,
}

pub use agent::{
    AgentEvent, AgentStream, Capabilities, Limits, Resume, Target, ToolCall, ToolHost, ToolOutcome,
};
pub use output::{ToolOutput, shape};
pub use policy::PermissionMode;
