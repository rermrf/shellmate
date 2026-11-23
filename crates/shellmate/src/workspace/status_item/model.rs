//! Presentation shared by the live status item, its menu and the debug hooks.
use super::super::{group::Prompt, ssh::SshStatus};
use crate::reconnect::MAX_ATTEMPTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Local,
    Connecting,
    Connected,
    Reconnecting(Option<u32>),
    Failed,
    ReconnectFailed,
    ChangedKey,
    RetryAuth,
    Ended,
    Closed,
}

impl Status {
    pub(super) fn ssh(status: SshStatus, prompt: Prompt, reconnect_failed: bool) -> Self {
        match status {
            SshStatus::Connecting | SshStatus::Reconnecting(_) if prompt == Prompt::ChangedKey => {
                Self::ChangedKey
            }
            SshStatus::Connecting | SshStatus::Reconnecting(_) if prompt == Prompt::RetryAuth => {
                Self::RetryAuth
            }
            SshStatus::Connecting => Self::Connecting,
            SshStatus::Connected => Self::Connected,
            SshStatus::Reconnecting(n) => Self::Reconnecting(Some(n)),
            SshStatus::Failed if reconnect_failed => Self::ReconnectFailed,
            SshStatus::Failed => Self::Failed,
            SshStatus::Ended => Self::Ended,
        }
    }

