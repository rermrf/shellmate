//! Root view: sidebar + session tabs + active session panel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

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
use shellmate_core::HostBook;
use shellmate_store::HostStore;
use shellmate_term::colors::to_hsla;
use shellmate_term::view::ToggleSearch;
use shellmate_term::{PtyOptions, Terminal, TerminalEvent, TerminalView};

use crate::sidebar::{Sidebar, SidebarEvent};
use crate::theme::{self, ActivePalette, CARD_RADIUS};
use crate::ui::{SegmentItem, card_shadow, dot, item_shadow, segmented, toolbar_button, window_drag_area};

actions!(
    shellmate,
    [NewLocalTerminal, CloseTab, NextTab, PreviousTab, ToggleSidebar, ToggleTheme, FocusHostSearch, Quit]
);

const CONTEXT: &str = "Workspace";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("cmd-ctrl-s", ToggleSidebar, Some(CONTEXT)),
        KeyBinding::new("cmd-k", FocusHostSearch, Some(CONTEXT)),
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

struct Session {
    id: usize,
    title: SharedString,
    view: Entity<TerminalView>,
    terminal: Entity<Terminal>,
    exited: bool,
    panel: Panel,
    _subscription: Subscription,
}

pub struct Workspace {
    store: Rc<RefCell<HostStore>>,
    book: Rc<HostBook>,
    sidebar: Entity<Sidebar>,
    sessions: Vec<Session>,
    active: Option<usize>,
    sidebar_open: bool,
    next_id: usize,
    user_host: SharedString,
    focus_handle: FocusHandle,
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
        let sidebar = cx.new(|cx| Sidebar::new(book.clone(), window, cx));
        let subscriptions = vec![cx.subscribe_in(&sidebar, window, Self::on_sidebar_event)];
        let mut this = Self {
            store: Rc::new(RefCell::new(store)),
            book,
            sidebar,
            sessions: Vec::new(),
            active: None,
            sidebar_open: true,
            next_id: 1,
            user_host: user_at_host().into(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        if let Some(message) = store_notice {
            push_notification_later(window, cx, Notification::warning(message).title("主机库无法打开"));
        }
        this.reload_book(window, cx);
        this.open_local(window, cx);
        this
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
            SidebarEvent::Connect(id) => {
                let name = self.book.host(id).map(|h| h.name.clone()).unwrap_or_default();
                window.push_notification(
                    Notification::info("SSH 连接界面将在下一步（M3）实现。").title(format!("暂不能连接 {name}")),
                    cx,
                );
            }
        }
    }

    // ---- sessions ------------------------------------------------------

