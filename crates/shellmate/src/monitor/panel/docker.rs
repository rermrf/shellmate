//! The Docker page: the header card, the containers by compose project with their ports, numbers and buttons,
//! and the log that opens under a container.

use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::tooltip::Tooltip;
use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, px};
use shellmate_monitor::commands::ContainerAction;
use shellmate_monitor::docker::Container;

use super::logs::{LogKind, LogPane, log_box};
use super::{MonitorPanel, MonitorPanelEvent, tone_text};
use crate::monitor::containers::{self, ContainerRow, Group, PortChip};
use crate::monitor::model::{Banner, Page, Tone};
use crate::monitor::services::LogSource;
use crate::theme::{ActivePalette as _, CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::{card_shadow, ellipsis};

/// The subject of the pane of a container whose page is not in front: nothing follows, nothing is kept.
const CLOSED: &str = "closed";

/// How many lines of a log are drawn under a container.
const LOG_LINES: usize = 5;

fn log_key(container: &Container) -> String {
    format!("container:{}:{}", container.name, container.state.is_running())
}

/// What a button of a container row looks like.
struct RowButton {
    id: &'static str,
    label: &'static str,
    enabled: bool,
    tone: Tone,
    open: bool,
}

impl MonitorPanel {
    /// The container called `name` in the newest list.
    fn container(&self, name: &str) -> Option<&Container> {
        self.data.docker.as_deref()?.containers.iter().find(|c| c.name == name)
    }

    fn runtime(&self) -> Option<&shellmate_monitor::ContainerRuntime> {
        self.data.probe.as_deref()?.usable_container_runtime()
    }

    /// Keeps the open log in step with the page and with the container: it follows only while the Docker page
    /// is in front, starts again when the container starts or stops (a stopped one shows its last lines, a
    /// running one follows), and closes when the container is gone.
    pub(super) fn sync_container_log(&mut self, cx: &mut Context<Self>) {
        let Some((name, pane)) = self.container_log.as_ref() else { return };
        let name = name.clone();
        let Some(container) = self.container(&name).cloned() else {
            self.container_log = None;
            return;
        };
        if self.page != Page::Docker {
            // Not looking: the channel on the server is closed, and the lines are not kept.
            if pane.subject != CLOSED {
                self.container_log = Some((name, LogPane::fixed(CLOSED.to_string(), LogKind::Docker, Vec::new())));
            }
            return;
        }
        if pane.subject == log_key(&container) {
            return;
        }
        self.start_container_log(&container, cx);
    }

    fn start_container_log(&mut self, container: &Container, cx: &mut Context<Self>) {
        let source = match self.runtime() {
            Some(runtime) => containers::log_source(container, runtime),
            None => LogSource::Denied("这台主机没有可用的容器工具"),
        };
        let pane = LogPane::start(log_key(container), LogKind::Docker, source, self.monitor.as_ref(), cx);
        self.container_log = Some((container.name.clone(), pane));
    }

    /// 日志 on a row: opens the log under the container, or closes it.
    fn toggle_container_log(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.container_log.as_ref().is_some_and(|(open, _)| open == name) {
            self.container_log = None;
        } else if let Some(container) = self.container(name).cloned() {
            self.start_container_log(&container, cx);
        }
        cx.notify();
    }

    /// The Docker page with the log of this container open (a click on 查看容器 in 需要关注).
    pub fn show_container(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(container) = self.container(name).cloned() else { return };
        self.page = Page::Docker;
        self.start_container_log(&container, cx);
        cx.emit(MonitorPanelEvent::PageChanged(Page::Docker));
        cx.notify();
    }

    /// Debug review hook: the log under a container with made-up lines.
    #[cfg(debug_assertions)]
    pub fn demo_container_log(
        &mut self,
        name: &str,
        lines: Vec<shellmate_monitor::services::LogLine>,
        cx: &mut Context<Self>,
    ) {
        if let Some(container) = self.container(name).cloned() {
            self.container_log = Some((name.to_string(), LogPane::fixed(log_key(&container), LogKind::Docker, lines)));
            cx.notify();
        }
    }

    // ---- drawing -----------------------------------------------------------------------------

    fn chip_button(&self, ix: (usize, usize), chip: &PortChip, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (bg, fg) = if chip.forwarded { (p.oks, p.ok) } else { (p.accs, p.acc) };
        let target = chip.target.clone();
        let tooltip: SharedString = format!("{}\n{}", chip.label, chip.tooltip).into();
        div()
            .id(("port-chip", ix.0 * 100 + ix.1))
            .px(px(8.))
            .py(px(3.))
            .rounded(px(7.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(11.5))
            .font_family(MONO_FONT)
            .font_weight(FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(|d| d.opacity(0.85))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .flex_none()
            .child(chip.label.clone())
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(MonitorPanelEvent::ForwardPort(target.clone()))))
            .into_any_element()
    }

    /// A button of a container row; `press` is what it does.
    fn row_button(
        &self,
        spec: RowButton,
        ix: usize,
        press: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (bg, fg) = match (spec.tone, spec.open) {
            (_, true) => (p.accs, p.acc),
            (Tone::Err, _) => (p.errs, p.err),
            (Tone::Plain, _) => (p.accs, p.acc),
            _ => (p.card2, p.tx),
        };
        let mut element = div()
            .id((spec.id, ix))
            .w(px(if spec.label == "×" { 28. } else { 44. }))
            .h(px(28.))
            .px(px(10.))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(8.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.))
            .line_height(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(spec.label);
        element = if spec.enabled {
            element
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .on_click(cx.listener(move |this, _, _, cx| press(this, cx)))
        } else {
            element.opacity(0.4).cursor_not_allowed()
        };
        element.into_any_element()
    }

    fn container_row(
        &self,
        group: usize,
        index: usize,
        row: &ContainerRow,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ix = group * 100 + index;
        let compact = crate::layout::Responsive::at(self.width).docker_compact;
        let name_width = if self.width >= 1100. { 241. } else { self.width * 0.20 };
        let status_width = if self.width >= 1100. { 199. } else { self.width * 0.16 };
        let cpu_width = crate::layout::numeric_width([row.cpu.as_str()], 7.2, 52.);
        let memory_width = crate::layout::numeric_width([row.memory.as_str()], 7.2, 120.);
        let labels = row.ports.iter().map(|port| port.label.as_str()).collect::<Vec<_>>();
        let inline_ports = crate::layout::inline_container_ports(self.width, cpu_width, memory_width, &labels);
        let log_open =
            self.container_log.as_ref().is_some_and(|(name, pane)| name == &row.name && pane.subject != CLOSED);
        let run = if row.running { ContainerAction::Restart } else { ContainerAction::Start };
        let dot_color = match row.dot {
            Tone::Ok => p.ok,
            Tone::Warn => p.warn,
            _ => p.tx3,
        };
        let status_color = match row.status_tone {
            Tone::Plain => p.tx2,
            tone => tone_text(p, tone),
        };
        let name = row.name.clone();
        let spec = |id, label, enabled, tone, open| RowButton { id, label, enabled, tone, open };
        let mut buttons = div()
            .flex_none()
            .flex()
            .gap(px(4.))
            .child({
                let name = name.clone();
                self.row_button(
                    spec("container-exec", "终端", row.can_exec, Tone::Plain, false),
                    ix,
                    move |_, cx| cx.emit(MonitorPanelEvent::ContainerTerminal(name.clone())),
                    p,
                    cx,
                )
            })
            .child({
                let name = name.clone();
                self.row_button(
                    spec("container-log", "日志", true, Tone::Off, log_open),
                    ix,
                    move |this, cx| this.toggle_container_log(&name, cx),
                    p,
                    cx,
                )
            });
        if compact {
            let this = cx.entity().downgrade();
            let name = name.clone();
            let label = row.run_label;
            let stop = row.can_stop;
            let remove = row.can_remove;
            buttons = buttons.child(
                Button::new(("container-more", ix)).ghost().label("⋯").w(px(28.)).h(px(28.)).dropdown_menu(
                    move |mut menu, _, _| {
                        for (label, action, enabled) in [
                            (label, run, true),
                            ("停止", ContainerAction::Stop, stop),
                            ("删除", ContainerAction::Remove, remove),
                        ] {
                            let this = this.clone();
                            let name = name.clone();
                            menu = menu.item(PopupMenuItem::new(label).disabled(!enabled).on_click(move |_, _, cx| {
                                this.update(cx, |_, cx| cx.emit(MonitorPanelEvent::Container(action, name.clone())))
                                    .ok();
                            }));
                        }
                        menu
                    },
                ),
            );
        } else {
            for (id, label, action, enabled, tone) in [
                ("container-run", row.run_label, run, true, Tone::Off),
                ("container-stop", "停止", ContainerAction::Stop, row.can_stop, Tone::Off),
                ("container-remove", "×", ContainerAction::Remove, row.can_remove, Tone::Err),
            ] {
                let name = name.clone();
                buttons = buttons.child(self.row_button(
                    spec(id, label, enabled, tone, false),
                    ix,
                    move |_, cx| cx.emit(MonitorPanelEvent::Container(action, name.clone())),
                    p,
                    cx,
                ));
            }
        }
        let stats = || {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(12.))
                .whitespace_nowrap()
                .font_family(MONO_FONT)
                .text_size(px(12.))
                .line_height(px(16.))
                .child(div().w(px(cpu_width)).flex_none().text_right().child(row.cpu.clone()))
                .child(div().w(px(memory_width)).flex_none().text_color(p.tx2).child(row.memory.clone()))
        };
        let line = div()
            .w_full()
            .min_w_0()
            .h(px(58.))
            .flex_none()
            .px(px(20.))
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().size(px(8.)).flex_none().rounded_full().bg(dot_color))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .when(compact, |d| d.flex_1())
                    .when(!compact, |d| d.w(px(name_width)).flex_none())
                    .child(
                        ellipsis(("container-name", ix), name.clone())
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(13.))
                            .line_height(px(16.)),
                    )
                    .child(
                        ellipsis(("container-image", ix), row.image.clone())
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .line_height(px(15.))
                            .text_color(p.tx2),
                    ),
            )
            .when(!compact, |d| {
                d.child(
                    ellipsis(("container-status", ix), row.status.clone())
                        .w(px(status_width))
                        .flex_none()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(status_color),
                )
            })
            .when(!compact, |d| {
                d.child(div().flex_1().min_w_0().flex().flex_wrap().gap(px(6.)).when(inline_ports, |d| {
                    d.children(row.ports.iter().enumerate().map(|(i, port)| self.chip_button((ix, i), port, p, cx)))
                }))
                .child(stats())
            })
            .child(buttons);
        let mut result = div().id(("container", ix)).w_full().min_w_0().flex_none().flex().flex_col().child(line);
        if compact {
            result = result.child(
                div()
                    .ml(px(38.))
                    .mr(px(20.))
                    .pb(px(8.))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        ellipsis(("container-status", ix), row.status.clone())
                            .flex_1()
                            .min_w(px(100.))
                            .text_size(px(12.))
                            .text_color(status_color),
                    )
                    .child(stats()),
            );
        }
        if !inline_ports && !row.ports.is_empty() {
            result = result.child(
                div()
                    .ml(px(38.))
                    .mr(px(20.))
                    .pb(px(10.))
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .children(row.ports.iter().enumerate().map(|(i, port)| self.chip_button((ix, i), port, p, cx))),
            );
        }
        if log_open {
            let caption = self.container(&name).map_or("", containers::log_caption);
            result = result.child(
                div()
                    .ml(px(38.))
                    .mr(px(20.))
                    .mb(px(6.))
                    .mt(px(2.))
                    .min_w_0()
                    .rounded(px(12.))
                    .bg(shellmate_term::colors::to_hsla(cx.terminal_style().palette.background))
                    .px(px(12.))
                    .py(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        ellipsis(("container-log-caption", ix), caption)
                            .text_size(px(11.5))
                            .line_height(px(18.))
                            .font_family(MONO_FONT)
                            .text_color(p.acc),
                    )
                    .child(
                        log_box(self.container_log.as_ref().map(|(_, pane)| pane), "没有日志", LOG_LINES, p)
                            .p_0()
                            .bg(gpui::transparent_black())
                            .h(px(LOG_LINES as f32 * 18.)),
                    ),
            );
        }
        result.into_any_element()
    }

    fn group_card(&self, index: usize, group: &Group, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .flex_none()
            .py(px(10.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(20.))
                    .py(px(6.))
                    .child(
                        ellipsis(("container-group", index), group.name.clone())
                            .max_w(px(280.))
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(14.))
                            .line_height(px(17.)),
                    )
                    .child(
                        ellipsis(("container-config", index), group.subtitle.clone())
                            .flex_1()
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .line_height(px(15.))
                            .text_color(p.tx2),
                    ),
            )
            .children(group.rows.iter().enumerate().map(|(i, row)| self.container_row(index, i, row, p, cx)))
            .into_any_element()
    }

    pub(super) fn render_docker(
        &self,
        stale: bool,
        banner: Option<Banner>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (Some(view), Some(runtime)) = (self.data.docker.clone(), self.runtime().cloned()) else {
            let page = crate::monitor::model::loading("容器列表");
            return self.message_card(&page.title, &page.detail, false, p, cx);
        };
        let header = containers::header(&view, &runtime);
        let groups = containers::groups(&view, &self.data.forwards);
        let stat = |value: String, caption: &'static str| {
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(4.))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(16.))
                        .line_height(px(18.))
                        .whitespace_nowrap()
                        .child(value),
                )
                .child(div().text_size(px(12.)).line_height(px(17.)).text_color(p.tx2).child(caption))
        };
        let (running, total) = containers::counts(&view);
        let prune_tip: SharedString = header.prune_tooltip.clone().into();
        let top = div()
            .min_w_0()
            .w_full()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(18.))
            .pl(px(18.))
            .pr(px(14.))
            .py(px(14.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(14.))
                            .line_height(px(17.))
                            .child(header.title.clone())
                            .whitespace_nowrap(),
                    )
                    .child(
                        ellipsis("docker-subtitle", header.subtitle.clone())
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .text_color(p.tx2),
                    ),
            )
            .child(stat(format!("{running} / {total} 个容器运行中"), "容器"))
            .child(stat(header.images.clone(), "镜像"))
            .child(stat(header.volumes.clone(), "数据卷"))
            .child(
                div()
                    .id("docker-prune")
                    .h(px(34.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .rounded(px(10.))
                    .bg(p.card2)
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.85))
                    .tooltip(move |window, cx| Tooltip::new(prune_tip.clone()).build(window, cx))
                    .child("清理")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(MonitorPanelEvent::PruneImages))),
            );

        let error = header.error.clone().map(|why| {
            div()
                .flex()
                .flex_none()
                .px(px(16.))
                .py(px(10.))
                .rounded(px(14.))
                .bg(p.warns)
                .text_color(p.warn)
                .text_size(px(12.))
                .line_height(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("读取容器时出错（显示的是上一次的结果）：{why}"))
        });

        div()
            .id("monitor-docker")
            .w_full()
            .min_w_0()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(banner.map(|b| self.banner(&b, p, cx)))
            .children(error)
            .child(top)
            .child(
                div()
                    .id("docker-groups")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.docker_scroll)
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .pb(px(10.))
                    .when(stale, |d| d.opacity(0.5))
                    .children(groups.iter().enumerate().map(|(i, group)| self.group_card(i, group, p, cx)))
                    .when(groups.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .py(px(40.))
                                .text_center()
                                .text_size(px(13.))
                                .text_color(p.tx3)
                                .child("这台主机上还没有容器"),
                        )
                    }),
            )
            .into_any_element()
    }
}
