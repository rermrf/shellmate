//! The 文件 panel: top bar, list and details of the directory the panel is in.

use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use gpui::assets::IconName as Lucide;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::popover::Popover;
use gpui::component::tooltip::Tooltip;
use gpui::component::{Icon, Sizable as _};
use gpui::prelude::*;
use gpui::{
    Anchor, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding,
    ScrollStrategy, SharedString, Stateful, Subscription, Task, UniformListScrollHandle, Window, actions, div, px,
    uniform_list, white,
};
use shellmate_sftp::{Entry, EntryKind, SftpError, SftpSession, format_size, mode_string, path};
use shellmate_ssh::SshRuntime;
use shellmate_term::{Terminal, TerminalEvent};
use tokio::task::AbortHandle;

use super::format::mtime_text;
use super::model::{Applied, FilesModel, LinkStatus, Listed, Request, Row, Subject, View};
use crate::theme::{ActivePalette, MONO_FONT, Palette};
use crate::ui::card_shadow;

/// The key context of the panel: its shortcuts only work while the list has focus, so none of them
/// can reach the terminal.
pub const CONTEXT: &str = "FilesPanel";

/// The keystrokes of the panel (each is bound to the action of the same row in [`init`]).
pub const KEYS: [&str; 4] = ["up", "down", "enter", "cmd-up"];

actions!(files, [FilesPrevious, FilesNext, FilesOpen, FilesParent]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(KEYS[0], FilesPrevious, Some(CONTEXT)),
        KeyBinding::new(KEYS[1], FilesNext, Some(CONTEXT)),
        KeyBinding::new(KEYS[2], FilesOpen, Some(CONTEXT)),
        KeyBinding::new(KEYS[3], FilesParent, Some(CONTEXT)),
    ]);
}

/// What the panel asks of its owner.
pub enum FilesEvent {
    /// The session could not be opened and the user asked again (刷新).
    RetrySession,
    /// An operation found this session unusable (the connection is gone, or the server stopped
    /// answering): the owner has to tell the link, which opens a new one.
    SessionLost(Arc<SftpSession>),
}

impl EventEmitter<FilesEvent> for FilesPanel {}

/// The snippets that make a shell report its directory.
const BASH_SNIPPET: &str = r#"PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'"#;
const ZSH_SNIPPET: &str = r#"precmd() { printf '\e]7;file://%s%s\a' "$HOST" "$PWD" }"#;

pub struct FilesPanel {
    model: FilesModel,
    session: Option<Arc<SftpSession>>,
    runtime: SshRuntime,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    hint_open: bool,
    /// Debug review hook: the entry to select as soon as it is listed.
    #[cfg(debug_assertions)]
    demo_select: Option<String>,
    listing: Option<Task<()>>,
    chmod: Option<Task<()>>,
    /// The work on the SSH runtime behind `listing` and `chmod`: stopped when it is superseded or
    /// the panel goes away, so a closed tab does not keep its connection busy.
    listing_abort: Option<AbortHandle>,
    chmod_abort: Option<AbortHandle>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for FilesPanel {
    fn drop(&mut self) {
        for handle in [self.listing_abort.take(), self.chmod_abort.take()].into_iter().flatten() {
            handle.abort();
        }
    }
}

impl Focusable for FilesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilesPanel {
    pub fn new(terminal: &Entity<Terminal>, runtime: SshRuntime, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut model = FilesModel::new();
        if let Some(dir) = terminal.read(cx).cwd() {
            model.on_terminal_cwd(dir);
        }
        let subscription = cx.subscribe_in(terminal, window, |this, _, event: &TerminalEvent, _, cx| {
            if let TerminalEvent::CwdChanged(dir) = event {
                let request = this.model.on_terminal_cwd(dir);
                this.start(request, cx);
                cx.notify();
            }
        });
        Self {
            model,
            session: None,
            runtime,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            hint_open: false,
            #[cfg(debug_assertions)]
            demo_select: None,
            listing: None,
            chmod: None,
            listing_abort: None,
            chmod_abort: None,
            _subscriptions: vec![subscription],
        }
    }

