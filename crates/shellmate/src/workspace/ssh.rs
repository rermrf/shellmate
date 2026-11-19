//! SSH sessions: connecting a saved host, answering its questions with dialogs, reconnecting
//! after a loss, and opening more tabs on a connection that is already up.
//!
//! A tab is a [`Session`] with an [`SshSession`] (its own state); the connection behind it is a
//! [`ConnGroup`] shared by every tab on that connection. All connecting, waiting and retrying
//! happens per group; a tab only reacts to what its group decides.

use shellmate_store::SecretStore as _;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::component::WindowExt as _;
use gpui::component::notification::Notification;
use gpui::{AppContext as _, Context, Entity, Task, Window};
use shellmate_core::{Host, HostId, PtyDims};
use shellmate_ssh::{Connection, PendingShell, SshError};
use shellmate_store::SecretKey;
use shellmate_term::{Terminal, TerminalView};
use tokio::task::JoinError;

use super::group::{AbortOnDrop, Attempt, ConnGroup, PoolKey, Prompt};
use super::{Panel, Session, SessionKind, Workspace};
use crate::connect::{self, AuthMemory, ConnectFailure, Connected, SaveReport, UiBridge, UiMsg};
use crate::dialogs::auth::{ChallengeDialog, SecretDialog};
use crate::dialogs::hostkey::HostKeyDialog;
use crate::reconnect::{self, Failure, Input, MAX_ATTEMPTS, Output, Phase, Reconnect, View};
use crate::runtime;
use crate::sidebar::LiveStatus;
use crate::theme::{ActivePalette, CARD_RADIUS};

// A rejected key still needs attention on a first connection, including at a jump hop.
fn cancelled_auth_prompt(error: &SshError) -> bool {
    match error {
        SshError::AuthCancelled => true,
        SshError::Jump { source, .. } => cancelled_auth_prompt(source),
        _ => false,
    }
}

/// Where an SSH tab stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshStatus {
    /// Connecting, verifying the host key or authenticating.
    Connecting,
    /// The shell is running.
    Connected,
    /// The connection broke; attempt `n` of 5 is waiting or running.
    Reconnecting(u32),
    /// The connection could not be established, or the automatic attempts ran out.
    Failed,
    /// Over: the remote shell exited, or the user cancelled.
    Ended,
}

impl SshStatus {
    /// Whether a connection attempt is going to give this tab a shell.
    pub fn wants_channel(self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting(_) | Self::Failed)
    }

    /// The status after the connection's reconnect loop moved to `view`. Ended tabs are not part
    /// of the loop; a tab that is up stays up unless the connection was lost.
    pub fn on_view(self, view: View) -> Self {
        match (self, view) {
            (Self::Ended, _) | (_, View::Connected) => self,
            (_, View::Reconnecting(n)) => Self::Reconnecting(n),
            (status, View::Connecting) => if status.wants_channel() { Self::Connecting } else { status },
            (status, View::Failed) => if status.wants_channel() { Self::Failed } else { status },
        }
    }
}

/// A tab's connection to one thing in flight: opening a channel on a connection that is up.
struct Opening {
    _abort: AbortOnDrop,
    _task: Task<()>,
}

/// The SSH side of a session.
pub struct SshSession {
    /// The saved host the tab belongs to.
    pub host: Host,
    /// The route as resolved at the last connect: jump hosts, then `host`.
    pub chain: Vec<Host>,
    pub status: SshStatus,
    /// The connection this tab is on ([`ConnGroup::id`]).
    pub group: usize,
    /// The shell channel number on the connection (the nth one opened on it).
    pub channel: u32,
    pub(super) mosh: Option<super::mosh::MoshTab>,
    /// A channel is being opened for this tab on a live connection.
    opening: Option<Opening>,
}

impl SshSession {
    #[cfg(debug_assertions)]
    pub(super) fn demo(host: Host) -> Self {
        let mut this = Self::new(host.clone(), vec![host], usize::MAX);
        this.status = SshStatus::Connected;
        this
    }

    fn new(host: Host, chain: Vec<Host>, group: usize) -> Self {
        let mosh=(host.protocol==shellmate_core::Protocol::Mosh).then(super::mosh::MoshTab::default);
        Self { host, chain, status: SshStatus::Connecting, group, channel: 1, opening: None,mosh }
    }

    /// A panel transport transition never changes an independent Mosh terminal.
    fn on_connection_view(&mut self,view:View){if self.mosh.as_ref().is_none_or(|m|!m.independent){self.status=self.status.on_view(view);}}

    /// Names of the jump hosts, in order.
    pub fn via(&self) -> Vec<&str> {
        let jumps = self.chain.len().saturating_sub(1);
        self.chain.iter().take(jumps).map(|h| h.name.as_str()).collect()
    }

    /// Sidebar dot of this session. A tab waiting to reconnect counts as connecting (yellow).
    pub fn live(&self) -> Option<LiveStatus> {
        match self.status {
            SshStatus::Connecting | SshStatus::Reconnecting(_) => Some(LiveStatus::Connecting),
            SshStatus::Connected => Some(LiveStatus::Ok),
            SshStatus::Failed => Some(LiveStatus::Failed),
            SshStatus::Ended => None,
        }
    }

    /// The badge on the tab: `重连 2/5` (yellow) or `连接失败` (red).
    pub fn badge(&self) -> Option<String> {
        match self.status {
            SshStatus::Reconnecting(n) => Some(format!("重连 {n}/{MAX_ATTEMPTS}")),
            SshStatus::Failed => Some("连接失败".to_string()),
            SshStatus::Connecting | SshStatus::Connected | SshStatus::Ended => None,
        }
    }

    /// What the header subtitle appends to the target: the state if it is not the usual one.
    pub fn status_note(&self) -> Option<String> {
        match self.status {
            SshStatus::Reconnecting(n) => Some(format!("重连中 {n}/{MAX_ATTEMPTS}")),
            SshStatus::Failed => Some("连接失败".to_string()),
            SshStatus::Ended => Some("已退出".to_string()),
            SshStatus::Connecting | SshStatus::Connected => None,
        }
    }
}

/// A command line to put in a tab's shell as soon as the shell is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedLine {
    /// What to type (pasted, so that a shell that asks for it does not take it for a series of keys).
    pub text: String,
    /// Whether to press Enter after it. Without, the user presses it (and types the password of a `sudo`).
    pub enter: bool,
}

/// How long the shell of a new tab must be quiet (after it wrote something) before a line is typed into it.
pub const TYPE_QUIET: Duration = Duration::from_millis(200);
/// How long a line waits for its shell at the most.
pub const TYPE_CAP: Duration = Duration::from_secs(3);

/// Whether it is time to type the pending line: the shell wrote something and has been quiet for
/// [`TYPE_QUIET`] (as of `now`), or [`TYPE_CAP`] has passed since the tab was opened.
pub fn typing_time(last_output: Option<std::time::Instant>, waited: Duration, now: std::time::Instant) -> bool {
    waited >= TYPE_CAP || last_output.is_some_and(|at| now.saturating_duration_since(at) >= TYPE_QUIET)
}

