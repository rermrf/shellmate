//! Errors of the SFTP layer. `Display` is the user-facing Chinese message.

use russh_sftp::client::error::Error as RawError;
use russh_sftp::protocol::StatusCode;
use shellmate_ssh::SshError;

use crate::text::MAX_TEXT_SIZE;

/// Why an SFTP operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[allow(missing_docs)] // the fields of the struct variants are named after what they hold
pub enum SftpError {
    /// The server refused for lack of permission.
    #[error("权限不足：{path}")]
    PermissionDenied { path: String },
    /// The file or directory does not exist.
    #[error("不存在：{path}")]
    NotFound { path: String },
    /// Something is already there (and is not overwritten).
    #[error("已存在：{path}")]
    AlreadyExists { path: String },
    /// A directory was expected.
    #[error("不是目录：{path}")]
    NotADirectory { path: String },
    /// A file was expected.
    #[error("是目录，不是文件：{path}")]
    IsADirectory { path: String },
    /// A regular file was expected (not a device, pipe or socket).
    #[error("不是普通文件：{path}")]
    NotAFile { path: String },
    /// The operation was refused to protect the user (deleting `/`).
    #[error("为避免误删，已拒绝删除 {path}")]
    Refused { path: String },
    /// The connection is gone (or the SFTP stream broke).
    #[error("连接已断开{}", detail(.0))]
    Disconnected(String),
    /// The server does not implement what was asked.
    #[error("服务器不支持此操作：{0}")]
    Unsupported(String),
    /// The server did not answer in time.
    #[error("服务器响应超时")]
    Timeout,
    /// The server said "no" without saying why (SFTP v3 has no better code for many failures).
    #[error("操作失败：{path}{}", detail(.message))]
    Failed { path: String, message: String },
    /// The file is bigger than the editor opens.
    #[error("文件太大（{}），内置编辑器只打开 {} 以内的文本，请下载后查看", format_size(*.size), format_size(MAX_TEXT_SIZE))]
    TooLarge { size: u64 },
    /// The file looks binary.
    #[error("这是二进制文件，不能作为文本打开，请下载后查看")]
    Binary,
    /// The file is text but not UTF-8.
    #[error("文件不是 UTF-8 编码的文本，不能打开，请下载后查看")]
    NotUtf8,
    /// The server sent something that is not valid SFTP.
    #[error("SFTP 协议错误：{0}")]
    Protocol(String),
    /// The SSH layer failed (opening the channel, the subsystem).
    #[error("{0}")]
    Ssh(String),
}

fn detail(text: &str) -> String {
    if text.is_empty() { String::new() } else { format!("（{text}）") }
}

/// `612 B`, `2.3 KB`, `1.8 MB`, `1.21 GB`.
pub(crate) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

impl SftpError {
    /// Whether the connection is gone (the caller waits for the reconnect, then tries again).
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Disconnected(_))
    }

    /// Whether the session cannot be relied on any more: the connection is gone, or the server
    /// stopped answering. A transfer that meets this stops and waits for a new session instead of
    /// failing its files.
    pub fn is_session_lost(&self) -> bool {
        matches!(self, Self::Disconnected(_) | Self::Timeout)
    }

    /// Whether the path does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    /// The same error, blaming `path`: a failure while writing a temporary file next to a target
    /// is reported for the target, which is what the user knows.
    pub fn at(self, path: &str) -> Self {
        let path = path.to_string();
        match self {
            Self::PermissionDenied { .. } => Self::PermissionDenied { path },
            Self::NotFound { .. } => Self::NotFound { path },
            Self::AlreadyExists { .. } => Self::AlreadyExists { path },
            Self::NotADirectory { .. } => Self::NotADirectory { path },
            Self::Failed { message, .. } => Self::Failed { path, message },
            other => other,
        }
    }

    /// Translates a protocol error for an operation on `path`.
    pub(crate) fn from_raw(error: RawError, path: &str) -> Self {
        match error {
            RawError::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => Self::NotFound { path: path.to_string() },
                StatusCode::PermissionDenied => Self::PermissionDenied { path: path.to_string() },
                StatusCode::NoConnection | StatusCode::ConnectionLost => Self::Disconnected(String::new()),
                StatusCode::OpUnsupported => Self::Unsupported(path.to_string()),
                StatusCode::BadMessage => Self::Protocol(status.error_message),
                StatusCode::Ok | StatusCode::Eof | StatusCode::Failure => {
                    Self::Failed { path: path.to_string(), message: stock_message(&status.error_message) }
                }
            },
            RawError::Timeout => Self::Timeout,
            RawError::IO(message) => Self::Disconnected(message),
            RawError::UnexpectedBehavior(message) if is_transport_loss(&message) => Self::Disconnected(String::new()),
            RawError::UnexpectedBehavior(message) | RawError::Limited(message) => Self::Protocol(message),
            RawError::UnexpectedPacket => Self::Protocol("服务器返回了意外的数据包".into()),
        }
    }
}