    /// The state of the session behind the panel changed.
    pub fn set_link(&mut self, status: LinkStatus, session: Option<Arc<SftpSession>>, cx: &mut Context<Self>) {
        let session_changed = match (&self.session, &session) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        if !session_changed && self.model.link() == &status {
            return;
        }
        self.session = session;
        let request = self.model.set_link(status);
        self.start(request, cx);
        cx.notify();
    }

    /// Debug review hook: goes to `dir` as if the user had.
    #[cfg(debug_assertions)]
    pub fn demo_go(&mut self, dir: &str, cx: &mut Context<Self>) {
        let request = self.model.go_to(dir);
        self.start(request, cx);
        cx.notify();
    }

    /// Debug review hook: selects the entry called `name` once it is listed (now, or when it is).
    #[cfg(debug_assertions)]
    pub fn demo_select(&mut self, name: &str) {
        self.demo_select = Some(name.to_string());
    }

    /// Debug review hook: a click on 刷新.
    #[cfg(debug_assertions)]
    pub fn demo_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    /// Debug review hook: opens the OSC 7 hint.
    #[cfg(debug_assertions)]
    pub fn demo_hint(&mut self, cx: &mut Context<Self>) {
        self.hint_open = true;
        cx.notify();
    }

    // ---- loading ------------------------------------------------------------------------------------