    fn attention(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::ReconnectFailed | Self::ChangedKey | Self::RetryAuth
        )
    }

    fn hint(self) -> String {
        match self {
            Self::Local => "本机",
            Self::Connecting => "连接中",
            Self::Connected => "已连接",
            Self::Reconnecting(Some(n)) => return format!("重连中 {n}/{MAX_ATTEMPTS}"),
            Self::Reconnecting(None) => "重连中",
            Self::Failed => "连接失败",
            Self::ChangedKey => "主机指纹已变更",
            Self::RetryAuth => "认证失败",
            Self::ReconnectFailed => "重连失败",
            Self::Ended => "已退出",
            Self::Closed => "已关闭",
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Icon {
    Idle,
    Connected(usize),
    Attention(usize),
}

impl Icon {
    /// One entry per session: its status, and whether something happened in it that nobody has
    /// looked at yet (a bell, a notification, a command that ended). Either needs attention.
    pub(super) fn from_sessions(statuses: impl Iterator<Item = (Status, bool)>) -> Self {
        let mut attention = 0;
        let mut connected = 0;
        for (status, alert) in statuses {
            if status.attention() || alert {
                attention += 1;
            } else if matches!(status, Status::Connected | Status::Reconnecting(_)) {
                connected += 1;
            }
        }
        if attention > 0 {
            Self::Attention(attention)
        } else if connected > 0 {
            Self::Connected(connected)
        } else {
            Self::Idle
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connected(_) => "connected",
            Self::Attention(_) => "attention",
        }
    }

    pub(super) fn tooltip(self) -> String {
        match self {
            Self::Idle => "Shellmate · 没有连接".into(),
            Self::Connected(n) => format!("Shellmate · {n} 个会话已连接"),
            Self::Attention(n) => format!("Shellmate · {n} 个会话需要处理"),
        }
    }
}

pub(super) struct Snapshot<'a> {
    pub id: usize,
    pub title: &'a str,
    pub status: Status,
    /// Something is waiting for a look in this session.
    pub alert: bool,
}

impl Snapshot<'_> {
    fn needs_attention(&self) -> bool {
        self.status.attention() || self.alert
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Show,
    Tab(usize),
    Local,
    Connect,
    Settings,
    CheckUpdate,
    OpenUpdate,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Row {
    Item(String, Option<Action>),
    Separator,
}

impl Row {
    fn item(title: &str, action: Action) -> Self {
        Self::Item(title.into(), Some(action))
    }
}

pub(super) fn menu(sessions: &[Snapshot], version: Option<String>) -> Vec<Row> {
    use Action::*;
    const LIMIT: usize = 10;
    let mut rows = vec![Row::item("打开 Shellmate", Show), Row::Separator];
    let ordered = sessions
        .iter()
        .filter(|s| s.needs_attention())
        .chain(sessions.iter().filter(|s| !s.needs_attention()));
    for session in ordered.take(LIMIT) {
        // A session with something waiting is marked with a dot.
        let mark = if session.alert { "● " } else { "" };
        rows.push(Row::Item(
            format!("{mark}{} — {}", session.title, session.status.hint()),
            Some(Tab(session.id)),
        ));
    }
    if sessions.is_empty() {
        rows.push(Row::Item("没有会话".into(), None));
    } else if sessions.len() > LIMIT {
        rows.push(Row::Item(
            format!("还有 {} 个会话…", sessions.len() - LIMIT),
            Some(Show),
        ));
    }
    rows.extend([
        Row::Separator,
        Row::item("新建本机终端", Local),
        Row::item("新建连接…", Connect),
        Row::Separator,
        Row::item("设置…", Settings),
        match version {
            Some(v) => Row::Item(format!("新版本 {v} 可用…"), Some(OpenUpdate)),
            None => Row::item("检查更新…", CheckUpdate),
        },
        Row::Separator,
        Row::item("退出 Shellmate", Quit),
    ]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_attention_clears_on_progress_and_skips_independent_connected_tabs() {
        use SshStatus::*;
        assert_eq!(
            Status::ssh(Failed, Prompt::None, true),
            Status::ReconnectFailed
        );
        assert_eq!(Status::ssh(Failed, Prompt::None, false), Status::Failed);
        assert!(Status::ssh(Connecting, Prompt::ChangedKey, false).attention());
        assert!(Status::ssh(Reconnecting(1), Prompt::RetryAuth, false).attention());
        assert!(!Status::ssh(Reconnecting(1), Prompt::None, false).attention());
        assert_eq!(
            Status::ssh(Connected, Prompt::ChangedKey, false),
            Status::Connected
        );
    }

    #[test]
    fn priority_counts_and_tooltips() {
        use Status::*;
        let cases: &[(&[Status], Icon, &str)] = &[
            (&[], Icon::Idle, "Shellmate · 没有连接"),
            (&[Local], Icon::Idle, "Shellmate · 没有连接"),
            (
                &[Closed, Ended, Connecting],
                Icon::Idle,
                "Shellmate · 没有连接",
            ),
            (
                &[Connected, Local, Reconnecting(Some(2))],
                Icon::Connected(2),
                "Shellmate · 2 个会话已连接",
            ),
            (
                &[Reconnecting(None), ReconnectFailed, Connected],
                Icon::Attention(1),
                "Shellmate · 1 个会话需要处理",
            ),
            (
                &[Failed, ReconnectFailed],
                Icon::Attention(2),
                "Shellmate · 2 个会话需要处理",
            ),
        ];
        for &(sessions, icon, tooltip) in cases {
            let actual = Icon::from_sessions(sessions.iter().map(|status| (*status, false)));
            assert_eq!(actual, icon);
            assert_eq!(actual.tooltip(), tooltip);
        }
    }

    #[test]
    fn menu_preserves_tab_order_within_priority_and_caps_sessions() {
        let sessions: Vec<_> = (0..13)
            .map(|id| Snapshot {
                id,
                title: "tab",
                status: match id {
                    7 => Status::Failed,
                    12 => Status::ReconnectFailed,
                    2 => Status::Reconnecting(Some(2)),
                    _ => Status::Local,
                },
                alert: false,
            })
            .collect();
        let rows = menu(&sessions, None);
        let ids: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(_, Some(Action::Tab(id))) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [7, 12, 0, 1, 2, 3, 4, 5, 6, 8]);
        assert_eq!(rows[3], Row::item("tab — 重连失败", Action::Tab(12)));
        assert_eq!(rows[6], Row::item("tab — 重连中 2/5", Action::Tab(2)));
        assert_eq!(rows[12], Row::item("还有 3 个会话…", Action::Show));
        assert!(
            !menu(&sessions[..10], None)
                .iter()
                .any(|r| matches!(r, Row::Item(t, _) if t.starts_with("还有")))
        );
    }

    #[test]
    fn a_session_with_something_waiting_needs_attention_whatever_its_status() {
        use Status::*;
        let icon = |sessions: &[(Status, bool)]| Icon::from_sessions(sessions.iter().copied());
        assert_eq!(icon(&[(Local, true)]), Icon::Attention(1), "a bell in a healthy session");
        assert_eq!(icon(&[(Connected, true), (Connected, false)]), Icon::Attention(1));
        // A failed session that also has an alert is still one session.
        assert_eq!(icon(&[(Failed, true), (Local, true), (Connected, true)]), Icon::Attention(3));
        assert_eq!(icon(&[(Failed, true), (Failed, false)]), Icon::Attention(2));
        assert_eq!(icon(&[(Connected, false), (Local, false)]), Icon::Connected(1));
        assert_eq!(icon(&[(Ended, true)]), Icon::Attention(1), "an ended session can have rung before it did");
        assert_eq!(icon(&[(Local, true)]).tooltip(), "Shellmate · 1 个会话需要处理");
    }

    #[test]
    fn the_menu_lists_sessions_with_something_waiting_first_and_marks_them() {
        let snapshot = |id, status, alert| Snapshot { id, title: "tab", status, alert };
        let sessions = [
            snapshot(0, Status::Local, false),
            snapshot(1, Status::Connected, true),
            snapshot(2, Status::Failed, false),
            snapshot(3, Status::Local, true),
        ];
        let rows = menu(&sessions, None);
        let tabs: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(title, Some(Action::Tab(id))) => Some((*id, title.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tabs,
            [(1, "● tab — 已连接"), (2, "tab — 连接失败"), (3, "● tab — 本机"), (0, "tab — 本机")],
            "attention first in tab order; only the ones with an alert carry the dot"
        );
    }

    #[test]
    fn empty_menu_and_update_actions() {
        use Action::*;
        let rows = menu(&[], None);
        assert_eq!(
            rows,
            [
                Row::item("打开 Shellmate", Show),
                Row::Separator,
                Row::Item("没有会话".into(), None),
                Row::Separator,
                Row::item("新建本机终端", Local),
                Row::item("新建连接…", Connect),
                Row::Separator,
                Row::item("设置…", Settings),
                Row::item("检查更新…", CheckUpdate),
                Row::Separator,
                Row::item("退出 Shellmate", Quit),
            ]
        );
        assert_eq!(
            menu(&[], Some("1.2.3".into()))[8],
            Row::item("新版本 1.2.3 可用…", OpenUpdate)
        );
    }
}
