//! Root view: sidebar + session tabs + active session panel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::assets::IconName as Lucide;
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
use shellmate_store::{HostStore, KeychainStore, SecretStore};
use shellmate_term::colors::to_hsla;
use shellmate_term::view::ToggleSearch;
use shellmate_term::{PtyOptions, Terminal, TerminalEvent, TerminalView};

use crate::connect;
use crate::editor::view::FileEditor;
use crate::sidebar::{Active, LiveStatus, Sidebar, SidebarEvent};
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
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-t", DuplicateSession, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("cmd-ctrl-s", ToggleSidebar, Some(CONTEXT)),
        KeyBinding::new("cmd-k", OpenPalette, Some(CONTEXT)),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// The panels a session can show. Only SSH sessions get more than the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Terminal,
    Files,
    Forward,
    Monitor,
}

impl Panel {
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
    Ssh(Box<ssh::SshSession>),
}

struct Session {
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

impl Session {
    fn ssh(&self) -> Option<&ssh::SshSession> {
        match &self.kind {
            SessionKind::Ssh(ssh) => Some(ssh),
            SessionKind::Local => None,
        }
    }
}

pub struct Workspace {
    store: Rc<RefCell<HostStore>>,
    /// Saved passwords and key passphrases (the macOS keychain).
    secrets: Arc<dyn SecretStore>,
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
        let sidebar = cx.new(|cx| Sidebar::new(book.clone(), window, cx));
        sidebar.update(cx, |sidebar, cx| sidebar.set_collapsed(collapsed, cx));
        let commands = cx.new(|_| crate::productivity::panel::Drawer {
            groups: store.snippets().unwrap_or_default(),
            broadcast: Default::default(),
        });
        let broadcast_bar=cx.new(|cx|crate::productivity::broadcast::BroadcastBar::new(window,cx));
        let subscriptions = vec![
            cx.subscribe_in(&sidebar, window, Self::on_sidebar_event),
            cx.subscribe_in(&commands, window, Self::on_command_event),
            cx.subscribe_in(&broadcast_bar,window,Self::on_broadcast_submit),
        ];
        let preferences=store.preferences().unwrap_or_default();
        let mut this = Self {
            preferences,
            data_root:shellmate_term::archive::data_root(),log_root:shellmate_term::archive::log_root(),
            running_recordings:HashMap::new(),archive_task:None,records_window:None,
            store: Rc::new(RefCell::new(store)),
            secrets: Arc::new(KeychainStore::new()),
            pending_forms: HashMap::new(),
            book,
            sidebar,
            sessions: Vec::new(),
            temporary_hosts: HashMap::new(),
            commands,
            commands_open: false,
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
            let mut view = TerminalView::new(terminal.clone(), style, cx);
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
                (SessionKind::Ssh(_), TerminalEvent::Exited(_)) => this.on_tab_exited(id, cx),
                (SessionKind::Ssh(_), TerminalEvent::Disconnected(_)) => this.on_tab_disconnected(id, window, cx),
                (SessionKind::Ssh(_), TerminalEvent::RestartRequested) => this.retry_tab(id, window, cx),
                _ => {}
            }
            this.sync_sidebar(cx);
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
        let selected = session.ssh().map_or_else(|| "local".to_string(), |ssh| ssh.host.id.to_string());
        self.split.select_tab(self.main_id(),new_id);
        self.session_menu=None;
        self.picker = None;
        self.picker_subscription = None;
        self.active = Some(ix);
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
        self.sessions.remove(ix);
        self.update_pane_radii(cx);
        if let Some(group) = group {
            self.release_group(group, window, cx);
        }
        match self.active {
            _ if self.sessions.is_empty() => {
                self.active = None;
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
        let summary = if crate::prototype_fixture(){"zsh · ~".to_string()} else if count == 0 { format!("{shell} · ~") } else { format!("{shell} · {count} 个标签") };
        let mut live: HashMap<String, LiveStatus> = HashMap::new();
        for ssh in self.sessions.iter().filter_map(Session::ssh) {
            if let Some(status) = ssh.live() {
                let entry = live.entry(ssh.host.id.to_string()).or_insert(status);
                if status.rank() < entry.rank() {
                    *entry = status;
                }
            }
        }
        let local_alive = self.sessions.iter().any(|s| matches!(s.kind, SessionKind::Local) && !s.exited);
        let active = match self.active.and_then(|ix| self.sessions.get(ix)) {
            None => Active::None,
            Some(session) => session.ssh().map_or(Active::Local, |ssh| Active::Host(ssh.host.id.to_string())),
        };
        self.sidebar.update(cx, |s, cx| {
            s.set_sessions(count, live, summary.into(), cx);
            s.set_rail_state(local_alive, active, cx);
        });
    }

    /// A click on a host of the rail: its tab comes to the front; with no tab the host is just selected.
    fn activate_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = self.sessions.iter().enumerate().filter_map(|(ix, s)| s.ssh().map(|ssh| (ix, ssh)));
        match ssh::existing_session(tabs, id) {
            ssh::Existing::Switch(ix) | ssh::Existing::Reconnect(ix) => self.activate(ix, window, cx),
            ssh::Existing::None => self.sidebar.update(cx, |s, cx| s.select(Some(id.to_string()), cx)),
        }
    }