/// What connecting a host does about tabs that already exist for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    /// No tab for the host: open one.
    None,
    /// A tab is connecting, connected or reconnecting: switch to it (the tab with this index).
    Switch(usize),
    /// A tab for the host has ended or failed: switch to it and connect again in place.
    Reconnect(usize),
}

/// Finds the tab to reuse when connecting `host`: a live one first, else one that ended.
/// `sessions` yields the index and SSH state of every SSH tab.
pub fn existing_session<'a>(sessions: impl Iterator<Item = (usize, &'a SshSession)>, host: &HostId) -> Existing {
    let mut ended = None;
    for (index, ssh) in sessions.filter(|(_, ssh)| &ssh.host.id == host) {
        match ssh.status {
            SshStatus::Connecting | SshStatus::Connected | SshStatus::Reconnecting(_) => return Existing::Switch(index),
            SshStatus::Failed | SshStatus::Ended => {
                ended.get_or_insert(index);
            }
        }
    }
    ended.map_or(Existing::None, Existing::Reconnect)
}

/// `deploy@10.0.1.21:22 · SSH · 私钥 · 经由 bastion`
pub fn subtitle(host: &Host, via: &[&str], default_user: &str) -> String {
    let user = host.user.as_deref().unwrap_or(default_user);
    if host.protocol==shellmate_core::Protocol::Mosh{return format!("{user}@{}:{} · Mosh",host.addr,host.port);}
    let mut parts =
        vec![format!("{user}@{}:{}", host.addr, host.port), host.protocol.label().to_string(), host.auth.label().to_string()];
    if !via.is_empty() {
        parts.push(format!("经由 {}", via.join(" → ")));
    }
    parts.join(" · ")
}

/// Status bar text: `PTY 142×35    xterm-256color    UTF-8    保活 30s`
pub fn status_line(cols: u16, rows: u16) -> String {
    format!("PTY {cols}×{rows}    xterm-256color    UTF-8    保活 30s")
}

/// The line a copied tab prints when it rides on the existing connection.
pub fn reuse_line(host: &str, channel: u32) -> String {
    format!("\x1b[2m→ 复用到 {host} 的已有连接 · 新开 channel #{channel}")
}

/// Why a panel is unavailable in an SSH session (the later phases bring them).
pub fn panel_hint(panel: Panel) -> &'static str {
    match panel {
        Panel::Terminal => "",
        Panel::Files => "",
        Panel::Forward => "",
        Panel::Monitor => "",
    }
}

impl Workspace {
    // ---- lookups ---------------------------------------------------------------------------

    pub(super) fn group(&self, id: usize) -> Option<&ConnGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub(super) fn group_mut(&mut self, id: usize) -> Option<&mut ConnGroup> {
        self.groups.iter_mut().find(|group| group.id == id)
    }

    pub(super) fn ssh_mut(&mut self, session_id: usize) -> Option<&mut SshSession> {
        self.sessions.iter_mut().find(|s| s.id == session_id).and_then(|s| match &mut s.kind {
            SessionKind::Ssh(ssh) => Some(&mut **ssh),
            SessionKind::Local | SessionKind::Stream(_) => None,
        })
    }

    /// The tabs of a connection that wait for a shell, with their terminals.
    fn participants(&self, group_id: usize) -> Vec<(usize, Entity<Terminal>)> {
        self.sessions
            .iter()
            .filter(|s| s.ssh().is_some_and(|ssh| ssh.group == group_id && ssh.status.wants_channel() && ssh.mosh.as_ref().is_none_or(|m|!m.independent)))
            .map(|s| (s.id, s.terminal.clone()))
            .collect()
    }

    /// Prints a line in the terminal of every tab waiting on the connection.
    fn notice_participants(&mut self, group_id: usize, line: &str, cx: &mut Context<Self>) {
        for (_, terminal) in self.participants(group_id) {
            terminal.update(cx, |terminal, cx| terminal.notice(line, cx));
        }
    }

    /// Brings the tabs of a connection in line with where its reconnect loop stands.
    fn sync_statuses(&mut self, group_id: usize) {
        let Some(view) = self.group(group_id).map(|group| group.machine.view()) else { return };
        for session in &mut self.sessions {
            if let SessionKind::Ssh(ssh) = &mut session.kind
                && ssh.group == group_id
            {
                ssh.on_connection_view(view);
            }
        }
    }

    /// A connection nobody is on any more goes away with whatever it was doing: the running
    /// attempt is aborted, and a dialog it asked is closed.
    pub(super) fn release_group(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.iter().any(|s| s.ssh().is_some_and(|ssh| ssh.group == group_id)) {
            return;
        }
        if let Some(ix) = self.groups.iter().position(|group| group.id == group_id) {
            let group = self.groups.remove(ix);
            if group.has_open_prompt() {
                window.close_all_dialogs(cx);
            }
            // The rules of the host go on with another connection to it, if there is one.
            let handoff = group.forward.as_ref().map(|forward| (group.key.host().clone(), forward.rules.clone()));
            drop(group);
            if let Some((host, rules)) = handoff {
                self.forward_handed_over(&host, rules, cx);
            }
        }
    }

    fn new_group(
        &mut self,
        chain: &[Host],
        machine: Reconnect,
        memory: Arc<AuthMemory>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let id = self.next_group;
        self.next_group += 1;
        let key = PoolKey::of(chain, &connect::default_user());
        self.groups.push(ConnGroup::new(id, key, machine, memory));
        self.attach_link(id, window, cx);
        self.attach_forwarder(id, window, cx);
        self.attach_monitor(id, window, cx);
        id
    }

    // ---- opening tabs ----------------------------------------------------------------------

