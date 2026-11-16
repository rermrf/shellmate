//! The 监控 panel of one tab: the header with its chips and sub-page tabs, the overview with its cards and
//! the list of what needs attention, the process table with the bar of actions under it, the list of
//! services with its detail and journal, and the containers with their logs. What it says comes from
//! [`super::model`], [`super::process`], [`super::services`] and [`super::containers`]; what the user does
//! with it goes to the workspace as [`MonitorPanelEvent`]s (which runs commands and opens tabs).

mod docker;
mod logs;
mod services;

use std::ops::Range;

use gpui::assets::IconName as Lucide;
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::{Icon, tooltip::Tooltip};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, ScrollHandle, ScrollStrategy,
    SharedString, Subscription, UniformListScrollHandle, Window, div, px, relative, uniform_list,
};
use shellmate_monitor::Monitor;
use shellmate_monitor::commands::{ContainerAction, ServiceAction};

use self::logs::LogPane;
use super::charts::sparkline;
use super::containers::ForwardTarget;
use super::model::{self, ALL_WELL, AttentionAction, Banner, Body, Cards, Chip, HeaderView, MonitorData, Page, Tone};
use super::process::{
    self, ActionButton, ButtonTone, ProcKey, ProcessAction, Row, SortKey, Table, resolve, selection_text,
};
use super::services::{ServiceFilter, ServiceRow};
use crate::theme::{ActivePalette, CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::{SegmentItem, card_shadow, item_shadow, segmented};

/// What the panel asks of the workspace.
pub enum MonitorPanelEvent {
    /// A button of the bar under the process table.
    Process(ProcessAction, ProcKey),
    /// 分析目录 on a full mount point.
    AnalyzeDisk(String),
    /// 重试 after the monitor gave up.
    Retry,
    /// A sub-page was shown: the monitor asks about what is on it.
    PageChanged(Page),
    /// A button of the detail of a service (or its autostart switch), for the unit with this name.
    Service(ServiceAction, String),
    /// A button of a container row.
    Container(ContainerAction, String),
    /// 终端 on a container: a new tab with a shell in it.
    ContainerTerminal(String),
    /// 清理 on the Docker page.
    PruneImages,
    /// A click on a port chip: where the forward goes, or why it cannot be made.
    ForwardPort(Result<ForwardTarget, &'static str>),
}

impl EventEmitter<MonitorPanelEvent> for MonitorPanel {}

/// The panel.
pub struct MonitorPanel {
    host: SharedString,
    data: MonitorData,
    /// What follows the logs (the monitor of the connection).
    monitor: Option<Monitor>,
    page: Page,
    query: Entity<InputState>,
    sort: SortKey,
    tree: bool,
    selected: Option<ProcKey>,
    table: Table,
    scroll: UniformListScrollHandle,
    overview_scroll: ScrollHandle,
    // the 服务 page
    service_query: Entity<InputState>,
    service_filter: ServiceFilter,
    service_selected: Option<String>,
    service_rows: Vec<ServiceRow>,
    service_scroll: UniformListScrollHandle,
    service_log: Option<LogPane>,
    // the Docker page
    container_log: Option<(String, LogPane)>,
    docker_scroll: ScrollHandle,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for MonitorPanel {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The background and text color of a tone (chips, badges, banners).
fn tone_colors(p: &Palette, tone: Tone) -> (Hsla, Hsla) {
    match tone {
        Tone::Ok => (p.oks, p.ok),
        Tone::Warn => (p.warns, p.warn),
        Tone::Err => (p.errs, p.err),
        Tone::Plain => (p.card2, p.tx2),
        Tone::Off => (p.card2, p.tx3),
    }
}

/// The text color of a number or status in a tone.
fn tone_text(p: &Palette, tone: Tone) -> Hsla {
    match tone {
        Tone::Ok => p.ok,
        Tone::Warn => p.warn,
        Tone::Err => p.err,
        Tone::Plain => p.tx,
        Tone::Off => p.tx3,
    }
}

impl MonitorPanel {
    /// A panel for the connection to `host`, showing `data`.
    pub fn new(
        host: impl Into<SharedString>,
        data: MonitorData,
        monitor: Option<Monitor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("过滤命令、用户或 PID"));
        let service_query = cx.new(|cx| InputState::new(window, cx).placeholder("搜索服务"));
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&service_query, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh_services(cx);
                    cx.notify();
                }
            }),
        ];
        let mut panel = Self {
            host: host.into(),
            data,
            monitor,
            page: Page::Overview,
            query,
            sort: SortKey::Cpu,
            tree: false,
            selected: None,
            table: Table { rows: Vec::new(), matches: 0, total: 0 },
            scroll: UniformListScrollHandle::new(),
            overview_scroll: ScrollHandle::new(),
            service_query,
            service_filter: ServiceFilter::All,
            service_selected: None,
            service_rows: Vec::new(),
            service_scroll: UniformListScrollHandle::new(),
            service_log: None,
            container_log: None,
            docker_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        panel.refresh(cx);
        panel.refresh_services(cx);
        panel
    }

    /// What the panel shows now.
    pub fn set_data(&mut self, data: MonitorData, cx: &mut Context<Self>) {
        self.data = data;
        self.refresh(cx);
        self.refresh_services(cx);
        self.sync_container_log(cx);
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn proto_order(&mut self,cx:&mut Context<Self>){
        self.service_rows.sort_by_key(|r|crate::proto::DATA["SERVICES"].a().iter().position(|s|s["n"].s()==r.name).unwrap_or(usize::MAX));cx.notify();
    }

    /// Shows a sub-page. The logs follow what is in front: the journal of the selected service on the 服务
    /// page, the open container log on the Docker page, nothing elsewhere.
    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.sync_service_log(cx);
            self.sync_container_log(cx);
            cx.emit(MonitorPanelEvent::PageChanged(page));
            cx.notify();
        }
    }

    /// What a followed log said (a batch of lines, or the end of the command): to the pane it belongs to.
    fn on_log_events(&mut self, key: &str, events: Vec<shellmate_monitor::FollowEvent>, cx: &mut Context<Self>) {
        let pane = if self.service_log.as_ref().is_some_and(|pane| pane.key == key) {
            self.service_log.as_mut()
        } else {
            self.container_log.as_mut().map(|(_, pane)| pane).filter(|pane| pane.key == key)
        };
        if let Some(pane) = pane {
            pane.push(events);
            cx.notify();
        }
    }

    /// Rebuilds the rows, and lets go of a selection whose process is gone.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.table = match self.data.snapshot.as_deref() {
            Some(snapshot) => {
                let query = self.query.read(cx).value().to_string();
                process::table(snapshot, &query, self.sort, self.tree)
            }
            None => Table { rows: Vec::new(), matches: 0, total: 0 },
        };
        if let (Some(key), Some(snapshot)) = (self.selected, self.data.snapshot.as_deref())
            && resolve(snapshot, key).is_none()
        {
            self.selected = None;
        }
    }

    /// Selects a process and scrolls to it.
    pub fn select(&mut self, key: ProcKey, cx: &mut Context<Self>) {
        self.selected = Some(key);
        cx.notify();
    }

    /// The process page with `pid` selected (a click on 占用最高 or on a zombie in 需要关注). A filter that hides
    /// it is cleared first.
    pub fn show_process(&mut self, pid: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.data.snapshot.as_deref().and_then(|s| s.process(pid)).map(ProcKey::of) else { return };
        self.set_page(Page::Processes, cx);
        if self.table.position(key).is_none() {
            self.query.update(cx, |input, cx| input.set_value("", window, cx));
            self.refresh(cx);
        }
        self.selected = Some(key);
        if let Some(ix) = self.table.position(key) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Debug review hook: the filter, the sort and the tree, as the controls would set them.
    #[cfg(debug_assertions)]
    pub fn demo_table(&mut self, query: &str, sort: SortKey, tree: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sort = sort;
        self.tree = tree;
        self.query.update(cx, |input, cx| input.set_value(query, window, cx));
        self.refresh(cx);
        cx.notify();
    }

    /// Debug review hook: the overview scrolled as far as it goes.
    #[cfg(debug_assertions)]
    pub fn demo_scroll_end(&mut self, cx: &mut Context<Self>) {
        let max = self.overview_scroll.max_offset();
        self.overview_scroll.set_offset(gpui::point(px(0.), -max.y));
        cx.notify();
    }

    fn set_sort(&mut self, sort: SortKey, cx: &mut Context<Self>) {
        self.sort = sort;
        self.refresh(cx);
        cx.notify();
    }

    fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.tree = !self.tree;
        self.refresh(cx);
        cx.notify();
    }

    // ---- drawing ---------------------------------------------------------------------------------

    fn chip(&self, chip: &Chip, p: &Palette) -> AnyElement {
        let (bg, fg) = tone_colors(p, chip.tone);
        div()
            .px(px(8.))
            .py(px(2.))
            .rounded(px(7.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .child(chip.text.clone())
            .into_any_element()
    }

    fn render_header(&self, header: &HeaderView, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let tabs = header.tabs.iter().map(|tab| {
            let page = tab.page;
            let mut button = div()
                .id(("monitor-page", page as usize))
                .h(px(32.))
                .pl(px(12.))
                .pr(px(if tab.badge.is_some() { 8. } else { 12. }))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(9.))
                .text_size(px(13.))
                .font_weight(if tab.active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                .text_color(if tab.active { p.tx } else { p.tx2 })
                .cursor_pointer()
                .when(tab.active, |d| d.bg(p.card).shadow(item_shadow(p)))
                .when(!tab.active, |d| d.hover(|d| d.text_color(p.tx)))
                .child(page.label())
                .on_click(cx.listener(move |this, _, _, cx| this.set_page(page, cx)));
            if let Some((text, tone)) = &tab.badge {
                let (bg, fg) = tone_colors(p, *tone);
                button = button.child(
                    div()
                        .min_w(px(20.))
                        .h(px(18.))
                        .px(px(6.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .bg(bg)
                        .text_color(fg)
                        .text_size(px(11.))
                        .font_weight(FontWeight::BOLD)
                        .child(text.clone()),
                );
            }
            button
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(14.)).h(px(64.))
            .py(px(10.))
            .pr(px(10.))
            .pl(px(18.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(13.5))
                            .child(div().flex_none().font_weight(FontWeight::BOLD).child(header.os.clone()))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.tx2)
                                    .child(header.meta.clone()),
                            ),
                    )
                    .child(div().flex().gap(px(6.)).children(header.chips.iter().map(|chip| self.chip(chip, p)))),
            )
            .child(
                div()
                    .id("monitor-pages")
                    .flex()
                    .flex_none()
                    .p(px(3.))
                    .gap(px(2.))
                    .rounded(px(12.))
                    .bg(p.card3)
                    .children(tabs),
            )
            .into_any_element()
    }

    fn banner(&self, banner: &Banner, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (bg, fg) = tone_colors(p, banner.tone);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .py(px(10.))
            .rounded(px(14.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .child(div().flex_1().min_w_0().child(banner.text.clone()))
            .when(banner.retry, |d| d.child(self.retry_button(p, cx)))
            .into_any_element()
    }

    fn retry_button(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("monitor-retry")
            .h(px(28.))
            .px(px(12.))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(9.))
            .bg(p.card)
            .text_color(p.tx)
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|d| d.opacity(0.85))
            .child("重试")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(MonitorPanelEvent::Retry)))
    }

    /// A card in the middle of the page with a sentence (and a button when there is something to retry).
    fn message_card(&self, title: &str, detail: &str, retry: bool, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .px(px(24.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .text_center()
            .child(div().text_size(px(17.)).font_weight(FontWeight::BOLD).child(title.to_string()))
            .child(div().text_size(px(13.)).text_color(p.tx2).line_height(px(21.)).child(detail.to_string()))
            .when(retry, |d| d.child(div().mt(px(8.)).child(self.retry_button(p, cx))))
            .into_any_element()
    }

    fn card(&self, p: &Palette) -> gpui::Div {
        div().flex().flex_col().px(px(18.)).py(px(16.)).bg(p.card).rounded(CARD_RADIUS).shadow(card_shadow(p))
    }

    fn big_number(&self, text: String, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .items_baseline()
            .child(div().text_size(px(30.)).font_weight(FontWeight::BOLD).child(text))
            .child(div().text_size(px(15.)).line_height(px(18.)).relative().top(px(-4.)).left(px(-2.)).text_color(p.tx2).font_weight(FontWeight::SEMIBOLD).child(" %"))
    }

    fn render_cards(&self, cards: &Cards, p: &Palette) -> AnyElement {
        let cpu = self
            .card(p)
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(12.5))
                    .text_color(p.tx2)
                    .child(cards.cpu.title.clone())
                    .child(cards.cpu.load.clone()),
            )
            .child(self.big_number(cards.cpu.percent.clone(), p))
            .child(div().h(px(64.)).w_full().child(sparkline(
                cards.cpu.history.iter().map(|v| f64::from(*v)).collect(),
                100.0,
                p.acc,
                Some(p.accs),
            )));

        let mem = &cards.memory;
        let memory = self
            .card(p)
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(12.5))
                    .text_color(p.tx2)
                    .child("内存")
                    .child(mem.total.clone()),
            )
            .child(self.big_number(mem.percent.clone(), p))
            .child(
                div()
                    .mt_auto()
                    .h(px(10.))
                    .flex()
                    .rounded(px(5.))
                    .bg(p.card3)
                    .overflow_hidden()
                    .child(div().h_full().w(relative(mem.used)).bg(p.ok))
                    .child(div().h_full().w(relative(mem.cache)).bg(p.oks)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(14.))
                    .text_size(px(11.5))
                    .text_color(p.tx2)
                    .children(mem.legend.iter().map(|text| div().child(text.clone()))),
            );

        let disks = self
            .card(p)
            .flex_1()
            .min_w_0()
            .gap(px(10.))
            .child(div().text_size(px(12.5)).line_height(px(19.)).text_color(p.tx2).child("磁盘"))
            .children(cards.disks.iter().map(|disk| {
                let color = match disk.tone {
                    Tone::Err => p.err,
                    Tone::Warn => p.warn,
                    _ => p.acc,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO_FONT)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(disk.mount.clone()),
                            )
                            .child(div().flex_none().text_color(p.tx2).child(disk.text.clone())),
                    )
                    .child(
                        div()
                            .h(px(6.))
                            .rounded(px(3.))
                            .bg(p.card3)
                            .overflow_hidden()
                            .child(div().h_full().w(relative(disk.fraction)).bg(color)),
                    )
            }))
            .when(cards.disks.is_empty(), |d| {
                d.child(div().text_size(px(12.)).text_color(p.tx3).child(if self.data.snapshot.is_some() {
                    "没有可显示的磁盘"
                } else {
                    "—"
                }))
            });

        let network = self.card(p).flex_1().min_w_0().gap(px(6.)).children(cards.network.as_ref().map(|net| {
            let top = model::scale_to(&[net.down_history.as_slice(), net.up_history.as_slice()].concat(), 10_240.0);
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(12.5))
                        .text_color(p.tx2)
                        .child(net.title.clone())
                        .child(net.unit),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(16.))
                        .items_baseline()
                        .child(
                            div()
                                .text_size(px(22.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.acc)
                                .child(format!("↓ {}", net.down)),
                        )
                        .child(
                            div()
                                .text_size(px(22.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.ok)
                                .child(format!("↑ {}", net.up)),
                        ),
                )
                .child(
                    div()
                        .h(px(64.))
                        .w_full()
                        .relative()
                        .child(sparkline(net.down_history.clone(), top, p.acc, None))
                        .child(div().absolute().inset_0().child(sparkline(net.up_history.clone(), top, p.ok, None))),
                )
        }));
        let network = if cards.network.is_none() {
            network.child(div().text_size(px(12.5)).text_color(p.tx2).child("网络")).child(
                div().text_size(px(12.)).text_color(p.tx3).child(if self.data.snapshot.is_some() {
                    "没有可显示的网卡"
                } else {
                    "—"
                }),
            )
        } else {
            network
        };
        div().flex().flex_none().h(px(157.)).gap(px(10.)).child(cpu).child(memory).child(disks).child(network).into_any_element()
    }

    fn render_attention(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let items = model::attention(&self.data);
        #[cfg(debug_assertions)] let items=if crate::proto::requested(){items.into_iter().map(|mut item|{if item.tag=="磁盘"{item.detail="按近 7 天增速约 11 天后写满 · 最大目录 /data/backups".into();}item}).collect::<Vec<_>>()}else{items};
        let list = items.into_iter().enumerate().map(|(ix, item)| {
            let (bg, fg) = tone_colors(p, if item.tone == Tone::Err { Tone::Err } else { Tone::Warn });
            let action = item.action.clone();
            div()
                .id(("attention", ix))
                .flex()
                .items_center()
                .gap(px(12.))
                .py(px(12.))
                .pl(px(14.))
                .pr(px(12.))
                .rounded(px(14.))
                .bg(p.card2)
                .cursor_pointer()
                .hover(|d| d.bg(p.card3))
                .child(div().w(px(4.)).self_stretch().flex_none().rounded(px(2.)).bg(fg))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .px(px(7.))
                                        .py(px(1.))
                                        .rounded(px(6.))
                                        .bg(bg)
                                        .text_color(fg)
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::BOLD)
                                        .child(item.tag),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(13.5)).line_height(px(if item.tag=="服务"{18.}else{19.}))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(item.title.clone()),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(4.))
                                .truncate()
                                .text_size(px(12.)).line_height(px(17.))
                                .text_color(p.tx2)
                                .font_family(MONO_FONT)
                                .child(item.detail.clone()),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(12.5)).line_height(px(17.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.acc)
                        .child(format!("{} →", item.action_label)),
                )
                .on_click(cx.listener(move |this, _, window, cx| match &action {
                    AttentionAction::AnalyzeDisk(mount) => cx.emit(MonitorPanelEvent::AnalyzeDisk(mount.clone())),
                    AttentionAction::ShowProcess(pid) => this.show_process(*pid, window, cx),
                    AttentionAction::ShowService(name) => this.show_service(name, window, cx),
                    AttentionAction::ShowContainer(name) => this.show_container(name, cx),
                }))
        });
        let list: Vec<_> = list.collect();
        self.card(p)
            .flex_1()
            .min_w_0()
            .gap(px(10.))
            .child(div().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child("需要关注"))
            .when(list.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(14.))
                        .py(px(14.))
                        .rounded(px(14.))
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .map(|d| {
                            if self.data.snapshot.is_some() {
                                d.bg(p.oks).text_color(p.ok)
                            } else {
                                d.bg(p.card2).text_color(p.tx3)
                            }
                        })
                        .child(if self.data.snapshot.is_some() {
                            format!("✓ {ALL_WELL}")
                        } else {
                            "—".to_string()
                        }),
                )
            })
            .children(list)
            .into_any_element()
    }

    fn render_cores_and_top(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = self.data.snapshot.as_deref();
        let per_core = snapshot.and_then(|s| s.cpu.as_ref()).map(|c| c.per_core.clone()).unwrap_or_default();
        let bars = model::core_bars(&per_core);
        // "cpu112-127" is longer than "cpu0": the names get one column as wide as the longest.
        let label_width =
            (bars.iter().map(|bar| bar.name.chars().count()).max().unwrap_or(0) as f32 * 7.8 + 4.).max(56.);
        let top = model::top_processes(snapshot, 4);
        let temperature = model::temperature_text(snapshot);
        self.card(p)
            .flex_1()
            .min_w_0()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(div().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child("各核占用"))
                    .children(temperature.map(|t| div().text_size(px(12.)).text_color(p.tx2).child(t))),
            )
            .child(div().flex().gap(px(22.)).children(model::core_columns(bars).into_iter().map(|column| {
                div().flex_1().min_w_0().flex().flex_col().gap(px(12.)).children(column.into_iter().map(|bar| {
                    let color = match bar.fraction {
                        Some(f) if f >= 0.95 => p.err,
                        Some(f) if f >= 0.8 => p.warn,
                        _ => p.acc,
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .text_size(px(12.5)).line_height(px(17.))
                        .child(
                            div()
                                .w(px(label_width))
                                .flex_none()
                                .whitespace_nowrap()
                                .font_family(MONO_FONT)
                                .text_color(p.tx2)
                                .child(bar.name),
                        )
                        .child(
                            div().flex_1().min_w_0().h(px(8.)).rounded(px(4.)).bg(p.card3).overflow_hidden().child(
                                div().h_full().w(relative(bar.fraction.unwrap_or(0.))).rounded(px(4.)).bg(color),
                            ),
                        )
                        .child(div().w(px(38.)).flex_none().text_right().font_family(MONO_FONT).child(bar.text))
                }))
            })))
            .when(per_core.is_empty(), |d| d.child(div().text_size(px(12.5)).line_height(px(17.)).text_color(p.tx3).child("—")))
            .child(div().h(px(1.)).bg(p.line).my(px(4.)))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(div().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child("占用最高"))
                    .child(div().text_size(px(12.)).text_color(p.tx2).child("点击查看进程")),
            )
            .children(top.into_iter().map(|proc| {
                let pid = proc.pid;
                div()
                    .id(("top-process", pid as usize))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(34.))
                    .px(px(8.))
                    .mx(px(-8.))
                    .rounded(px(10.))
                    .text_size(px(12.5)).line_height(px(17.))
                    .cursor_pointer()
                    .hover(|d| d.bg(p.card2))
                    .child(div().w(px(44.)).flex_none().font_family(MONO_FONT).text_color(p.tx2).child(pid.to_string()))
                    .child(div().flex_1().min_w_0().truncate().font_family(MONO_FONT).child(proc.command))
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .text_right()
                            .font_family(MONO_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(proc.cpu),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.show_process(pid, window, cx)))
            }))
            .into_any_element()
    }

    fn render_overview(&self, stale: bool, banner: Option<Banner>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let cards = model::cards(self.data.snapshot.as_deref());
        div()
            .id("monitor-overview")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(banner.map(|b| self.banner(&b, p, cx)))
            .child(
                div()
                    .id("monitor-overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.overview_scroll)
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .when(stale, |d| d.opacity(0.5))
                    .child(self.render_cards(&cards, p))
                    .child(
                        div()
                            .flex()
                            .flex_grow(1.)
                            .flex_shrink(0.)
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_grow(1.35)
                                    .flex_basis(px(0.))
                                    .min_w_0()
                                    .flex()
                                    .child(self.render_attention(p, cx)),
                            )
                            .child(
                                div()
                                    .flex_grow(1.)
                                    .flex_basis(px(0.))
                                    .min_w_0()
                                    .flex()
                                    .child(self.render_cores_and_top(p, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    // ---- the process page ------------------------------------------------------------------------

    fn render_process_row(&self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row: &Row = self.table.rows.get(ix)?;
        let key = row.key;
        let selected = self.selected == Some(key);
        Some(
            div()
                .id(("process", ix))
                .w_full()
                .flex()
                .items_center()
                .gap(px(12.))
                .h(px(34.))
                .px(px(20.))
                .rounded(px(10.))
                .text_size(px(12.5)).line_height(px(17.))
                .whitespace_nowrap()
                .when(selected, |d| d.bg(p.accs))
                .when(!selected, |d| d.hover(|d| d.bg(p.card2)))
                .when(row.context, |d| d.opacity(0.55))
                .child(div().w(px(72.)).flex_none().font_family(MONO_FONT).text_color(p.tx2).child(key.pid.to_string()))
                .child(div().w(px(96.)).flex_none().truncate().child(row.user.clone()))
                .child(
                    div()
                        .w(px(130.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        // 1600.0 is what sixteen busy threads of one process add up to.
                        .child(
                            div()
                                .w(px(36.))
                                .flex_none()
                                .whitespace_nowrap()
                                .font_family(MONO_FONT)
                                .child(row.cpu.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h(px(5.))
                                .rounded(px(3.))
                                .bg(p.card3)
                                .overflow_hidden()
                                .child(div().h_full().w(relative(row.cpu_bar)).bg(p.acc)),
                        ),
                )
                .child(div().w(px(56.)).flex_none().font_family(MONO_FONT).child(row.mem.clone()))
                .child(div().w(px(72.)).flex_none().font_family(MONO_FONT).text_color(p.tx2).child(row.rss.clone()))
                .child(
                    div()
                        .w(px(40.))
                        .flex_none()
                        .font_family(MONO_FONT)
                        .font_weight(FontWeight::BOLD)
                        .text_color(match row.state_tone {
                            Tone::Plain => p.tx2,
                            other => tone_text(p, other),
                        })
                        .child(row.state.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .font_family(MONO_FONT)
                        .pl(px(row.depth as f32 * 14.))
                        .child(div().flex_none().text_color(p.tx3).child(row.prefix))
                        .child(div().min_w_0().truncate().child(row.command.clone())),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select(key, cx)))
                .into_any_element(),
        )
    }

    fn action_button(&self, button: &ActionButton, key: ProcKey, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (bg, fg, weight) = match button.tone {
            ButtonTone::Plain => (p.card2, p.tx, FontWeight::SEMIBOLD),
            ButtonTone::Accent => (p.accs, p.acc, FontWeight::SEMIBOLD),
            ButtonTone::Danger => (p.errs, p.err, FontWeight::BOLD),
        };
        let action = button.action;
        let tooltip = button.tooltip.clone();
        let mut element = div()
            .id(("process-action", action as usize))
            .h(px(32.))
            .px(px(12.))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(10.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(weight)
            .child(button.label.clone());
        element = if button.enabled {
            element
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(MonitorPanelEvent::Process(action, key))))
        } else {
            element.opacity(0.4).cursor_not_allowed()
        };
        if let Some(text) = tooltip {
            let text: SharedString = text.into();
            element = element.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
        }
        element.into_any_element()
    }

    fn render_processes(&self, stale: bool, banner: Option<Banner>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let sorts = SortKey::ALL.iter().map(|key| SegmentItem::new(key.label(), *key == self.sort)).collect::<Vec<_>>();
        let on_pick = {
            let panel = cx.entity();
            move |ix: usize, _: &mut Window, cx: &mut gpui::App| {
                panel.update(cx, |this, cx| this.set_sort(SortKey::ALL[ix], cx));
            }
        };
        let tree = self.tree;
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .pt(px(12.))
            .pb(px(10.))
            .pl(px(18.))
            .pr(px(14.))
            .child(
                div()
                    .w(px(284.))
                    .h(px(34.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(11.))
                    .bg(p.card2)
                    .text_size(px(13.))
                    .child(Icon::new(Lucide::Search).size(px(14.)).text_color(p.tx3))
                    .child(div().flex_1().min_w_0().child(Input::new(&self.query).appearance(false).bordered(false).p_0().h(px(18.)).text_size(px(13.)).relative().left(px(2.5)))),
            )
            .child(segmented("process-sort", sorts, on_pick, cx).flex_none())
            .child(
                div()
                    .id("process-tree")
                    .h(px(34.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .rounded(px(11.))
                    .bg(if tree { p.accs } else { p.card2 })
                    .text_color(if tree { p.acc } else { p.tx2 })
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.85))
                    .child("树形视图")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_tree(cx))),
            )
            .child(div().flex_1())
            .child(div().text_size(px(12.)).text_color(p.tx2).child(self.table.count_text()));

        let heading = |text: &'static str, width: Option<f32>| {
            let cell = div().flex_none().child(text);
            match width {
                Some(w) => cell.w(px(w)),
                None => cell.flex_1().min_w_0(),
            }
        };
        let columns = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(28.))
            .py(px(6.))
            .text_size(px(11.5)).line_height(px(16.))
            .text_color(p.tx2)
            .border_b_1()
            .border_color(p.line)
            .child(heading("PID", Some(72.)))
            .child(heading("用户", Some(96.)))
            .child(heading("CPU %", Some(130.)))
            .child(heading("MEM %", Some(56.)))
            .child(heading("RSS", Some(72.)))
            .child(heading("状态", Some(40.)))
            .child(heading("命令", None));

        let list = uniform_list(
            "process-rows",
            self.table.rows.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let p = cx.palette().clone();
                range.filter_map(|ix| this.render_process_row(ix, &p, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();

        let probe = self.data.probe.as_deref();
        let selected = self
            .selected
            .zip(self.data.snapshot.as_deref())
            .and_then(|(key, snapshot)| resolve(snapshot, key).map(|process| (key, process)));
        let bar = selected.map(|(key, process)| {
            let (pid, what) = selection_text(process);
            let buttons = process::buttons(process, probe);
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .pt(px(10.))
                .pb(px(12.))
                .pl(px(18.))
                .pr(px(14.))
                .border_t_1()
                .border_color(p.line)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .overflow_hidden()
                        .text_size(px(12.5))
                        .font_family(MONO_FONT)
                        .child(div().flex_none().font_weight(FontWeight::BOLD).child(pid))
                        .child(div().min_w_0().truncate().text_color(p.tx2).child(what.strip_prefix(' ').unwrap_or(&what).to_string())),
                )
                .children(buttons.iter().map(|button| self.action_button(button, key, p, cx)))
        });

        let empty = self.table.rows.is_empty();
        let rows_area = if empty {
            let text = match (&self.data.snapshot, self.query.read(cx).value().trim().is_empty()) {
                (None, _) => "正在读取进程列表…",
                (Some(_), true) => "没有读到进程",
                (Some(_), false) => "没有匹配的进程",
            };
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(p.tx3)
                .child(text)
                .into_any_element()
        } else {
            div().flex_1().min_h_0().px(px(8.)).py(px(4.)).child(list).into_any_element()
        };

        div()
            .id("monitor-processes")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(banner.map(|b| self.banner(&b, p, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .bg(p.card)
                    .rounded(CARD_RADIUS)
                    .shadow(card_shadow(p))
                    .overflow_hidden()
                    .child(toolbar)
                    .child(columns)
                    .child(div().flex_1().min_h_0().flex().flex_col().when(stale, |d| d.opacity(0.5)).child(rows_area))
                    .children(bar),
            )
            .into_any_element()
    }
}

impl Render for MonitorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let header = model::header(&self.data, self.page, &self.host);
        let body = model::body(&self.data);
        let probe = self.data.probe.as_deref();
        let unsupported = matches!(self.data.phase, shellmate_monitor::Phase::Unsupported(_));
        let content = match (self.page, body) {
            (_, Body::Message(message)) if unsupported => {
                self.message_card(&message.title, &message.detail, message.retry, &p, cx)
            }
            (Page::Services, Body::Data { stale, banner }) => match model::services_page(probe) {
                Some(page) => self.message_card(&page.title, &page.detail, false, &p, cx),
                None => self.render_services(stale, banner, &p, cx),
            },
            (Page::Docker, Body::Data { stale, banner }) => match model::docker_page(probe) {
                Some(page) => self.message_card(&page.title, &page.detail, false, &p, cx),
                None => self.render_docker(stale, banner, &p, cx),
            },
            (Page::Services, Body::Message(_)) => {
                let page = model::services_page(probe).unwrap_or_else(|| model::loading("服务列表"));
                self.message_card(&page.title, &page.detail, false, &p, cx)
            }
            (Page::Docker, Body::Message(_)) => {
                let page = model::docker_page(probe).unwrap_or_else(|| model::loading("容器列表"));
                self.message_card(&page.title, &page.detail, false, &p, cx)
            }
            (_, Body::Message(message)) => self.message_card(&message.title, &message.detail, message.retry, &p, cx),
            (Page::Overview, Body::Data { stale, banner }) => self.render_overview(stale, banner, &p, cx),
            (Page::Processes, Body::Data { stale, banner }) => self.render_processes(stale, banner, &p, cx),
        };
        div()
            .id("monitor")
            .track_focus(&self.focus)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(self.render_header(&header, &p, cx))
            .child(content)
    }
}