    // ---- actions -------------------------------------------------------

    fn new_local_terminal(&mut self, _: &NewLocalTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.open_local(window, cx);
    }

    fn duplicate_session(&mut self, _: &DuplicateSession, window: &mut Window, cx: &mut Context<Self>) {
        self.duplicate_active(window, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.close(ix, window, cx);
        }
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.activate((ix + 1) % self.sessions.len(), window, cx);
        }
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
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

    fn render_tab_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let preferred=crate::productivity::multi::tab_limit(self.sidebar_open,self.commands_open);
        let limit=crate::layout::tabs_that_fit(f32::from(window.viewport_size().width)-if self.sidebar_open {284.} else {76.}-12.,preferred);
        let visible=crate::productivity::multi::visible_tabs(self.sessions.len(),self.active,limit);
        let tabs = visible.into_iter().map(|ix| { let s=&self.sessions[ix];
            let active = Some(ix) == self.active;
            div()
                .id(("tab", s.id)).max_w(px(190.)).min_w_0()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .h(px(36.))
                .pl(px(12.))
                .pr(px(6.))
                .rounded(px(12.))
                .cursor_pointer()
                .text_size(px(13.)).line_height(px(16.))
                .when(active, |d| d.bg(p.card).shadow(item_shadow(&p)).font_weight(FontWeight::SEMIBOLD).text_color(p.tx))
                .when(!active, |d| d.font_weight(FontWeight::MEDIUM).text_color(p.tx2).hover(|d| d.bg(p.card.opacity(0.5))))
                .child(dot(session_dot(s, &p), 7.))
                .child(crate::ui::ellipsis(("tab-title",s.id),s.title.clone()).flex_1())
                .when(self.broadcast.enabled && self.broadcast.checked.contains(&s.id),|d|d.child(div().px(px(5.)).rounded(px(4.)).bg(p.warns).text_color(p.warn).text_size(px(10.)).child("广播")))
                .children(s.ssh().and_then(|ssh| ssh.badge().map(|badge| (ssh.status, badge))).map(|(status, badge)| {
                    let failed = status == ssh::SshStatus::Failed;
                    let color = if failed { p.err } else { p.warn };
                    let id = s.id;
                    div()
                        .id(("tab-badge", id))
                        .h(px(16.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .bg(color.opacity(0.14))
                        .text_color(color)
                        .text_size(px(10.)).line_height(px(12.))
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
                        .id(("close-tab", s.id)).flex_none()
                        .size(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .hover(|d| d.bg(p.card3))
                        .text_size(px(15.)).line_height(px(15.)).text_color(p.tx3).child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close(ix, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
        });

        let new_tab = div()
            .id("new-tab")
            .size(px(22.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|d| d.bg(p.card3.opacity(0.7)))
            .child(Icon::new(Lucide::Plus).size(px(15.)).text_color(p.tx2))
            .tooltip(|window, cx| Tooltip::new("新建本机终端 ⌘T").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_local(window, cx)));

        let theme_icon = if cx.is_dark() { Lucide::Sun } else { Lucide::Moon };
        div()
            .h(px(58.)).flex_none().pt(px(4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex().items_center().gap(px(6.)).min_w_0().overflow_hidden().children(tabs))
            .child(new_tab)
            .when(crate::productivity::multi::tab_overflow(self.sessions.len(),self.sidebar_open,self.commands_open)||self.sessions.len()>limit,|d|d.child(
                div().id("tab-list-button").flex_none().cursor_pointer().px(px(8.)).py(px(5.)).rounded(px(8.)).text_size(px(12.)).text_color(p.tx2)
                    .child(format!("{} 个标签 ▾",self.sessions.len()))
                    .on_click(cx.listener(|this,event: &gpui::ClickEvent,w,cx|this.open_session_menu(crate::productivity::session_menu::Kind::Tabs,event.position(),w,cx)))
            ))
            .child(window_drag_area(div().id("tabbar-drag")).flex_1().h_full())
            .child(toolbar_button("broadcast", Lucide::RadioTower, Some("广播"), "向多个会话按行广播", true, cx)
                .when(self.broadcast.enabled,|b|b.bg(p.warns).text_color(p.warn))
                .on_click(cx.listener(|this,_,_,cx|{this.commands_open=true;this.sync_productivity(cx);cx.notify();})))
            .child(toolbar_button("ai", Lucide::Sparkles, Some("AI"), "AI 助手（P7 实现）", false, cx))
            .child(
                toolbar_button("snippets", Lucide::Zap, Some("快捷命令"), "快捷命令", true, cx)
                    .when(self.commands_open, |b| b.bg(p.accs).text_color(p.acc))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.commands_open = !this.commands_open;
                        cx.notify();
                    })),
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
                    (Some(_), Panel::Terminal) => item,
                    (Some(s), Panel::Files | Panel::Forward | Panel::Monitor) if s.ssh().is_some() => item,
                    (Some(s), other) if s.ssh().is_some() => item.disabled(ssh::panel_hint(*other)),
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
        #[cfg(debug_assertions)] if crate::proto::requested(){if session.ssh().is_none(){return "chen@MacBook-Pro · /bin/zsh -l · ~ · 本机进程，不经过网络".into();}
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

    /// `chen@MacBook-Pro · /bin/zsh -l · ~/projects · 本机进程，不经过网络`
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
        match fg {
            Some(fg) if fg.name != shell && !session.exited => parts.push(format!("正在运行 {}", fg.name)),
            _ => parts.push("本机进程，不经过网络".into()),
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
        #[cfg(debug_assertions)] let status_fields=if crate::proto::requested(){vec![format!("PTY {}×38",if self.split.other.is_some(){66}else if self.commands_open{96}else{134}),"xterm-256color".into(),"UTF-8".into(),"延迟 28ms · 保活 30s".into()]}else{status_fields};
        let view = focused.view.clone();
        let recording=self.running_recordings.get(&focused.id);
        let recording_label=recording.map_or_else(||"● 录制".to_string(),|r|format!("REC {} · .cast",shellmate_term::archive::mmss(r.display_seconds())));

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
                        .line_height(px(15.)).font_weight(FontWeight::NORMAL).bg(if recording.is_some(){p.err}else{gpui::rgba(0x7f7f7f29).into()}).text_color(if recording.is_some(){gpui::white()}else{term_fg}).child(recording_label)
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
        let p = cx.palette().clone();
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
            .on_action(cx.listener(Self::new_local_terminal))
            .on_action(cx.listener(Self::duplicate_session))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::focus_host_search))
            .on_action(cx.listener(Self::open_palette))
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
                            .when(self.commands_open, |d| d.child(self.commands.clone())),
                    ),
            )
            .children(self.session_menu.as_ref().map(|menu| gpui::deferred(
                gpui::anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(menu.view.clone())
            ).with_priority(3)))
            .children(palette)
            .child(gpui::canvas(|_, _, _| (), move |bounds, _, window, cx| {
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