    fn open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let style = cx.terminal_style();
        let palette = style.palette.clone();
        let terminal = cx.new(|cx| Terminal::local(PtyOptions::default(), palette, cx));
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal.clone(), style, cx);
            view.top_radius = CARD_RADIUS;
            view
        });

        let id = self.next_id;
        self.next_id += 1;
        let subscription = cx.subscribe(&terminal, move |this: &mut Self, _, event: &TerminalEvent, cx| {
            let Some(session) = this.sessions.iter_mut().find(|s| s.id == id) else { return };
            match event {
                TerminalEvent::Exited(_) | TerminalEvent::Disconnected(_) => session.exited = true,
                TerminalEvent::Restarted => session.exited = false,
                TerminalEvent::Output
                | TerminalEvent::TitleChanged
                | TerminalEvent::Bell
                | TerminalEvent::RestartRequested => {}
            }
            this.sync_sidebar(cx);
            cx.notify();
        });

        let open_locals = self.sessions.len();
        let title = if open_locals == 0 { "本机".to_string() } else { format!("本机 · {}", open_locals + 1) };
        self.sessions.push(Session {
            id,
            title: title.into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            _subscription: subscription,
        });
        self.activate(self.sessions.len() - 1, window, cx);
    }

    fn focus_or_open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active.or((!self.sessions.is_empty()).then_some(0)) {
            Some(ix) => self.activate(ix, window, cx),
            None => self.open_local(window, cx),
        }
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(ix) else { return };
        self.active = Some(ix);
        window.focus(&session.view.focus_handle(cx), cx);
        self.sidebar.update(cx, |s, cx| s.select(Some("local".into()), cx));
        self.sync_sidebar(cx);
        cx.notify();
    }

    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.sessions.len() {
            return;
        }
        self.sessions.remove(ix);
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
        cx.notify();
    }

    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        let count = self.sessions.len();
        let shell = shell_name(&PtyOptions::default().shell_path());
        let summary = if count == 0 { format!("{shell} · ~") } else { format!("{shell} · {count} 个标签") };
        self.sidebar.update(cx, |s, cx| s.set_sessions(count, HashMap::new(), summary.into(), cx));
    }

    // ---- actions -------------------------------------------------------

    fn new_local_terminal(&mut self, _: &NewLocalTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.open_local(window, cx);
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

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn focus_host_search(&mut self, _: &FocusHostSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        let sidebar = self.sidebar.clone();
        sidebar.update(cx, |s, cx| s.focus_search(window, cx));
        cx.notify();
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
        if self.focus_handle.is_focused(window)
            && let Some(session) = self.active.and_then(|ix| self.sessions.get(ix))
        {
            window.focus(&session.view.focus_handle(cx), cx);
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

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let tabs = self.sessions.iter().enumerate().map(|(ix, s)| {
            let active = Some(ix) == self.active;
            div()
                .id(("tab", s.id))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .h(px(34.))
                .pl(px(12.))
                .pr(px(6.))
                .rounded(px(10.))
                .cursor_pointer()
                .text_size(px(13.))
                .when(active, |d| d.bg(p.card).shadow(item_shadow(&p)).font_weight(FontWeight::SEMIBOLD).text_color(p.tx))
                .when(!active, |d| d.text_color(p.tx2).hover(|d| d.bg(p.card.opacity(0.5))))
                .child(dot(if s.exited { p.tx3 } else { p.ok }, 7.))
                .child(s.title.clone())
                .child(
                    div()
                        .id(("close-tab", s.id))
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .hover(|d| d.bg(p.card3))
                        .child(Icon::new(Lucide::X).size(px(12.)).text_color(p.tx3))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close(ix, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
        });

        let new_tab = div()
            .id("new-tab")
            .size(px(28.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|d| d.bg(p.card3.opacity(0.7)))
            .child(Icon::new(Lucide::Plus).size(px(16.)).text_color(p.tx2))
            .tooltip(|window, cx| Tooltip::new("新建本机终端 ⌘T").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_local(window, cx)));

        let theme_icon = if cx.is_dark() { Lucide::Sun } else { Lucide::Moon };
        div()
            .h(px(52.))
            .flex()
            .items_center()
            .gap(px(4.))
            .child(div().flex().items_center().gap(px(2.)).min_w_0().overflow_hidden().children(tabs))
            .child(new_tab)
            .child(window_drag_area(div().id("tabbar-drag")).flex_1().h_full())
            .child(toolbar_button("broadcast", Lucide::RadioTower, Some("广播"), "向多个会话广播输入（P6 实现）", false, cx))
            .child(toolbar_button("ai", Lucide::Sparkles, Some("AI"), "AI 助手（P7 实现）", false, cx))
            .child(toolbar_button("snippets", Lucide::Zap, Some("快捷命令"), "快捷命令（P6 实现）", false, cx))
            .child(
                toolbar_button("theme", theme_icon, None, "切换浅色 / 深色", true, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(&ToggleTheme, window, cx))),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let session = self.active.and_then(|ix| self.sessions.get(ix));
        let (title, subtitle) = match session {
            Some(s) => (s.title.clone(), self.local_subtitle(s, cx)),
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
                    (Some(_), other) => item.disabled(other.local_hint()),
                }
            })
            .collect();

        window_drag_area(div().id("header-drag"))
            .h(px(46.))
            .flex()
            .items_center()
            .gap(px(12.))
            .mb(px(8.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .flex_1()
                    .min_w_0()
                    .child(div().flex_none().text_size(px(22.)).font_weight(FontWeight::BOLD).text_color(p.tx).child(title))
                    .child(div().min_w_0().truncate().text_size(px(12.)).text_color(p.tx2).child(subtitle)),
            )
            .child(segmented("panels", items, |_, _, _| {}, cx).flex_none())
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
        let size = session.terminal.read(cx).size();
        let status = format!("PTY {}×{}    xterm-256color    UTF-8    本机 PTY", size.cols, size.rows);
        let view = session.view.clone();

        let pill = |id: &'static str, label: &'static str, tip: &'static str, enabled: bool| {
            div()
                .id(id)
                .h(px(24.))
                .px(px(9.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .bg(p.card2)
                .text_size(px(11.5))
                .text_color(p.tx2)
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(p.card3).text_color(p.tx)))
                .when(!enabled, |d| d.opacity(0.5))
                .child(label)
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(CARD_RADIUS)
            .bg(term_bg)
            .shadow(card_shadow(&p))
            .child(div().flex_1().min_h_0().child(session.view.clone()))
            .child(
                div()
                    .h(px(38.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .rounded_b(CARD_RADIUS)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(11.))
                            .text_color(p.tx3)
                            .child(status),
                    )
                    .child(pill("rec", "● 录制", "录制为 asciinema .cast（P6 实现）", false))
                    .child(pill("logs", "记录", "会话日志与录像（P6 实现）", false))
                    .child(pill("history", "历史 ⌃R", "跨主机命令历史（P6 实现）", false))
                    .child(pill("search", "搜索 ⌘F", "在输出中搜索", true).on_click(move |_, window, cx| {
                        view.update(cx, |v, cx| {
                            window.focus(&v.focus_handle(cx), cx);
                            v.toggle_search(&ToggleSearch, window, cx);
                        });
                    }))
                    .child(pill("split", "分屏", "分屏（P6 实现）", false)),
            )
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
            Some(session) => self.render_terminal_card(session, cx).into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };

        div()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_local_terminal))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::focus_host_search))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::restore_terminal_focus))
            .size_full()
            .flex()
            .bg(p.win)
            .text_color(p.tx)
            .when(self.sidebar_open, |d| d.child(self.sidebar.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .pr(px(12.))
                    .pb(px(12.))
                    .pl(px(if self.sidebar_open { 8. } else { 86. }))
                    .child(self.render_tab_bar(cx))
                    .child(self.render_header(cx))
                    .child(content),
            )
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
