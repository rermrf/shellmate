//! Broadcast and split workspace integration. Session IDs remain stable across tab selection.
use super::*;
use crate::productivity::{
    broadcast::{BroadcastBar, BroadcastView, SubmitCommand},
    multi::{self, Axis, Pane, SendPlan, SendRequest, SessionRow, Status, Target},
    session_menu::{self, SessionMenu},
};
use shellmate_store::Snippet;

pub(super) struct OpenSessionMenu {
    pub kind: session_menu::Kind,
    pub position: gpui::Point<gpui::Pixels>,
    pub view: Entity<SessionMenu>,
    pub _subscription: Subscription,
}
impl Workspace {
    pub(super) fn main_id(&self) -> Option<usize> {
        self.active.and_then(|i| self.sessions.get(i)).map(|s| s.id)
    }
    pub(super) fn focused_session(&self) -> Option<&Session> {
        let main = self.active.and_then(|i| self.sessions.get(i))?;
        let id = self.split.focused(main.id, main.panel == Panel::Terminal);
        self.sessions.iter().find(|s| s.id == id)
    }
    pub(super) fn session_rows(&self, cx: &App) -> Vec<SessionRow> {
        self.sessions
            .iter()
            .map(|s| SessionRow {
                id: s.id,
                title: s.title.to_string(),
                local: s.ssh().is_none(),
                status: if self.command_available(Some(s.id), cx) {
                    Status::Connected
                } else {
                    match s.ssh().map(|s| s.status) {
                        Some(ssh::SshStatus::Connecting) => Status::Connecting,
                        Some(ssh::SshStatus::Reconnecting(_)) => Status::Reconnecting,
                        Some(ssh::SshStatus::Failed) => Status::Failed,
                        _ => Status::Offline,
                    }
                },
            })
            .collect()
    }
    pub(super) fn sync_productivity(&mut self, cx: &mut Context<Self>) {
        let sessions = self.session_rows(cx);
        let view = BroadcastView {
            state: self.broadcast.clone(),
            sessions,
        };
        if let Some(menu) = &self.session_menu {
            let rows = view
                .sessions
                .iter()
                .filter(|s| menu.kind == session_menu::Kind::Tabs || Some(s.id) != self.main_id())
                .cloned()
                .collect::<Vec<_>>();
            let active = if menu.kind == session_menu::Kind::Tabs {
                self.main_id()
            } else {
                self.split.other
            };
            menu.view.update(cx, |m, cx| {
                if m.rows != rows || m.active != active {
                    m.rows = rows;
                    m.active = active;
                    cx.notify();
                }
            });
        }
        let count = view.state.targets(&view.sessions).len();
        let chip = view.state.chip(&view.sessions);
        self.commands.update(cx, |d, cx| {
            if d.broadcast != view {
                d.broadcast = view;
                cx.notify();
            }
        });
        self.broadcast_bar.update(cx, |b, cx| b.targets(count, chip, cx));
    }
    pub(super) fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let history = self
            .store
            .borrow()
            .history("", None, shellmate_store::HISTORY_LIMIT)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.command)
            .collect();
        let snippets = self
            .commands
            .read(cx)
            .groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.command.clone()))
            .collect();
        self.broadcast_bar
            .update(cx, |b, cx| b.sources(history, snippets, cx));
    }
    pub(super) fn set_broadcast(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.session_rows(cx);
        self.broadcast
            .enable(enabled, self.focused_session().map(|s| s.id), &rows);
        self.commands_open = true;
        self.sync_productivity(cx);
        self.refresh_suggestions(cx);
        if enabled {
            self.broadcast_bar.update(cx, |b, cx| b.focus(window, cx));
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }
    pub(super) fn on_broadcast_submit(
        &mut self,
        _: &Entity<BroadcastBar>,
        event: &SubmitCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_broadcast(&event.0, window, cx);
    }
    pub(super) fn submit_broadcast(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim();
        let rows = self.session_rows(cx);
        match multi::broadcast_plan(&self.broadcast, &rows, text) {
            SendPlan::Disabled => {}
            SendPlan::Confirm(ids) => {
                let request = self.send_request(ids, true, true);
                self.open_confirmation(
                    Snippet {
                        name: "广播输入".into(),
                        command: text.into(),
                    },
                    request,
                    window,
                    cx,
                );
            }
            SendPlan::Direct(ids) => {
                let request = self.send_request(ids, true, true);
                if let Err(error) = self.send_reviewed(&request, text, window, cx) {
                    window.push_notification(Notification::error(error), cx);
                }
            }
        }
    }
    pub(super) fn send_request(&self, ids: Vec<usize>, broadcast: bool, clear_input: bool) -> SendRequest {
        let targets = ids
            .into_iter()
            .filter_map(|id| self.sessions.iter().find(|s| s.id == id))
            .map(|s| {
                let production = s.ssh().is_some_and(|ssh| {
                    let group = ssh
                        .host
                        .group
                        .as_ref()
                        .and_then(|id| self.book.group(id))
                        .map_or("", |g| g.name.as_str());
                    crate::productivity::model::production(group, &ssh.host.tags)
                });
                Target {
                    id: s.id,
                    title: s.title.to_string(),
                    production,
                }
            })
            .collect();
        SendRequest {
            targets,
            broadcast,
            clear_input,
        }
    }
    pub(super) fn open_confirmation(
        &mut self,
        command: Snippet,
        request: SendRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = crate::productivity::panel::Confirmation::open(command, request, &cx.entity(), window, cx);
        #[cfg(debug_assertions)]
        {
            self.review_confirmation = Some(body.downgrade());
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = body;
        }
    }
    pub(super) fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(main) = self.main_id() else { return };
        if self
            .active
            .and_then(|i| self.sessions.get(i))
            .is_none_or(|s| s.panel != Panel::Terminal)
        {
            return;
        }
        if !self
            .split
            .toggle(main, &self.session_rows(cx), &self.recent_sessions)
        {
            window.push_notification(Notification::info("没有可用于分屏的其他会话"), cx);
            return;
        }
        self.session_menu = None;
        self.update_pane_radii(cx);
        self.focus_active(window, cx);
        cx.notify();
    }
    pub(super) fn update_pane_radii(&self, cx: &mut Context<Self>) {
        let main = self.main_id();
        let other = self.split.other;
        for s in &self.sessions {
            let radius = if other.is_some() && (Some(s.id) == main || Some(s.id) == other) {
                px(0.)
            } else {
                CARD_RADIUS
            };
            s.view.update(cx, |v, cx| {
                if v.top_radius != radius {
                    v.top_radius = radius;
                    cx.notify();
                }
            });
        }
    }
    pub(super) fn focus_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        self.split.focus = pane;
        if let Some(id) = self.focused_session().map(|s| s.id) {
            self.touch_session(id);
        }
        self.focus_active(window, cx);
        cx.notify();
    }
    pub(super) fn note_terminal_focus(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.split.other == Some(id) {
            self.split.focus = Pane::Other;
        } else if self.main_id() == Some(id) {
            self.split.focus = Pane::Main;
        }
        self.touch_session(id);
        cx.notify();
    }
    pub(super) fn touch_session(&mut self, id: usize) {
        self.recent_sessions.retain(|s| *s != id);
        self.recent_sessions.insert(0, id);
    }
    pub(super) fn flip_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.split.flip();
        self.focus_active(window, cx);
        cx.notify();
    }
    pub(super) fn open_session_menu(
        &mut self,
        kind: session_menu::Kind,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_menu.as_ref().is_some_and(|m| m.kind == kind) {
            self.session_menu = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        let main = self.main_id();
        let rows = self
            .session_rows(cx)
            .into_iter()
            .filter(|s| kind == session_menu::Kind::Tabs || Some(s.id) != main)
            .collect();
        let active = if kind == session_menu::Kind::Tabs {
            main
        } else {
            self.split.other
        };
        let view = cx.new(|cx| SessionMenu::new(rows, active, window, cx));
        let subscription = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
            this.session_menu = None;
            match event {
                session_menu::Event::Close => this.focus_active(window, cx),
                session_menu::Event::Pick(id) => {
                    if kind == session_menu::Kind::Tabs {
                        if let Some(ix) = this.sessions.iter().position(|s| s.id == *id) {
                            this.activate(ix, window, cx);
                        }
                    } else if let Some(main) = this.main_id() {
                        this.split.choose(main, *id, &this.session_rows(cx));
                        this.update_pane_radii(cx);
                        this.focus_pane(Pane::Other, window, cx);
                    }
                }
            }
            cx.notify();
        });
        self.session_menu = Some(OpenSessionMenu {
            kind,
            position,
            view,
            _subscription: subscription,
        });
        cx.notify();
    }
    pub(super) fn render_panes(&self, main: &Session, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(other) = self
            .split
            .other
            .and_then(|id| self.sessions.iter().find(|s| s.id == id && s.id != main.id))
        else {
            return div()
                .flex_1()
                .min_h_0()
                .child(main.view.clone())
                .into_any_element();
        };
        let p = cx.palette().clone();
        let pane = |session: &Session, which: Pane, cx: &mut Context<Self>| {
            let focused = self.split.focus == which;
            let size = session.terminal.read(cx).size();
            let mut header = div()
                .h(px(32.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .border_b_1()
                .border_color(p.line)
                .when(focused, |d| d.bg(p.accs))
                .child(dot(session_dot(session, &p), 6.));
            if which == Pane::Other {
                header = header.child(
                    div()
                        .id("split-session-title")
                        .cursor_pointer()
                        .text_size(px(12.))
                        .child(format!("{} ▾", session.title))
                        .on_click(cx.listener(|this, event: &gpui::ClickEvent, w, cx| {
                            cx.stop_propagation();
                            this.open_session_menu(session_menu::Kind::Split, event.position(), w, cx);
                        })),
                );
            } else {
                header = header.child(div().text_size(px(12.)).child(session.title.clone()));
            }
            header = header
                .child(
                    div()
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.))
                        .text_color(p.tx3)
                        .child(format!("{}×{}", size.cols, size.rows)),
                )
                .child(div().flex_1())
                .when(focused, |d| {
                    d.child(div().text_size(px(11.5)).text_color(p.acc).child("输入焦点"))
                });
            if which == Pane::Other {
                header = header.child(
                    div()
                        .id("split-flip")
                        .cursor_pointer()
                        .rounded(px(6.))
                        .bg(p.card2)
                        .px(px(7.))
                        .py(px(3.))
                        .text_size(px(11.5))
                        .text_color(p.tx2)
                        .child(if self.split.axis == Axis::Horizontal {
                            "改为上下"
                        } else {
                            "改为左右"
                        })
                        .on_click(cx.listener(|this, _, w, cx| {
                            cx.stop_propagation();
                            this.flip_split(w, cx);
                        })),
                );
            }
            div()
                .id(("pane", session.id))
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, w, cx| this.focus_pane(which, w, cx)),
                )
                .child(header)
                .child(div().flex_1().min_h_0().child(session.view.clone()))
        };
        div()
            .rounded_t(CARD_RADIUS)
            .overflow_hidden()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .when(self.split.axis == Axis::Vertical, |d| d.flex_col())
            .child(pane(main, Pane::Main, cx))
            .child(
                div()
                    .flex_none()
                    .bg(p.line)
                    .when(self.split.axis == Axis::Horizontal, |d| d.w(px(1.)).h_full())
                    .when(self.split.axis == Axis::Vertical, |d| d.h(px(1.)).w_full()),
            )
            .child(pane(other, Pane::Other, cx))
            .into_any_element()
    }
}
