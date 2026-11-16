//! Left column: host search, filter, grouped host list, local terminal entry; or, collapsed, the
//! narrow rail of the same things ([`rail`]).

mod rail;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::assets::IconName as Lucide;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::tooltip::Tooltip;
use gpui::component::{Icon, Sizable as _};
use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, FontWeight, KeyBinding, SharedString, Subscription,
    Window, actions, div, px,
};
use shellmate_core::{Host, HostBook, HostFilter, HostId, HostSection, minutes_since, relative_time, sections};

use crate::theme::{ActivePalette, Palette};
use crate::ui::{card_shadow, dot, item_shadow, kbd, segmented, window_drag_area, SegmentItem};

pub const SIDEBAR_WIDTH: f32 = 272.;

/// What the sidebar and the rail say about the connection state of the footer: no sync yet.
pub const SYNC_STATUS: &str = "本地配置 · 未开启同步";

actions!(sidebar, [ConnectSelected]);

const LIST_CONTEXT: &str = "HostList";

/// Key bindings of the host list (Enter connects the selected host).
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("enter", ConnectSelected, Some(LIST_CONTEXT))]);
}

/// Connection state of a host that has an open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Ok,
    /// Connecting (or reconnecting): yellow.
    Connecting,
    Failed,
}

impl LiveStatus {
    /// Which status wins when a host has several sessions (lower is better).
    pub fn rank(self) -> u8 {
        match self {
            LiveStatus::Ok => 0,
            LiveStatus::Connecting => 1,
            LiveStatus::Failed => 2,
        }
    }
}

/// The tab on screen, as far as the sidebar and the rail are concerned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Active {
    #[default]
    None,
    /// A local terminal.
    Local,
    /// A tab of this saved host.
    Host(String),
}

pub enum SidebarEvent {
    OpenLocal,
    Connect(HostId),
    /// A click on a host of the rail that has a tab: show it.
    ActivateHost(HostId),
    /// 搜索主机 on the rail: show the sidebar and put the cursor in its search field.
    SearchHosts,
    ToggleSidebar,
    /// Open the editor for a new host.
    NewHost,
    EditHost(HostId),
    /// Open the editor for a new host prefilled from this one.
    DuplicateHost(HostId),
    DeleteHost(HostId),
    /// Open the ssh config importer.
    ImportConfig,
}

