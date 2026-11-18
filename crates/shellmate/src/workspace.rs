//! Root view: sidebar + session tabs + active session panel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::assets::IconName as Lucide;
use crate::icons::AppIcon;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::notification::Notification;
use gpui::component::tooltip::Tooltip;
use gpui::component::{Icon, WindowExt as _};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, KeyBinding, KeyDownEvent,
    MouseButton, MouseUpEvent, SharedString, Subscription, Window, actions, div, px,
};
use shellmate_core::{HostBook, HostId};
use shellmate_store::{HostStore, SecretStore};
use shellmate_term::colors::to_hsla;
use shellmate_term::view::ToggleSearch;
use shellmate_term::{PtyOptions, Terminal, TerminalEvent, TerminalView};

use crate::connect;
use crate::editor::view::FileEditor;
use crate::sidebar::{Active, LiveStatus, Sidebar, SidebarEvent, SIDEBAR_WIDTH, RAIL_WIDTH};
use crate::theme::{self, ActivePalette, CARD_RADIUS};
use crate::ui::{SegmentItem, card_shadow, dot, item_shadow, segmented, toolbar_button, window_drag_area};

mod files;
mod forward;
mod group;
mod hosts;
mod monitor;
mod productivity;
mod multi;
mod archive;
#[cfg(debug_assertions)]
mod proto;
#[cfg(debug_assertions)]
mod archive_demo;
mod ssh;
mod streams;
mod mosh;
mod ai;
mod drawer_resize;
mod menu_actions;

actions!(
    shellmate,
    [
        NewLocalTerminal,
        DuplicateSession,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleSidebar,
        ToggleTheme,
        FocusHostSearch,
        OpenPalette,
        Quit
    ]
);

const CONTEXT: &str = "Workspace";

/// The setting that remembers the sidebar collapsed to its rail (`1`) across restarts.
const SIDEBAR_COLLAPSED: &str = "sidebar_collapsed";

pub fn init(cx: &mut App) {
    crate::productivity::panel::init(cx);
    crate::productivity::broadcast::init(cx);
    crate::productivity::session_menu::init(cx);
    cx.bind_keys(bindings());
}

pub(crate) fn bindings() -> Vec<KeyBinding> {
    use crate::menus::*;
    vec![
        KeyBinding::new("cmd-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-t", DuplicateSession, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("cmd-ctrl-s", ToggleSidebar, Some(CONTEXT)),
        KeyBinding::new("cmd-k", OpenPalette, Some(CONTEXT)),
        KeyBinding::new("cmd-,", Settings, Some(CONTEXT)),
        KeyBinding::new("cmd-n", NewConnection, Some(CONTEXT)),
        KeyBinding::new("cmd-d", ToggleSplit, Some(CONTEXT)),
        KeyBinding::new("alt-cmd-1", ShowTerminal, Some(CONTEXT)),
        KeyBinding::new("alt-cmd-2", ShowFiles, Some(CONTEXT)),
        KeyBinding::new("alt-cmd-3", ShowForward, Some(CONTEXT)),
        KeyBinding::new("alt-cmd-4", ShowMonitor, Some(CONTEXT)),
    ]
}

/// The panels a session can show. Only SSH sessions get more than the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Terminal,
    Files,
    Forward,
    Monitor,
}

impl Panel {
    fn supported(self, protocol: Option<shellmate_core::Protocol>) -> bool {
        self == Panel::Terminal || matches!(protocol, Some(shellmate_core::Protocol::Ssh | shellmate_core::Protocol::Mosh))
    }
    const ALL: [Panel; 4] = [Panel::Terminal, Panel::Files, Panel::Forward, Panel::Monitor];

    fn label(self) -> &'static str {
        match self {
            Panel::Terminal => "终端",
            Panel::Files => "文件",
            Panel::Forward => "端口转发",
            Panel::Monitor => "监控",
        }
    }

    /// Why the panel is unavailable for a local session (prototype wording).
    fn local_hint(self) -> &'static str {
        match self {
            Panel::Terminal => "",
            Panel::Files => "本机文件请使用 Finder",
            Panel::Forward => "本机会话不需要端口转发",
            Panel::Monitor => "本机状态请使用「活动监视器」",
        }
    }
}

/// What a session is connected to.
enum SessionKind {
    Local,
    Stream(Box<streams::StreamSession>),
    Ssh(Box<ssh::SshSession>),
}

struct Session {
    ai_alive: Arc<std::sync::atomic::AtomicBool>,
    id: usize,
    title: SharedString,
    view: Entity<TerminalView>,
    terminal: Entity<Terminal>,
    /// The local shell exited (SSH sessions track their state in [`ssh::SshSession`]).
    exited: bool,
    panel: Panel,
    /// The file panel, made the first time the tab shows it (SSH tabs only).
    files: Option<files::FilesTab>,
    /// The port forwarding panel, made the first time the tab shows it (SSH tabs only).
    forward: Option<forward::ForwardTab>,
    /// The monitor panel, made the first time the tab shows it (SSH tabs only).
    monitor: Option<monitor::MonitorTab>,
    /// A command line to type into the shell as soon as it is there (a tab opened for a command: strace, du,
    /// or a `sudo` command the user has to type the password for).
    typed: Option<ssh::TypedLine>,
    kind: SessionKind,
    _subscription: Subscription,
}

impl Drop for Session {
    fn drop(&mut self) { self.ai_alive.store(false, std::sync::atomic::Ordering::Release); }
}

impl Session {
    fn supports_panel(&self, panel: Panel) -> bool {
        panel.supported(self.host().map(|host| host.protocol))
    }

    fn duplicable(protocol: Option<shellmate_core::Protocol>) -> bool {
        protocol != Some(shellmate_core::Protocol::Serial)
    }

    fn host(&self) -> Option<&shellmate_core::Host> { match &self.kind {SessionKind::Ssh(s)=>Some(&s.host),SessionKind::Stream(s)=>Some(&s.host),SessionKind::Local=>None} }
    fn stream_connected(&self)->bool { match &self.kind {SessionKind::Stream(s)=>s.connected(),_=>true} }
    fn ssh(&self) -> Option<&ssh::SshSession> {
        match &self.kind {
            SessionKind::Ssh(ssh) => Some(ssh),
            SessionKind::Local | SessionKind::Stream(_) => None,
        }
    }
}

