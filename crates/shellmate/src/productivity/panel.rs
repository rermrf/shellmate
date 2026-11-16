//! Palette, history picker, quick-command drawer, and editable confirmation/management dialogs.
use super::model::{self, Choice, Item, SessionItem};
use super::{broadcast::BroadcastView, multi::SendRequest};
use crate::{
    dialogs::{self, DialogBody, Tone, button, field, hint, stack},
    theme::{ActivePalette, MONO_FONT},
    ui::card_shadow,
};
use gpui::{
    App, Context, Entity, EventEmitter, Focusable, FontWeight, KeyBinding, ScrollHandle, SharedString,
    Subscription, Window, actions,
    assets::IconName as Lucide,
    component::{
        Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    div,
    prelude::*,
    px,
};
use shellmate_core::HostBook;
use shellmate_store::{HistoryEntry, HostStore, Snippet, SnippetGroup};
use std::{cell::RefCell, rc::Rc};

actions!(productivity, [PickUp, PickDown, PickConfirm, PickClose]);
pub fn init(cx: &mut App) {
    for context in ["ProductivityPicker", "ProductivityPicker > Input"] {
        cx.bind_keys([
            KeyBinding::new("up", PickUp, Some(context)),
            KeyBinding::new("down", PickDown, Some(context)),
            KeyBinding::new("ctrl-r", PickDown, Some(context)),
            KeyBinding::new("enter", PickConfirm, Some(context)),
            KeyBinding::new("escape", PickClose, Some(context)),
        ]);
    }
}

pub enum Event {
    Choose(Choice),
    Close,
    Manage,
    Samples,
    Send(Snippet),
    Broadcast(bool),
    CheckSession(usize),
}

pub struct PaletteData {
    pub book: Rc<HostBook>,
    pub sessions: Vec<SessionItem>,
    pub active: Option<usize>,
    pub snippets: Vec<SnippetGroup>,
    pub sidebar_open: bool,
    pub split: bool,
}
enum Source {
    Palette(PaletteData),
    History {
        store: Rc<RefCell<HostStore>>,
        host: Option<String>,
        current: bool,
    },
}
pub struct Picker {
    source: Source,
    input: Entity<InputState>,
    items: Vec<Item>,
    history: Vec<HistoryEntry>,
    selected: usize,
    error: Option<String>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<Event> for Picker {}
impl Picker {
    pub fn palette(data: PaletteData, query: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(Source::Palette(data), query, window, cx)
    }
    pub fn history(
        store: Rc<RefCell<HostStore>>,
        host: Option<String>,
        current: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(Source::History { store, host, current }, "", window, cx)
    }
    fn new(source: Source, query: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = if matches!(source, Source::Palette(_)) {
            "搜索主机、会话、命令，或输入 user@host 快速连接"
        } else {
            "输入关键词，搜索所有会话的历史命令"
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(query.to_string())
        });
        let subscriptions = vec![cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.selected = 0;
                this.refresh(cx);
            }
        })];
        let mut this = Self {
            source,
            input,
            items: vec![],
            history: vec![],
            selected: 0,
            error: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this.focus(window, cx);
        this
    }
    pub fn is_history(&self) -> bool {
        matches!(self.source, Source::History { .. })
    }
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value();
        self.error = None;
        match &self.source {
            Source::Palette(data) => {
                self.items = model::palette(
                    &query,
                    &data.book,
                    &data.sessions,
                    data.active,
                    &data.snippets,
                    data.sidebar_open,
                    data.split,
                )
            }
            Source::History { store, host, current } => match store.borrow().history(
                &query,
                if *current {
                    Some(host.as_deref().unwrap_or(""))
                } else {
                    None
                },
                8,
            ) {
                Ok(rows) => self.history = rows,
                Err(_) => {
                    self.history.clear();
                    self.error = Some("无法读取命令历史".into());
                }
            },
        }
        self.scroll.scroll_to_item(0);
        cx.notify();
    }
    fn count(&self) -> usize {
        if self.is_history() {
            self.history.len()
        } else {
            self.items.len()
        }
    }
    fn reveal(&self) {
        let headers = if self.is_history() {
            0
        } else {
            self.items
                .iter()
                .take(self.selected + 1)
                .enumerate()
                .filter(|(i, item)| *i == 0 || self.items[*i - 1].group != item.group)
                .count()
        };
        self.scroll.scroll_to_item(self.selected + headers);
    }
    fn pick(&mut self, cx: &mut Context<Self>) {
        let choice = if self.is_history() {
            self.history.get(self.selected).map(|h| {
                Choice::Command(Snippet {
                    name: "历史命令".into(),
                    command: h.command.clone(),
                })
            })
        } else {
            self.items.get(self.selected).map(|i| i.choice.clone())
        };
        if let Some(choice) = choice {
            cx.emit(Event::Choose(choice));
        }
    }
}
impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let history = self.is_history();
        let mut header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(if history { 14. } else { 18. }))
            .h(px(49.))
            .flex_none()
            .border_b_1()
            .border_color(p.line);
        if history {
            header = header.child(
                div()
                    .flex_none()
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .text_color(p.tx2)
                    .child("(reverse-i-search)"),
            );
        } else {
            header = header.child(Icon::new(Lucide::Search).size(px(17.)).text_color(p.tx3));
        }
        header = header.child(
            div().flex_1().min_w_0().child(
                Input::new(&self.input)
                    .appearance(false)
                    .p_0()
                    .h(px(22.))
                    .text_size(px(if history { 13. } else { 15. })).when(history,|i|i.font_family(MONO_FONT)),
            ),
        );
        if let Source::History { current, .. } = &self.source {
            let selected = *current;
            header =
                header.child(
                    div()
                        .flex()
                        .p(px(2.))
                        .gap(px(2.))
                        .rounded(px(9.))
                        .bg(p.card3)
                        .children([(false, "全部会话"), (true, "当前主机")].into_iter().map(
                            |(value, label)| {
                                div()
                                    .id(("scope", usize::from(value)))
                                    .px(px(9.))
                                    .py(px(5.))
                                    .rounded(px(7.))
                                    .text_size(px(11.5)).line_height(px(13.)).h(px(24.)).font_weight(if selected==value{FontWeight::SEMIBOLD}else{FontWeight::MEDIUM})
                                    .text_color(if selected == value { p.tx } else { p.tx2 })
                                    .when(selected == value, |d| d.bg(p.card))
                                    .cursor_pointer()
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Source::History { current, .. } = &mut this.source {
                                            *current = value;
                                        }
                                        this.selected = 0;
                                        this.refresh(cx);
                                    }))
                            },
                        )),
                );
        }
        header = header.child(
            div()
                .id("picker-close")
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(p.tx2)
                .bg(p.card2)
                .rounded(px(6.))
                .px(px(7.))
                .py(px(3.))
                .child(if history { "×" } else { "esc" })
                .on_click(cx.listener(|_, _, _, cx| cx.emit(Event::Close))),
        );
        let mut rows = div()
            .id("picker-rows")
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .max_h(px(if history { 270. } else { 514. }))
            .when(history,|d|d.h(px(270.)).flex_none())
            .px(px(if history { 6. } else { 8. }))
            .pt(px(4.))
            .pb(px(if history { 6. } else { 8. }));
        let mut group = "";
        for index in 0..self.count() {
            let (title, detail, badge) = if history {
                let h = &self.history[index];
                (h.command.clone(), h.label.clone(), time_label(h.time))
            } else {
                let item = &self.items[index];
                if group != item.group {
                    rows = rows.child(
                        div()
                            .px(px(10.))
                            .pt(px(10.))
                            .pb(px(7.))
                            .text_size(px(11.5))
                            .line_height(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.tx2)
                            .child(item.group),
                    );
                    group = item.group;
                }
                (item.title.clone(), item.detail.clone(), item.hint.clone())
            };
            rows = rows.child(
                div()
                    .id(("picker-row", index))
                    .h(px(if history { 34. } else { 40. })).flex_none()
                    .px(px(if history { 10. } else { 12. }))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .rounded(px(if history { 9. } else { 11. }))
                    .cursor_pointer()
                    .when(index == self.selected, |d| d.bg(p.accs))
                    .text_color(if index == self.selected { p.acc } else { p.tx })
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered && !crate::prototype_fixture() {
                            this.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = index;
                        this.pick(cx);
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .when(history, |d| d.flex_1().font_family(MONO_FONT))
                            .text_size(px(if history { 12.5 } else { 13.5 }))
                            .font_weight(if history {
                                FontWeight::NORMAL
                            } else {
                                FontWeight::SEMIBOLD
                            })
                            .line_height(px(if history { 17. } else { 16. }))
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .when(!history, |d| d.flex_1().font_family(MONO_FONT))
                            .when(history, |d| d.bg(p.card2).px(px(7.)).rounded(px(6.)))
                            .text_size(px(if history { 11. } else { 12. }))
                            .text_color(p.tx2)
                            .truncate()
                            .child(detail),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .line_height(px(13.))
                            .text_color(p.tx2)
                            .when(history, |d| d.w(px(40.)).text_right())
                            .when(!history && !badge.is_empty(), |d| {
                                d.bg(p.card2).px(px(7.)).py(px(3.)).rounded(px(6.))
                            })
                            .child(badge),
                    ),
            );
        }
        if self.count() == 0 {
            rows = rows.child(
                div()
                    .py(px(24.))
                    .text_center()
                    .text_size(px(12.5))
                    .text_color(p.tx2)
                    .child(self.error.clone().unwrap_or_else(|| {
                        if history {
                            "没有匹配的历史命令".into()
                        } else {
                            "没有匹配的结果".into()
                        }
                    })),
            );
        }
        div()
            .id("productivity-picker")
            .occlude()
            .key_context("ProductivityPicker")
            .flex()
            .flex_col()
            .w_full()
            .rounded(px(if history { 16. } else { 22. }))
            .bg(p.card)
            .shadow(if history { vec![gpui::BoxShadow { color:gpui::rgba(0x0000004d).into(), offset:gpui::point(px(0.),px(16.)), blur_radius:px(44.), spread_radius:px(0.), inset:false }] } else { dialogs::modal_shadow(Some((p.line,1.))) })
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &PickUp, _, cx| {
                this.selected = this.selected.saturating_sub(1);
                this.reveal();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PickDown, _, cx| {
                this.selected = if this.is_history() {
                    (this.selected + 1) % this.count().max(1)
                } else {
                    (this.selected + 1).min(this.count().saturating_sub(1))
                };
                this.reveal();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PickConfirm, _, cx| this.pick(cx)))
            .on_action(cx.listener(|_, _: &PickClose, _, cx| cx.emit(Event::Close)))
            .child(header)
            .child(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .px(px(if history {14.} else {18.}))
                    .py(px(if history { 7. } else { 10. }))
                    .line_height(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_size(px(if history {11.}else{11.5}))
                    .text_color(p.tx2)
                    .child(if history {
                        "↑↓ 或 ⌃R 切换"
                    } else {
                        "↑↓ 选择"
                    })
                    .child(if history {
                        "↵ 预览后执行"
                    } else {
                        "↵ 执行"
                    })
                    .child("esc 关闭")
                    .when(history, |d| {
                        d.child(div().flex_1()).child("历史保存在本机，可在设置中关闭")
                    }),
            )
    }
}

