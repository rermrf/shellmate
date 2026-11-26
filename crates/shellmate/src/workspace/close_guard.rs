//! One close-risk snapshot for tabs, windows, and application quit.
use super::{Session, SessionKind, Workspace, forward::ForwardImpact, ssh::SshStatus};
use gpui::App;
use shellmate_core::{Protocol, io::CloseActivity};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Reason {
    Local(CloseActivity),
    Connection(Protocol, bool),
}

impl Reason {
    fn covered_by(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Local(_), Self::Local(CloseActivity::Unknown)) => true,
            (Self::Local(CloseActivity::Busy(now)), Self::Local(CloseActivity::Busy(before))) => {
                now.iter().all(|job| before.contains(job))
            }
            (Self::Connection(protocol, _), Self::Connection(old, _)) => protocol == old,
            _ => self == previous,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Local(CloseActivity::Busy(jobs)) => {
                let names: BTreeSet<_> = jobs.iter().map(|job| job.name.as_str()).collect();
                let mut names = names.into_iter().take(4).collect::<Vec<_>>().join("、");
                if jobs.len() > 4 {
                    names.push('等');
                }
                format!("正在运行 {names}。关闭后可能终止这些任务，未保存的内容可能丢失。")
            }
            Self::Local(_) => "无法确认终端是否空闲，关闭后可能中断正在运行的任务。".into(),
            Self::Connection(Protocol::Serial, connecting) => if *connecting {
                "正在打开或重连串口，关闭后会取消连接。"
            } else {
                "串口已打开，关闭后会断开与设备的连接。"
            }
            .into(),
            Self::Connection(protocol, connecting) => {
                let name = match protocol {
                    Protocol::Mosh => "Mosh",
                    Protocol::Telnet => "Telnet",
                    _ => "SSH",
                };
                if *connecting {
                    format!("{name} 正在连接或重连，关闭后会取消连接。")
                } else {
                    format!("将关闭此 {name} 会话，远端任务可能受到影响。")
                }
            }
        }
    }
}

fn local_reason(exited: bool, activity: CloseActivity) -> Option<Reason> {
    (!exited && activity != CloseActivity::Idle).then_some(Reason::Local(activity))
}

fn connection_reason(protocol: Protocol, connected: bool, connecting: bool) -> Option<Reason> {
    (connected || connecting).then_some(Reason::Connection(protocol, !connected))
}