    /// Carries out a listing request on the SSH runtime.
    fn start(&mut self, request: Option<Request>, cx: &mut Context<Self>) {
        let (Some(request), Some(session)) = (request, self.session.clone()) else { return };
        let id = request.id;
        let began = Instant::now();
        if let Some(previous) = self.listing_abort.take() {
            previous.abort();
        }
        let used = session.clone();
        let join = self.runtime.spawn(async move {
            // `.` is the home directory: ask the server where that is.
            let dir = if request.dir == "." {
                match session.home().await {
                    Ok(home) => home,
                    Err(err) => return Listed::failed(err),
                }
            } else {
                request.dir.clone()
            };
            // The directory's own attributes come even when listing it is refused (`/root`).
            let (entries, stat) = tokio::join!(session.list(&dir), session.stat_named(&dir));
            Listed { dir: Some(dir), entries, stat: stat.ok() }
        });
        self.listing_abort = Some(join.abort_handle());
        self.listing = Some(cx.spawn(async move |this, cx| {
            let result =
                join.await.unwrap_or_else(|err| Listed::failed(SftpError::Protocol(format!("任务异常终止：{err}"))));
            this.update(cx, |this, cx| {
                let listed = began.elapsed();
                let summary =
                    result.entries.as_ref().ok().zip(result.dir.clone()).map(|(entries, dir)| (dir, entries.len()));
                let before = this.model.cwd().to_string();
                let applying = Instant::now();
                if this.model.apply_listed(id, result) == Applied::SessionLost {
                    cx.emit(FilesEvent::SessionLost(used));
                }
                if let Some((dir, count)) = summary {
                    log::info!(
                        "文件面板：{dir} 共 {count} 项，列目录 {} ms，整理 {} ms",
                        listed.as_millis(),
                        applying.elapsed().as_millis()
                    );
                }
                #[cfg(debug_assertions)]
                if let Some(name) = this.demo_select.take()
                    && let Some(index) = (0..this.model.row_count())
                        .find(|i| matches!(this.model.row(*i), Some(Row::Entry(e)) if e.name == name))
                {
                    this.model.select_row(index);
                    this.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                }
                if this.model.cwd() != before {
                    this.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if matches!(self.model.link(), LinkStatus::Failed(_)) {
            cx.emit(FilesEvent::RetrySession);
            return;
        }
        let request = self.model.refresh();
        self.start(request, cx);
        cx.notify();
    }

    fn go_to(&mut self, dir: &str, cx: &mut Context<Self>) {
        let request = self.model.go_to(dir);
        self.start(request, cx);
        cx.notify();
    }

    fn go_parent(&mut self, cx: &mut Context<Self>) {
        let request = self.model.go_parent();
        self.start(request, cx);
        cx.notify();
    }

    fn open_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let request = self.model.open_row(index);
        self.start(request, cx);
        cx.notify();
    }

    fn select_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model.select_row(index);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        let request = self.model.set_follow(on);
        self.start(request, cx);
        cx.notify();
    }

    fn apply_chmod(&mut self, cx: &mut Context<Self>) {
        let (Some((path, mode)), Some(session)) = (self.model.chmod_request(), self.session.clone()) else { return };
        let used = session.clone();
        let join = self.runtime.spawn(async move { session.chmod(&path, mode).await });
        self.chmod_abort = Some(join.abort_handle());
        self.chmod = Some(cx.spawn(async move |this, cx| {
            let result = join.await.unwrap_or_else(|err| Err(SftpError::Protocol(format!("任务异常终止：{err}"))));
            this.update(cx, |this, cx| {
                if result.as_ref().is_err_and(SftpError::is_session_lost) {
                    this.lose_session(used, cx);
                    return;
                }
                let request = this.model.chmod_done(result);
                this.start(request, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// An operation found `session` unusable: the panel waits for a new one, and the owner is asked
    /// to open it.
    fn lose_session(&mut self, session: Arc<SftpSession>, cx: &mut Context<Self>) {
        self.model.session_lost();
        cx.emit(FilesEvent::SessionLost(session));
        cx.notify();
    }

    // ---- actions -----------------------------------------------------------------------------------------

    fn on_previous(&mut self, _: &FilesPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.model.move_selection(-1) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn on_next(&mut self, _: &FilesNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.model.move_selection(1) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn on_open(&mut self, _: &FilesOpen, _: &mut Window, cx: &mut Context<Self>) {
        let request = self.model.open_selected();
        self.start(request, cx);
        cx.notify();
    }

    fn on_parent(&mut self, _: &FilesParent, _: &mut Window, cx: &mut Context<Self>) {
        self.go_parent(cx);
    }

    // ---- drawing ---------------------------------------------------------------------------------------------

    fn button(id: &'static str, label: &'static str, primary: bool, p: &Palette) -> Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(32.))
            .px(px(if primary { 14. } else { 12. }))
            .rounded(px(10.))
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(if primary { p.acc } else { p.card2 })
            .text_color(if primary { white() } else { p.tx })
            .child(label)
    }

    /// A button that is there but cannot be used yet.
    fn soon(id: &'static str, label: &'static str, primary: bool, p: &Palette) -> impl IntoElement {
        Self::button(id, label, primary, p)
            .opacity(0.45)
            .cursor_not_allowed()
            .tooltip(|window, cx| Tooltip::new("下一步提供").build(window, cx))
    }

    fn render_breadcrumbs(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let crumbs = self.model.crumbs();
        let last = crumbs.len().saturating_sub(1);
        let piece = |id: (&'static str, usize), label: String, dir: String, strong: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(px(4.))
                .py(px(2.))
                .rounded(px(6.))
                .cursor_pointer()
                .font_weight(if strong { FontWeight::BOLD } else { FontWeight::MEDIUM })
                .text_color(if strong { p.tx } else { p.tx2 })
                .hover(|d| d.bg(p.card2))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.go_to(&dir, cx)))
        };
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .font_family(MONO_FONT)
            .text_size(px(13.))
            .child(piece(("crumb", 0), "/".into(), "/".into(), crumbs.is_empty(), cx))
            .children(crumbs.into_iter().enumerate().map(|(i, (name, dir))| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(piece(("crumb", i + 1), name, dir, i == last, cx))
                    .child(div().text_color(p.tx3).child("/"))
            }))
    }

    fn render_follow(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.model.follow();
        let panel = cx.entity();
        let hint = (!self.model.osc7_seen()).then(|| {
            let panel_for_open = panel.clone();
            let (tx2, card2) = (p.tx2, p.card2);
            Popover::new("osc7-hint")
                .anchor(Anchor::TopRight)
                .open(self.hint_open)
                .on_open_change(move |open, _, cx| {
                    panel_for_open.update(cx, |this, cx| {
                        this.hint_open = *open;
                        cx.notify();
                    });
                })
                .trigger(Button::new("osc7-hint-trigger").ghost().small().label("终端未上报目录"))
                .content(move |_, _, _| {
                    let snippet = |id: &'static str, shell: &'static str, text: &'static str| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().text_size(px(12.)).text_color(tx2).child(shell))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .px(px(10.))
                                            .py(px(7.))
                                            .rounded(px(8.))
                                            .bg(card2)
                                            .font_family(MONO_FONT)
                                            .text_size(px(11.5))
                                            .child(text),
                                    )
                                    .child(Button::new(id).small().label("复制").on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                                    })),
                            )
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .w(px(560.))
                        .p(px(4.))
                        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("让终端报告当前目录"))
                        .child(div().text_size(px(12.)).text_color(tx2).child(
                            "远程 shell 默认不上报目录。把下面一行加进 ~/.bashrc 或 ~/.zshrc，面板就能跟随终端所在目录。",
                        ))
                        .child(snippet("copy-bash", "bash", BASH_SNIPPET))
                        .child(snippet("copy-zsh", "zsh", ZSH_SNIPPET))
                })
        });
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(p.tx2)
            .children(hint)
            .child("跟随终端目录 · OSC 7")
            .child(
                div()
                    .id("follow-switch")
                    .flex()
                    .flex_none()
                    .items_center()
                    .w(px(42.))
                    .h(px(24.))
                    .p(px(2.))
                    .rounded(px(12.))
                    .cursor_pointer()
                    .bg(if on { p.ok } else { p.card3 })
                    .when(on, |d| d.justify_end())
                    .child(div().size(px(20.)).rounded_full().bg(white()).shadow(vec![gpui::BoxShadow {
                        color: gpui::black().opacity(0.2),
                        offset: gpui::point(px(0.), px(1.)),
                        blur_radius: px(3.),
                        spread_radius: px(0.),
                        inset: false,
                    }]))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_follow(!on, cx))),
            )
    }

    fn render_top_bar(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .pl(px(18.))
            .pr(px(14.))
            .pt(px(12.))
            .pb(px(10.))
            .child(self.render_breadcrumbs(p, cx))
            .child(self.render_follow(p, cx))
            .child(Self::soon("new-folder", "新建文件夹", false, p))
            .child(Self::soon("download", "下载", false, p))
            .child(Self::soon("upload", "上传", true, p))
            .child(
                div()
                    .id("refresh")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(32.))
                    .rounded(px(10.))
                    .bg(p.card2)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.card3))
                    .child(Icon::new(Lucide::RefreshCw).size(px(15.)).text_color(p.tx2))
                    .tooltip(|window, cx| Tooltip::new("刷新").build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn header_cell(label: &'static str, width: Option<f32>, right: bool) -> gpui::Div {
        let cell = div().child(label);
        let cell = match width {
            Some(w) => cell.w(px(w)).flex_none(),
            None => cell.flex_1().min_w_0(),
        };
        if right { cell.text_right() } else { cell }
    }

    fn render_header(&self, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(18.))
            .py(px(6.))
            .text_size(px(11.5))
            .text_color(p.tx2)
            .border_b_1()
            .border_color(p.line)
            .child(Self::header_cell("名称", None, false))
            .child(Self::header_cell("大小", Some(84.), true))
            .child(Self::header_cell("修改时间", Some(110.), false))
            .child(Self::header_cell("权限", Some(104.), false))
    }