pub struct Workspace {
    last_menu_state: Option<crate::menus::MenuState>,
    store: shellmate_store::SharedStore,
    /// Saved passwords and key passphrases (the macOS keychain).
    secrets: Arc<shellmate_store::CredentialStore>,
    security_task: Option<gpui::Task<()>>,
    pending_forms: HashMap<HostId, (connect::Endpoint, connect::FormCredential)>,
    book: Rc<HostBook>,
    sidebar: Entity<Sidebar>,
    sessions: Vec<Session>,
    preferences: shellmate_store::Preferences,
    data_root: PathBuf,
    log_root: PathBuf,
    running_recordings: HashMap<usize,archive::RunningRecording>,
    archive_task: Option<gpui::Task<()>>,
    records_window: Option<gpui::WeakEntity<crate::records::panel::RecordsWindow>>,
    temporary_hosts: HashMap<HostId, shellmate_core::Host>,
    commands: Entity<crate::productivity::panel::Drawer>,
    commands_open: bool,
    ai_drawer: Option<Entity<crate::ai::panel::Drawer>>,
    ai_open: bool,
    ai_drawer_width: f32,
    ai_drawer_drag: drawer_resize::Drag,
    broadcast: crate::productivity::multi::Broadcast,
    broadcast_bar: Entity<crate::productivity::broadcast::BroadcastBar>,
    split: crate::productivity::multi::Split,
    recent_sessions: Vec<usize>,
    session_menu: Option<multi::OpenSessionMenu>,
    #[cfg(debug_assertions)]
    review_confirmation: Option<gpui::WeakEntity<crate::productivity::panel::Confirmation>>,
    #[cfg(debug_assertions)]
    live_review: bool,
    picker: Option<Entity<crate::productivity::panel::Picker>>,
    picker_subscription: Option<Subscription>,
    /// The host library is a temporary in-memory one (the saved file could not be opened): nothing
    /// saved in it, rules included, survives the run.
    library_is_temporary: bool,
    /// What the servers granted for remote forwards in this run, shared by every connection (see `GrantMemory`).
    forward_grants: shellmate_forward::GrantMemory,
    /// Debug review hook: the monitor panels show made-up data and ignore what the servers say.
    monitor_frozen: bool,
    /// The SSH connections behind the SSH tabs; a connection is shared by the tabs copied from each other.
    groups: Vec<group::ConnGroup>,
    active: Option<usize>,
    sidebar_open: bool,
    next_id: usize,
    next_group: usize,
    user_host: SharedString,
    focus_handle: FocusHandle,
    /// The editor modal, if one is open (one at a time per window).
    editor: Option<Entity<FileEditor>>,
    _editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// Shows a notification once the window's root view exists: notifications cannot be pushed
/// while the root view is still being built (`Workspace::new`).
fn push_notification_later(window: &mut Window, cx: &mut App, notification: Notification) {
    window.defer(cx, move |window, cx| window.push_notification(notification, cx));
}

impl Workspace {
    /// `store_notice` is shown once when the host library could not be opened and
    /// `store` is a temporary in-memory fallback.
    pub fn new(store: HostStore, store_notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let book = Rc::new(HostBook::default());
        // The sidebar comes back the way it was left.
        let collapsed = match store.setting(SIDEBAR_COLLAPSED) {
            Ok(value) => value.as_deref() == Some("1"),
            Err(err) => {
                log::warn!("could not read the sidebar setting: {err}");
                false
            }
        };
        let ai_drawer_width = drawer_resize::stored_width(store.setting(drawer_resize::SETTING).ok().flatten().as_deref());
        let sidebar = cx.new(|cx| Sidebar::new(book.clone(), window, cx));
        sidebar.update(cx, |sidebar, cx| sidebar.set_collapsed(collapsed, cx));
        let commands = cx.new(|_| crate::productivity::panel::Drawer {
            groups: store.snippets().unwrap_or_default(),
            scroll: gpui::ScrollHandle::new(),
            broadcast: Default::default(),
        });
        let broadcast_bar=cx.new(|cx|crate::productivity::broadcast::BroadcastBar::new(window,cx));
        let subscriptions = vec![
            cx.subscribe_in(&sidebar, window, Self::on_sidebar_event),
            cx.observe_window_activation(window, |s, w, cx| {
                if !w.is_window_active() { s.end_drawer_drag(cx); }
            }),
            cx.subscribe_in(&commands, window, Self::on_command_event),
            cx.subscribe_in(&broadcast_bar,window,Self::on_broadcast_submit),
        ];
        let preferences=store.preferences().unwrap_or_default();
        let store=shellmate_store::SharedStore::new(store);
        let secrets=crate::security::coordinator(store.clone());
        let workspace=cx.entity().downgrade();
        let mut subscriptions=subscriptions;
        subscriptions.push(cx.intercept_keystrokes(move|_,_,cx|{workspace.update(cx,|s,_|s.secrets.activity()).ok();}));
        let mut this = Self {
            last_menu_state: None,
            preferences,
            data_root:shellmate_term::archive::data_root(),log_root:shellmate_term::archive::log_root(),
            running_recordings:HashMap::new(),archive_task:None,records_window:None,
            store,
            secrets, security_task: None,
            pending_forms: HashMap::new(),
            book,
            sidebar,
            sessions: Vec::new(),
            temporary_hosts: HashMap::new(),
            commands,
            commands_open: false, ai_drawer:None,ai_open:false,
            ai_drawer_width, ai_drawer_drag: Default::default(),
            broadcast: Default::default(), broadcast_bar, split: Default::default(),recent_sessions:Vec::new(),session_menu:None,
            #[cfg(debug_assertions)] review_confirmation:None,
            #[cfg(debug_assertions)] live_review:false,
            picker: None,
            picker_subscription: None,
            library_is_temporary: store_notice.is_some(),
            forward_grants: shellmate_forward::GrantMemory::new(),
            monitor_frozen: false,
            groups: Vec::new(),
            active: None,
            sidebar_open: !collapsed,
            next_id: 1,
            next_group: 1,
            user_host: user_at_host().into(),
            focus_handle: cx.focus_handle(),
            editor: None,
            _editor_subscription: None,
            _subscriptions: subscriptions,
        };
        this.security_task=Some(cx.spawn_in(window,async move|this,cx|{loop{cx.background_executor().timer(std::time::Duration::from_secs(1)).await; if this.update_in(cx,|s,w,cx|{if s.secrets.tick(){w.push_notification(Notification::info("凭据库已锁定"),cx);}}).is_err(){break}}}));
        if let Some(message) = store_notice {
            push_notification_later(window, cx, Notification::warning(message).title("主机库无法打开"));
        }
        this.reload_book(window, cx);
        let open_local=!crate::prototype_fixture();
        if open_local {this.open_local(window, cx);}
        this.archive_timer(window,cx);
        this
    }