pub struct Sidebar {
    book: Rc<HostBook>,
    search: Entity<InputState>,
    filter: HostFilter,
    collapsed: HashSet<String>,
    selected: Option<String>,
    /// Focus of the host list, so Enter reaches it after a click on a row.
    list_focus: FocusHandle,
    live: HashMap<String, LiveStatus>,
    session_count: usize,
    local_summary: SharedString,
    /// Collapsed to the rail.
    rail: bool,
    /// A local terminal is running.
    local_alive: bool,
    active: Active,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

/// Current unix time in seconds, for "x minutes ago" labels.
fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Sidebar {
    pub fn new(book: Rc<HostBook>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索名称、地址、标签"));
        let subscriptions = vec![cx.observe(&search, |_, _, cx| cx.notify())];
        Self {
            book,
            search,
            filter: HostFilter::All,
            collapsed: HashSet::new(),
            selected: None,
            list_focus: cx.focus_handle(),
            live: HashMap::new(),
            session_count: 0,
            local_summary: "zsh · ~".into(),
            rail: false,
            local_alive: false,
            active: Active::None,
            _subscriptions: subscriptions,
        }
    }

    fn connect_selected(&mut self, _: &ConnectSelected, _: &mut Window, cx: &mut Context<Self>) {
        let host = self.selected.as_deref().and_then(|id| self.book.host(&HostId::from_raw(id)));
        if let Some(host) = host {
            cx.emit(SidebarEvent::Connect(host.id.clone()));
        }
    }

    /// Replaces the host list (after loading or editing the library).
    pub fn set_book(&mut self, book: Rc<HostBook>, cx: &mut Context<Self>) {
        self.book = book;
        cx.notify();
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Updates live-session info shown in the list.
    pub fn set_sessions(
        &mut self,
        count: usize,
        live: HashMap<String, LiveStatus>,
        local_summary: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.session_count = count;
        self.live = live;
        self.local_summary = local_summary;
        cx.notify();
    }

    /// Shows the sidebar (`false`) or only its rail (`true`).
    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.rail != collapsed {
            self.rail = collapsed;
            cx.notify();
        }
    }

    /// What the rail shows besides the hosts: whether a local terminal is running and which tab is on screen.
    pub fn set_rail_state(&mut self, local_alive: bool, active: Active, cx: &mut Context<Self>) {
        self.local_alive = local_alive;
        self.active = active;
        cx.notify();
    }

    pub fn select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let icon_button = |id: &'static str, icon: Lucide, tip: &'static str| {
            div()
                .id(id)
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|d| d.bg(p.card3.opacity(0.7)))
                .child(Icon::new(icon).size(px(16.)).text_color(p.tx2))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        };
        let top = window_drag_area(div().id("sidebar-drag"))
            .h(px(52.))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.))
            .pr(px(4.))
            .child(
                icon_button("toggle-sidebar", Lucide::PanelLeft, "隐藏侧栏 ⌃⌘S")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ToggleSidebar))),
            )
            .child(
                icon_button("new-host", Lucide::Plus, "新建主机")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewHost))),
            );

        let host_count = self.book.hosts.len();
        div()
            .child(top)
            .child(
                div()
                    .px(px(6.))
                    .pt(px(4.))
                    .pb(px(14.))
                    .child(div().text_size(px(24.)).font_weight(FontWeight::BOLD).text_color(p.tx).child("主机"))
                    .child(
                        div()
                            .mt(px(2.))
                            .text_size(px(12.))
                            .text_color(p.tx3)
                            .child(format!("{host_count} 台 · {} 个会话", self.session_count)),
                    ),
            )
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .flex()
            .items_center()
            .h(px(38.))
            .px(px(12.))
            .gap(px(8.))
            .rounded(px(12.))
            .bg(p.card)
            .shadow(card_shadow(&p))
            .child(Icon::new(Lucide::Search).size(px(15.)).text_color(p.tx3))
            .child(div().flex_1().child(Input::new(&self.search).small().appearance(false).cleanable(true)))

    }

    fn render_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items = HostFilter::ALL
            .iter()
            .map(|f| SegmentItem::new(f.label(), *f == self.filter))
            .collect();
        let this = cx.entity().downgrade();
        segmented(
            "host-filter",
            items,
            move |i, _, cx| {
                this.update(cx, |s, cx| {
                    s.filter = HostFilter::ALL[i];
                    cx.notify();
                })
                .ok();
            },
            cx,
        )
        .mt(px(12.))
    }

    fn render_local(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let selected = self.selected.as_deref() == Some("local");
        div()
            .id("local-terminal")
            .mt(px(14.))
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(10.))
            .rounded(px(12.))
            .cursor_pointer()
            .when(selected, |d| d.bg(p.card).shadow(item_shadow(&p)))
            .when(!selected, |d| d.hover(|d| d.bg(p.card.opacity(0.55))))
            .child(
                div()
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.line)
                    .child(Icon::new(Lucide::SquareTerminal).size(px(16.)).text_color(p.tx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(p.tx).child("本机终端"))
                    .child(div().text_size(px(11.5)).text_color(p.tx3).truncate().child(self.local_summary.clone())),
            )
            .child(kbd("⌘T", cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.selected = Some("local".into());
                cx.emit(SidebarEvent::OpenLocal);
            }))
    }

    fn render_section(&self, section: &HostSection, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let title: SharedString = section.title.clone().into();
        let key = section.key();
        let open = !self.collapsed.contains(&key);
        let header = div()
            .id(SharedString::from(format!("group-{key}")))
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(30.))
            .px(px(6.))
            .mt(px(6.))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(p.tx2)
            .child(
                Icon::new(if open { Lucide::ChevronDown } else { Lucide::ChevronRight })
                    .size(px(12.))
                    .text_color(p.tx3),
            )
            .child(div().flex_1().child(title))
            .child(div().text_size(px(11.5)).text_color(p.tx3).child(section.hosts.len().to_string()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&key) {
                    this.collapsed.insert(key.clone());
                }
                cx.notify();
            }));

        let rows = open.then(|| {
            let tone = section.tone;
            section
                .hosts
                .iter()
                .map(|h| self.render_host(h, tone, &p, cx).into_any_element())
                .collect::<Vec<_>>()
        });
        div().child(header).children(rows.into_iter().flatten())
    }

    fn render_host(
        &self,
        host: &Host,
        tone: shellmate_core::GroupTone,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected.as_deref() == Some(host.id.as_str());
        let live = self.live.get(host.id.as_str()).copied();
        let sub = match (self.filter, host.last_connected_at) {
            (HostFilter::Recent, Some(at)) => {
                format!("{} · {}", relative_time(minutes_since(unix_now(), at)), host.addr)
            }
            _ => host.address_line(),
        };
        let id = host.id.clone();

        let avatar = host_avatar(host, tone, live, Avatar { size: 32., radius: 9., dot: 10. }, p);

        let name_line = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.tx)
                    .truncate()
                    .child(host.name.clone()),
            )
            .when(host.favorite, |d| d.child(Icon::new(Lucide::Star).size(px(11.)).text_color(p.warn)));

        div()
            .id(SharedString::from(format!("host-{}", host.id)))
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(10.))
            .rounded(px(12.))
            .cursor_pointer()
            .when(selected, |d| d.bg(p.card).shadow(item_shadow(p)))
            .when(!selected, |d| d.hover(|d| d.bg(p.card.opacity(0.55))))
            .child(avatar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(name_line)
                    .child(div().text_size(px(11.5)).text_color(p.tx2).truncate().child(sub)),
            )
            .children(host.badge().map(|b| {
                div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(5.))
                    .bg(p.card3.opacity(0.8))
                    .text_size(px(10.5))
                    .text_color(p.tx2)
                    .child(b)
            }))
            .children(selected.then(|| {
                let connect_id = host.id.clone();
                div()
                    .id(SharedString::from(format!("connect-{}", host.id)))
                    .flex_none()
                    .px(px(8.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .bg(p.accs)
                    .text_color(p.acc)
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.acc).text_color(gpui::white()))
                    .child("连接")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(SidebarEvent::Connect(connect_id.clone()));
                    }))
            }))
            .children(selected.then(|| {
                let sidebar = cx.entity().downgrade();
                let host_id = host.id.clone();
                Button::new(SharedString::from(format!("menu-{}", host.id)))
                    .ghost()
                    .compact()
                    .icon(Icon::new(Lucide::Ellipsis).size(px(15.)))
                    .tooltip("更多")
                    .dropdown_menu(move |menu, _, _| {
                        let item = |label: &'static str, icon: Lucide, event: fn(HostId) -> SidebarEvent| {
                            let (sidebar, host_id) = (sidebar.clone(), host_id.clone());
                            PopupMenuItem::new(label).icon(Icon::new(icon)).on_click(move |_, _, cx| {
                                sidebar.update(cx, |_, cx| cx.emit(event(host_id.clone()))).ok();
                            })
                        };
                        menu.item(item("连接", Lucide::Play, SidebarEvent::Connect))
                            .item(item("编辑", Lucide::Pencil, SidebarEvent::EditHost))
                            .item(item("复制为新主机", Lucide::Copy, SidebarEvent::DuplicateHost))
                            .separator()
                            .item(item("删除", Lucide::Trash, SidebarEvent::DeleteHost))
                    })
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selected = Some(id.to_string());
                window.focus(&this.list_focus, cx);
                if event.click_count() >= 2 {
                    cx.emit(SidebarEvent::Connect(id.clone()));
                }
                cx.notify();
            }))
    }

    /// Shown when the library has no hosts at all.
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let button = |id: &'static str, label: &'static str, icon: Option<Lucide>| {
            div()
                .id(id)
                .w_full()
                .h(px(34.))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .rounded(px(10.))
                .bg(p.card)
                .border_1()
                .border_color(p.line)
                .text_size(px(12.5))
                .text_color(p.tx)
                .cursor_pointer()
                .hover(|d| d.bg(p.card2))
                .children(icon.map(|icon| Icon::new(icon).size(px(14.)).text_color(p.tx2)))
                .child(label)
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .pt(px(36.))
            .px(px(10.))
            .child(
                div()
                    .size(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(12.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.line)
                    .child(Icon::new(Lucide::RadioTower).size(px(20.)).text_color(p.tx3)),
            )
            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(p.tx).child("还没有主机"))
            .child(
                div()
                    .mb(px(6.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(p.tx3)
                    .child("添加第一台服务器，或从 ~/.ssh/config 导入已有的主机。"),
            )
            .child(
                button("empty-new-host", "新建主机", Some(Lucide::Plus))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewHost))),
            )
            .child(
                button("empty-import", "从 ~/.ssh/config 导入", None)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ImportConfig))),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(44.))
            .px(px(12.))
            .mt(px(8.))
            .rounded(px(12.))
            .bg(p.card)
            .shadow(card_shadow(&p))
            .child(dot(p.tx3, 7.))
            .child(div().flex_1().text_size(px(12.)).text_color(p.tx2).child(SYNC_STATUS))
            .child(
                div()
                    .id("settings")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .opacity(0.5)
                    .child(Icon::new(Lucide::Settings).size(px(15.)).text_color(p.tx2))
                    .tooltip(|window, cx| Tooltip::new("设置（后续阶段实现）").build(window, cx)),
            )
    }
}

