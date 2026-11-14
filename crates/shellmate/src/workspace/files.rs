//! The 文件 panel inside the workspace: one panel per SSH tab, all of a connection's panels on the
//! one SFTP session (and transfer queue) of its [`ConnGroup`](super::group::ConnGroup).
//!
//! The session is opened the first time a panel of the connection is shown. It follows the
//! connection: gone when the connection is lost, opened again after a reconnect if it was open
//! before, and reopened (once) when the SFTP subsystem dies under a live connection. All of that
//! is [`SftpLink`]'s job; this module opens it, tells it what happens to the connection and hands
//! what it says to the panels.

use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Focusable as _, Subscription, Window};
use shellmate_sftp::SftpSession;
use shellmate_sftp::link::{LinkState, SftpLink};
use shellmate_sftp::transfer::TransferEvent;
use shellmate_ssh::Connection;

use super::ssh::SshStatus;
use super::{Panel, Workspace};
use crate::files::model::LinkStatus;
use crate::files::panel::{FilesEvent, FilesPanel};
use crate::runtime;

/// The files panel of a tab and what keeps it wired to the workspace.
pub struct FilesTab {
    pub panel: Entity<FilesPanel>,
    _subscription: Subscription,
}

/// What a panel shows about the session, given the link and the state of its own tab.
pub fn link_status(state: &LinkState, tab: SshStatus) -> LinkStatus {
    match state {
        LinkState::Ready(_) => LinkStatus::Ready,
        LinkState::Opening => LinkStatus::Connecting,
        LinkState::Failed(reason) => LinkStatus::Failed(reason.clone()),
        // The link thinks the connection is gone, but the tab has not said so (yet): it will, and a
        // reconnect follows. Until then the session is being replaced, not waiting for a reconnect
        // that may never come.
        LinkState::Disconnected if tab == SshStatus::Connected => LinkStatus::Connecting,
        LinkState::Disconnected | LinkState::Closed => LinkStatus::Disconnected,
        // Nothing has asked for a session yet: it is on its way if the connection is up (or coming),
        // and not coming while the connection is being re-made or is given up.
        LinkState::Idle => match tab {
            SshStatus::Connecting | SshStatus::Connected => LinkStatus::Connecting,
            SshStatus::Reconnecting(_) | SshStatus::Failed | SshStatus::Ended => LinkStatus::Disconnected,
        },
    }
}