    /// Debug review hook: one step of a rail scenario (see `demo::run`, `rail:`).
    #[cfg(debug_assertions)]
    pub(crate) fn demo_rail_step(&mut self, step: &str, window: &mut Window, cx: &mut Context<Self>) {
        match step {
            "collapse" => self.set_sidebar_open(false, cx),
            "expand" => self.set_sidebar_open(true, cx),
            "search" => self.open_palette(&OpenPalette, window, cx),
            "local" => self.focus_or_open_local(window, cx),
            other if other.starts_with("log:") => {
                // The size every terminal has just now (the status bar of a tab shows the same).
                let sizes: Vec<String> = self
                    .sessions
                    .iter()
                    .map(|s| {
                        let size = s.terminal.read(cx).size();
                        format!("{}={}×{}", s.title, size.cols, size.rows)
                    })
                    .collect();
                log::info!("终端尺寸 [{}] {}", &other[4..], sizes.join(" "));
            }
            other => log::warn!("unknown rail step {other:?}"),
        }
    }

    /// Re-reads hosts and groups from the store into the sidebar.
    fn reload_book(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = self.store.borrow().book();
        match loaded {
            Ok(book) => {
                self.book = Rc::new(book);
                self.sidebar.update(cx, |sidebar, cx| sidebar.set_book(self.book.clone(), cx));
            }
            Err(err) => {
                log::error!("failed to load hosts: {err}");
                push_notification_later(window, cx, Notification::error(err.to_string()).title("无法读取主机列表"));
            }
        }
    }

    fn on_sidebar_event(
        &mut self,
        _: &Entity<Sidebar>,
        event: &SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::OpenLocal => self.focus_or_open_local(window, cx),
            SidebarEvent::ToggleSidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            SidebarEvent::Connect(id) => self.connect_host(id, window, cx),
            SidebarEvent::ActivateHost(id) => self.activate_host(id, window, cx),
            SidebarEvent::SearchHosts => self.open_palette(&OpenPalette, window, cx),
            SidebarEvent::Settings => self.open_settings(0,window,cx),
            SidebarEvent::NewHost => self.new_host(window, cx),
            SidebarEvent::EditHost(id) => self.edit_host(id, false, window, cx),
            SidebarEvent::DuplicateHost(id) => self.edit_host(id, true, window, cx),
            SidebarEvent::DeleteHost(id) => self.confirm_delete_host(id, window, cx),
            SidebarEvent::ImportConfig => self.open_import(window, cx),
        }
    }

    /// The loaded host book (debug review hook).
    #[cfg(debug_assertions)]
    pub(crate) fn book(&self) -> Rc<HostBook> {
        self.book.clone()
    }

    // ---- sessions ------------------------------------------------------

