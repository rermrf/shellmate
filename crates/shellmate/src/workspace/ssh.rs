//! SSH sessions: connecting a saved host, answering its questions with dialogs, session state.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::component::WindowExt as _;
use gpui::component::notification::Notification;
use gpui::{AppContext as _, Context, Task, Window};
use shellmate_core::{Host, HostId};
use shellmate_ssh::SshError;
use shellmate_store::SecretKey;
use shellmate_term::{Terminal, TerminalView};
use tokio::task::{AbortHandle, JoinError};

use super::{Panel, Session, SessionKind, Workspace};
use crate::connect::{self, Connected, UiBridge, UiMsg};
use crate::dialogs::auth::{ChallengeDialog, SecretDialog};
use crate::dialogs::hostkey::HostKeyDialog;
use crate::runtime;
use crate::sidebar::LiveStatus;
use crate::theme::{ActivePalette, CARD_RADIUS};

/// Where an SSH session stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshStatus {
    /// Connecting, verifying the host key or authenticating.
    Connecting,
    /// The shell is running.
    Connected,
    /// The connection could not be established or broke.
    Failed,
    /// Over: the remote shell exited, or the user cancelled.
    Ended,
}

/// The SSH side of a session.
pub struct SshSession {
    /// The saved host the tab belongs to.
    pub host: Host,
    /// The route as resolved at the last connect: jump hosts, then `host`.
    pub chain: Vec<Host>,
    pub status: SshStatus,
    /// A dialog of this session may be open (closed together with the tab).
    prompting: bool,
    abort: Option<AbortHandle>,
    _tasks: Vec<Task<()>>,
}

impl SshSession {
    fn new(host: Host, chain: Vec<Host>) -> Self {
        Self { host, chain, status: SshStatus::Connecting, prompting: false, abort: None, _tasks: Vec::new() }
    }

    /// Whether a dialog of this session may still be open.
    pub fn has_open_prompt(&self) -> bool {
        self.prompting
    }

    /// The remote program ended.
    pub fn on_exited(&mut self) {
        self.status = SshStatus::Ended;
    }

    /// The connection broke (after it was up) or the attempt failed.
    pub fn on_disconnected(&mut self) {
        if self.status == SshStatus::Connected {
            self.status = SshStatus::Failed;
        }
    }

    /// Names of the jump hosts, in order.
    pub fn via(&self) -> Vec<&str> {
        let jumps = self.chain.len().saturating_sub(1);
        self.chain.iter().take(jumps).map(|h| h.name.as_str()).collect()
    }

    /// Sidebar dot of this session.
    pub fn live(&self) -> Option<LiveStatus> {
        match self.status {
            SshStatus::Connecting => Some(LiveStatus::Connecting),
            SshStatus::Connected => Some(LiveStatus::Ok),
            SshStatus::Failed => Some(LiveStatus::Failed),
            SshStatus::Ended => None,
        }
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        // Closing a tab that is still connecting cancels the connection task.
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

/// `deploy@10.0.1.21:22 · SSH · 私钥 · 经由 bastion`
pub fn subtitle(host: &Host, via: &[&str], default_user: &str) -> String {
    let user = host.user.as_deref().unwrap_or(default_user);
    let mut parts =
        vec![format!("{user}@{}:{}", host.addr, host.port), "SSH".to_string(), host.auth.label().to_string()];
    if !via.is_empty() {
        parts.push(format!("经由 {}", via.join(" → ")));
    }
    parts.join(" · ")
}

/// Status bar text: `PTY 142×35    xterm-256color    UTF-8    保活 30s`
pub fn status_line(cols: u16, rows: u16) -> String {
    format!("PTY {cols}×{rows}    xterm-256color    UTF-8    保活 30s")
}

/// Why a panel is unavailable in an SSH session (the later phases bring them).
pub fn panel_hint(panel: Panel) -> &'static str {
    match panel {
        Panel::Terminal => "",
        Panel::Files => "文件管理（P3 实现）",
        Panel::Forward => "端口转发（P4 实现）",
        Panel::Monitor => "服务器监控（P5 实现）",
    }
}

impl Workspace {
    /// Opens a tab for the saved host and starts connecting.
    pub(super) fn connect_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let resolved = self.store.borrow().resolve_route(id);
        let chain = match resolved {
            Ok(chain) => chain,
            Err(err) => {
                window.push_notification(Notification::error(err.to_string()).title("无法连接"), cx);
                return;
            }
        };
        let Some(host) = chain.last().cloned() else { return };
        if host.protocol != shellmate_core::Protocol::Ssh {
            let note = format!("{} 将在后续阶段（P7）实现，目前只能连接 SSH 主机。", host.protocol.label());
            window.push_notification(Notification::info(note).title(format!("暂不能连接 {}", host.name)), cx);
            return;
        }