impl Workspace {
    /// Gives a new connection its SFTP link (the session itself is opened lazily) and starts
    /// following it.
    pub(super) fn attach_link(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = runtime::get(cx) else { return };
        let (link, changes, events) = SftpLink::new(runtime.handle());
        let changed = cx.spawn_in(window, async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this.update_in(cx, |this, _, cx| this.sync_files(group_id, cx)).is_err() {
                    break;
                }
            }
        });
        let queue = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update_in(cx, |this, _, cx| this.on_transfer_event(group_id, &event, cx)).is_err() {
                    break;
                }
            }
        });
        if let Some(group) = self.group_mut(group_id) {
            group.link = Some(link);
            group.link_tasks = vec![changed, queue];
        }
    }

    /// An event of the connection's transfer queue. The link reopens a session that died; the
    /// transfer list (next step) shows the rest.
    fn on_transfer_event(&mut self, group_id: usize, event: &TransferEvent, cx: &mut Context<Self>) {
        if let Some(link) = self.group(group_id).and_then(|group| group.link.as_ref()) {
            link.handle_event(event);
        }
        cx.notify();
    }

    /// The connection came up (the first time, or after a reconnect).
    pub(super) fn link_connected(&mut self, group_id: usize, connection: &Connection, cx: &mut Context<Self>) {
        let wanted =
            self.sessions.iter().any(|s| s.files.is_some() && s.ssh().is_some_and(|ssh| ssh.group == group_id));
        if let Some(link) = self.group(group_id).and_then(|group| group.link.as_ref()) {
            // Reopens the session only if there was one; a panel that has been waiting for the
            // connection asks for it.
            link.connection_restored(connection);
            if wanted {
                link.open(connection);
            }
        }
        self.sync_files(group_id, cx);
    }

    /// The connection was lost: the session is gone, the transfers wait.
    pub(super) fn link_lost(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if let Some(link) = self.group(group_id).and_then(|group| group.link.as_ref()) {
            link.connection_lost();
        }
        self.sync_files(group_id, cx);
    }

    /// Opens the session of the connection (if it is not open or opening) for a panel that needs it.
    pub(super) fn open_sftp(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if let Some(group) = self.group(group_id)
            && let (Some(link), Some(connection)) = (group.link.as_ref(), group.live_connection())
        {
            link.open(&connection);
        }
        self.sync_files(group_id, cx);
    }

    /// Tells every panel of the connection where the session stands.
    pub(super) fn sync_files(&mut self, group_id: usize, cx: &mut Context<Self>) {
        let Some(group) = self.group(group_id) else { return };
        let state = group.link.as_ref().map(SftpLink::state);
        let panels: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|s| s.ssh().filter(|ssh| ssh.group == group_id).zip(s.files.as_ref()))
            .map(|(ssh, files)| (files.panel.clone(), ssh.status))
            .collect();
        for (panel, tab) in panels {
            let (status, session) = match &state {
                Some(state) => (link_status(state, tab), state.session().cloned()),
                None => (LinkStatus::Failed("SSH 运行时未能启动".into()), None),
            };
            panel.update(cx, |panel, cx| panel.set_link(status, session, cx));
        }
    }

    /// The panel the user picked in the header for the active tab.
    pub(super) fn show_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.active else { return };
        let Some(session) = self.sessions.get(ix) else { return };
        let available = match panel {
            Panel::Terminal => true,
            Panel::Files => session.ssh().is_some(),
            Panel::Forward | Panel::Monitor => false,
        };
        if !available {
            return;
        }
        if panel == Panel::Files && !self.ensure_files_tab(ix, window, cx) {
            return;
        }
        self.sessions[ix].panel = panel;
        if panel == Panel::Files
            && let Some(group) = self.sessions[ix].ssh().map(|ssh| ssh.group)
        {
            self.open_sftp(group, cx);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Creates the files panel of the tab at `ix` if it has none yet. `false` if it cannot be made.
    fn ensure_files_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.sessions[ix].files.is_some() {
            return true;
        }
        let Some(runtime) = runtime::get(cx) else { return false };
        let (session_id, terminal) = (self.sessions[ix].id, self.sessions[ix].terminal.clone());
        let panel = cx.new(|cx| FilesPanel::new(&terminal, runtime, window, cx));
        let subscription = cx.subscribe(&panel, move |this: &mut Self, _, event: &FilesEvent, cx| match event {
            FilesEvent::RetrySession => this.retry_sftp(session_id, cx),
            FilesEvent::SessionLost(session) => this.session_lost(session_id, session, cx),
        });
        self.sessions[ix].files = Some(FilesTab { panel, _subscription: subscription });
        true
    }

    /// A panel found its session unusable. The link reopens it if it is still the current one; either
    /// way every panel of the connection hears where the link stands (a panel that used an older
    /// session picks up the newer one).
    fn session_lost(&mut self, session_id: usize, session: &Arc<SftpSession>, cx: &mut Context<Self>) {
        let Some(group_id) =
            self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group)
        else {
            return;
        };
        if let Some(link) = self.group(group_id).and_then(|group| group.link.as_ref()) {
            link.session_lost(session);
        }
        self.sync_files(group_id, cx);
    }

    /// The panel asked for a new session after the last attempt failed.
    fn retry_sftp(&mut self, session_id: usize, cx: &mut Context<Self>) {
        if let Some(group) =
            self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group)
        {
            self.open_sftp(group, cx);
        }
    }

    /// Puts keyboard focus where the active tab's panel wants it: the terminal, or the file list.
    pub(super) fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active.and_then(|ix| self.sessions.get(ix)) else { return };
        let handle = match (&session.files, session.panel) {
            (Some(files), Panel::Files) => files.panel.focus_handle(cx),
            _ => session.view.focus_handle(cx),
        };
        window.focus(&handle, cx);
    }

    /// Debug review hook: refreshes the active tab's files panel.
    #[cfg(debug_assertions)]
    pub(crate) fn demo_refresh_files(&mut self, cx: &mut Context<Self>) {
        let panel = self.active.and_then(|ix| self.sessions.get(ix)).and_then(|s| s.files.as_ref());
        if let Some(panel) = panel.map(|files| files.panel.clone()) {
            panel.update(cx, |panel, cx| panel.demo_refresh(cx));
        }
    }

    /// Debug review hook: connects `host`, shows its files panel, goes to `dir` and selects `select`.
    #[cfg(debug_assertions)]
    pub(crate) fn demo_files(
        &mut self,
        dir: Option<&str>,
        select: Option<&str>,
        hint: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_panel(Panel::Files, window, cx);
        let Some(panel) = self.active.and_then(|ix| self.sessions.get(ix)).and_then(|s| s.files.as_ref()) else {
            return;
        };
        let panel = panel.panel.clone();
        panel.update(cx, |panel, cx| {
            if let Some(dir) = dir {
                panel.demo_go(dir, cx);
            }
            if let Some(name) = select {
                panel.demo_select(name);
            }
            if hint {
                panel.demo_hint(cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panel_waits_for_the_connection_and_gives_the_session_time_to_open() {
        let idle = LinkState::Idle;
        assert_eq!(link_status(&idle, SshStatus::Connecting), LinkStatus::Connecting);
        assert_eq!(link_status(&idle, SshStatus::Connected), LinkStatus::Connecting, "about to be opened");
        assert_eq!(link_status(&LinkState::Opening, SshStatus::Connected), LinkStatus::Connecting);
    }

    #[test]
    fn a_lost_connection_shows_as_disconnected_whatever_the_session_was() {
        for tab in [SshStatus::Reconnecting(2), SshStatus::Failed, SshStatus::Ended] {
            assert_eq!(link_status(&LinkState::Idle, tab), LinkStatus::Disconnected, "{tab:?}");
        }
        assert_eq!(link_status(&LinkState::Disconnected, SshStatus::Reconnecting(1)), LinkStatus::Disconnected);
        assert_eq!(link_status(&LinkState::Closed, SshStatus::Connected), LinkStatus::Disconnected);
    }

    #[test]
    fn a_connection_the_tab_still_thinks_is_up_means_a_new_session_not_a_reconnect() {
        // The link lost its session while the tab says Connected (a dead SFTP subsystem, say):
        // "连接已断开，重连后自动刷新" would promise a reconnect that never comes.
        assert_eq!(link_status(&LinkState::Disconnected, SshStatus::Connected), LinkStatus::Connecting);
        assert_eq!(link_status(&LinkState::Disconnected, SshStatus::Reconnecting(1)), LinkStatus::Disconnected);
    }

    #[test]
    fn a_session_that_could_not_be_opened_says_why() {
        let failed = LinkState::Failed("SFTP 子系统被服务器拒绝".into());
        assert_eq!(link_status(&failed, SshStatus::Connected), LinkStatus::Failed("SFTP 子系统被服务器拒绝".into()));
    }
}
