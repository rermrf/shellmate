//! The 服务 page: a searchable list of the server's services, and the detail of the selected one with its
//! buttons, the autostart switch and the journal.

use std::ops::Range;

use gpui::component::input::Input;
use crate::ui::Toggle as Switch;
use gpui::component::tooltip::Tooltip;
use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, px, uniform_list};
use shellmate_monitor::commands::ServiceAction;

use super::logs::{LogKind, LogPane, log_box};
use super::{MonitorPanel, MonitorPanelEvent, tone_colors};
use crate::monitor::model::{Banner, Page};
use crate::monitor::services::{self, Detail, ServiceButton, ServiceFilter};
use crate::theme::{ActivePalette, CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::{ellipsis, SegmentItem, card_shadow, segmented};

/// How tall a row of the list is.
const ROW_HEIGHT: f32 = 56.0;

impl MonitorPanel {
    /// Rebuilds the rows of the list for the search and the filter, and keeps the selection in what is shown.
    pub(super) fn refresh_services(&mut self, cx: &mut Context<Self>) {
        self.service_rows = match self.data.services.as_deref() {
            Some(list) if self.show_apple => services::filtered_rows(list, self.service_query.read(cx).value().as_ref(), self.service_filter, true),
            Some(list) => services::rows(list, self.service_query.read(cx).value().as_ref(), self.service_filter),
            None => Vec::new(),
        };
        self.sync_service_log(cx);
    }

    /// The name of the service the detail shows: the selected one, or the first of the list.
    pub(super) fn shown_service(&self) -> Option<String> {
        let list = self.data.services.as_deref()?;
        services::shown(list, &self.service_rows, self.service_selected.as_deref()).map(|service| service.key().into())
    }

    /// Follows the journal of the shown service while the 服务 page is in front, and stops otherwise (and when
    /// there is another service to show).
    pub(super) fn sync_service_log(&mut self, cx: &mut Context<Self>) {
        let wanted = if self.page == Page::Services { self.shown_service() } else { None };
        if self.data.local_services {
            let selected = wanted.filter(|key| self.local_visible && self.following_file.as_ref() == Some(key));
            let file = selected.as_ref().and_then(|key| self.data.services.as_ref()?.get(key)?.launchd.as_ref()?.log.as_deref());
            if let (Some(key), Some(path)) = (selected, file) {
                let subject = format!("file:{key}:{path}");
                if !self.service_log.as_ref().is_some_and(|pane| pane.subject == subject) {
                    self.service_log = Some(LogPane::start(subject, LogKind::File, services::LogSource::File { key, path: path.into() }, self.monitor.as_ref(), cx));
                }
            } else {
                self.following_file = None;
                self.service_log = None;
            }
            return;
        }
        let key = wanted.as_ref().map(|name| format!("service:{name}"));
        if self.service_log.as_ref().map(|pane| &pane.subject) == key.as_ref() {
            return;
        }
        self.service_log = match (wanted, self.data.probe.clone()) {
            (Some(name), Some(probe)) => {
                let source = services::log_source(&name, &probe);
                Some(LogPane::start(format!("service:{name}"), LogKind::Journal, source, self.monitor.as_ref(), cx))
            }
            _ => None,
        };
    }

    fn set_service_filter(&mut self, filter: ServiceFilter, cx: &mut Context<Self>) {
        self.service_filter = filter;
        self.refresh_services(cx);
        cx.notify();
    }

    fn select_service(&mut self, name: String, cx: &mut Context<Self>) {
        self.service_selected = Some(name);
        self.sync_service_log(cx);
        cx.notify();
    }

    /// The service page with this unit selected (a click on 查看服务 in 需要关注). A filter or a search that hides it
    /// is cleared first.
    pub fn show_service(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let known = self.data.services.as_deref().is_some_and(|list| list.get(name).is_some());
        if !known {
            return;
        }
        self.service_filter = ServiceFilter::All;
        self.service_query.update(cx, |input, cx| input.set_value("", window, cx));
        self.service_selected = Some(name.to_string());
        self.page = Page::Services;
        self.refresh_services(cx);
        if let Some(ix) = self.service_rows.iter().position(|row| row.key == name) {
            self.service_scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
        cx.emit(MonitorPanelEvent::PageChanged(Page::Services));
        cx.notify();
    }

    /// Debug review hook: the filter and the search of the list, as the controls would set them.
    #[cfg(debug_assertions)]
    pub fn demo_services(
        &mut self,
        query: &str,
        filter: ServiceFilter,
        select: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.service_filter = filter;
        self.service_query.update(cx, |input, cx| input.set_value(query, window, cx));
        if let Some(name) = select {
            self.service_selected = Some(name.to_string());
        }
        self.refresh_services(cx);
        cx.notify();
    }

    /// Debug review hook: a journal with made-up lines.
    #[cfg(debug_assertions)]
    pub fn demo_service_log(&mut self, lines: Vec<shellmate_monitor::services::LogLine>, cx: &mut Context<Self>) {
        if let Some(name) = self.shown_service() {
            self.service_log = Some(LogPane::fixed(format!("service:{name}"), LogKind::Journal, lines));
            cx.notify();
        }
    }

    // ---- drawing -----------------------------------------------------------------------------

    fn service_row(&self, ix: usize, shown: Option<&str>, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let extra=crate::layout::Responsive::at(self.width).service_extra_columns;
        let row = self.service_rows.get(ix)?;
        let name = row.key.clone();
        let selected = shown == Some(row.key.as_str());
        let (bg, fg) = tone_colors(p, row.tone);
        Some(
            div()
                .id(("service", ix))
                .w_full()
                .h(px(ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(20.))
                .rounded(px(12.))
                .cursor_pointer()
                .when(selected, |d| d.bg(p.accs))
                .when(!selected, |d| d.hover(|d| d.bg(p.card2)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            ellipsis(("service-name",ix),row.name.clone())
                                .font_family(MONO_FONT)
                                .font_weight(FontWeight::BOLD)
                                .text_size(px(13.)),
                        )
                        .child(ellipsis(("service-description",ix),row.description.clone()).text_size(px(12.)).text_color(p.tx2)),
                )
                .child(
                    div().w(px(90.)).flex_none().flex().child(
                        div()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(7.))
                            .bg(bg)
                            .text_color(fg)
                            .text_size(px(11.5))
                            .font_family(MONO_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap().child(row.state.clone()),
                    ),
                )
                .when(extra,|d|d.child(
                    div()
                        .w(px(56.))
                        .flex_none()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if row.autostart_on { p.tx } else { p.tx3 })
                        .child(row.autostart),
                ))
                .when(extra,|d|d.child(
                    div()
                        .w(px(88.))
                        .flex_none()
                        .text_right().whitespace_nowrap()
                        .font_family(MONO_FONT)
                        .text_size(px(12.5))
                        .text_color(if row.memory == "—" { p.tx3 } else { p.tx2 })
                        .child(row.memory.clone()),
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.select_service(name.clone(), cx)))
                .into_any_element(),
        )
    }

    fn service_button(&self, button: &ServiceButton, name: &str, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (bg, fg) = match button.action {
            ServiceAction::Start => (p.accs, p.acc),
            ServiceAction::Stop => (p.errs, p.err),
            _ => (p.card2, p.tx),
        };
        let (action, unit) = (button.action, name.to_string());
        let mut element = div()
            .id(("service-action", action as usize))
            .flex_1()
            .h(px(34.))
            .when(self.data.local_services, |button| button.flex_auto().flex_none())
            .px(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .child(button.label.clone());
        element = if button.enabled {
            element
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(MonitorPanelEvent::Service(action, unit.clone()))))
        } else {
            element.opacity(0.4).cursor_not_allowed()
        };
        if let Some(text) = button.tooltip.clone() {
            let text: SharedString = text.into();
            element = element.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
        }
        element.into_any_element()
    }

    fn mini_card(&self, caption: &'static str, value: String, p: &Palette) -> AnyElement {
        div()
            .flex_1().min_w(px(crate::layout::numeric_width([value.as_str()],7.8,84.)+24.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(12.))
            .bg(p.card2)
            .child(div().text_size(px(11.5)).line_height(px(16.)).text_color(p.tx2).child(caption))
            .child(
                div().whitespace_nowrap().font_family(MONO_FONT).font_weight(FontWeight::BOLD).text_size(px(13.)).line_height(px(17.)).child(value),
            )
            .into_any_element()
    }

    fn render_service_detail(&self, detail: &Detail, stale: bool, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let layout=crate::layout::Responsive::at(self.width);
        let (bg, fg) = tone_colors(p, detail.tone);
        let name = detail.key.clone();
        let switch = &detail.autostart;
        let toggle_action = if switch.on { ServiceAction::Disable } else { ServiceAction::Enable };
        let enabled = switch.enabled;
        let switch_row = div().flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .px(px(14.))
            .py(px(10.))
            .rounded(px(12.))
            .bg(p.card2)
            .child(div().min_w_0().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(if switch.enabled && !self.data.local_services {format!("开机自启 {}",if switch.on{"enable"}else{"disable"})}else{switch.label.clone()}))
            .child({
                let unit = name.clone();
                let mut element = div().id("service-autostart").flex_none().child(
                    Switch::new("service-autostart-switch").checked(switch.on).color(p.ok).disabled(!enabled).on_click(
                        cx.listener(move |_, _: &bool, _, cx| {
                            cx.emit(MonitorPanelEvent::Service(toggle_action, unit.clone()));
                        }),
                    ),
                );
                if let Some(text) = switch.tooltip.clone() {
                    let text: SharedString = text.into();
                    element = element.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
                }
                element
            });

        let probe = self.data.probe.as_deref();
        let log_header = div().flex_none().min_w_0()
            .flex()
            .items_baseline()
            .justify_between()
            .child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).child("日志"))
            .child(
                div().flex_none().min_w_0()
                    .font_family(MONO_FONT)
                    .text_size(px(11.5))
                    .text_color(p.tx3)
                    .child(probe.map_or("journalctl -u -f", services::log_hint)),
            );

        let content = div().id("service-detail-viewport")
            .flex_1().min_h_0().when(layout.services_stacked, |d| d.flex_auto())
            .overflow_y_scroll().track_scroll(&self.service_detail_scroll)
            .when(stale, |d| d.opacity(0.5))
            .flex().flex_col().gap(px(12.))
            .child(
                div().flex_none().min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        ellipsis("service-detail-name",detail.name.clone())
                            .font_family(MONO_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(16.)).line_height(px(21.)),
                    )
                    .child(ellipsis("service-detail-description",detail.description.clone()).text_size(px(12.5)).line_height(px(15.)).text_color(p.tx2))
                    .when(self.data.local_services, |d| d.child(ellipsis("service-target",name.clone()).text_size(px(12.)).text_color(p.tx2)))
                    .when_some(detail.plist.clone(), |d, path| d.child(ellipsis("service-plist",path).text_size(px(12.)).text_color(p.tx2))),
            )
            .child(
                div().flex_none().min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(10.))
                    .gap_y(px(4.))
                    .child(
                        div()
                            .flex_none()
                            .px(px(9.))
                            .py(px(3.))
                            .rounded(px(8.))
                            .bg(bg)
                            .text_color(fg)
                            .text_size(px(12.)).line_height(px(15.))
                            .font_family(MONO_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(detail.state.clone()),
                    )
                    .child(ellipsis("service-since",detail.since.clone()).flex_1().text_size(px(12.)).text_color(p.tx2)),
            )
            .when_some(detail.failure.clone().filter(|why|!self.service_log.as_ref().is_some_and(|log|log.lines.iter().any(|line|line.text==*why))), |d, why| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(9.))
                        .rounded(px(10.))
                        .bg(bg)
                        .text_color(fg)
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .font_family(MONO_FONT)
                        .child(if self.data.local_services { div().children(why.lines().enumerate().map(|(index, line)| ellipsis(("service-error-line",index),line.to_owned()))) } else { div().child(ellipsis("service-failure",why)) }),
                )
            })
            .child(
                div().flex_none().min_w_0()
                    .flex().flex_wrap()
                    .gap(px(8.))
                    .child(self.mini_card("主进程", detail.main_pid.clone(), p))
                    .child(self.mini_card("内存", detail.memory.clone(), p))
                    .when(!self.data.local_services, |d| d.child(self.mini_card("CPU 时间", detail.cpu.clone(), p))),
            )
            .child(
                div().flex_none().min_w_0()
                    .flex()
                    .gap(px(6.))
                    .when(self.data.local_services, |d| d.flex_wrap())
                    .children(detail.buttons.iter().map(|button| self.service_button(button, &name, p, cx))),
            )
            .child(switch_row)
            .when(!self.data.local_services, |d| d.child(log_header))
            .when_some(detail.log.as_ref(), |d, _| {
                let following = self.following_file.as_ref() == Some(&name);
                d.child(div().id("follow-service-file").cursor_pointer().text_color(p.acc).text_size(px(13.))
                    .child(if following { "关闭日志" } else { "跟随日志" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.following_file = (!following).then(|| name.clone());
                        this.sync_service_log(cx); cx.notify();
                    })))
            })
            .when(!self.data.local_services || self.service_log.is_some(), |d| d.child(log_box(self.service_log.as_ref(), "选中服务后在这里跟随它的日志", 40, p).flex_1().min_h(px(120.))));
        div().id("service-detail")
            .w(px(layout.service_detail_width)).when(layout.services_stacked, |d| d.w_full())
            .flex_none().min_h_0().when(layout.services_stacked, |d| d.min_h(px(480.)))
            .flex().flex_col().p(px(18.))
            .bg(p.card).rounded(CARD_RADIUS).shadow(card_shadow(p))
            .child(div().relative().flex_1().min_h_0().flex().flex_col()
                .when(layout.services_stacked, |d| d.flex_auto())
                .child(content)
                .child(crate::ui::scroll::scroll_fades(self.service_detail_scroll.clone(), p.card)))
            .into_any_element()
    }

    pub(super) fn render_services(
        &self,
        stale: bool,
        banner: Option<Banner>,
        clock: super::Clock,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let layout=crate::layout::Responsive::at(self.width);
        let Some(list) = self.data.services.clone() else {
            let page = crate::monitor::model::loading("服务列表");
            return self.message_card(&page.title, &page.detail, false, p, cx);
        };
        let opacity = clock.fade("services-layout", (layout.services_stacked, layout.service_extra_columns), window, cx);
        let filters = ServiceFilter::ALL
            .iter()
            .map(|filter| SegmentItem::new(filter.label(), *filter == self.service_filter))
            .collect::<Vec<_>>();
        let on_pick = {
            let panel = cx.entity();
            move |ix: usize, _: &mut Window, cx: &mut gpui::App| {
                panel.update(cx, |this, cx| this.set_service_filter(ServiceFilter::ALL[ix], cx));
            }
        };
        let toolbar = div()
            .flex().flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .pt(px(12.))
            .pb(px(10.))
            .pl(px(18.))
            .pr(px(14.))
            .child(
                div()
                    .w(px(if layout.service_extra_columns{260.}else{160.})).min_w_0()
                    .h(px(34.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(11.))
                    .bg(p.card2)
                    .text_size(px(13.))
                    .child(crate::icons::AppIcon::Search.at(14.).text_color(p.tx3))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.service_query).appearance(false).bordered(false).p_0().h(px(18.)).text_size(px(13.)).relative().left(px(2.5))),
                    ),
            )
            .child(segmented("service-filter", filters, on_pick, cx).flex_none())
            .when(self.data.local_services, |d| d.child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).child("显示系统服务")
                .child(Switch::new("show-apple-services").checked(self.show_apple).on_click(cx.listener(|this, value: &bool, _, cx| {
                    this.show_apple = *value; this.refresh_services(cx); cx.notify();
                })))))
            .child(div().flex_1().min_w_0())
            .child(div().text_size(px(12.)).text_color(p.tx2).child(format!(
                "{} / {} 个服务",
                self.service_rows.len(),
                list.services.iter().filter(|service| self.show_apple || !service.apple()).count()
            )));

        let heading = |text: &'static str, width: Option<f32>, right: bool| {
            let cell = div().flex_none().child(text);
            let cell = if right { cell.text_right() } else { cell };
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
            .text_size(px(11.5))
            .text_color(p.tx2)
            .border_b_1()
            .border_color(p.line)
            .child(heading("服务", None, false))
            .child(heading("状态", Some(90.), false))
            .when(layout.service_extra_columns,|d|d.child(heading("自启",Some(56.),false)))
            .when(layout.service_extra_columns,|d|d.child(heading("内存",Some(88.),true)));

        let shown = self.shown_service();
        let shown_for_rows = shown.clone();
        let rows = uniform_list(
            "service-rows",
            self.service_rows.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let p = cx.palette().clone();
                range.filter_map(|ix| this.service_row(ix, shown_for_rows.as_deref(), &p, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.service_scroll)
        .size_full();

        let probe = self.data.probe.as_deref();
        let uptime = self.data.snapshot.as_deref().map(|s| s.host.uptime_secs).filter(|secs| *secs > 0.0);
        let detail =
            shown.as_deref().and_then(|name| list.get(name)).map(|service| services::detail(service, uptime, probe));

        let list_card = div()
            .flex_1().when(layout.services_stacked,|d|d.h(px(300.)).flex_none().overflow_hidden())
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(toolbar.when(stale, |d| d.opacity(0.5)))
            .child(columns.when(stale, |d| d.opacity(0.5)))
            .child(div().relative().flex_1().min_h_0().px(px(8.)).py(px(6.)).child(div().size_full().when(stale, |d| d.opacity(0.5)).child(if self.service_rows.is_empty() {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .text_color(p.tx3)
                    .child("没有符合条件的服务")
                    .into_any_element()
            } else {
                rows.into_any_element()
            })).when(!self.service_rows.is_empty(), |d| d.child(crate::ui::scroll::scroll_fades(self.service_scroll.0.borrow().base_handle.clone(), p.card))));

        let page = div()
            .id("monitor-services").min_w_0().when(layout.services_stacked,|d|d.overflow_y_scroll())
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
                    .flex().min_w_0().opacity(opacity).when(layout.services_stacked,|d|d.flex_col().flex_none())
                    .gap(px(10.))
                    .child(list_card)
                    .children(detail.map(|detail| self.render_service_detail(&detail, stale, p, cx))),
            );
        if layout.services_stacked {
            crate::ui::scroll::page_scroll("services-fades", page, &self.services_scroll, p.win, 0.).into_any_element()
        } else {
            page.into_any_element()
        }
    }
}
