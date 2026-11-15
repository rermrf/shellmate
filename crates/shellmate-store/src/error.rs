//! Errors of the host store. `Display` is the user-facing Chinese message.

use std::path::PathBuf;

use shellmate_core::{GroupId, HostId};
use shellmate_forward::RuleId;

/// Why a host store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database engine reported an error.
    #[error("主机库出错：{0}")]
    Db(#[from] rusqlite::Error),
    /// The database file or its directory could not be prepared.
    #[error("无法创建主机库 {}：{source}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The database was written by a newer Shellmate.
    #[error("主机库版本较新（v{found}），当前 Shellmate 只支持到 v{supported}，请升级后再试")]
    TooNew {
        /// `user_version` found in the file.
        found: i64,
        /// Highest version this build understands.
        supported: i64,
    },
    /// A stored value could not be understood.
    #[error("主机库内容已损坏：{0}")]
    Corrupt(String),
    /// Another group already has this name.
    #[error("分组「{0}」已存在")]
    DuplicateGroup(String),
    /// No host has this id.
    #[error("找不到主机（{0}）")]
    HostNotFound(HostId),
    /// No group has this id.
    #[error("找不到分组（{0}）")]
    GroupNotFound(GroupId),
    /// No forwarding rule has this id.
    #[error("找不到端口转发规则（{0}）")]
    RuleNotFound(RuleId),
    /// A host lists a jump host that does not exist.
    #[error("主机「{host}」引用的跳板机不存在（{jump}）")]
    MissingJump {
        /// Name of the host holding the reference.
        host: String,
        /// The dangling id.
        jump: HostId,
    },
    /// The jump hosts of a host lead back to itself.
    #[error("跳板机配置形成了循环：{}", path.join(" → "))]
    JumpCycle {
        /// Host names along the cycle, first and last being the same host.
        path: Vec<String>,
    },
    /// Other hosts still use the host as a jump host.
    #[error("无法删除「{name}」：主机{}将它用作跳板机", dependents.iter().map(|d| format!("「{d}」")).collect::<Vec<_>>().join("、"))]
    HostInUse {
        /// Name of the host that was to be deleted.
        name: String,
        /// Names of the hosts that jump through it.
        dependents: Vec<String>,
    },
}

/// Result alias of the store.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;