fn time_label(time: i64) -> String {
    #[cfg(debug_assertions)]
    if crate::proto::requested() && time < crate::proto::timestamp("11-12 00:00") {
        return "昨天".into();
    }
    let time = time as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference valid values for the duration of localtime_r.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

pub struct Drawer {
    pub groups: Vec<SnippetGroup>,
    pub broadcast: BroadcastView,
}
impl EventEmitter<Event> for Drawer {}
impl Render for Drawer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut body = div()
            .id("commands-drawer")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .rounded(px(18.))
            .bg(p.card)
            .shadow(card_shadow(&p))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("快捷命令"),
                    )
                    .child(
                        Button::new("manage")
                            .h(px(26.))
                            .px(px(10.))
                            .rounded(px(8.))
                            .text_size(px(12.))
                            .label("管理")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Event::Manage))),
                    ),
            );
        if self.groups.is_empty() {
            body = body.child(
                div()
                    .py(px(30.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(hint("还没有快捷命令", &p))
                    .child(hint("保存常用命令，发送前可预览和修改。", &p))
                    .child(
                        Button::new("samples")
                            .label("添加示例命令")
                            .primary()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Event::Samples))),
                    ),
            );
        }
        for group in &self.groups {
            let mut section = div().flex_none().flex().flex_col().gap(px(4.)).child(
                div()
                    .px(px(4.))
                    .pb(px(2.))
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .text_color(p.tx2)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(group.name.clone()),
            );
            for (index, command) in group.items.iter().enumerate() {
                let send = command.clone();
                section = section.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .p(px(8.))
                        .pl(px(10.))
                        .rounded(px(12.))
                        .bg(p.card2)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .line_height(px(18.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(command.name.clone()),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(11.5))
                                        .line_height(px(14.))
                                        .text_color(p.tx2)
                                        .truncate()
                                        .child(command.command.clone()),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("send-{}-{index}", group.name)))
                                .cursor_pointer()
                                .px(px(12.))
                                .h(px(28.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(9.))
                                .bg(p.accs)
                                .text_color(p.acc)
                                .text_size(px(12.))
                                .font_weight(FontWeight::BOLD)
                                .child("发送")
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(Event::Send(send.clone())))),
                        ),
                );
            }
            body = body.child(section);
        }
        let mut broadcast = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .pb(px(14.))
            .rounded(px(18.))
            .bg(p.card)
            .shadow(card_shadow(&p))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .line_height(px(21.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("广播输入"),
                            )
                            .child(
                                div()
                                    .mt(px(2.))
                                    .text_size(px(12.))
                                    .line_height(px(17.))
                                    .text_color(p.tx2)
                                    .child("同一条命令同时发往勾选的会话"),
                            ),
                    )
                    .child(
                        crate::ui::Toggle::new("broadcast-enabled")
                            .checked(self.broadcast.state.enabled)
                            .color(p.ok)
                            .on_click(
                                cx.listener(|_, enabled: &bool, _, cx| cx.emit(Event::Broadcast(*enabled))),
                            ),
                    ),
            );
        let mut sessions = div()
            .id("broadcast-session-list")
            .max_h(px(240.))
            .overflow_y_scroll();
        for row in &self.broadcast.sessions {
            let id = row.id;
            let checked = self.broadcast.state.checked.contains(&id);
            sessions = sessions.child(
                div()
                    .id(("broadcast-check", id))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(8.))
                    .h(px(38.))
                    .rounded(px(10.))
                    .when(!row.connected(), |d| d.opacity(0.5))
                    .when(row.connected(), |d| d.cursor_pointer().hover(|d| d.bg(p.card2)))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Event::CheckSession(id))))
                    .child(
                        div()
                            .size(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(if checked { p.acc } else { p.tx3 })
                            .when(checked, |d| d.bg(p.acc))
                            .text_color(gpui::white())
                            .text_size(px(12.))
                            .child(if checked { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .child(row.title.clone()),
                    )
                    .child(div().text_size(px(11.5)).text_color(p.tx2).child(row.subtitle())),
            );
        }
        broadcast = broadcast.child(sessions);
        div()
            .w(px(300.))
            .flex_none()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(broadcast)
            .child(body)
    }
}