    /// Adds a tab with a fresh terminal for `host` on connection `group` and makes it current.
    fn push_ssh_tab(
        &mut self,
        title: String,
        host: Host,
        chain: Vec<Host>,
        group: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let style = cx.terminal_style();
        let palette = style.palette.clone();
        let terminal = cx.new(|cx| Terminal::new(palette, cx));
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal.clone(), style, cx).with_search_icon(crate::icons::AppIcon::Search.at(14.));
            view.top_radius = CARD_RADIUS;
            view
        });
        let session_id = self.next_id;
        self.next_id += 1;
        let subscription = self.watch_terminal(session_id, &terminal, window, cx);
        self.sessions.push(Session {
            ai_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            id: session_id,
            title: title.into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            files: None,
            forward: None,
            monitor: None,
            typed: None,
            kind: SessionKind::Ssh(Box::new(SshSession::new(host, chain, group))),
            _subscription: subscription,
        });
        self.start_session_archives(session_id,window,cx);
        self.activate(self.sessions.len() - 1, window, cx);
        session_id
    }

    /// Resolves the route of a saved host for connecting, or tells the user why not.
    fn resolve_for_connect(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) -> Option<(Host, Vec<Host>)> {
        let resolved = self.temporary_hosts.get(id)
            .map(|h|crate::library::editor_route(&self.book,h))
            .unwrap_or_else(|| self.store.borrow().resolve_route(id).map_err(|e|e.to_string()));
        let chain = match resolved {
            Ok(chain) => chain,
            Err(err) => {
                window.push_notification(Notification::error(err.to_string()).title("无法连接"), cx);
                return None;
            }
        };
        let host = chain.last().cloned()?;
        // Existing SSH tabs can outlive an edit that changes the saved host's protocol.
        if !matches!(host.protocol,shellmate_core::Protocol::Ssh|shellmate_core::Protocol::Mosh) {
            let note = format!("主机配置已改为{}，请关闭该主机的旧会话后重新连接。", host.protocol.label());
            window.push_notification(Notification::info(note).title(format!("{} 的协议已更改", host.name)), cx);
            return None;
        }
        if host.protocol==shellmate_core::Protocol::Mosh {
            if chain.len()>1 || !host.jump.is_empty(){self.offer_mosh_ssh(host,chain,window,cx);return None;}
            if !crate::prototype_fixture() && let Err(e)=shellmate_ssh::mosh::find_client(){window.push_notification(Notification::error(e),cx);return None;}
        }
        Some((host, chain))
    }

    /// Opens a tab for the saved host and starts connecting; a host that already has a tab
    /// switches to it instead (a copy of a session is a separate action).
    pub(super) fn connect_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host)=self.book.host(id).or_else(||self.temporary_hosts.get(id)).filter(|h|matches!(h.protocol,shellmate_core::Protocol::Telnet|shellmate_core::Protocol::Serial)).cloned() {
            if let Some(ix)=self.sessions.iter().position(|s|s.host().is_some_and(|h|&h.id==id)) {self.activate(ix,window,cx);if !self.sessions[ix].stream_connected(){self.start_stream(self.sessions[ix].id,window,cx);}return;}
            self.open_stream(host,window,cx);return;
        }

        let tabs = self.sessions.iter().enumerate().filter_map(|(ix, s)| s.ssh().map(|ssh| (ix, ssh)));
        match existing_session(tabs, id) {
            Existing::Switch(ix) => return self.activate(ix, window, cx),
            Existing::Reconnect(ix) => {
                let session_id = self.sessions[ix].id;
                self.activate(ix, window, cx);
                return self.retry_tab(session_id, window, cx);
            }
            Existing::None => {}
        }
        let Some((host, chain)) = self.resolve_for_connect(id, window, cx) else { return };
        let group = self.new_group(&chain, Reconnect::initial(), AuthMemory::new(), window, cx);
        self.push_ssh_tab(host.name.clone(), host, chain, group, window, cx);
        self.launch_attempt(group, window, cx);
    }

    pub(super) fn open_ssh_fallback(&mut self,host:Host,chain:Vec<Host>,source:HostId,window:&mut Window,cx:&mut Context<Self>){
        let credential=self.pending_forms.get(&source).map(|(_,c)|c.clone()).or_else(||{
            self.secrets.get(&SecretKey::HostPassword(source)).ok().flatten().map(|secret|connect::FormCredential{method:shellmate_core::AuthMethod::Password,key:None,answer:connect::Answer{secret,save:false}})
        });
        if let Some(c)=credential {self.pending_forms.insert(host.id.clone(),(connect::Endpoint::of(&host,&connect::default_user()),c));}
        self.temporary_hosts.insert(host.id.clone(),host.clone());
        let group=self.new_group(&chain,Reconnect::initial(),AuthMemory::new(),window,cx);
        self.push_ssh_tab(host.name.clone(),host,chain,group,window,cx);
        self.launch_attempt(group,window,cx);
    }

    pub(super) fn connect_temporary(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        self.temporary_hosts.insert(host.id.clone(), host.clone());
        self.connect_host(&host.id, window, cx);
    }

    /// Connects the saved host called `name` (debug review hook).
    #[cfg(debug_assertions)]
    pub(crate) fn connect_by_name(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.book.hosts.iter().find(|h| h.name == name).map(|h| h.id.clone()) {
            Some(id) => self.connect_host(&id, window, cx),
            None => log::warn!("no saved host called {name:?}"),
        }
    }

    /// Opens a tab on a second connection to the host of the current tab, as ⌘⇧T does when the
    /// connection of the source cannot be shared (debug review hook).
    #[cfg(debug_assertions)]
    pub(crate) fn demo_second_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ssh) = self.active.and_then(|ix| self.sessions.get(ix)).and_then(|s| s.ssh()) else { return };
        let (source_group, host_id) = (ssh.group, ssh.host.id.clone());
        let Some((host, chain)) = self.resolve_for_connect(&host_id, window, cx) else { return };
        let title = format!("{} · 2", host.name);
        let memory = self.group(source_group).map_or_else(AuthMemory::new, |group| group.memory.duplicate());
        let group = self.new_group(&chain, Reconnect::initial(), memory, window, cx);
        self.push_ssh_tab(title, host, chain, group, window, cx);
        self.launch_attempt(group, window, cx);
    }

    // ---- connecting ------------------------------------------------------------------------

    /// Starts a connection attempt for the tabs of a group that wait for a shell: one connection,
    /// one channel each.
    fn launch_attempt(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let participants = self.participants(group_id);
        let host_id = participants
            .first()
            .and_then(|(id, _)| self.sessions.iter().find(|s| s.id == *id))
            .and_then(|s| s.ssh())
            .map(|ssh| ssh.host.id.clone())
            .or_else(||self.group(group_id).map(|g|g.key.host().clone()));
        let Some(host_id) = host_id else {
            return self.release_group(group_id, window, cx);
        };
        let fail = |this: &mut Self, reason: String, window: &mut Window, cx: &mut Context<Self>| {
            this.attempt_failed(group_id, Failure::Stop(reason), false, window, cx)
        };
        let Some(runtime) = runtime::get(cx) else {
            return fail(self, "SSH 运行时未能启动".into(), window, cx);
        };
        // The host may have been edited since the last attempt.
        let resolved = self.temporary_hosts.get(&host_id)
            .map(|h|crate::library::editor_route(&self.book,h))
            .unwrap_or_else(|| self.store.borrow().resolve_route(&host_id).map_err(|e|e.to_string()));
        let chain = match resolved {
            Ok(chain) => chain,
            Err(err) => return fail(self, err.to_string(), window, cx),
        };
        let Some(host) = chain.last().cloned() else { return };

        let tabs: Vec<(usize, PtyDims)> =
            participants.iter().map(|(id, terminal)| (*id, terminal.read(cx).size().dims())).collect();
        let memory = match self.group(group_id) {
            Some(group) => group.memory.clone(),
            None => return,
        };
        let (tx, rx) = async_channel::unbounded();
        let bridge = UiBridge::new(tx, &chain, &connect::default_user(), memory);
        for host in &chain {
            if let Some((endpoint, credential)) = self.pending_forms.remove(&host.id)
                && endpoint == connect::Endpoint::of(host, &connect::default_user())
            { bridge.supply_form(endpoint, credential); }
        }
        let names = Rc::new(connect::names_by_address(&chain));
        let join = runtime.spawn(connect::connect_attempt(chain.clone(), self.secrets.clone(), bridge, tabs));
        let abort = join.abort_handle();

        let pump_rx = rx.clone();
        let pump_names = names.clone();
        let pump = cx.spawn_in(window, async move |this, cx| {
            while let Ok(msg) = pump_rx.recv().await {
                let names = pump_names.clone();
                if this.update_in(cx, |this, window, cx| this.on_group_message(group_id, &names, msg, window, cx)).is_err()
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
                if this.update_in(cx, |this, window, cx| this.on_group_message(group_id, &names, msg, window, cx)).is_err()
                {
                    return;
                }
            }
            this.update_in(cx, |this, window, cx| this.on_attempt_result(group_id, outcome, window, cx)).ok();
        });

        let key = PoolKey::of(&chain, &connect::default_user());
        if let Some(group) = self.group_mut(group_id) {
            group.key = key;
            group.prompt = Prompt::None;
            group.attempt = Some(Attempt::new(abort, vec![pump, result]));
        }
        for session in &mut self.sessions {
            if let SessionKind::Ssh(ssh) = &mut session.kind
                && ssh.group == group_id
            {
                ssh.host = host.clone();
                ssh.chain = chain.clone();
            }
        }
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// Feeds the connection's reconnect loop and carries out what it asks for.
    pub(super) fn step_group(&mut self, group_id: usize, input: Input, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.group_mut(group_id) else { return };
        let outputs = group.machine.step(input);
        self.sync_statuses(group_id);
        for output in outputs {
            match output {
                Output::Wait { attempt, delay } => {
                    let last = self.group(group_id).and_then(|g| g.last_failure.clone());
                    self.notice_participants(group_id, &reconnect::waiting_line(attempt, delay, last.as_deref()), cx);
                    self.start_timer(group_id, delay, window, cx);
                }
                Output::Attempt { attempt } => {
                    let reconnecting = match self.group_mut(group_id) {
                        Some(group) => {
                            group.timer = None;
                            group.machine.has_connected()
                        }
                        None => return,
                    };
                    if reconnecting {
                        self.notice_participants(group_id, &reconnect::attempt_line(attempt), cx);
                    }
                    self.launch_attempt(group_id, window, cx);
                }
                // A success carries the shells: see `on_attempt_result`.
                Output::Connected { .. } => {}
                Output::GaveUp { reason, attempts } => self.give_up(group_id, &reason, attempts, window, cx),
            }
        }
        self.sync_files(group_id, cx);
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// Counts down to the next attempt.
    fn start_timer(&mut self, group_id: usize, delay: Duration, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |this, window, cx| this.step_group(group_id, Input::TimerElapsed, window, cx)).ok();
        });
        if let Some(group) = self.group_mut(group_id) {
            group.timer = Some(task);
        }
    }

    /// No more automatic attempts: every waiting tab shows the reason and waits for Enter.
    fn give_up(&mut self, group_id: usize, reason: &str, attempts: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.group_mut(group_id) else { return };
        let retried = group.machine.has_connected();
        // Cancelling an authentication prompt on the first connect ends the tab.
        let ended = group.declined && !retried;
        group.stop();
        let text = reconnect::gave_up_text(reason, attempts, retried);
        for (session_id, terminal) in self.participants(group_id) {
            if let Some(ssh) = self.ssh_mut(session_id) {
                ssh.status = if ended { SshStatus::Ended } else { SshStatus::Failed };
            }
            terminal.update(cx, |terminal, cx| {
                // Nothing reconnects by itself any more: the line says to press Enter.
                terminal.set_auto_reconnect(false);
                terminal.disconnect(text.clone(), cx)
            });
        }
        let _ = window;
    }

    /// An attempt failed (or could not even start): the loop decides what next.
    fn attempt_failed(&mut self, group_id: usize, failure: Failure, declined: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.group_mut(group_id) {
            group.attempt = None;
            group.prompt = Prompt::None;
            group.declined = declined;
            group.last_failure = match &failure {
                Failure::Retry(reason) | Failure::Stop(reason) => Some(reason.clone()),
            };
        }
        self.step_group(group_id, Input::Failed(failure), window, cx);
    }

    /// A message from the SSH task: progress to print, or a question to answer.
    fn on_group_message(
        &mut self,
        group_id: usize,
        names: &HashMap<String, String>,
        msg: UiMsg,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = match &msg {
            UiMsg::Event(_) => Prompt::None,
            UiMsg::HostKey { request, .. } if request.state == shellmate_core::FingerprintState::Changed => Prompt::ChangedKey,
            UiMsg::Password { request, .. } if request.attempt > 0 => Prompt::RetryAuth,
            UiMsg::Passphrase { request, .. } if request.attempt > 0 => Prompt::RetryAuth,
            UiMsg::Challenge { request, .. } if request.attempt > 0 => Prompt::RetryAuth,
            _ => Prompt::Waiting,
        };
        let Some(group) = self.group_mut(group_id) else { return };
        if group.prompt != prompt {
            group.prompt = prompt;
            cx.notify();
        }
        if prompt != Prompt::None {
            // The question is about this connection: show the tab it belongs to.
            let on_group = |s: &Session| s.ssh().is_some_and(|ssh| ssh.group == group_id);
            let showing = self.active.and_then(|ix| self.sessions.get(ix)).is_some_and(on_group);
            if !showing && let Some(ix) = self.sessions.iter().position(on_group) {
                self.activate(ix, window, cx);
            }
        }
        match msg {
            UiMsg::VaultUnlock { reason, reply } => crate::security::ensure_unlocked(self.secrets.clone(),reason,window,cx,move|ok,_,_|{let _=reply.send(ok);}),
            UiMsg::Event(event) => {
                let line = connect::format_event(&event, names);
                self.notice_participants(group_id, &line, cx);
            }
            UiMsg::HostKey { request, reply } => HostKeyDialog::open(request, reply, window, cx),
            UiMsg::Password { request, reply } => SecretDialog::open_password(request, reply, window, cx),
            UiMsg::Passphrase { request, reply } => SecretDialog::open_passphrase(request, reply, window, cx),
            UiMsg::Challenge { request, reply } => ChallengeDialog::open(request, reply, window, cx),
        }
    }

    /// The SSH task finished: attach the shells, or let the loop decide about the failure.
    fn on_attempt_result(
        &mut self,
        group_id: usize,
        outcome: Result<Result<Connected, ConnectFailure>, JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(Ok(connected)) => self.on_connected(group_id, connected, window, cx),
            Ok(Err(ConnectFailure { error, saves })) => {
                self.announce_saved(&saves, window, cx);
                let declined = cancelled_auth_prompt(&error);
                self.attempt_failed(group_id, reconnect::classify(&error), declined, window, cx);
            }
            Err(err) if err.is_cancelled() => {}
            Err(err) => {
                let reason = format!("连接任务异常终止：{err}");
                self.attempt_failed(group_id, Failure::Stop(reason), false, window, cx);
            }
        }
    }

    /// The connection is up: every tab that was waiting gets its shell.
    fn on_connected(&mut self, group_id: usize, connected: Connected, window: &mut Window, cx: &mut Context<Self>) {
        let Connected { connection, shells, saves } = connected;
        // SSH shells retain the transport; Mosh's group retains it for panels
        // because its terminal is a local client PTY, not an SSH channel.
        let live = Some(connection.clone());
        let mosh_group=self.sessions.iter().any(|s|s.ssh().is_some_and(|ssh|ssh.group==group_id && ssh.mosh.is_some()));
        let preload = self.preferences.get(shellmate_store::Preference::PreloadPanels) && std::env::var_os("SHELLMATE_DEMO").is_none();
        let Some(group) = self.group_mut(group_id) else { return };
        let reconnected = matches!(group.machine.step(Input::Succeeded).first(), Some(Output::Connected { reconnected: true }));
        group.preload_after = preload.then(|| (std::time::Instant::now() + Duration::from_secs(1), reconnected));
        let manual = std::mem::take(&mut group.manual);
        group.stop();
        group.last_failure = None;
        group.declined = false;
        group.connection = Some(connection.downgrade());
        group.auxiliary=mosh_group.then(||connection.clone());
        group.channels = shells.len() as u32;
        for session in &self.sessions {
            if let Some(ssh)=session.ssh().filter(|s|s.group==group_id) && let Some(lease)=ssh.mosh.as_ref().and_then(|m|m.lease.as_ref()){lease.reconnect(connection.clone());}
        }

        let mut first_host = None;
        for (channel, (session_id, shell)) in (1u32..).zip(shells) {
            let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { continue };
            let terminal = session.terminal.clone();
            if let SessionKind::Ssh(ssh) = &mut session.kind {
                ssh.status = SshStatus::Connected;
                ssh.channel = channel;
                first_host.get_or_insert_with(|| ssh.host.id.clone());
            }
            if reconnected {
                terminal.update(cx, |terminal, cx| terminal.notice(&reconnect::healed_line(manual), cx));
            }
            Self::attach_shell(&terminal, shell, cx);
            self.type_pending(session_id, window, cx);
        }
        // Tabs that started waiting while the attempt ran have no shell yet: they get a channel
        // on the connection that is up now.
        self.open_channels_for_waiting(group_id, window, cx);
        if let Some(live) = live {
            self.link_connected(group_id, &live, cx);
            self.forward_connected(group_id, &live, cx);
            self.monitor_connected(group_id, &live, cx);
        }

        if !reconnected && let Some(host_id) = first_host && !self.temporary_hosts.contains_key(&host_id) {
            if let Err(err) = self.store.borrow_mut().touch_connected(&host_id) {
                log::warn!("could not record the connection time: {err}");
            }
            self.reload_book(window, cx);
        }
        self.announce_saved(&saves, window, cx);
        self.sync_sidebar(cx);
        cx.notify();
    }

    fn attach_shell(terminal: &Entity<Terminal>, shell: PendingShell, cx: &mut Context<Self>) {
        terminal.update(cx, |terminal, cx| {
            // With a shell running, losing the connection starts the automatic reconnect.
            terminal.set_auto_reconnect(true);
            terminal.attach(move |sink, dims| Ok(Box::new(shell.start(sink, dims))), cx);
        });
    }

    /// Gives every tab of the group that still waits for a shell a channel on its live connection.
    fn open_channels_for_waiting(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let waiting: Vec<usize> = self.participants(group_id).into_iter().map(|(id, _)| id).collect();
        for session_id in waiting {
            let Some(group) = self.group_mut(group_id) else { return };
            let Some(connection) = group.live_connection() else { return };
            let number = group.next_channel();
            self.open_channel(session_id, connection, number, window, cx);
        }
    }

    // ---- extra channels on a live connection -----------------------------------------------

    /// Opens a shell channel for the tab on `connection`, which is already up: no new
    /// connection, no authentication.
    fn open_channel(
        &mut self,
        session_id: usize,
        connection: Connection,
        number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sessions.iter().find(|s|s.id==session_id).and_then(Session::ssh).is_some_and(|s|s.mosh.is_some()) {
            self.open_mosh(session_id,connection,window,cx);return;
        }
        let Some(terminal) = self.sessions.iter().find(|s| s.id == session_id).map(|s| s.terminal.clone()) else {
            return;
        };
        let Some(runtime) = runtime::get(cx) else {
            return self.connect_separately(session_id, "SSH 运行时未能启动", window, cx);
        };
        let dims = terminal.read(cx).size().dims();
        let join = runtime.spawn(async move { connection.open_shell(dims).await });
        let abort = join.abort_handle();
        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = join.await;
            this.update_in(cx, |this, window, cx| this.on_channel_opened(session_id, number, outcome, window, cx)).ok();
        });
        if let Some(ssh) = self.ssh_mut(session_id) {
            ssh.status = SshStatus::Connecting;
            ssh.opening = Some(Opening { _abort: AbortOnDrop::new(abort), _task: task });
        }
        self.sync_sidebar(cx);
        cx.notify();
    }

    fn on_channel_opened(
        &mut self,
        session_id: usize,
        number: u32,
        outcome: Result<Result<PendingShell, SshError>, JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { return };
        let terminal = session.terminal.clone();
        let SessionKind::Ssh(ssh) = &mut session.kind else { return };
        ssh.opening = None;
        match outcome {
            Ok(Ok(shell)) => {
                ssh.status = SshStatus::Connected;
                ssh.channel = number;
                let line = reuse_line(&ssh.host.name, number);
                terminal.update(cx, |terminal, cx| terminal.notice(&line, cx));
                Self::attach_shell(&terminal, shell, cx);
            }
            Ok(Err(err)) => self.connect_separately(session_id, &err.to_string(), window, cx),
            Err(err) if err.is_cancelled() => {}
            Err(err) => self.connect_separately(session_id, &format!("连接任务异常终止：{err}"), window, cx),
        }
        self.type_pending(session_id, window, cx);
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// Types what the tab was opened for into its shell once the shell has settled: it has written something
    /// (the login banner, the prompt) and then been quiet for [`TYPE_QUIET`], so that a slow login or a long
    /// message of the day cannot swallow the line or print it twice. Gives up waiting after [`TYPE_CAP`] (a
    /// shell that says nothing at all).
    pub(super) fn type_pending(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) else { return };
        let Some(typed) = session.typed.take() else { return };
        let terminal = session.terminal.clone();
        // When the shell last wrote something (anything that happens after the shell was attached).
        let last_output: Rc<std::cell::Cell<Option<std::time::Instant>>> = Rc::new(std::cell::Cell::new(None));
        let watcher = {
            let last_output = last_output.clone();
            cx.subscribe(&terminal, move |_, _, event: &shellmate_term::TerminalEvent, _| {
                if matches!(event, shellmate_term::TerminalEvent::Output) {
                    last_output.set(Some(std::time::Instant::now()));
                }
            })
        };
        cx.spawn_in(window, async move |_, cx| {
            let started = std::time::Instant::now();
            loop {
                cx.background_executor().timer(Duration::from_millis(40)).await;
                if typing_time(last_output.get(), started.elapsed(), std::time::Instant::now()) {
                    break;
                }
            }
            drop(watcher);
            terminal.update(cx, |terminal, _| {
                terminal.paste(&typed.text);
                if typed.enter {
                    terminal.input(b"\r");
                }
            });
        })
        .detach();
    }

    /// Opens a tab on the connection of tab `source` (no new login) and types `text` into its shell: what a
    /// monitor action does that is for a terminal (strace, the analysis of a disk, a `sudo` line for the user to
    /// finish). The new tab is called `<host> · <label>`.
    pub(super) fn open_command_tab(
        &mut self,
        source: usize,
        label: &str,
        text: String,
        enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.sessions.iter().position(|s| s.id == source && matches!(s.kind, super::SessionKind::Local)) {
            if self.sessions[ix].exited { return; }
            if enter {
                self.open_local(window, cx);
                if let Some(tab) = self.sessions.last_mut() {
                    tab.title = format!("本机 · {label}").into();
                    tab.typed = Some(TypedLine { text, enter });
                    let id = tab.id;
                    self.type_pending(id, window, cx);
                }
                return;
            }
            self.activate(ix, window, cx);
            self.show_panel(Panel::Terminal, window, cx);
            self.sessions[ix].terminal.update(cx, |terminal, _| terminal.paste(&text));
            return;
        }
        let Some((source_group, host_id)) =
            self.sessions.iter().find(|s| s.id == source).and_then(|s| s.ssh()).map(|ssh| (ssh.group, ssh.host.id.clone()))
        else {
            return;
        };
        let Some((host, chain)) = self.resolve_for_connect(&host_id, window, cx) else { return };
        let key = PoolKey::of(&chain, &connect::default_user());
        let live = self
            .group_mut(source_group)
            .filter(|group| group.key == key)
            .and_then(|group| group.live_connection().map(|connection| (connection, group.next_channel())));
        let Some((connection, number)) = live else {
            window.push_notification(
                Notification::warning("连接已断开，重连后再试").title(format!("{} · {label}", host.name)),
                cx,
            );
            return;
        };
        let title = format!("{} · {label}", host.name);
        let session_id = self.push_ssh_tab(title, host, chain, source_group, window, cx);
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.typed = Some(TypedLine { text, enter });
        }
        self.open_channel(session_id, connection, number, window, cx);
    }

    /// The connection could not take another channel (it died, or the server refused): the tab
    /// connects on its own, with what authenticated so far.
    fn connect_separately(&mut self, session_id: usize, why: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((old_group, chain, terminal)) = self.sessions.iter().find(|s| s.id == session_id).and_then(|s| {
            s.ssh().map(|ssh| (ssh.group, ssh.chain.clone(), s.terminal.clone()))
        }) else {
            return;
        };
        let memory = self.group(old_group).map_or_else(AuthMemory::new, |group| group.memory.duplicate());
        let group = self.new_group(&chain, Reconnect::initial(), memory, window, cx);
        if let Some(ssh) = self.ssh_mut(session_id) {
            ssh.group = group;
            ssh.status = SshStatus::Connecting;
            ssh.channel = 1;
            ssh.opening = None;
        }
        terminal.update(cx, |terminal, cx| {
            terminal.notice(&format!("\x1b[33m! 无法在已有连接上新开通道（{why}），改为重新连接"), cx)
        });
        self.release_group(old_group, window, cx);
        self.sync_files(group, cx);
        self.launch_attempt(group, window, cx);
    }

    // ---- tab events ------------------------------------------------------------------------

    /// The tab's terminal reported the connection lost (its shell channel ended without the
    /// remote program exiting). The first tab of a connection to say so starts the loop; the
    /// others on it are already covered.
    pub(super) fn on_tab_disconnected(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ssh) = self.ssh_mut(session_id) else { return };
        if ssh.mosh.is_some() {ssh.status=SshStatus::Failed;self.sync_sidebar(cx);return;}
        // Disconnects the app itself caused (a failed attempt) find the tab already waiting.
        if ssh.status != SshStatus::Connected {
            return;
        }
        let group_id = ssh.group;
        if let Some(group) = self.group_mut(group_id) {
            group.last_failure = None;
            group.declined = false;
            group.manual = false;
        }
        // The SFTP session went with the connection; its transfers wait for the next one. The
        // forwards keep their local ports and wait too.
        self.link_lost(group_id, cx);
        self.forward_lost(group_id, cx);
        self.monitor_lost(group_id, cx);
        self.step_group(group_id, Input::Lost, window, cx);
        if let Some(ssh) = self.ssh_mut(session_id)
            && ssh.status == SshStatus::Connected
        {
            ssh.status = SshStatus::Failed;
        }
        self.sync_sidebar(cx);
    }

    /// The remote program ended: the tab is over, and not reconnected by itself.
    pub(super) fn on_tab_exited(&mut self, session_id: usize, cx: &mut Context<Self>) {
        if let Some(ssh) = self.ssh_mut(session_id) {
            ssh.status = SshStatus::Ended;
        }
        self.sync_sidebar(cx);
    }

    /// Enter in a tab whose session is over, or a click on its badge: try again now.
    pub(super) fn retry_tab(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ssh) = self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()) else { return };
        let (group_id, status) = (ssh.group, ssh.status);
        let mosh=ssh.mosh.is_some();
        if mosh && status!=SshStatus::Connected && let Some(ssh)=self.ssh_mut(session_id){ssh.mosh=Some(super::mosh::MoshTab::default());}
        let Some(phase) = self.group(group_id).map(|group| group.machine.phase()) else { return };
        match phase {
            // Cut the wait short, or start over after giving up. A tab that had ended joins in.
            Phase::Waiting { .. } | Phase::Failed => {
                if phase == Phase::Failed
                    && let Some(group) = self.group_mut(group_id)
                {
                    group.manual = true;
                }
                if status == SshStatus::Ended
                    && let Some(ssh) = self.ssh_mut(session_id)
                {
                    ssh.status = SshStatus::Failed;
                }
                self.step_group(group_id, Input::RetryNow, window, cx);
            }
            // An attempt is already running.
            Phase::Connecting { .. } => {}
            // The remote shell exited: a new channel on the connection if it is still up (other
            // tabs keep it open), else a connection of its own again.
            Phase::Idle => {
                if let Some(ssh) = self.ssh_mut(session_id) {
                    ssh.status = SshStatus::Connecting;
                }
                let live = self.group_mut(group_id).and_then(|group| group.live_connection().map(|c| (c, group.next_channel())));
                match live {
                    Some((connection, number)) => self.open_channel(session_id, connection, number, window, cx),
                    None => {
                        if let Some(group) = self.group_mut(group_id) {
                            group.manual = true;
                        }
                        self.step_group(group_id, Input::Start, window, cx)
                    }
                }
            }
        }
    }

    // ---- ⌘⇧T ---------------------------------------------------------------------------------

    /// Copies the current session: an SSH tab opens a new channel on the same connection (no new
    /// login), a local one a new shell in the directory the current one is working in.
    pub(crate) fn duplicate_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.active.filter(|ix| *ix < self.sessions.len()) else {
            return self.open_local(window, cx);
        };
        if !Session::duplicable(self.sessions[ix].host().map(|host| host.protocol)) {window.push_notification(Notification::info("串口设备不能同时打开两次，请先关闭端口"),cx);return;}
        if let SessionKind::Stream(s)=&self.sessions[ix].kind {
            let host=s.host.clone();self.open_stream(host,window,cx);return;
        }
        let Some(ssh) = self.sessions[ix].ssh() else {
            let terminal = self.sessions[ix].terminal.clone();
            let cwd = terminal
                .update(cx, |terminal, _| terminal.foreground_process())
                .and_then(|process| process.cwd)
                .filter(|cwd| cwd.is_dir());
            return self.open_local_in(cwd, window, cx);
        };
        let (source_group, host_id) = (ssh.group, ssh.host.id.clone());
        let Some((host, chain)) = self.resolve_for_connect(&host_id, window, cx) else { return };

        let copies = self.sessions.iter().filter(|s| s.ssh().is_some_and(|ssh| ssh.host.id == host_id)).count();
        let title = format!("{} · {}", host.name, copies + 1);
        let key = PoolKey::of(&chain, &connect::default_user());
        // Same host, same route as the connection the source tab is on, and that connection is up.
        let reusable = self
            .group_mut(source_group)
            .filter(|group| group.key == key)
            .and_then(|group| group.live_connection().map(|connection| (connection, group.next_channel())));
        match reusable {
            Some((connection, number)) => {
                let session_id = self.push_ssh_tab(title, host, chain, source_group, window, cx);
                self.open_channel(session_id, connection, number, window, cx);
            }
            None => {
                let memory = self.group(source_group).map_or_else(AuthMemory::new, |group| group.memory.duplicate());
                let group = self.new_group(&chain, Reconnect::initial(), memory, window, cx);
                self.push_ssh_tab(title, host, chain, group, window, cx);
                self.launch_attempt(group, window, cx);
            }
        }
    }

    /// Tells the user what went to the keychain (or why not) after an attempt.
    fn announce_saved(&mut self, saves: &SaveReport, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(error) = saves.errors.first() {
            window.push_notification(Notification::warning(format!("无法保存凭据：{error}")).title("凭据"), cx);
        } else if !saves.saved.is_empty() {
            let what = if saves.saved.iter().any(|k| matches!(k, SecretKey::KeyPassphrase(_))) {
                "私钥口令"
            } else {
                "密码"
            };
            window.push_notification(
                Notification::success(format!("已保存{what}")).title("凭据"),
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use shellmate_core::AuthMethod;

    use super::*;

    #[test]
    fn only_cancelled_authentication_ends_a_first_connect_quietly() {
        assert!(cancelled_auth_prompt(&SshError::AuthCancelled));
        for error in [SshError::HostKeyChanged, SshError::HostKeyRejected, SshError::AuthRejected(AuthMethod::Password)] {
            assert!(!cancelled_auth_prompt(&error));
            assert!(!cancelled_auth_prompt(&SshError::Jump { host: "jump".into(), port: 22, source: Box::new(error) }));
        }
    }

    #[test]
    fn local_terminal_summary_counts_only_local_sessions() {
        use super::super::{local_terminal_summary, streams::StreamSession};
        use shellmate_core::Protocol;

        let mut kinds = vec![
            SessionKind::Local,
            SessionKind::Ssh(Box::new(SshSession::new(host("remote", None), vec![], 1))),
        ];
        assert_eq!(local_terminal_summary("zsh", &kinds), "zsh · 1 个会话");
        for protocol in [Protocol::Telnet, Protocol::Serial, Protocol::Mosh] {
            let mut remote = host("remote", None);
            remote.protocol = protocol;
            kinds.push(if protocol == Protocol::Mosh {
                SessionKind::Ssh(Box::new(SshSession::new(remote, vec![], 2)))
            } else {
                SessionKind::Stream(Box::new(StreamSession::new(remote)))
            });
        }
        assert_eq!(local_terminal_summary("zsh", &kinds), "zsh · 1 个会话");
        kinds.push(SessionKind::Local);
        assert_eq!(local_terminal_summary("bash", &kinds), "bash · 2 个会话");
        kinds.retain(|kind| !matches!(kind, SessionKind::Local));
        assert_eq!(local_terminal_summary("zsh", &kinds), "zsh · ~");
        assert_eq!(local_terminal_summary("zsh", &[]), "zsh · ~");
    }

    #[test]
    fn mosh_terminal_survives_panel_transport_loss_and_reconnect(){
        let mut host=Host::new("dev","example.test");host.protocol=shellmate_core::Protocol::Mosh;
        let mut s=SshSession::new(host,vec![],1);s.status=SshStatus::Connected;s.mosh.as_mut().unwrap().independent=true;
        for view in [View::Reconnecting(1),View::Connecting,View::Failed,View::Connected]{s.on_connection_view(view);assert_eq!(s.status,SshStatus::Connected);}
        s.status=SshStatus::Ended;s.on_connection_view(View::Reconnecting(1));assert_eq!(s.status,SshStatus::Ended);
    }

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

    fn tab(host: &Host, status: SshStatus) -> SshSession {
        let mut session = SshSession::new(host.clone(), vec![host.clone()], 1);
        session.status = status;
        session
    }

    #[test]
    fn a_host_with_a_live_tab_switches_to_it() {
        let (web, db) = (host("web", None), host("db", None));
        let tabs = [tab(&db, SshStatus::Connected), tab(&web, SshStatus::Connected), tab(&web, SshStatus::Connecting)];
        let existing = |id: &HostId| existing_session(tabs.iter().enumerate(), id);
        assert_eq!(existing(&web.id), Existing::Switch(1), "the first live tab of the host");
        assert_eq!(existing(&db.id), Existing::Switch(0));
        assert_eq!(existing(&HostId::generate()), Existing::None);
    }

    #[test]
    fn connecting_and_reconnecting_count_as_live() {
        let web = host("web", None);
        for status in [SshStatus::Connecting, SshStatus::Reconnecting(3)] {
            let tabs = [tab(&web, status)];
            assert_eq!(existing_session(tabs.iter().enumerate(), &web.id), Existing::Switch(0), "{status:?}");
        }
    }

    #[test]
    fn an_ended_or_failed_tab_is_reconnected_in_place() {
        let web = host("web", None);
        for status in [SshStatus::Failed, SshStatus::Ended] {
            let tabs = [tab(&web, status)];
            assert_eq!(existing_session(tabs.iter().enumerate(), &web.id), Existing::Reconnect(0), "{status:?}");
        }
    }

    #[test]
    fn a_live_tab_wins_over_an_ended_one() {
        let web = host("web", None);
        let tabs = [tab(&web, SshStatus::Ended), tab(&web, SshStatus::Failed), tab(&web, SshStatus::Connected)];
        assert_eq!(existing_session(tabs.iter().enumerate(), &web.id), Existing::Switch(2));
        let only_ended = [tab(&web, SshStatus::Failed), tab(&web, SshStatus::Ended)];
        assert_eq!(
            existing_session(only_ended.iter().enumerate(), &web.id),
            Existing::Reconnect(0),
            "the first ended tab"
        );
    }

    #[test]
    fn status_line_wording() {
        assert_eq!(status_line(142, 35), "PTY 142×35    xterm-256color    UTF-8    保活 30s");
    }

    #[test]
    fn reuse_line_wording() {
        assert_eq!(reuse_line("prod-web-01", 2), "\x1b[2m→ 复用到 prod-web-01 的已有连接 · 新开 channel #2");
    }

    #[test]
    fn panels_name_the_phase_that_brings_them() {
        assert_eq!(panel_hint(Panel::Files), "", "the files panel is there for SSH sessions");
        assert_eq!(panel_hint(Panel::Forward), "", "the port forwarding panel is there for SSH sessions");
        assert_eq!(panel_hint(Panel::Monitor), "", "the monitor is there for SSH sessions");
    }

    #[test]
    fn status_maps_to_dots_badges_and_notes() {
        let mut session = SshSession::new(host("db", None), vec![host("bastion", None), host("db", None)], 1);
        assert_eq!(session.via(), ["bastion"]);
        let cases = [
            (SshStatus::Connecting, Some(LiveStatus::Connecting), None, None),
            (SshStatus::Connected, Some(LiveStatus::Ok), None, None),
            (SshStatus::Reconnecting(2), Some(LiveStatus::Connecting), Some("重连 2/5"), Some("重连中 2/5")),
            (SshStatus::Failed, Some(LiveStatus::Failed), Some("连接失败"), Some("连接失败")),
            (SshStatus::Ended, None, None, Some("已退出")),
        ];
        for (status, live, badge, note) in cases {
            session.status = status;
            assert_eq!(session.live(), live, "{status:?}");
            assert_eq!(session.badge().as_deref(), badge, "{status:?}");
            assert_eq!(session.status_note().as_deref(), note, "{status:?}");
        }
    }

    #[test]
    fn only_waiting_tabs_take_part_in_an_attempt() {
        assert!(SshStatus::Connecting.wants_channel());
        assert!(SshStatus::Reconnecting(1).wants_channel());
        assert!(SshStatus::Failed.wants_channel());
        assert!(!SshStatus::Connected.wants_channel());
        assert!(!SshStatus::Ended.wants_channel(), "a tab whose remote shell exited is not reconnected by itself");
    }

    #[test]
    fn the_loop_moves_the_tabs_and_leaves_ended_ones_alone() {
        use SshStatus::*;
        // A loss: tabs that were up now wait, with the attempt number.
        assert_eq!(Connected.on_view(View::Reconnecting(1)), Reconnecting(1));
        assert_eq!(Reconnecting(1).on_view(View::Reconnecting(2)), Reconnecting(2));
        assert_eq!(Failed.on_view(View::Reconnecting(1)), Reconnecting(1), "retrying after giving up");
        assert_eq!(Ended.on_view(View::Reconnecting(1)), Ended);
        // Giving up.
        assert_eq!(Reconnecting(5).on_view(View::Failed), Failed);
        assert_eq!(Connecting.on_view(View::Failed), Failed);
        assert_eq!(Ended.on_view(View::Failed), Ended);
        assert_eq!(Connected.on_view(View::Failed), Connected, "a tab that is up is not failed by someone else's attempt");
        // First connect.
        assert_eq!(Failed.on_view(View::Connecting), Connecting);
        assert_eq!(Connected.on_view(View::Connecting), Connected);
        // Success: the shells are attached one by one, not by the loop.
        for status in [Connecting, Connected, Reconnecting(2), Failed, Ended] {
            assert_eq!(status.on_view(View::Connected), status);
        }
    }

    #[test]
    fn a_line_is_typed_when_the_shell_has_said_something_and_gone_quiet() {
        use std::time::Instant;
        let start = Instant::now();
        let after = |ms: u64| start + Duration::from_millis(ms);
        // Nothing yet: wait (the shell has not said a word, it may still be logging in).
        assert!(!typing_time(None, Duration::from_millis(500), after(500)));
        // Output just now, or 150 ms ago: still talking (a motd comes in pieces).
        assert!(!typing_time(Some(after(1000)), Duration::from_millis(1000), after(1000)));
        assert!(!typing_time(Some(after(1000)), Duration::from_millis(1150), after(1150)));
        // 200 ms of quiet after the last output: the prompt is there.
        assert!(typing_time(Some(after(1000)), Duration::from_millis(1200), after(1200)));
        // A shell that never says anything is not waited for forever.
        assert!(!typing_time(None, Duration::from_millis(2999), after(2999)));
        assert!(typing_time(None, TYPE_CAP, after(3000)));
        // A shell that keeps on writing is cut off by the same cap.
        assert!(typing_time(Some(after(2990)), TYPE_CAP, after(3000)));
        // A clock that went back is not "quiet".
        assert!(!typing_time(Some(after(2000)), Duration::from_millis(100), after(1000)));
    }

    #[test]
    fn a_status_follows_the_machine_through_a_loss_and_a_recovery() {
        let mut machine = Reconnect::connected();
        let mut tabs = [SshStatus::Connected, SshStatus::Connected, SshStatus::Ended];
        fn sync(tabs: &mut [SshStatus], machine: &Reconnect) {
            tabs.iter_mut().for_each(|status| *status = status.on_view(machine.view()));
        }

        machine.step(Input::Lost);
        sync(&mut tabs, &machine);
        assert_eq!(tabs, [SshStatus::Reconnecting(1), SshStatus::Reconnecting(1), SshStatus::Ended]);
        machine.step(Input::TimerElapsed);
        machine.step(Input::Failed(Failure::Retry("x".into())));
        sync(&mut tabs, &machine);
        assert_eq!(tabs, [SshStatus::Reconnecting(2), SshStatus::Reconnecting(2), SshStatus::Ended]);
        for _ in 0..4 {
            machine.step(Input::TimerElapsed);
            machine.step(Input::Failed(Failure::Retry("x".into())));
        }
        sync(&mut tabs, &machine);
        assert_eq!(tabs, [SshStatus::Failed, SshStatus::Failed, SshStatus::Ended]);
        machine.step(Input::RetryNow);
        sync(&mut tabs, &machine);
        assert_eq!(tabs, [SshStatus::Reconnecting(1), SshStatus::Reconnecting(1), SshStatus::Ended]);
    }
}