    fn entry_icon(entry: &Entry, p: &Palette) -> Icon {
        match entry.kind {
            EntryKind::Dir => Icon::new(Lucide::Folder).text_color(p.acc),
            EntryKind::Symlink if entry.opens_as_dir() => Icon::new(Lucide::Link).text_color(p.acc),
            EntryKind::Symlink => Icon::new(Lucide::Link).text_color(p.tx2),
            _ => Icon::new(Lucide::File).text_color(p.tx3),
        }
        .size(px(17.))
    }

    fn render_row(&self, index: usize, p: &Palette, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let row = self.model.row(index)?;
        let selected = self.model.selected_index() == Some(index);
        let (icon, name, note, size, time, perms) = match row {
            Row::Parent => (
                Icon::new(Lucide::Folder).size(px(17.)).text_color(p.acc),
                "..".to_string(),
                None,
                String::new(),
                String::new(),
                String::new(),
            ),
            Row::Entry(entry) => {
                let note = entry.link.as_ref().map(|link| {
                    if link.resolved.is_none() {
                        (format!("→ {}（目标不存在）", link.target), p.err)
                    } else {
                        (format!("→ {}", link.target), p.tx3)
                    }
                });
                let size = if entry.kind == EntryKind::File { format_size(entry.size) } else { "—".to_string() };
                let perms = entry.mode.map_or_else(String::new, mode_string);
                (Self::entry_icon(entry, p), entry.name.clone(), note, size, mtime_text(entry.modified), perms)
            }
        };
        let dangling = matches!(row, Row::Entry(e) if e.is_dangling());
        Some(
            div()
                .id(("file-row", index))
                .flex()
                .w_full()
                .items_center()
                .gap(px(12.))
                .h(px(38.))
                .px(px(10.))
                .rounded(px(10.))
                .text_size(px(13.))
                .cursor_default()
                .when(selected, |d| d.bg(p.accs))
                .when(!selected, |d| d.hover(|d| d.bg(p.card2)))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap(px(10.))
                        .child(icon)
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if selected {
                                    p.acc
                                } else if dangling {
                                    p.err
                                } else {
                                    p.tx
                                })
                                .child(name),
                        )
                        .children(note.map(|(text, color)| {
                            div().min_w_0().truncate().text_size(px(11.5)).text_color(color).child(text)
                        })),
                )
                .child(div().w(px(84.)).flex_none().text_right().text_size(px(12.)).text_color(p.tx2).child(size))
                .child(div().w(px(110.)).flex_none().text_size(px(12.)).text_color(p.tx2).child(time))
                .child(
                    div()
                        .w(px(104.))
                        .flex_none()
                        .text_size(px(12.))
                        .font_family(MONO_FONT)
                        .text_color(p.tx2)
                        .child(perms),
                )
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.select_row(index, window, cx);
                    if event.click_count() >= 2 {
                        this.open_row(index, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// A centered message in the list area, optionally with a button.
    fn message(
        &self,
        text: impl Into<SharedString>,
        color: Hsla,
        button: Option<Stateful<gpui::Div>>,
        icon: Option<Lucide>,
    ) -> gpui::AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .px(px(24.))
            .py(px(40.))
            .text_size(px(13.))
            .text_color(color)
            .children(icon.map(|icon| Icon::new(icon).size(px(26.)).text_color(color)))
            .child(div().max_w(px(520.)).text_center().child(text.into()))
            .children(button)
            .into_any_element()
    }

    fn render_list_area(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        match (self.model.link(), self.model.view()) {
            (LinkStatus::Disconnected, _) => {
                self.message("连接已断开，重连后自动刷新", p.warn, None, Some(Lucide::TriangleAlert))
            }
            (LinkStatus::Failed(reason), _) => {
                let retry = Self::button("retry", "刷新", false, p)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
                self.message(reason.clone(), p.err, Some(retry), Some(Lucide::CircleAlert))
            }
            (_, View::Loading) => self.message("加载中…", p.tx2, None, None),
            (_, View::Error(reason)) => {
                let back = self.model.parent_dir().is_some().then(|| {
                    Self::button("back", "返回上级", false, p)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.go_parent(cx)))
                });
                self.message(reason.clone(), p.err, back, Some(Lucide::CircleAlert))
            }
            (_, View::List) if self.model.entries().is_empty() => {
                let parent = if self.model.has_parent() { self.render_row(0, p, cx) } else { None };
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(8.))
                    .py(px(4.))
                    .children(parent)
                    .child(
                        div()
                            .py(px(40.))
                            .text_center()
                            .text_size(px(13.))
                            .text_color(p.tx2)
                            .child("空目录 · 拖入文件或新建文件夹"),
                    )
                    .into_any_element()
            }
            (_, View::List) => {
                let list = uniform_list(
                    "file-rows",
                    self.model.row_count(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let began = Instant::now();
                        let p = cx.palette().clone();
                        let rows = range.clone().filter_map(|index| this.render_row(index, &p, cx)).collect::<Vec<_>>();
                        log::debug!("文件面板：构建第 {range:?} 行用时 {} µs", began.elapsed().as_micros());
                        rows
                    }),
                )
                .track_scroll(&self.scroll)
                .size_full();
                div().flex_1().min_h_0().px(px(8.)).py(px(4.)).child(list).into_any_element()
            }
        }
    }

    fn render_files_card(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = self.model.banner().map(|text| {
            div()
                .mx(px(18.))
                .mb(px(8.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(p.errs)
                .text_size(px(12.))
                .text_color(p.err)
                .child(text.to_string())
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .rounded(crate::theme::CARD_RADIUS)
            .bg(p.card)
            .shadow(card_shadow(p))
            .overflow_hidden()
            .child(self.render_top_bar(p, cx))
            .children(banner)
            .child(self.render_header(p))
            .child(self.render_list_area(p, cx))
            .child(
                div()
                    .px(px(18.))
                    .pt(px(10.))
                    .pb(px(12.))
                    .text_size(px(12.))
                    .text_color(p.tx2)
                    .border_t_1()
                    .border_color(p.line)
                    .child("拖拽文件到此处上传 · 双击进入目录或编辑文件 · 右键更多操作"),
            )
    }

    fn info_row(label: &'static str, value: String, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .gap(px(12.))
            .child(div().w(px(30.)).flex_none().text_color(p.tx2).child(label))
            .child(div().flex_1().min_w_0().child(value))
    }

    fn render_details(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let subject = self.model.subject();
        let dash = || "—".to_string();
        let attributes = |entry: &Entry| {
            let owner = if entry.owner_display().is_empty() { dash() } else { entry.owner_display() };
            (mtime_text(entry.modified), owner)
        };
        let (title, kind, size, time, owner) = match subject {
            Subject::Entry(entry) => {
                let kind = match (&entry.kind, &entry.link) {
                    (EntryKind::Dir, _) => "目录".to_string(),
                    (EntryKind::File, _) => "文件".to_string(),
                    (EntryKind::Symlink, Some(link)) if link.resolved.is_none() => {
                        format!("符号链接 → {}（目标不存在）", link.target)
                    }
                    (EntryKind::Symlink, Some(link)) => format!("符号链接 → {}", link.target),
                    (EntryKind::Symlink, None) => "符号链接".to_string(),
                    (EntryKind::Other, _) => "特殊文件".to_string(),
                };
                let size = if entry.kind == EntryKind::File { format_size(entry.size) } else { dash() };
                let (time, owner) = attributes(entry);
                (entry.name.clone(), kind, size, time, owner)
            }
            // Nothing selected: the directory itself, with what `stat` said about it.
            Subject::Dir(stat) => {
                let name = path::file_name(self.model.cwd()).unwrap_or("/").to_string();
                let (time, owner) = stat.map_or_else(|| (dash(), dash()), attributes);
                (name, "目录".into(), dash(), time, owner)
            }
            Subject::Parent => ("..".into(), "上级目录".into(), dash(), dash(), dash()),
        };
        let editable_file = matches!(subject, Subject::Entry(entry) if entry.kind == EntryKind::File);
        let grid_enabled = self.model.chmod_enabled();
        let draft = self.model.draft();
        let can_apply = self.model.chmod_request().is_some();
        let rows = ["所有者", "用户组", "其他"];
        let columns = ["读", "写", "执行"];
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(268.))
            .gap(px(12.))
            .p(px(18.))
            .rounded(crate::theme::CARD_RADIUS)
            .bg(p.card)
            .shadow(card_shadow(p))
            .child(div().text_size(px(17.)).font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(12.5))
                    .child(Self::info_row("类型", kind, p))
                    .child(Self::info_row("大小", size, p))
                    .child(Self::info_row("修改", time, p))
                    .child(Self::info_row("属主", owner, p)),
            )
            .child(div().h(px(1.)).bg(p.line))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("权限"))
                    .child(
                        div().font_family(MONO_FONT).text_size(px(12.5)).text_color(p.tx2).child(
                            draft.map_or_else(|| "—".to_string(), |d| format!("{} · {}", d.octal(), d.symbolic())),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .opacity(if draft.is_some() && grid_enabled { 1.0 } else { 0.45 })
                    .children(rows.iter().enumerate().map(|(row, label)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().w(px(48.)).text_size(px(12.)).text_color(p.tx2).child(*label))
                            .children(columns.iter().enumerate().map(|(column, name)| {
                                let on = draft.is_some_and(|d| d.is_set(row, column));
                                div()
                                    .id(("perm", row * 3 + column))
                                    .flex()
                                    .flex_1()
                                    .items_center()
                                    .justify_center()
                                    .h(px(30.))
                                    .rounded(px(9.))
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .bg(if on { p.acc } else { p.card2 })
                                    .text_color(if on { white() } else { p.tx2 })
                                    .when(draft.is_some() && grid_enabled, |d| {
                                        d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                            this.model.toggle_perm(row, column);
                                            cx.notify();
                                        }))
                                    })
                                    .child(*name)
                            }))
                    })),
            )
            .child(
                Self::button("apply-chmod", "应用 chmod", false, p)
                    .h(px(34.))
                    .rounded(px(11.))
                    .when(!can_apply, |d| d.opacity(0.5))
                    .when(can_apply, |d| {
                        d.cursor_pointer().on_click(cx.listener(|this, _, _, cx| this.apply_chmod(cx)))
                    }),
            )
            .children(
                self.model
                    .chmod_error()
                    .map(|error| div().text_size(px(12.)).text_color(p.err).child(error.to_string())),
            )
            .child(div().flex_1())
            .children(editable_file.then(|| {
                Self::button("open-editor", "用内置编辑器打开", true, p)
                    .h(px(38.))
                    .rounded(px(12.))
                    .text_size(px(13.))
                    .opacity(0.45)
                    .cursor_not_allowed()
                    .tooltip(|window, cx| Tooltip::new("内置编辑器稍后提供").build(window, cx))
            }))
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .id("files-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_previous))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_parent))
            .size_full()
            .flex()
            .gap(px(10.))
            .child(self.render_files_card(&p, cx))
            .child(self.render_details(&p, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::Keystroke;

    use super::*;

    #[test]
    fn the_panels_shortcuts_are_valid_keystrokes_in_its_own_context() {
        for key in KEYS {
            assert!(Keystroke::parse(key).is_ok(), "{key}");
        }
        assert_eq!(KEYS, ["up", "down", "enter", "cmd-up"]);
        assert_eq!(CONTEXT, "FilesPanel");
    }

    #[test]
    fn the_snippets_are_the_documented_ones() {
        assert_eq!(BASH_SNIPPET, r#"PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'"#);
        assert_eq!(ZSH_SNIPPET, r#"precmd() { printf '\e]7;file://%s%s\a' "$HOST" "$PWD" }"#);
    }
}