impl Session {
    fn close_reason(&self, cx: &App) -> Option<Reason> {
        match &self.kind {
            SessionKind::Local => {
                local_reason(self.exited, self.terminal.read(cx).close_activity())
            }
            SessionKind::Ssh(ssh) => connection_reason(
                ssh.host.protocol,
                ssh.status == SshStatus::Connected,
                matches!(
                    ssh.status,
                    SshStatus::Connecting | SshStatus::Reconnecting(_)
                ),
            ),
            SessionKind::Stream(stream) => {
                let (connected, connecting) = stream.close_connection_state();
                connection_reason(stream.host.protocol, connected, connecting)
            }
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct CloseImpact {
    sessions: Vec<(usize, String, Reason)>,
    conversations: Vec<(String, String)>,
    pub(super) transfers: usize,
    pub(super) forwards: ForwardImpact,
}

impl CloseImpact {
    pub(super) fn needs_confirmation(&self) -> bool {
        !self.conversations.is_empty() || !self.sessions.is_empty() || self.transfers > 0 || self.forwards != ForwardImpact::default()
    }

    /// Decreases are safe. A new task or newly affected resource needs fresh consent.
    pub(crate) fn covered_by(&self, previous: &Self) -> bool {
        self.conversations.iter().all(|(id,_)|previous.conversations.iter().any(|(old,_)|id==old))
            && self.transfers <= previous.transfers
            && self.forwards.rules <= previous.forwards.rules
            && self.forwards.connections <= previous.forwards.connections
            && self.sessions.iter().all(|(id, _, reason)| {
                previous
                    .sessions
                    .iter()
                    .any(|(old, _, before)| id == old && reason.covered_by(before))
            })
    }

    pub(crate) fn message(&self) -> String {
        let mut parts: Vec<_> = self
            .sessions
            .iter()
            .take(4)
            .map(|(_, title, reason)| format!("「{title}」{}", reason.message()))
            .collect();
        if self.sessions.len() > 4 {
            parts.push(format!(
                "另有 {} 个会话存在运行任务或连接。",
                self.sessions.len() - 4
            ));
        }
        if !self.conversations.is_empty() {
            parts.push(format!("将影响 AI 对话：{}。此终端上的操作会中断，其他目标结果仍保留。",self.conversations.iter().take(4).map(|(_,title)|format!("「{title}」")).collect::<Vec<_>>().join("、")));
        }
        if self.transfers > 0 {
            parts.push(format!("将中断 {} 个未完成的文件传输。", self.transfers));
        }
        if self.forwards.rules > 0 {
            parts.push(format!("将停止 {} 条端口转发。", self.forwards.rules));
        }
        if self.forwards.connections > 0 {
            parts.push(format!("将断开 {} 个转发连接。", self.forwards.connections));
        }
        parts.join("\n")
    }
}

fn lost_groups(removed: BTreeSet<usize>, remaining: BTreeSet<usize>) -> BTreeSet<usize> {
    removed.difference(&remaining).copied().collect()
}

impl Workspace {
    pub(super) fn close_impact(&self, ids: &[usize], cx: &App) -> CloseImpact {
        let mut impact = CloseImpact { conversations:self.hub.conversations.affected(Some(ids)), ..Default::default() };
        let mut removed = BTreeSet::new();
        let mut remaining = BTreeSet::new();
        for session in &self.sessions {
            let closing = ids.contains(&session.id);
            if closing && let Some(reason) = session.close_reason(cx) {
                impact
                    .sessions
                    .push((session.id, session.title.to_string(), reason));
            }
            if let Some(ssh) = session.ssh() {
                if closing {
                    removed.insert(ssh.group);
                } else {
                    remaining.insert(ssh.group);
                }
            }
        }
        for entry in self.peers(super::Route::All, cx) {
            if let Some(peer) = entry.workspace.upgrade() {
                remaining.extend(
                    peer.read(cx)
                        .sessions
                        .iter()
                        .filter_map(|s| s.ssh().map(|s| s.group)),
                );
            }
        }
        let lost = lost_groups(removed, remaining);
        impact.transfers = self
            .hub
            .groups
            .borrow()
            .iter()
            .filter(|g| lost.contains(&g.id))
            .filter_map(|g| g.link.as_ref())
            .map(|link| {
                link.queue()
                    .snapshot()
                    .iter()
                    .filter(|item| !item.state.is_finished())
                    .count()
            })
            .sum();
        impact.forwards = self.forward_impact(|s| ids.contains(&s.id), cx);
        impact
    }

    pub(crate) fn quit_impact(&self, cx: &App) -> CloseImpact {
        let mut impact = CloseImpact { conversations:self.hub.conversations.affected(None), ..Default::default() };
        let mut collect = |session: &Session| {
            if let Some(reason) = session.close_reason(cx) {
                impact
                    .sessions
                    .push((session.id, session.title.to_string(), reason));
            }
        };
        for session in &self.sessions {
            collect(session);
        }
        for entry in self.peers(super::Route::All, cx) {
            if let Some(peer) = entry.workspace.upgrade() {
                for session in &peer.read(cx).sessions {
                    collect(session);
                }
            }
        }
        // Shared engines are owned by Hub, not by individual window/session views.
        for group in self.hub.groups.borrow().iter() {
            if let Some(link) = &group.link {
                impact.transfers += link
                    .queue()
                    .snapshot()
                    .iter()
                    .filter(|item| !item.state.is_finished())
                    .count();
            }
            if let Some(forward) = &group.forward {
                impact.forwards.rules += forward.forwarder.listening_rules();
                impact.forwards.connections = impact
                    .forwards
                    .connections
                    .saturating_add(forward.forwarder.active_connections());
            }
        }
        impact
    }

    pub(super) fn confirm_tab_close(
        &mut self,
        id: usize,
        reviewed: &CloseImpact,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(ix) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        if self.close_impact(&[id], cx).covered_by(reviewed) {
            self.close_session(id, window, cx);
        } else {
            self.close(ix, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_core::io::ProcessInfo;
    fn task(pid: i32, name: &str) -> Reason {
        Reason::Local(CloseActivity::Busy(vec![ProcessInfo {
            pid,
            name: name.into(),
            cwd: None,
        }]))
    }
    fn with_task(id: usize, reason: Reason) -> CloseImpact {
        CloseImpact {
            sessions: vec![(id, "测试会话".into(), reason)],
            ..Default::default()
        }
    }

    #[test]
    fn idle_and_ended_local_sessions_close_without_prompt() {
        assert_eq!(local_reason(false, CloseActivity::Idle), None);
        assert_eq!(local_reason(true, CloseActivity::Unknown), None);
        assert!(local_reason(false, CloseActivity::Unknown).is_some());
        assert!(!CloseImpact::default().needs_confirmation());
        assert!(with_task(1, task(42, "vim")).needs_confirmation());
    }

    #[test]
    fn all_connection_protocols_confirm_live_and_reconnecting_only() {
        for protocol in [
            Protocol::Ssh,
            Protocol::Mosh,
            Protocol::Telnet,
            Protocol::Serial,
        ] {
            assert!(connection_reason(protocol, true, false).is_some());
            assert!(connection_reason(protocol, false, true).is_some());
            assert_eq!(connection_reason(protocol, false, false), None);
        }
        assert!(
            Reason::Connection(Protocol::Serial, false)
                .message()
                .contains("串口已打开")
        );
        assert!(
            Reason::Connection(Protocol::Ssh, false)
                .message()
                .contains("可能受到影响")
        );
    }

    #[test]
    fn shared_group_survives_other_tabs_and_other_windows() {
        assert_eq!(lost_groups([1, 2, 3].into(), [2, 4].into()), [1, 3].into());
        assert!(lost_groups([1].into(), [1].into()).is_empty());
    }

    #[test]
    fn confirmation_covers_finished_tasks_but_not_new_tasks_or_resources() {
        let before = with_task(1, task(10, "vim"));
        assert!(CloseImpact::default().covered_by(&before));
        assert!(before.covered_by(&before));
        assert!(!with_task(1, task(11, "codex")).covered_by(&before));
        assert!(!with_task(2, task(10, "vim")).covered_by(&before));
        for impact in [
            CloseImpact { conversations:vec![("a".into(),"检查目录".into())], ..Default::default() },
            CloseImpact {
                transfers: 1,
                ..Default::default()
            },
            CloseImpact {
                forwards: ForwardImpact {
                    rules: 1,
                    connections: 0,
                },
                ..Default::default()
            },
            CloseImpact {
                forwards: ForwardImpact {
                    rules: 0,
                    connections: 1,
                },
                ..Default::default()
            },
        ] {
            assert!(impact.needs_confirmation());
            assert!(!impact.covered_by(&before));
        }
        assert!(before.covered_by(&with_task(1, Reason::Local(CloseActivity::Unknown))));
    }

    #[test]
    fn one_summary_describes_local_remote_and_resource_impact() {
        let mut impact = with_task(1, task(10, "vim"));
        impact
            .sessions
            .push((2, "server".into(), Reason::Connection(Protocol::Ssh, false)));
        impact.transfers = 2;
        impact.forwards = ForwardImpact {
            rules: 1,
            connections: 3,
        };
        let text = impact.message();
        for expected in [
            "vim",
            "SSH",
            "可能",
            "2 个未完成的文件传输",
            "1 条端口转发",
            "3 个转发连接",
        ] {
            assert!(text.contains(expected), "{text}");
        }
    }
}
