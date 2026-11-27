//! Transport-neutral observations and bounded keyboard input for an existing terminal.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Debug)]
pub struct Controller {
    pub id: String,
    pub label: String,
    pub active: AtomicBool,
}
impl Controller {
    pub fn new(label: String) -> Self {
        Self {
            id: crate::HostId::generate().to_string(),
            label,
            active: AtomicBool::new(true),
        }
    }
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn release(&self) {
        self.active.store(false, Ordering::Release);
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub row: u16,
    pub column: u16,
    pub rows: u16,
    pub columns: u16,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Text {
        text: String,
    },
    Paste {
        text: String,
    },
    Key {
        key: String,
        #[serde(default = "one")]
        repeat: u8,
    },
    Click {
        row: u16,
        column: u16,
        #[serde(default)]
        button: MouseButton,
    },
    Scroll {
        row: u16,
        column: u16,
        lines: i16,
    },
    Drag {
        row: u16,
        column: u16,
        to_row: u16,
        to_column: u16,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Middle,
    Right,
}
fn one() -> u8 {
    1
}
impl Action {
    pub fn summary(&self) -> String {
        match self {
            Self::Click {
                row,
                column,
                button,
            } => format!("点击第 {row} 行、第 {column} 列（{button:?}）"),
            Self::Scroll { row, column, lines } => {
                format!("在 {row},{column} 滚动 {lines} 行（正数向下）")
            }
            Self::Drag {
                row,
                column,
                to_row,
                to_column,
            } => format!("从 {row},{column} 拖动至 {to_row},{to_column}"),
            Self::Text { text } => format!("输入文字：{text}"),
            Self::Paste { text } => format!("粘贴文字：{text}"),
            Self::Key { key, repeat } => {
                if *repeat == 1 {
                    key.clone()
                } else {
                    format!("{key} × {repeat}")
                }
            }
        }
    }
}
pub fn navigation(actions: &[Action]) -> bool {
    !actions.is_empty()&&actions.iter().all(|a|matches!(a,Action::Key{key,..}if matches!(key.as_str(),"up"|"down"|"left"|"right"|"pageup"|"pagedown"|"home"|"end"|"tab"|"shift-tab")))
}
pub fn validate_actions(actions: &[Action]) -> Result<(), String> {
    if actions.is_empty() || actions.len() > 8 {
        return Err("每次需要 1–8 个短动作".into());
    }
    let mut total = 0;
    for a in actions {
        match a {
            Action::Click { .. } => total += 1,
            Action::Drag {
                row,
                column,
                to_row,
                to_column,
            } => {
                if row.abs_diff(*to_row).max(column.abs_diff(*to_column)) > 80 {
                    return Err("拖动距离超过 80 个单元格，请分步操作".into());
                }
                total += 1;
            }
            Action::Scroll { lines, .. } => {
                if *lines == 0 || lines.unsigned_abs() > 20 {
                    return Err("每次滚动 1–20 行".into());
                }
                total += usize::from(lines.unsigned_abs());
            }
            Action::Text { text } | Action::Paste { text } => {
                if text.is_empty()
                    || text.len() > 4096
                    || text.chars().any(|c| {
                        c.is_control()
                            && (!matches!(a, Action::Paste { .. }) || !matches!(c, '\n' | '\t'))
                    })
                {
                    return Err("文字输入需要 1–4096 字节，不能含 ESC 等控制字符".into());
                }
                total += 1;
            }
            Action::Key { key, repeat } => {
                if *repeat == 0 || *repeat > 20 || key.len() > 32 || !valid_key(key) {
                    return Err(
                        "按键无效；使用 enter、escape、方向键、F1–F12 或 ctrl/alt/shift 组合"
                            .into(),
                    );
                }
                total += usize::from(*repeat);
            }
        }
    }
    if total > 40 {
        return Err("按键过多，请分步观察后继续".into());
    }
    Ok(())
}
fn valid_key(key: &str) -> bool {
    if printable_key(key) {
        return true;
    }
    let mut parts = key.split('-').collect::<Vec<_>>();
    let Some(key) = parts.pop() else {
        return false;
    };
    if parts.len() > 3
        || parts
            .iter()
            .any(|p| !matches!(*p, "ctrl" | "alt" | "shift"))
    {
        return false;
    }
    if parts
        .iter()
        .enumerate()
        .any(|(i, p)| parts[..i].contains(p))
    {
        return false;
    }
    matches!(
        key,
        "enter"
            | "escape"
            | "tab"
            | "backspace"
            | "delete"
            | "insert"
            | "space"
            | "up"
            | "down"
            | "left"
            | "right"
            | "pageup"
            | "pagedown"
            | "home"
            | "end"
    ) || key
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n))
        || (!parts.is_empty() && key.len() == 1 && key.bytes().all(|c| c.is_ascii_alphanumeric()))
}
/// A literal single character is also a valid key (including case and punctuation).
pub fn printable_key(key: &str) -> bool {
    key.chars().count() == 1 && key.chars().all(|c| !c.is_control())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub row: u16,
    pub column: u16,
}
/// Explicit preconditions. With no text/cursor predicate the region must be unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub region: Region,
    pub text: Option<String>,
    pub absent_text: Option<String>,
    pub cursor: Option<Position>,
}
impl Guard {
    pub fn matches(&self, screen: &Screen) -> bool {
        self.text.as_ref().is_none_or(|t| screen.text.contains(t))
            && self
                .absent_text
                .as_ref()
                .is_none_or(|t| !screen.text.contains(t))
            && self
                .cursor
                .as_ref()
                .is_none_or(|p| p.row == screen.cursor.row && p.column == screen.cursor.column)
    }
    pub fn exact(&self) -> bool {
        self.text.is_none() && self.absent_text.is_none() && self.cursor.is_none()
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.region.rows == 0
            || self.region.columns == 0
            || [&self.text, &self.absent_text]
                .into_iter()
                .flatten()
                .any(|s| s.trim().is_empty() || s.len() > 2000)
        {
            return Err("校验区域和文字不能为空，文字最多 2000 字节".into());
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|p| !contains(&self.region, p.row, p.column))
        {
            return Err("光标条件必须位于校验区域内".into());
        }
        Ok(())
    }
}
pub fn contains(region: &Region, row: u16, column: u16) -> bool {
    row >= region.row
        && column >= region.column
        && u32::from(row) < u32::from(region.row) + u32::from(region.rows)
        && u32::from(column) < u32::from(region.column) + u32::from(region.columns)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observe {
    pub region: Option<Region>,
    pub text: Option<String>,
    pub absent_text: Option<String>,
    pub stable_ms: Option<u64>,
    #[serde(default = "observe_timeout")]
    pub timeout_ms: u64,
}
fn observe_timeout() -> u64 {
    600
}
impl Default for Observe {
    fn default() -> Self {
        Self {
            region: None,
            text: None,
            absent_text: None,
            stable_ms: None,
            timeout_ms: observe_timeout(),
        }
    }
}
impl Observe {
    pub fn validate(&self) -> Result<(), String> {
        if self.timeout_ms > 2000
            || self.stable_ms.is_some_and(|n| !(40..=1000).contains(&n))
            || [&self.text, &self.absent_text]
                .into_iter()
                .flatten()
                .any(|s| s.is_empty() || s.len() > 2000)
        {
            return Err(
                "输入后观察最多 2000 毫秒；稳定时间为 40–1000 毫秒，文字最多 2000 字节".into(),
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ScreenChanged,
    UserTakeover,
    InvalidReference,
    InvalidArguments,
    ConnectionChanged,
    ControllerBusy,
    Stopped,
    WriteFailed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InteractionError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default)]
    pub actions_sent: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<Box<Screen>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_rows: Vec<u16>,
}
impl InteractionError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            actions_sent: 0,
            screen: None,
            changed_rows: vec![],
        }
    }
}
impl From<String> for InteractionError {
    fn from(message: String) -> Self {
        Self::new(ErrorCode::ConnectionChanged, message)
    }
}
impl From<&str> for InteractionError {
    fn from(message: &str) -> Self {
        Self::new(ErrorCode::ConnectionChanged, message)
    }
}
impl std::fmt::Display for InteractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for InteractionError {}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub column: u16,
    pub columns: u16,
    pub foreground: String,
    pub background: String,
    pub inverse: bool,
    pub bold: bool,
    pub underline: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub row: u16,
    pub text: String,
    pub styles: Vec<Span>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub column: u16,
    pub visible: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    #[serde(default)]
    pub ended: bool,
    pub screen_id: String,
    pub fingerprint: String,
    pub rows: u16,
    pub columns: u16,
    pub cursor: Cursor,
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_reporting: bool,
    pub region: Region,
    pub lines: Vec<Row>,
    pub text: String,
    pub truncated: bool,
    pub controller: Option<String>,
    pub paused: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputResult {
    pub screen: Screen,
    pub actions_sent: usize,
    pub error: Option<String>,
}
