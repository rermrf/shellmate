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