    fn open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_local_in(None, window, cx);
    }

    /// Opens a local shell tab, starting in `cwd` (the home directory if `None`).
    fn open_local_in(&mut self, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let style = cx.terminal_style();
        let palette = style.palette.clone();
        let options = PtyOptions { cwd, login:self.preferences.get(shellmate_store::Preference::LoginShell), ..PtyOptions::default() };
        let terminal = cx.new(|cx| Terminal::new(palette, cx));
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal.clone(), style, cx).with_search_icon(crate::icons::AppIcon::Search.at(14.));
            view.top_radius = CARD_RADIUS;
            view.history_shortcut = self.store.borrow().history_shortcut().unwrap_or(true);
            view
        });

        let id = self.next_id;
        self.next_id += 1;
        let subscription = self.watch_terminal(id, &terminal, window, cx);

        let open_locals = self.sessions.len();
        let title = if open_locals == 0 { "本机".to_string() } else { format!("本机 · {}", open_locals + 1) };
        self.sessions.push(Session {
            ai_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            id,
            title: title.into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            files: None,
            forward: None,
            monitor: None,
            typed: None,
            kind: SessionKind::Local,
            _subscription: subscription,
        });
        self.start_session_archives(id,window,cx);
        self.sessions.last().unwrap().terminal.update(cx,|t,cx|t.start_local(options,cx));
        self.activate(self.sessions.len() - 1, window, cx);
    }

    /// Follows a session's terminal: exits and disconnects update the session state. For an SSH
    /// session a disconnect starts the reconnect loop of its connection and Enter on an ended
    /// session retries.
    fn watch_terminal(
        &mut self,
        id: usize,
        terminal: &Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(terminal, window, move |this: &mut Self, _, event: &TerminalEvent, window, cx| {
            if let TerminalEvent::Command { command, cwd, time } = event {
                this.save_command_history(id, command, cwd.clone(), *time, cx);
                return;
            }
            if matches!(event, TerminalEvent::Focused) {this.note_terminal_focus(id,cx);return;}
            if matches!(event, TerminalEvent::HistoryRequested) {
                this.note_terminal_focus(id,cx);
                this.open_history(window, cx);
                return;
            }
            let Some(session) = this.sessions.iter_mut().find(|s| s.id == id) else { return };
            match (&mut session.kind, event) {
                (SessionKind::Local, TerminalEvent::Exited(_) | TerminalEvent::Disconnected(_)) => session.exited = true,
                (SessionKind::Local, TerminalEvent::Restarted) => session.exited = false,
                (SessionKind::Stream(s), TerminalEvent::Disconnected(_) | TerminalEvent::Exited(_)) => {s.connected=false;session.exited=true;},
                (SessionKind::Stream(_), TerminalEvent::RestartRequested) => this.start_stream(id,window,cx),
                (SessionKind::Ssh(_), TerminalEvent::Exited(_)) => this.on_tab_exited(id, cx),
                (SessionKind::Ssh(_), TerminalEvent::Disconnected(_)) => this.on_tab_disconnected(id, window, cx),
                (SessionKind::Ssh(_), TerminalEvent::RestartRequested) => this.retry_tab(id, window, cx),
                _ => {}
            }
            this.sync_sidebar(cx);
            this.sync_ai_context(cx);
            cx.notify();
        })
    }

    /// 本机终端 in the sidebar or on the rail: the local tab comes to the front (the first one, if there
    /// are several), or one is opened when there is none.
    fn focus_or_open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.sessions.iter().position(|s| matches!(s.kind, SessionKind::Local)) {
            Some(ix) => self.activate(ix, window, cx),
            None => self.open_local(window, cx),
        }
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(ix) else { return };
        let new_id=session.id;
        let selected = session.host().map_or_else(|| "local".to_string(), |host| host.id.to_string());
        self.split.select_tab(self.main_id(),new_id);
        self.session_menu=None;
        self.picker = None;
        self.picker_subscription = None;
        self.active = Some(ix);
        self.sync_ai_context(cx);
        self.sidebar.update(cx, |s, cx| s.select(Some(selected), cx));
        self.touch_session(new_id);
        self.update_pane_radii(cx);
        self.focus_active(window, cx);
        self.pump_active_clash(window, cx);
        self.sync_sidebar(cx);
        self.sync_monitor_activity();
        cx.notify();
    }

    /// Closes the tab at `ix` right away (see [`Workspace::close`] for the confirmation).
    fn close_now(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.sessions.len() {
            return;
        }
        // Dropping the session ends its channel; the connection closes with its last tab, and a
        // connection that is still being made is cancelled together with the dialog it asked.
        let group = self.sessions[ix].ssh().map(|ssh| ssh.group);
        let closed=self.sessions[ix].id;
        let was_recording=self.running_recordings.contains_key(&closed);
        if let Some(error)=self.finalize_archives(closed,cx){window.push_notification(Notification::error(error),cx);}
        else if was_recording{window.push_notification(Notification::success("已保存 .cast · 可在「记录」中回放"),cx);}
        self.split.close_session(self.main_id(),closed);
        self.broadcast.checked.remove(&closed);
        self.recent_sessions.retain(|id|*id!=closed);
        self.session_menu=None;
        self.sessions[ix].terminal.read(cx).agent_liveness().store(false, std::sync::atomic::Ordering::Release);
        self.sessions.remove(ix);
        self.update_pane_radii(cx);
        if let Some(group) = group {
            self.release_group(group, window, cx);
        }
        match self.active {
            _ if self.sessions.is_empty() => {
                self.active = None;
                self.sync_ai_context(cx);
                window.focus(&self.focus_handle, cx);
                self.sidebar.update(cx, |s, cx| s.select(None, cx));
            }
            Some(active) if active >= ix => self.activate(active.saturating_sub(1).min(self.sessions.len() - 1), window, cx),
            _ => {}
        }
        self.sync_sidebar(cx);
        self.sync_monitor_activity();
        cx.notify();
    }

    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sync_productivity(cx);
        let count = self.sessions.len();
        let shell = shell_name(&PtyOptions::default().shell_path());
        let summary = if crate::prototype_fixture() {
            "zsh · ~".to_string()
        } else {
            local_terminal_summary(&shell, self.sessions.iter().map(|s| &s.kind))
        };
        let mut live: HashMap<String, LiveStatus> = HashMap::new();
        for ssh in self.sessions.iter().filter_map(Session::ssh) {
            if let Some(status) = ssh.live() {
                let entry = live.entry(ssh.host.id.to_string()).or_insert(status);
                if status.rank() < entry.rank() {
                    *entry = status;
                }
            }
        }
        for session in &self.sessions {if let SessionKind::Stream(stream)=&session.kind {live.insert(stream.host.id.to_string(),if stream.connected(){LiveStatus::Ok}else{LiveStatus::Failed});}}
        let local_alive = self.sessions.iter().any(|s| matches!(s.kind, SessionKind::Local) && !s.exited);
        let active = match self.active.and_then(|ix| self.sessions.get(ix)) {
            None => Active::None,
            Some(session) => session.host().map_or(Active::Local, |host| Active::Host(host.id.to_string())),
        };
        self.sidebar.update(cx, |s, cx| {
            s.set_sessions(count, live, summary.into(), cx);
            s.set_rail_state(local_alive, active, cx);
        });
    }

    /// A click on a host of the rail: its tab comes to the front; with no tab the host is just selected.
    fn activate_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix)=self.sessions.iter().position(|s|s.host().is_some_and(|h|&h.id==id)){self.activate(ix,window,cx);return;}

        let tabs = self.sessions.iter().enumerate().filter_map(|(ix, s)| s.ssh().map(|ssh| (ix, ssh)));
        match ssh::existing_session(tabs, id) {
            ssh::Existing::Switch(ix) | ssh::Existing::Reconnect(ix) => self.activate(ix, window, cx),
            ssh::Existing::None => self.sidebar.update(cx, |s, cx| s.select(Some(id.to_string()), cx)),
        }
    }

    // ---- actions -------------------------------------------------------

    fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.close(ix, window, cx);
        }
    }

    fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.activate((ix + 1) % self.sessions.len(), window, cx);
        }
    }

    fn previous_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            let n = self.sessions.len();
            self.activate((ix + n - 1) % n, window, cx);
        }
    }

    /// Shows the whole sidebar or only its rail, and remembers which.
    fn set_sidebar_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.sidebar_open == open {
            return;
        }
        self.sidebar_open = open;
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_collapsed(!open, cx));
        if let Err(err) = self.store.borrow_mut().set_setting(SIDEBAR_COLLAPSED, if open { "0" } else { "1" }) {
            log::warn!("could not save the sidebar setting: {err}");
        }
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_open(!self.sidebar_open, cx);
    }

    fn focus_host_search(&mut self, _: &FocusHostSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_open(true, cx);
        // From the rail the search field does not exist yet: it gets the cursor once the sidebar is open.
        let sidebar = self.sidebar.clone();
        window.defer(cx, move |window, cx| sidebar.update(cx, |s, cx| s.focus_search(window, cx)));
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        theme::apply(!cx.is_dark(), cx);
        let style = cx.terminal_style();
        for session in &self.sessions {
            session.view.update(cx, |v, cx| v.set_style(style.clone(), cx));
        }
        cx.notify();
    }

    /// Clicking non-focusable chrome (toolbar, header, empty space) moves focus
    /// to the workspace root; hand it back to the terminal so typing keeps working.
    fn restore_terminal_focus(&mut self, _: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.focus_active(window, cx);
        }
    }

    /// ⌘1…⌘8 select a tab, ⌘9 the last one.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;
        if !m.platform || m.control || m.alt || m.shift || self.sessions.is_empty() {
            return;
        }
        if let Ok(n @ 1..=9) = event.keystroke.key.parse::<usize>() {
            let ix = if n == 9 { self.sessions.len() - 1 } else { n - 1 };
            if ix < self.sessions.len() {
                self.activate(ix, window, cx);
                cx.stop_propagation();
            }
        }
    }

    // ---- rendering -----------------------------------------------------

    fn render_tab_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        // Shape the same fonts/weights the strip renders; titles are capped, chips are not.
        let measure = |text: &str, size: f32, weight: FontWeight, window: &mut Window| {
            let font = gpui::Font { weight, ..gpui::font(theme::UI_FONT) };
            let run = gpui::TextRun {
                len: text.len(),
                font,
                color: p.tx,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            f32::from(window.text_system().shape_line(text.to_string().into(), px(size), &[run], None).width).ceil()
        };
        let widths = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, session)| {
                let title = measure(
                    &session.title,
                    13.,
                    if Some(ix) == self.active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM },
                    window,
                );
                let reconnect =
                    session.ssh().and_then(|ssh| ssh.badge()).map(|badge| measure(&badge, 10., FontWeight::BOLD, window));
                let broadcast = (self.broadcast.enabled && self.broadcast.checked.contains(&session.id)).then(|| {
                    measure(
                        "广播",
                        10.,
                        if Some(ix) == self.active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM },
                        window,
                    )
                });
                crate::tabs::tab_width(title, matches!(session.kind, SessionKind::Local), reconnect, broadcast)
            })
            .collect::<Vec<_>>();
        let toolbar_width = 32.
            + [("广播", 15.), ("AI", 14.), ("快捷命令", 14.)]
                .iter()
                .map(|(label, icon)| 24. + icon + 6. + measure(label, 12.5, FontWeight::SEMIBOLD, window))
                .sum::<f32>();
        let content_width =
            f32::from(window.viewport_size().width) - if self.sidebar_open { SIDEBAR_WIDTH } else { RAIL_WIDTH } - 12.;
        let available = (content_width - toolbar_width - 4. * 8.).max(0.);
        let overflow_label = format!("{} 个会话 ▾", self.sessions.len());
        let overflow_width = measure(&overflow_label, 12., FontWeight::NORMAL, window) + 16.;
        let cap = None;
        #[cfg(debug_assertions)]
        let cap = if crate::proto::requested()
            && std::env::var("SHELLMATE_DEMO").is_ok_and(|s| !s.starts_with("proto:stress-"))
        {
            // Ordinary prototype scenes use a count-based sliding window, even when another
            // tab physically fits. Real sessions and stress fixtures use only measured widths.
            Some((if self.sidebar_open { 4 } else { 5 }) - usize::from(self.commands_open||self.ai_open))
        } else {
            cap
        };
        let fit = crate::tabs::fit_tabs(&widths, available, overflow_width, self.active, cap);
        let visible = fit.visible;
        let strip_width = visible.iter().map(|ix| widths[*ix].min(fit.single_tab_max)).sum::<f32>()
            + crate::tabs::TAB_GAP * visible.len().saturating_sub(1) as f32;
        let has_tabs = !visible.is_empty();
        let tabs = visible.into_iter().map(|ix| {
            let s = &self.sessions[ix];
            let active = Some(ix) == self.active;
            div()
                .id(("tab", s.id))
                .w(px(widths[ix].min(fit.single_tab_max)))
                .min_w_0()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .h(px(36.))
                .pl(px(12.))
                .pr(px(6.))
                .rounded(px(12.))
                .cursor_pointer()
                .text_size(px(13.))
                .line_height(px(16.))
                .when(active, |d| d.bg(p.card).shadow(item_shadow(&p)).font_weight(FontWeight::SEMIBOLD).text_color(p.tx))
                .when(!active, |d| d.font_weight(FontWeight::MEDIUM).text_color(p.tx2).hover(|d| d.bg(p.card.opacity(0.5))))
                .when(matches!(s.kind, SessionKind::Local), |d| {
                    d.child(AppIcon::Laptop.icon().text_color(session_dot(s, &p)))
                })
                .when(!matches!(s.kind, SessionKind::Local), |d| d.child(dot(session_dot(s, &p), 7.)))
                .child(crate::ui::ellipsis(("tab-title", s.id), s.title.clone()).flex_1().max_w(px(crate::tabs::TITLE_MAX)))
                .when(self.broadcast.enabled && self.broadcast.checked.contains(&s.id), |d| {
                    d.child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(p.warns)
                            .text_color(p.warn)
                            .text_size(px(10.))
                            .child("广播"),
                    )
                })
                .children(s.ssh().and_then(|ssh| ssh.badge().map(|badge| (ssh.status, badge))).map(|(status, badge)| {
                    let failed = status == ssh::SshStatus::Failed;
                    let color = if failed { p.err } else { p.warn };
                    let id = s.id;
                    div()
                        .id(("tab-badge", id))
                        .flex_none()
                        .h(px(16.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .bg(color.opacity(0.14))
                        .text_color(color)
                        .text_size(px(10.))
                        .line_height(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .child(badge)
                        .tooltip(|window, cx| Tooltip::new("点击或按回车立即重试").build(window, cx))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.retry_tab(id, window, cx);
                        }))
                }))
                .child(
                    div()
                        .id(("close-tab", s.id))
                        .flex_none()
                        .size(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .hover(|d| d.bg(p.card3))
                        .text_size(px(15.))
                        .line_height(px(15.))
                        .text_color(p.tx3)
                        .child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close(ix, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
        });

        let new_tab = div()
            .id("new-tab")
            .size(px(crate::tabs::PLUS_WIDTH))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|d| d.bg(p.card3.opacity(0.7)))
            .child(AppIcon::Plus.at(15.).text_color(p.tx2))
            .tooltip(|window, cx| Tooltip::new("新建本机终端 ⌘T").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_local(window, cx)));

        let theme_icon = if cx.is_dark() { AppIcon::Sun } else { AppIcon::Moon };
        div()
            .h(px(58.))
            .flex_none()
            .pt(px(4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .w(px(available))
                    .flex_none()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(strip_width))
                            .flex_none()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(crate::tabs::TAB_GAP))
                            .children(tabs),
                    )
                    .child(div().flex_none().when(has_tabs, |d| d.ml(px(crate::tabs::TAB_GAP))).child(new_tab))
                    .child(window_drag_area(div().id("tabbar-drag")).flex_1().min_w_0().h_full())
                    .when(fit.overflow, |d| {
                        d.child(
                            div()
                                .id("tab-list-button")
                                .w(px(overflow_width))
                                .ml(px(crate::tabs::OVERFLOW_GAP))
                                .flex_none()
                                .cursor_pointer()
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(8.))
                                .text_size(px(12.))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(p.tx2)
                                .child(overflow_label)
                                .on_click(cx.listener(|this, event: &gpui::ClickEvent, w, cx| {
                                    this.open_session_menu(
                                        crate::productivity::session_menu::Kind::Tabs,
                                        event.position(),
                                        w,
                                        cx,
                                    )
                                })),
                        )
                    }),
            )
            .child(
                toolbar_button("broadcast", AppIcon::Broadcast, Some("广播"), "向多个会话按行广播", true, cx)
                    .when(self.broadcast.enabled, |b| b.bg(p.warns).text_color(p.warn))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_broadcast(window, cx))),
            )
            .child(toolbar_button("ai", AppIcon::Ai, Some("AI"), "AI 助手", true, cx).when(self.ai_open,|b|b.bg(p.accs).text_color(p.acc)).on_click(cx.listener(|this,_,w,cx|this.toggle_ai(w,cx))))
            .child(
                toolbar_button("snippets", AppIcon::Commands, Some("快捷命令"), "快捷命令", true, cx)
                    .when(self.commands_open, |b| b.bg(p.accs).text_color(p.acc))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_commands(window, cx))),
            )
            .child(
                toolbar_button("theme", theme_icon, None, "切换浅色 / 深色", true, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(&ToggleTheme, window, cx))),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let session = self.active.and_then(|ix| self.sessions.get(ix));
        let (title, subtitle) = match session {
            Some(s) => (s.title.clone(), self.session_subtitle(s, cx)),
            None => ("没有打开的会话".into(), "从左侧选择主机，或按 ⌘T 打开本机终端".to_string()),
        };
        let active_panel = session.map(|s| s.panel);
        let items = Panel::ALL
            .iter()
            .map(|panel| {
                let item = SegmentItem::new(panel.label(), Some(*panel) == active_panel);
                match (session, panel) {
                    (None, _) => item.disabled("没有打开的会话"),
                    (Some(s), panel) if s.supports_panel(*panel) => item,
                    (Some(s), other) if s.ssh().is_some() => item.disabled(ssh::panel_hint(*other)),
                    (Some(s), _) if s.host().is_some() => item.disabled("串口 / Telnet 会话不支持此面板"),
                    (Some(_), other) => item.disabled(other.local_hint()),
                }
            })
            .collect();

        window_drag_area(div().id("header-drag"))
            .h(px(50.)).flex_none().px(px(4.)).pr(px(0.)).pb(px(6.))
            .flex()
            .items_center()
            .gap(px(12.))
            .mb(px(0.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .flex_1()
                    .min_w_0().overflow_hidden()
                    .child(crate::ui::ellipsis("session-heading",title).flex_shrink(1.).max_w(px(310.)).text_size(px(20.)).line_height(px(23.)).font_weight(FontWeight::BOLD).text_color(p.tx))
                    .child(crate::ui::ellipsis("session-subtitle",subtitle).flex_1().min_w_0().text_size(px(12.)).line_height(px(15.)).text_color(p.tx2)),
            )
            .child(
                segmented(
                    "panels",
                    items,
                    {
                        let this = cx.entity();
                        move |ix, window, cx| {
                            this.update(cx, |this, cx| this.show_panel(Panel::ALL[ix], window, cx));
                        }
                    },
                    cx,
                )
                .flex_none().h(px(36.)).rounded(px(12.)),
            )
    }

    /// Header subtitle: the SSH target of an SSH session, the shell of a local one.
    fn session_subtitle(&self, session: &Session, cx: &mut Context<Self>) -> String {
        if let SessionKind::Stream(s)=&session.kind {return if let Some(state)=&s.snapshot {format!("{} · {} {} · 串口",s.host.addr,state.settings.baud,state.settings.framing())}else{format!("{} · Telnet",s.host.address_line())};}

        if let Some(ssh)=session.ssh().filter(|s|s.mosh.is_some()){return ssh::subtitle(&ssh.host,&ssh.via(),&connect::default_user());}
        #[cfg(debug_assertions)] if crate::proto::requested(){if session.ssh().is_none(){return "chen@MacBook-Pro · /bin/zsh -l · ~".into();}
        if let Some(ssh)=session.ssh(){return format!("{}@{}:{} · {} · {}",ssh.host.user.as_deref().unwrap_or(""),ssh.host.addr,ssh.host.port,ssh.host.protocol.label(),ssh.host.auth.label());}}
        match session.ssh() {
            Some(ssh) => {
                let target = ssh::subtitle(&ssh.host, &ssh.via(), &connect::default_user());
                match ssh.status_note() {
                    Some(note) => format!("{target} · {note}"),
                    None => target,
                }
            }
            None => self.local_subtitle(session, cx),
        }
    }

    /// `chen@MacBook-Pro · /bin/zsh -l · ~/projects`
    fn local_subtitle(&self, session: &Session, cx: &mut Context<Self>) -> String {
        let terminal = session.terminal.clone();
        let (command, shell, fg) = terminal.update(cx, |t, _| {
            let options = t.options().cloned().unwrap_or_default();
            (options.command_line(), shell_name(&options.shell_path()), t.foreground_process())
        });
        let mut parts = vec![self.user_host.to_string(), command];
        if let Some(cwd) = fg.as_ref().and_then(|f| f.cwd.as_deref()) {
            parts.push(tilde(cwd));
        }
        if let Some(fg) = fg.filter(|fg| fg.name != shell && !session.exited) {
            parts.push(format!("正在运行 {}", fg.name));
        }
        if session.exited {
            parts.push("已退出".into());
        }
        parts.join(" · ")
    }

    fn render_terminal_card(&self, session: &Session, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let term_bg = to_hsla(cx.terminal_style().palette.background);
        let term_fg = to_hsla(cx.terminal_style().palette.foreground);
        let focused=self.focused_session().unwrap_or(session);
        let size = focused.terminal.read(cx).size();
        let status=if focused.ssh().is_some(){ssh::status_line(size.cols,size.rows)}else{format!("PTY {}×{}    xterm-256color    UTF-8    本机 PTY",size.cols,size.rows)};
        let status_fields=status.split("    ").map(str::to_string).collect::<Vec<_>>();
        #[cfg(debug_assertions)] let status_fields=if crate::proto::requested(){vec![format!("PTY {}×38",if self.split.other.is_some(){66}else if self.commands_open||self.ai_open{96}else{134}),"xterm-256color".into(),"UTF-8".into(),"延迟 28ms · 保活 30s".into()]}else{status_fields};
        let status_fields=if let SessionKind::Stream(s)=&focused.kind {
            vec![if crate::prototype_fixture(){"PTY 134×38".into()}else{format!("PTY {}×{}",size.cols,size.rows)},"xterm-256color".into(),"UTF-8".into(),s.snapshot.as_ref().map_or_else(||"Telnet 明文".into(),|s|format!("RX {:.1} KB",s.received as f64/1024.))]
        }else{status_fields};
        let status_fields=if let Some(mosh)=focused.ssh().and_then(|s|s.mosh.as_ref()) {
            vec![format!("PTY {}×{}",size.cols,size.rows),"xterm-256color".into(),"UTF-8".into(),mosh.footer()]
        }else{status_fields};
        let view = focused.view.clone();
        let recording=self.running_recordings.get(&focused.id);
        let recording_seconds = recording.map(|r| r.display_seconds());
        #[cfg(debug_assertions)]
        let recording_seconds = if std::env::var("SHELLMATE_DEMO").is_ok_and(|s| crate::proto::request(s.strip_prefix("proto:").unwrap_or(&s)).0 == "rv-recording") {
            Some(29.)
        } else { recording_seconds };
        let recording_label = crate::records::recording_label(recording_seconds);

        let pill = |id: &'static str, label: &'static str, tip: &'static str, enabled: bool| {
            div()
                .id(id)
                .h(px(22.))
                .px(px(9.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .bg(gpui::rgba(0x7f7f7f29))
                .font_family(theme::MONO_FONT).line_height(px(15.))
                .text_size(px(11.5))
                .text_color(term_fg)
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(p.card3).text_color(p.tx)))
                .when(!enabled, |d| d.opacity(0.5))
                .child(label)
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        };

        let overlay:Option<gpui::AnyElement>=None;
        #[cfg(debug_assertions)] let overlay=self.proto_reconnect_overlay(cx).or(overlay);
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(CARD_RADIUS)
            .bg(term_bg)
            .shadow(crate::ui::terminal_shadow(&p))
            .child(self.render_panes(session,cx))
            .when(crate::ai::model::error_line(&focused.terminal.read(cx).recent_displayed_text(size.rows as usize)).is_some(),|d|d.child(div().absolute().top(px(if focused.host().is_some_and(|h|h.protocol==shellmate_core::Protocol::Serial){56.}else{12.})).right(px(14.))
                .child(toolbar_button("ai-explain",AppIcon::Ai,Some("AI 解释这个报错"),"解释当前终端报错",true,cx).bg(p.card).text_color(p.acc).on_click(cx.listener(|s,_,w,cx|s.explain_ai(w,cx))))))
            .when(self.broadcast.enabled,|d|d.child(self.broadcast_bar.clone()))
            .child(
                div()
                    .h(px(33.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(10.)).pl(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .rounded_b(CARD_RADIUS)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden().flex().items_center().gap(px(14.))
                            .font_family(theme::MONO_FONT)
                            .text_size(px(11.5))
                            .text_color(to_hsla(cx.terminal_style().palette.dim))
                            .children(status_fields.into_iter().map(|text|div().flex_none().line_height(px(15.)).child(text))),
                    )
                    .child(div().id("record-session").h(px(22.)).font_family(theme::MONO_FONT).px(px(9.)).flex().items_center().rounded(px(7.)).cursor_pointer().text_size(px(11.5))
                        .line_height(px(15.)).font_weight(FontWeight::NORMAL).bg(if recording_seconds.is_some(){to_hsla(cx.terminal_style().palette.ansi[1])}else{gpui::rgba(0x7f7f7f29).into()}).text_color(if recording_seconds.is_some(){gpui::white()}else{term_fg}).child(recording_label)
                        .on_click(cx.listener(|this,_,w,cx|this.toggle_recording(w,cx))))
                    .child(pill("logs", "记录", "会话记录与回放", true).on_click(cx.listener(|this,_,w,cx|this.open_records(None,false,w,cx))))
                    .child(
                        pill("history", if focused.view.read(cx).history_shortcut { "历史 ⌃R" } else { "历史" }, "跨主机命令历史", true)
                            .on_click(cx.listener(|this, _, window, cx| this.open_history(window, cx))),
                    )
                    .child(pill("search", "搜索 ⌘F", "在输出中搜索", true).on_click(move |_, window, cx| {
                        view.update(cx, |v, cx| {
                            window.focus(&v.focus_handle(cx), cx);
                            v.toggle_search(&ToggleSearch, window, cx);
                        });
                    }))
                    .child(pill("split", if self.split.other.is_some(){"取消分屏"}else{"分屏"}, "切换分屏", true)
                        .on_click(cx.listener(|this,_,w,cx|this.toggle_split(w,cx)))),
            ).children(overlay)

    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .rounded(CARD_RADIUS)
            .bg(p.card)
            .shadow(card_shadow(&p))
            .child(Icon::new(Lucide::SquareTerminal).size(px(36.)).text_color(p.tx3))
            .child(div().text_size(px(14.)).text_color(p.tx2).child("没有打开的会话"))
            .child(
                Button::new("open-local")
                    .primary()
                    .label("打开本机终端  ⌘T")
                    .on_click(cx.listener(|this, _, window, cx| this.open_local(window, cx))),
            )
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_menus(cx);
        let p = cx.palette().clone();
        let activity=self.secrets.clone();
        let content = match self.active.and_then(|ix| self.sessions.get(ix)) {
            Some(Session { panel: Panel::Files, files: Some(files), .. }) => {
                div().flex_1().min_w_0().min_h_0().flex().child(files.panel.clone()).into_any_element()
            }
            Some(Session { panel: Panel::Forward, forward: Some(forward), .. }) => {
                div().flex_1().min_w_0().min_h_0().flex().child(forward.panel.clone()).into_any_element()
            }
            Some(Session { panel: Panel::Monitor, monitor: Some(monitor), .. }) => {
                div().flex_1().min_w_0().min_h_0().flex().child(monitor.panel.clone()).into_any_element()
            }
            Some(session) => self.render_terminal_card(session, cx).into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };

        let drag_owner = cx.entity().downgrade();
        let body = div()
            .relative().flex_1().min_w_0().min_h_0().flex().child(content)
            .children(self.picker.as_ref().filter(|picker| picker.read(cx).is_history()).map(|picker| {
                div().absolute().left(px(14.)).right(px(14.)).bottom(px(44.)).child(picker.clone())
            }));
        let palette = self.picker.as_ref().filter(|picker| !picker.read(cx).is_history()).map(|picker| {
            div().id("palette-overlay").occlude().absolute().inset_0()
                .bg(gpui::rgba(0x0a0c1029))
                .flex().justify_center().items_start().pt(px(110.))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.picker = None;
                    this.focus_active(window, cx);
                    cx.notify();
                }))
                .child(div().id("palette-card").w(px(640.))
                    .on_click(|_, _, cx| cx.stop_propagation()).child(picker.clone()))
        });

        div()
            .id("workspace")
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .map(|root| self.menu_listeners(root, cx))
            .on_action(cx.listener(Self::focus_host_search))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::restore_terminal_focus))
            .size_full()
            .flex()
            .bg(p.win)
            .font_family(theme::UI_FONT).text_size(px(13.)).line_height(gpui::relative(1.3))
            .text_color(p.tx)
            .child(self.sidebar.clone())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .pr(px(12.))
                    .pb(px(12.))
                    // Beside the sidebar there is a gap; beside the rail the tabs start right after it.
                    .pl(px(0.))
                    .child(self.render_tab_bar(_window,cx))
                    .child(self.render_header(cx))
                    .child(
                        div().flex_1().min_h_0().flex().gap(px(12.)).child(body)
                            .when(self.commands_open, |d| d.child(self.commands.clone())).when(self.ai_open, |d| d.child(
                                div().relative().w(px(self.drawer_width(_window))).flex_none().h_full().min_h_0()
                                    .children(self.ai_drawer.clone()).child(self.drawer_handle(cx))
                            )),
                    ),
            )
            .children(self.session_menu.as_ref().map(|menu| gpui::deferred(
                gpui::anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(menu.view.clone())
            ).with_priority(3)))
            .children(palette)
            .child(gpui::canvas(|_, _, _| (), move |bounds, _, window, cx| {
                if let Some(owner) = drag_owner.upgrade() {
                    let moved = owner.clone();
                    window.on_mouse_event::<gpui::MouseMoveEvent>(move |e, phase, w, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            moved.update(cx, |s, cx| s.move_drawer_drag(e, w, cx));
                        }
                    });
                    window.on_mouse_event::<gpui::MouseUpEvent>(move |e, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Capture && e.button == MouseButton::Left {
                            owner.update(cx, |s, cx| {
                                if s.ai_drawer_drag.active() {
                                    s.end_drawer_drag(cx);
                                    cx.stop_propagation();
                                }
                            });
                        }
                    });
                }
                let moved=activity.clone();window.on_mouse_event::<gpui::MouseMoveEvent>(move|_,phase,_,_|{if phase==gpui::DispatchPhase::Capture{moved.activity();}});
                let clicked=activity.clone();window.on_mouse_event::<gpui::MouseDownEvent>(move|_,phase,_,_|{if phase==gpui::DispatchPhase::Capture{clicked.activity();}});
                let scrolled=activity.clone();window.on_mouse_event::<gpui::ScrollEvent>(move|_,phase,_,_|{if phase==gpui::DispatchPhase::Capture{scrolled.activity();}});
                // Paint at the window layer, independent of a dialog content box or sidebar clipping.
                // The component dialog host retains focus trapping and input occlusion above this.
                if window.has_active_dialog(cx) {
                    window.paint_quad(gpui::fill(bounds, p.scrim));
                }
            }).absolute().inset_0())
    }
}

