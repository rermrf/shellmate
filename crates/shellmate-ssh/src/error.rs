//! Errors of the SSH layer. `Display` is the user-facing Chinese message.

use std::path::PathBuf;

use shellmate_core::AuthMethod;

/// Why a connection, authentication or channel operation failed.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)] // the fields of the struct variants are named after what they hold
pub enum SshError {
    /// The TCP connection could not be established.
    #[error("无法连接 {host}:{port}：{reason}")]
    Connect { host: String, port: u16, reason: String },
    /// The connection attempt used up its time budget (waiting for the user does not count).
    #[error("连接超时（{}内没有响应）", describe_limit(*.limit))]
    Timeout { limit: std::time::Duration },
    /// The TCP connection did not complete in time.
    #[error("连接 {host}:{port} 超时（{secs} 秒）")]
    ConnectTimeout { host: String, port: u16, secs: u64 },
    /// The SSH handshake (version exchange, key exchange) failed.
    #[error("SSH 握手失败：{0}")]
    Handshake(String),
    /// The user declined to trust an unknown host key.
    #[error("主机指纹未被信任，连接已取消")]
    HostKeyRejected,
    /// The user did not confirm a changed host key.
    #[error("主机指纹已变更且未被确认，连接已取消")]
    HostKeyChanged,
    /// The server presented something other than a plain host key.
    #[error("服务器使用了暂不支持的主机密钥类型（SSH 证书）")]
    UnsupportedHostKey,
    /// `known_hosts` could not be read.
    #[error("无法读取 known_hosts：{0}")]
    KnownHosts(String),
    /// The server rejected the credentials of the given method.
    #[error("{}", auth_rejected_text(*.0))]
    AuthRejected(AuthMethod),
    /// The configured method is not offered by the server.
    #[error("认证失败：服务器不支持{}认证（服务器提供：{}）", .method.label(), .offered.join("、"))]
    AuthMethodUnavailable { method: AuthMethod, offered: Vec<String> },
    /// The user cancelled a password / passphrase / challenge prompt.
    #[error("已取消认证")]
    AuthCancelled,
    /// A private key file could not be read or decrypted.
    #[error("无法读取私钥 {}：{reason}", .path.display())]
    KeyUnreadable { path: PathBuf, reason: String },
    /// Key authentication was requested but no key file exists.
    #[error("认证失败：找不到可用的私钥文件（已查找 {}）", .searched.join("、"))]
    NoPrivateKey { searched: Vec<String> },
    /// ssh-agent could not be used.
    #[error("认证失败：{0}")]
    Agent(String),
    /// No authentication method could be tried.
    #[error("认证失败：没有可用的认证方式")]
    NoAuthMethod,
    /// A session channel or PTY could not be set up.
    #[error("无法打开会话：{0}")]
    Channel(String),
    /// The connection dropped.
    #[error("连接已断开：{0}")]
    Disconnected(String),
    /// The server did not open a `direct-tcpip` channel to `host:port`.
    #[error("{}", tcpip_text(host, *port, *reason))]
    Tcpip { host: String, port: u16, reason: TcpipFailure },
    /// The server refused to listen for a remote forward.
    #[error("服务器拒绝了远程转发（可能禁用了 AllowTcpForwarding）")]
    RemoteForwardDenied,
    /// The saved host cannot be connected (wrong protocol, empty route).
    #[error("无法连接：{0}")]
    Unsupported(String),
    /// A hop of a ProxyJump chain failed.
    #[error("跳板机 {host}:{port}：{source}")]
    Jump {
        host: String,
        port: u16,
        #[source]
        source: Box<SshError>,
    },
}

/// Why the server did not open a `direct-tcpip` channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpipFailure {
    /// The server forbids forwarding to this target (`AllowTcpForwarding`, `PermitOpen`).
    Prohibited,
    /// The server could not connect to the target (refused, unreachable, unknown name; SSH
    /// reports all of these with the same code).
    ConnectFailed,
    /// The server has no channels left.
    ResourceShortage,
    /// The server did not answer in time.
    Timeout,
    /// The SSH connection is gone.
    Disconnected,
    /// Any other refusal.
    Other,
}

fn tcpip_text(host: &str, port: u16, reason: TcpipFailure) -> String {
    match reason {
        TcpipFailure::Prohibited => format!("服务器不允许转发到 {host}:{port}（AllowTcpForwarding / PermitOpen 限制）"),
        TcpipFailure::ConnectFailed => format!("服务器无法连接 {host}:{port}"),
        TcpipFailure::ResourceShortage => "服务器上的转发通道已用完".into(),
        TcpipFailure::Timeout => format!("经服务器连接 {host}:{port} 超时"),
        TcpipFailure::Disconnected => "SSH 连接已断开".into(),
        TcpipFailure::Other => format!("服务器无法打开到 {host}:{port} 的转发通道"),
    }
}

/// `20 秒`, or milliseconds for sub-second limits.
fn describe_limit(limit: std::time::Duration) -> String {
    if limit.as_secs() >= 1 { format!("{} 秒", limit.as_secs()) } else { format!("{} 毫秒", limit.as_millis()) }
}

fn auth_rejected_text(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::PrivateKey => "认证失败：服务器拒绝了私钥",
        AuthMethod::Password => "认证失败：用户名或密码错误",
        AuthMethod::Agent => "认证失败：服务器拒绝了 ssh-agent 中的所有密钥",
        AuthMethod::KeyboardInteractive => "认证失败：键盘交互验证未通过",
    }
}

impl SshError {
    /// Whether the user cancelled an interactive step (not a real failure).
    pub fn is_cancelled(&self) -> bool {
        match self {
            SshError::AuthCancelled | SshError::HostKeyRejected | SshError::HostKeyChanged => true,
            SshError::Jump { source, .. } => source.is_cancelled(),
            _ => false,
        }
    }
}

impl From<russh::Error> for SshError {
    fn from(err: russh::Error) -> Self {
        SshError::Handshake(describe_russh_error(&err))
    }
}

/// Short Chinese description of a transport-level error.
pub(crate) fn describe_russh_error(err: &russh::Error) -> String {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, TimedOut, UnexpectedEof};
    match err {
        russh::Error::HUP => "服务器关闭了连接".into(),
        russh::Error::Disconnect => "连接已关闭".into(),
        russh::Error::KeepaliveTimeout => "保活超时，服务器长时间没有响应".into(),
        russh::Error::InactivityTimeout => "连接空闲超时".into(),
        russh::Error::ConnectionTimeout => "连接超时".into(),
        russh::Error::SendError | russh::Error::RecvError => "连接已被服务器关闭".into(),
        russh::Error::Version => "对方不是 SSH 服务（版本标识无效）".into(),
        russh::Error::IO(io)
            if matches!(io.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe) =>
        {
            format!("网络连接被中断（{io}）")
        }
        russh::Error::IO(io) if io.kind() == TimedOut => "网络连接超时".into(),
        russh::Error::NoCommonAlgo { kind, .. } => format!("与服务器没有共同支持的{kind:?}算法"),
        other => other.to_string(),
    }
}