type TargetAvailable = Box<dyn Fn(usize, &App) -> bool>;
type SendCommand = Box<dyn Fn(&str, &mut Window, &mut App) -> Result<(), String>>;

pub struct Confirmation {
    title: String,
    input: Entity<InputState>,
    request: SendRequest,
    available: TargetAvailable,
    send: SendCommand,
    error: Option<String>,
    sent: bool,
    _subscriptions: Vec<Subscription>,
}
impl Confirmation {
    pub fn open(
        command: Snippet,
        request: SendRequest,
        workspace: &Entity<crate::workspace::Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let weak = workspace.downgrade();
        let check = weak.clone();
        let body = cx.new(|cx| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(command.command));
            let subscriptions = vec![
                // Single-line Input propagates Enter to the dialog's Confirm binding.
                // Subscribing to PressEnter as well would risk sending the same command twice.
                cx.subscribe(&input, |_: &mut Self, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }),
                cx.observe(workspace, |_, _, cx| cx.notify()),
            ];
            Self {
                title: command.name,
                input,
                request: request.clone(),
                available: Box::new(move |id, cx| {
                    check
                        .upgrade()
                        .is_some_and(|w| w.read(cx).command_available(Some(id), cx))
                }),
                send: Box::new(move |text, window, cx| {
                    weak.update(cx, |w, cx| w.send_reviewed(&request, text, window, cx))
                        .unwrap_or_else(|_| Err("会话已关闭".into()))
                }),
                error: None,
                sent: false,
                _subscriptions: subscriptions,
            }
        });
        let opened = body.clone();
        let first = std::cell::Cell::new(true);
        crate::dialogs::open_modal(window, cx, move |dialog, window, cx| {
            if first.replace(false) {
                body.update(cx, |b, cx| b.focus(window, cx));
            }
            let ok = body.clone();
            let cancel = body.clone();
            dialog
                .w(px(540.))
                .border_0()
                .rounded(px(24.))
                .shadow(dialogs::modal_shadow(None))
                .centered()
                .p(px(24.))
                .close_button(false)
                .overlay_closable(false)
                .on_ok(move |_, w, cx| ok.update(cx, |b, cx| b.confirm(w, cx)))
                .on_cancel(move |_, w, cx| {
                    cancel.update(cx, |b, cx| b.cancel(w, cx));
                    true
                })
                .child(body.clone())
        });
        opened
    }
    fn count(&self, cx: &App) -> usize {
        self.request
            .targets
            .iter()
            .filter(|t| (self.available)(t.id, cx))
            .count()
    }
    fn mode(&self, cx: &App) -> String {
        if self.request.broadcast {
            format!("广播到 {} 个会话", self.count(cx))
        } else {
            "发送到当前会话".into()
        }
    }
}
impl DialogBody for Confirmation {
    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.sent {
            return true;
        }
        let value = self.input.read(cx).value().to_string();
        if value.trim().is_empty() || self.count(cx) == 0 {
            return false;
        }
        match (self.send)(&value, window, cx) {
            Ok(()) => {
                self.sent = true;
                true
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                false
            }
        }
    }
}
impl Render for Confirmation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let risky = model::risky(&self.input.read(cx).value());
        let available = self.count(cx) > 0;
        let production = self
            .request
            .targets
            .iter()
            .any(|t| t.production && (self.available)(t.id, cx));
        stack()
            .pb_0()
            .child(
                div()
                    .h(px(26.))
                    .flex_none()
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(19.))
                            .line_height(px(22.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .line_height(px(15.))
                            .text_color(p.tx2)
                            .child(self.mode(cx)),
                    ),
            )
            .child(
                div()
                    .font_family(MONO_FONT)
                    .rounded(px(13.))
                    .bg(shellmate_term::colors::to_hsla(
                        cx.terminal_style().palette.background,
                    ))
                    .text_color(shellmate_term::colors::to_hsla(
                        cx.terminal_style().palette.foreground,
                    ))
                    .h(px(44.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .child(
                        Input::new(&self.input)
                            .appearance(false)
                            .p_0()
                            .h(px(44.))
                            .text_size(px(13.5)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .children(self.request.targets.iter().map(|target| {
                        let connected = (self.available)(target.id, cx);
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .h(px(28.))
                            .rounded(px(9.))
                            .bg(p.card2)
                            .text_size(px(12.5)).line_height(px(15.)).font_weight(FontWeight::MEDIUM)
                            .when(!connected, |d| d.opacity(0.5))
                            .child(target.title.clone())
                            .when(target.production, |d| {
                                d.child(
                                    div()
                                        .px(px(6.))
                                        .rounded(px(5.))
                                        .bg(p.accs)
                                        .text_color(p.acc)
                                        .text_size(px(10.5))
                                        .font_weight(FontWeight::BOLD)
                                        .child("生产"),
                                )
                            })
                            .when(!connected, |d| d.child("离线 · 将跳过"))
                    })),
            )
            .when(risky, |d| {
                d.child(dialogs::notice(
                    dialogs::Notice::Error,
                    if production {
                        "会修改系统状态，且目标包含生产环境主机。请再次核对命令和目标。"
                    } else {
                        "该命令会修改系统状态，请核对后执行。"
                    },
                    &p,
                ))
            })
            .when(!available, |d| d.child(hint("会话未连接，无法发送", &p)))
            .children(
                self.error
                    .clone()
                    .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex_1().child(hint("可修改后再发送 · 回车执行", &p)))
                    .child(
                        button("qc-cancel", "取消", Tone::Plain, true)
                            .h(px(40.))
                            .px(px(18.))
                            .rounded(px(13.))
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        button(
                            "qc-send",
                            if risky { "确认执行" } else { "发送" },
                            if risky { Tone::Danger } else { Tone::Primary },
                            available && !self.input.read(cx).value().trim().is_empty(),
                        )
                        .h(px(40.))
                        .px(px(20.))
                        .rounded(px(13.))
                        .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
                    ),
            )
    }
}

struct EditGroup {
    name: Entity<InputState>,
    items: Vec<(Entity<InputState>, Entity<InputState>)>,
}
pub struct Manager {
    groups: Vec<EditGroup>,
    store: Rc<RefCell<HostStore>>,
    workspace: gpui::WeakEntity<crate::workspace::Workspace>,
    error: Option<String>,
}
impl Manager {
    pub fn open(
        groups: Vec<SnippetGroup>,
        store: Rc<RefCell<HostStore>>,
        workspace: gpui::WeakEntity<crate::workspace::Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let body = cx.new(|cx| {
            let mut this = Self {
                groups: vec![],
                store,
                workspace,
                error: None,
            };
            this.load(groups, window, cx);
            this
        });
        dialogs::open(window, cx, "管理快捷命令", 720., body);
    }
    fn values(&self, cx: &App) -> Vec<SnippetGroup> {
        self.groups
            .iter()
            .map(|g| SnippetGroup {
                name: g.name.read(cx).value().to_string(),
                items: g
                    .items
                    .iter()
                    .map(|(n, c)| Snippet {
                        name: n.read(cx).value().to_string(),
                        command: c.read(cx).value().to_string(),
                    })
                    .collect(),
            })
            .collect()
    }
    fn load(&mut self, groups: Vec<SnippetGroup>, window: &mut Window, cx: &mut Context<Self>) {
        let mut input = |value: String| cx.new(|cx| InputState::new(window, cx).default_value(value));
        self.groups = groups
            .into_iter()
            .map(|g| EditGroup {
                name: input(g.name),
                items: g
                    .items
                    .into_iter()
                    .map(|s| (input(s.name), input(s.command)))
                    .collect(),
            })
            .collect();
        cx.notify();
    }
    fn edit(
        &mut self,
        edit: impl FnOnce(&mut Vec<SnippetGroup>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut groups = self.values(cx);
        edit(&mut groups);
        self.load(groups, window, cx);
    }
}
impl DialogBody for Manager {
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let groups = self.values(cx);
        if let Err(error) = model::validate(&groups) {
            self.error = Some(error.into());
            cx.notify();
            return false;
        }
        if self.store.borrow_mut().save_snippets(&groups).is_err() {
            self.error = Some("无法保存快捷命令".into());
            cx.notify();
            return false;
        }
        self.workspace.update(cx, |w, cx| w.refresh_snippets(cx)).ok();
        true
    }
}
impl Render for Manager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut list = div()
            .id("manage-list")
            .max_h(px(430.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.));
        for (g, group) in self.groups.iter().enumerate() {
            let mut section = div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .bg(p.card2)
                .rounded(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .child(field("分组名称", &p, Input::new(&group.name))),
                        )
                        .child(
                            Button::new(("group-up", g))
                                .small()
                                .label("↑")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.edit(
                                        |gs| {
                                            if g > 0 {
                                                gs.swap(g, g - 1);
                                            }
                                        },
                                        w,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            Button::new(("group-down", g))
                                .small()
                                .label("↓")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.edit(
                                        |gs| {
                                            if g + 1 < gs.len() {
                                                gs.swap(g, g + 1);
                                            }
                                        },
                                        w,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            Button::new(("group-delete", g))
                                .small()
                                .label("删除分组")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.edit(
                                        |gs| {
                                            gs.remove(g);
                                        },
                                        w,
                                        cx,
                                    )
                                })),
                        ),
                );
            for (i, (name, command)) in group.items.iter().enumerate() {
                let id = g * 100000 + i;
                section =
                    section.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().w(px(110.)).child(Input::new(name)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(MONO_FONT)
                                    .child(Input::new(command)),
                            )
                            .child(
                                Button::new(("cmd-up", id))
                                    .xsmall()
                                    .label("↑")
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.edit(
                                            |gs| {
                                                if i > 0 {
                                                    gs[g].items.swap(i, i - 1);
                                                }
                                            },
                                            w,
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                Button::new(("cmd-down", id))
                                    .xsmall()
                                    .label("↓")
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.edit(
                                            |gs| {
                                                if i + 1 < gs[g].items.len() {
                                                    gs[g].items.swap(i, i + 1);
                                                }
                                            },
                                            w,
                                            cx,
                                        )
                                    })),
                            )
                            .child(Button::new(("cmd-delete", id)).xsmall().label("删除").on_click(
                                cx.listener(move |this, _, w, cx| {
                                    this.edit(
                                        |gs| {
                                            gs[g].items.remove(i);
                                        },
                                        w,
                                        cx,
                                    )
                                }),
                            )),
                    );
            }
            section = section.child(Button::new(("cmd-add", g)).small().label("添加命令").on_click(
                cx.listener(move |this, _, w, cx| {
                    this.edit(
                        |gs| {
                            gs[g].items.push(Snippet {
                                name: "新命令".into(),
                                command: String::new(),
                            })
                        },
                        w,
                        cx,
                    )
                }),
            ));
            list = list.child(section);
        }
        stack()
            .child(hint("编辑名称与命令，使用 ↑↓ 调整顺序。保存后生效。", &p))
            .child(list)
            .child(
                Button::new("group-add")
                    .label("添加分组")
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.edit(
                            |gs| {
                                gs.push(SnippetGroup {
                                    name: "新分组".into(),
                                    items: vec![],
                                })
                            },
                            w,
                            cx,
                        )
                    })),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
            )
            .child(dialogs::actions(vec![
                button("manage-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                button("manage-save", "保存", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
            ]))
    }
}