/// The server's own explanation, unless it is one of the stock names every server repeats.
fn stock_message(message: &str) -> String {
    const STOCK: [&str; 9] = [
        "Ok",
        "Eof",
        "No such file",
        "Permission denied",
        "Failure",
        "Bad message",
        "No connection",
        "Connection lost",
        "Operation unsupported",
    ];
    if message.is_empty() || STOCK.iter().any(|stock| stock.eq_ignore_ascii_case(message)) {
        String::new()
    } else {
        message.to_string()
    }
}

/// What the client library says when the stream under it is gone.
fn is_transport_loss(message: &str) -> bool {
    message.contains("sender dropped") || message.contains("session closed") || message.contains("SendError")
}

impl From<SshError> for SftpError {
    fn from(error: SshError) -> Self {
        match error {
            SshError::Disconnected(reason) => Self::Disconnected(reason),
            other => Self::Ssh(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use russh_sftp::protocol::Status;

    use super::*;

    fn status(code: StatusCode, message: &str) -> RawError {
        RawError::Status(Status {
            id: 1,
            status_code: code,
            error_message: message.to_string(),
            language_tag: "en".to_string(),
        })
    }

    #[test]
    fn status_codes_become_clear_messages() {
        let map = |code, message: &str| SftpError::from_raw(status(code, message), "/srv/a.txt").to_string();
        assert_eq!(map(StatusCode::NoSuchFile, "No such file"), "不存在：/srv/a.txt");
        assert_eq!(map(StatusCode::PermissionDenied, "Permission denied"), "权限不足：/srv/a.txt");
        assert_eq!(map(StatusCode::Failure, "Failure"), "操作失败：/srv/a.txt");
        assert_eq!(map(StatusCode::Failure, "Directory not empty"), "操作失败：/srv/a.txt（Directory not empty）");
        assert_eq!(map(StatusCode::ConnectionLost, ""), "连接已断开");
        assert_eq!(map(StatusCode::OpUnsupported, ""), "服务器不支持此操作：/srv/a.txt");
    }

    #[test]
    fn an_error_can_blame_another_path() {
        let denied = SftpError::PermissionDenied { path: "/d/.f.shellmate-edit".into() };
        assert_eq!(denied.at("/d/f").to_string(), "权限不足：/d/f");
        let failed = SftpError::Failed { path: "/tmp".into(), message: "x".into() };
        assert_eq!(failed.at("/d/f").to_string(), "操作失败：/d/f（x）");
        assert_eq!(SftpError::Timeout.at("/d/f"), SftpError::Timeout);
    }

    #[test]
    fn transport_trouble_is_a_disconnect() {
        let map = |error| SftpError::from_raw(error, "/x");
        assert!(map(RawError::UnexpectedBehavior("sender dropped".into())).is_disconnected());
        assert!(map(RawError::UnexpectedBehavior("session closed".into())).is_disconnected());
        assert!(map(RawError::IO("broken pipe".into())).is_disconnected());
        assert_eq!(map(RawError::IO("broken pipe".into())).to_string(), "连接已断开（broken pipe）");
        assert_eq!(map(RawError::Timeout), SftpError::Timeout);
        assert!(matches!(map(RawError::UnexpectedBehavior("odd".into())), SftpError::Protocol(_)));
        assert!(matches!(map(RawError::UnexpectedPacket), SftpError::Protocol(_)));
    }

    #[test]
    fn ssh_errors_keep_their_wording() {
        assert!(SftpError::from(SshError::Disconnected("保活超时".into())).is_disconnected());
        let refused = SftpError::from(SshError::Channel("服务器拒绝了「sftp」子系统请求".into()));
        assert_eq!(refused.to_string(), "无法打开会话：服务器拒绝了「sftp」子系统请求");
    }

    #[test]
    fn text_rejections_explain_themselves() {
        assert_eq!(
            SftpError::TooLarge { size: 7 * 1024 * 1024 }.to_string(),
            "文件太大（7.0 MB），内置编辑器只打开 5.0 MB 以内的文本，请下载后查看"
        );
        assert!(SftpError::Binary.to_string().contains("二进制"));
        assert!(SftpError::NotUtf8.to_string().contains("UTF-8"));
    }

    #[test]
    fn sizes_use_the_prototype_format() {
        assert_eq!(format_size(612), "612 B");
        assert_eq!(format_size(2355), "2.3 KB");
        assert_eq!(format_size(1_887_437), "1.8 MB");
        assert_eq!(format_size(1_298_000_000), "1.21 GB");
    }
}
