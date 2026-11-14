//! The 文件 panel inside the workspace: one panel per SSH tab, all of a connection's panels on the
//! one SFTP session (and transfer queue) of its [`ConnGroup`](super::group::ConnGroup).
//!
//! The session is opened the first time a panel of the connection is shown. It follows the
//! connection: gone when the connection is lost, opened again after a reconnect if it was open
//! before, and reopened (once) when the SFTP subsystem dies under a live connection. All of that
//! is [`SftpLink`]'s job; this module opens it, tells it what happens to the connection and hands
//! what it says to the panels.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Focusable as _, Subscription, Window};
use shellmate_sftp::SftpSession;
use shellmate_sftp::link::{LinkState, SftpLink};
use shellmate_sftp::transfer::{Choice, Clash, ItemId, TransferEvent, TransferState};
use shellmate_ssh::Connection;

use super::ssh::SshStatus;
use super::{Panel, Workspace};
use crate::dialogs::clash::{Answer, ClashDialog};
use crate::files::model::LinkStatus;
use crate::files::panel::{FilesEvent, FilesPanel};
use crate::files::transfers::TransferList;
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
                if this.update_in(cx, |this, window, cx| this.on_transfer_event(group_id, &event, window, cx)).is_err()
                {
                    break;
                }
            }
        });
        let transfers = cx.new(|_| TransferList::new());
        if let Some(group) = self.group_mut(group_id) {
            group.link = Some(link);
            group.link_tasks = vec![changed, queue];
            group.transfers = Some(transfers);
        }
    }

    /// An event of the connection's transfer queue: the link reopens a session that died, the list
    /// every panel of the connection shows takes it in, a finished upload makes the panels that show
    /// its directory list it again, and names that exist are asked about.
    fn on_transfer_event(
        &mut self,
        group_id: usize,
        event: &TransferEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.group(group_id) {
            if let Some(link) = &group.link {
                link.handle_event(event);
            }
            if let Some(transfers) = &group.transfers {
                transfers.update(cx, |list, cx| {
                    list.apply(event);
                    cx.notify();
                });
            }
        }
        match event {
            TransferEvent::RemoteChanged(dir) => self.remote_changed(group_id, dir, cx),
            TransferEvent::Added(view) | TransferEvent::State(view) if view.state == TransferState::AwaitingClash => {
                self.pump_clash(group_id, window, cx)
            }
            _ => {}
        }
        cx.notify();
    }

    /// Something changed in `dir` on the server: every panel of the connection that shows it lists it again.
    fn remote_changed(&mut self, group_id: usize, dir: &str, cx: &mut Context<Self>) {
        let panels: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| s.ssh().is_some_and(|ssh| ssh.group == group_id))
            .filter_map(|s| s.files.as_ref().map(|files| files.panel.clone()))
            .collect();
        for panel in panels {
            panel.update(cx, |panel, cx| panel.on_remote_changed(dir, cx));
        }
    }

    // ---- names that exist ---------------------------------------------------------------------------

    /// Asks about the first upload of the connection that is waiting for an answer, if the tab on
    /// screen is one of the connection's and shows its files (otherwise the question waits for the
    /// user to get there). One dialog at a time; one closed with Esc is not asked again by itself.
    pub(super) fn pump_clash(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let showing = self
            .active
            .and_then(|ix| self.sessions.get(ix))
            .is_some_and(|s| s.panel == Panel::Files && s.ssh().is_some_and(|ssh| ssh.group == group_id));
        let Some(group) = self.group(group_id).filter(|group| showing && group.clash.open.is_none()) else { return };
        let Some(transfers) = group.transfers.clone() else { return };
        let next = transfers
            .read(cx)
            .awaiting_clash()
            .find(|view| !group.clash.dismissed.contains(&view.id))
            .and_then(|view| view.clash.clone().map(|clash| (view.id, clash)));
        if let Some((id, clash)) = next {
            self.open_clash(group_id, id, clash, window, cx);
        }
    }

    /// The group of the tab on screen, if it is an SSH tab.
    fn active_group(&self) -> Option<usize> {
        self.active.and_then(|ix| self.sessions.get(ix)).and_then(|s| s.ssh()).map(|ssh| ssh.group)
    }

    /// Asks the question of the connection waiting on screen, if any (after switching tabs or panels).
    pub(super) fn pump_active_clash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group_id) = self.active_group() {
            self.pump_clash(group_id, window, cx);
        }
    }

    fn open_clash(&mut self, group_id: usize, id: ItemId, clash: Clash, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.group_mut(group_id) {
            group.clash.open = Some(id);
            group.clash.dismissed.remove(&id);
        }
        let workspace = cx.entity().downgrade();
        // The dialog is closing when it answers: what that sets going (the next question, say) waits until it is gone.
        let answer: Answer = Rc::new(move |id, choice, window, cx| {
            let workspace = workspace.clone();
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |this, cx| this.on_clash_answer(group_id, id, choice, window, cx)).ok();
            });
        });
        ClashDialog::open(id, clash, answer, window, cx);
    }

    /// The dialog was answered (`Some`) or closed with Esc (`None`: the item keeps waiting).
    fn on_clash_answer(
        &mut self,
        group_id: usize,
        id: ItemId,
        choice: Option<Choice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.group_mut(group_id) {
            group.clash.open = None;
            match choice {
                Some(choice) => {
                    group.clash.dismissed.remove(&id);
                    if let Some(link) = &group.link {
                        link.queue().answer_clash(id, choice);
                    }
                }
                None => {
                    group.clash.dismissed.insert(id);
                }
            }
        }
        self.pump_clash(group_id, window, cx);
    }

    /// 处理 on a queue row: the question about item `id` again.
    fn handle_clash(&mut self, session_id: usize, id: ItemId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group_id) =
            self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group)
        else {
            return;
        };
        let Some(group) = self.group(group_id).filter(|group| group.clash.open.is_none()) else { return };
        let clash = group
            .transfers
            .as_ref()
            .and_then(|transfers| {
                transfers.read(cx).get(id).filter(|v| v.state == TransferState::AwaitingClash).cloned()
            })
            .and_then(|view| view.clash);
        if let Some(clash) = clash {
            self.open_clash(group_id, id, clash, window, cx);
        }
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
        let queue = group.link.as_ref().map(|link| link.queue().clone());
        let transfers = group.transfers.clone();
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
            panel.update(cx, |panel, cx| {
                panel.set_transfers(queue.clone(), transfers.clone(), cx);
                panel.set_link(status, session, cx);
            });
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
        self.pump_active_clash(window, cx);
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
        let subscription =
            cx.subscribe_in(&panel, window, move |this: &mut Self, _, event: &FilesEvent, window, cx| match event {
                FilesEvent::RetrySession => this.retry_sftp(session_id, cx),
                FilesEvent::SessionLost(session) => this.session_lost(session_id, session, cx),
                FilesEvent::HandleClash(id) => this.handle_clash(session_id, *id, window, cx),
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

    /// Debug review hook: the files panel of the active tab.
    #[cfg(debug_assertions)]
    fn demo_panel(&self) -> Option<Entity<FilesPanel>> {
        self.active.and_then(|ix| self.sessions.get(ix)).and_then(|s| s.files.as_ref()).map(|files| files.panel.clone())
    }

    /// Debug review hook: something the user does to the entry `name`, done by calling the panel.
    /// `what` is `menu`, `rename-error` (with `arg` typed in), `delete`, `permissions`, `drag`, `download`, `newfolder` or `upload` (the path in `arg`).
    #[cfg(debug_assertions)]
    pub(crate) fn demo_act(&mut self, what: &str, name: &str, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::{point, px};
        let Some(panel) = self.demo_panel() else { return };
        panel.update(cx, |panel, cx| match what {
            "menu" => panel.demo_menu(name, point(px(400.), px(300.)), cx),
            "rename-error" => panel.demo_rename(name, arg, cx),
            "delete" => panel.demo_delete(name, window, cx),
            "permissions" => panel.demo_permissions(name, window, cx),
            "drag" => panel.demo_drag(true, cx),
            "download" => {
                panel.demo_select(name);
                panel.demo_download(cx)
            }
            "newfolder" => panel.demo_new_folder(cx),
            "upload" => panel.demo_upload(vec![std::path::PathBuf::from(arg)], cx),
            other => log::warn!("unknown files action {other:?}"),
        });
    }

    /// Debug review hook: one step of the transfer queue show. Everything it uploads comes from
    /// `$SHELLMATE_DEMO_UPLOADS`, and every download goes to `$SHELLMATE_DOWNLOADS`.
    #[cfg(debug_assertions)]
    pub(crate) fn demo_queue_step(&mut self, step: u8, window: &mut Window, cx: &mut Context<Self>) {
        use std::path::PathBuf;

        use gpui::component::WindowExt as _;
        let Some(panel) = self.demo_panel() else { return };
        let Some(uploads) = std::env::var_os("SHELLMATE_DEMO_UPLOADS").map(PathBuf::from) else {
            return log::error!("set SHELLMATE_DEMO_UPLOADS to a scratch directory with the files to send");
        };
        if std::env::var_os("SHELLMATE_DOWNLOADS").is_none() {
            return log::error!(
                "set SHELLMATE_DOWNLOADS to a scratch directory: the demo must not write to ~/Downloads"
            );
        }
        let at = |name: &str| uploads.join(name);
        match step {
            // A small upload that is done in no time, a folder with a link in it (skipped), one that
            // is refused, and a big one that will be paused.
            1 => panel.update(cx, |panel, cx| {
                let dir = panel.demo_cwd();
                if let Some(queue) = panel.demo_queue() {
                    queue.enqueue_upload(vec![at("done.txt")], &dir);
                    queue.enqueue_upload(vec![at("tree")], &dir);
                    queue.enqueue_upload(vec![at("fail.txt")], "/root");
                    queue.enqueue_upload(vec![at("paused.bin")], &dir);
                }
                cx.notify();
            }),
            2 => panel.update(cx, |panel, _| panel.demo_pause_named("paused.bin")),
            // Two big ones running, one queued behind them, a download, and one whose name exists.
            3 => panel.update(cx, |panel, cx| {
                let dir = panel.demo_cwd();
                if let (Some(queue), Ok(downloads)) =
                    (panel.demo_queue(), crate::files::transfers::ensure_downloads_dir())
                {
                    queue.enqueue_upload(vec![at("nginx.conf")], &dir);
                    queue.enqueue_upload(vec![at("big-1.bin")], &dir);
                    queue.enqueue_upload(vec![at("big-2.bin")], &dir);
                    queue.enqueue_upload(vec![at("queued.bin")], &dir);
                    queue.enqueue_download(vec![format!("{dir}/app.log")], downloads);
                }
                cx.notify();
            }),
            // Esc on the dialog.
            4 => {
                let Some((group_id, id)) = self
                    .active_group()
                    .and_then(|g| self.group(g).and_then(|group| group.clash.open).map(|id| (g, id)))
                else {
                    return;
                };
                window.close_all_dialogs(cx);
                self.on_clash_answer(group_id, id, None, window, cx);
            }
            5 => panel.update(cx, |panel, cx| {
                panel.demo_scroll_queue(3);
                cx.notify();
            }),
            // 处理 on the row that is still waiting.
            6 => panel.update(cx, |panel, cx| panel.demo_handle_clash(cx)),
            other => log::warn!("unknown queue step {other}"),
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