/// Color of a tab's status dot: connecting yellow, ok green, failed red, ended grey.
fn session_dot(session: &Session, p: &theme::Palette) -> gpui::Hsla {
    match &session.kind {
        SessionKind::Stream(s) => if s.connected() {p.ok}else{p.tx3},
        SessionKind::Local if session.exited => p.tx3,
        SessionKind::Local => p.ok,
        SessionKind::Ssh(ssh) => match ssh.status {
            ssh::SshStatus::Connecting | ssh::SshStatus::Reconnecting(_) => p.warn,
            ssh::SshStatus::Connected => p.ok,
            ssh::SshStatus::Failed => p.err,
            ssh::SshStatus::Ended => p.tx3,
        },
    }
}

fn user_at_host() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let mut buf = [0u8; 256];
    // SAFETY: buffer is valid for its length; gethostname NUL-terminates on success.
    let host = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).trim_end_matches(".local").to_string()
    } else {
        "localhost".into()
    };
    format!("{user}@{host}")
}

fn shell_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn local_terminal_summary<'a>(shell: &str, kinds: impl IntoIterator<Item = &'a SessionKind>) -> String {
    let count = kinds.into_iter().filter(|kind| matches!(kind, SessionKind::Local)).count();
    if count == 0 { format!("{shell} · ~") } else { format!("{shell} · {count} 个会话") }
}

/// Abbreviates `$HOME` to `~`.
fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if path.starts_with(&home) => {
            let rest = path.strip_prefix(&home).unwrap_or(path);
            if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) }
        }
        _ => path.display().to_string(),
    }
}