/// The measures of a host's avatar: the list's and the rail's differ a little.
#[derive(Clone, Copy)]
pub(crate) struct Avatar {
    pub size: f32,
    pub radius: f32,
    /// The status dot's diameter.
    pub dot: f32,
}

/// The tinted square with the host's initials and, when it has a tab, the dot that says how that tab is.
pub(crate) fn host_avatar(
    host: &Host,
    tone: shellmate_core::GroupTone,
    live: Option<LiveStatus>,
    avatar: Avatar,
    p: &Palette,
) -> gpui::Div {
    let (tone_bg, tone_fg) = p.tone(tone);
    div()
        .relative()
        .size(px(avatar.size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(avatar.radius))
        .bg(tone_bg)
        .text_color(tone_fg)
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .child(host.monogram())
        .children(live.map(|status| {
            let color = match status {
                LiveStatus::Ok => p.ok,
                LiveStatus::Connecting => p.warn,
                LiveStatus::Failed => p.err,
            };
            div()
                .absolute()
                .right(px(-2.))
                .bottom(px(-2.))
                .size(px(avatar.dot))
                .rounded_full()
                .border_2()
                .border_color(p.win)
                .bg(color)
        }))
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rail {
            return self.render_rail(cx).into_any_element();
        }
        let query = self.search.read(cx).value();
        let book = self.book.clone();
        let sections = sections(&book, self.filter, &query);
        let p = cx.palette().clone();

        let list = if book.hosts.is_empty() {
            vec![self.render_empty(cx).into_any_element()]
        } else if sections.is_empty() {
            vec![
                div()
                    .py(px(28.))
                    .text_center()
                    .text_size(px(12.5))
                    .text_color(p.tx3)
                    .child("没有匹配的主机")
                    .into_any_element(),
            ]
        } else {
            sections.iter().map(|s| self.render_section(s, cx).into_any_element()).collect()
        };

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .pl(px(12.))
            .pr(px(4.))
            .pb(px(12.))
            .child(self.render_header(cx))
            .child(self.render_search(cx))
            .child(self.render_filter(cx))
            .child(self.render_local(cx))
            .child(
                div()
                    .id("host-list")
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.list_focus)
                    .on_action(cx.listener(Self::connect_selected))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(px(8.))
                    .children(list),
            )
            .child(self.render_footer(cx))
            .into_any_element()
    }
}