        let style = cx.terminal_style();
        let palette = style.palette.clone();
        let terminal = cx.new(|cx| Terminal::new(palette, cx));
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal.clone(), style, cx);
            view.top_radius = CARD_RADIUS;
            view
        });
        let session_id = self.next_id;
        self.next_id += 1;
        let subscription = self.watch_terminal(session_id, &terminal, window, cx);
        self.sessions.push(Session {
            id: session_id,
            title: host.name.clone().into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            kind: SessionKind::Ssh(Box::new(SshSession::new(host, chain))),
            _subscription: subscription,
        });
        self.activate(self.sessions.len() - 1, window, cx);
        self.start_connect(session_id, window, cx);
    }

    /// Connects the saved host called `name` (debug review hook).
    #[cfg(debug_assertions)]
    pub(crate) fn connect_by_name(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.book.hosts.iter().find(|h| h.name == name).map(|h| h.id.clone()) {
            Some(id) => self.connect_host(&id, window, cx),
            None => log::warn!("no saved host called {name:?}"),
        }
    }

    /// Starts (or restarts) the connection of an SSH session.
    fn start_connect(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.sessions.iter().position(|s| s.id == session_id) else { return };
        let terminal = self.sessions[ix].terminal.clone();
        let Some(host_id) = self.sessions[ix].ssh().map(|s| s.host.id.clone()) else { return };

        let Some(runtime) = runtime::get(cx) else {
            self.fail(session_id, "SSH 运行时未能启动", window, cx);
            return;
        };
        // The host may have been edited since the last attempt.
        let resolved = self.store.borrow().resolve_route(&host_id);
        let chain = match resolved {
            Ok(chain) => chain,
            Err(err) => {
                self.fail(session_id, &err.to_string(), window, cx);
                return;
            }
        };
        let Some(host) = chain.last().cloned() else { return };

        let (tx, rx) = async_channel::unbounded();
        let bridge = UiBridge::new(tx, &chain, &connect::default_user());
        let names = Rc::new(connect::names_by_address(&chain));
        let dims = terminal.read(cx).size().dims();
        let join = runtime.spawn(connect::connect_and_open(chain.clone(), self.secrets.clone(), bridge, dims));
        let abort = join.abort_handle();

        let pump_rx = rx.clone();
        let pump_names = names.clone();
        let pump = cx.spawn_in(window, async move |this, cx| {
            while let Ok(msg) = pump_rx.recv().await {
                let names = pump_names.clone();
                if this
                    .update_in(cx, |this, window, cx| this.on_ui_message(session_id, &names, msg, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let result = cx.spawn_in(window, async move |this, cx| {
            let outcome = join.await;
            // Deliver what the SSH task said before it finished, so that progress lines and
            // dialogs come before the shell's own output.
            while let Ok(msg) = rx.try_recv() {
                let names = names.clone();
                if this
                    .update_in(cx, |this, window, cx| this.on_ui_message(session_id, &names, msg, window, cx))
                    .is_err()
                {
                    return;
                }
            }
            this.update_in(cx, |this, window, cx| this.on_connect_result(session_id, outcome, window, cx)).ok();
        });

        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id)
            && let SessionKind::Ssh(ssh) = &mut session.kind
        {
            ssh.host = host;
            ssh.chain = chain;
            ssh.status = SshStatus::Connecting;
            ssh.prompting = false;
            ssh.abort = Some(abort);
            ssh._tasks = vec![pump, result];
        }
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// Enter on a session whose connection ended: connect again, in the same tab.
    pub(super) fn reconnect(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_connect(session_id, window, cx);
    }

    fn fail(&mut self, session_id: usize, reason: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { return };
        if let SessionKind::Ssh(ssh) = &mut session.kind {
            ssh.status = SshStatus::Failed;
        }
        session.terminal.update(cx, |terminal, cx| terminal.disconnect(reason.to_string(), cx));
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// A message from the SSH task: progress to print, or a question to answer.
    fn on_ui_message(
        &mut self,
        session_id: usize,
        names: &HashMap<String, String>,
        msg: UiMsg,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { return };
        let prompting = !matches!(msg, UiMsg::Event(_));
        if let SessionKind::Ssh(ssh) = &mut session.kind {
            ssh.prompting = prompting;
        }
        match msg {
            UiMsg::Event(event) => {
                let line = connect::format_event(&event, names);
                session.terminal.update(cx, |terminal, cx| terminal.notice(&line, cx));
            }
            UiMsg::HostKey { request, reply } => HostKeyDialog::open(request, reply, window, cx),
            UiMsg::Password { request, reply } => SecretDialog::open_password(request, reply, window, cx),
            UiMsg::Passphrase { request, reply } => SecretDialog::open_passphrase(request, reply, window, cx),
            UiMsg::Challenge { request, reply } => ChallengeDialog::open(request, reply, window, cx),
        }
    }

    /// The SSH task finished: attach the shell, or show why it failed.
    fn on_connect_result(
        &mut self,
        session_id: usize,
        outcome: Result<Result<Connected, SshError>, JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { return };
        let terminal = session.terminal.clone();
        let SessionKind::Ssh(ssh) = &mut session.kind else { return };
        ssh.prompting = false;
        ssh.abort = None;
        let host_id = ssh.host.id.clone();

        match outcome {
            Ok(Ok(connected)) => {
                ssh.status = SshStatus::Connected;
                let Connected { shell, saved, save_errors } = connected;
                terminal.update(cx, |terminal, cx| {
                    terminal.attach(move |sink, dims| Ok(Box::new(shell.start(sink, dims))), cx)
                });
                if let Err(err) = self.store.borrow_mut().touch_connected(&host_id) {
                    log::warn!("could not record the connection time: {err}");
                }
                self.reload_book(window, cx);
                self.announce_saved(&saved, &save_errors, window, cx);
            }
            Ok(Err(err)) => {
                ssh.status = if err.is_cancelled() { SshStatus::Ended } else { SshStatus::Failed };
                terminal.update(cx, |terminal, cx| terminal.disconnect(err.to_string(), cx));
            }
            Err(err) if err.is_cancelled() => {}
            Err(err) => {
                ssh.status = SshStatus::Failed;
                terminal.update(cx, |terminal, cx| terminal.disconnect(format!("连接任务异常终止：{err}"), cx));
            }
        }
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// Tells the user what went to the keychain (or why not) after a successful login.
    fn announce_saved(&mut self, saved: &[SecretKey], errors: &[String], window: &mut Window, cx: &mut Context<Self>) {
        if let Some(error) = errors.first() {
            window.push_notification(
                Notification::warning(format!("已连接，但无法存入钥匙串：{error}")).title("钥匙串"),
                cx,
            );
        } else if !saved.is_empty() {
            let what =
                if saved.iter().any(|k| matches!(k, SecretKey::KeyPassphrase(_))) { "私钥口令" } else { "密码" };
            window.push_notification(
                Notification::success(format!("{what}已存入 macOS 钥匙串，下次自动填写")).title("已连接"),
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use shellmate_core::AuthMethod;

    use super::*;

    fn host(name: &str, user: Option<&str>) -> Host {
        let mut host = Host::new(name, format!("{name}.example.com"));
        host.user = user.map(String::from);
        host
    }

    #[test]
    fn subtitle_matches_the_plan() {
        let mut db = host("db", Some("deploy"));
        db.auth = AuthMethod::PrivateKey;
        assert_eq!(subtitle(&db, &[], "me"), "deploy@db.example.com:22 · SSH · 私钥");
        assert_eq!(subtitle(&db, &["bastion"], "me"), "deploy@db.example.com:22 · SSH · 私钥 · 经由 bastion");
        assert_eq!(subtitle(&db, &["a", "b"], "me"), "deploy@db.example.com:22 · SSH · 私钥 · 经由 a → b");
        db.user = None;
        db.port = 2222;
        db.auth = AuthMethod::Agent;
        assert_eq!(subtitle(&db, &[], "me"), "me@db.example.com:2222 · SSH · ssh-agent");
    }

    #[test]
    fn status_line_wording() {
        assert_eq!(status_line(142, 35), "PTY 142×35    xterm-256color    UTF-8    保活 30s");
    }

    #[test]
    fn panels_name_the_phase_that_brings_them() {
        assert_eq!(panel_hint(Panel::Files), "文件管理（P3 实现）");
        assert_eq!(panel_hint(Panel::Forward), "端口转发（P4 实现）");
        assert_eq!(panel_hint(Panel::Monitor), "服务器监控（P5 实现）");
    }

    #[test]
    fn status_maps_to_sidebar_dots() {
        let mut session = SshSession::new(host("db", None), vec![host("bastion", None), host("db", None)]);
        assert_eq!(session.live(), Some(LiveStatus::Connecting));
        assert_eq!(session.via(), ["bastion"]);
        session.status = SshStatus::Connected;
        assert_eq!(session.live(), Some(LiveStatus::Ok));
        session.on_disconnected();
        assert_eq!((session.status, session.live()), (SshStatus::Failed, Some(LiveStatus::Failed)));
        session.on_exited();
        assert_eq!((session.status, session.live()), (SshStatus::Ended, None));
    }

    #[test]
    fn a_failed_attempt_is_not_downgraded_by_its_own_disconnect_event() {
        let mut session = SshSession::new(host("db", None), vec![host("db", None)]);
        session.status = SshStatus::Ended;
        session.on_disconnected();
        assert_eq!(session.status, SshStatus::Ended);
        session.status = SshStatus::Connecting;
        session.on_disconnected();
        assert_eq!(session.status, SshStatus::Connecting);
    }
}
