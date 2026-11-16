//! The Docker page: the header card, the containers by compose project with their ports, numbers and buttons,
//! and the log that opens under a container.

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
use crate::theme::{CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::card_shadow;

/// The subject of the pane of a container whose page is not in front: nothing follows, nothing is kept.
const CLOSED: &str = "closed";

/// How many lines of a log are drawn under a container.
const LOG_LINES: usize = 8;

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
        let tooltip: SharedString = chip.tooltip.clone().into();
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
            .h(px(28.))
            .px(px(10.))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(8.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.))
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
        let name = row.name.clone();
        let log_open =
            self.container_log.as_ref().is_some_and(|(open, pane)| open == &row.name && pane.subject != CLOSED);
        let dot_color = match row.dot {
            Tone::Ok => p.ok,
            Tone::Warn => p.warn,
            _ => p.tx3,
        };
        let run_action = if row.running { ContainerAction::Restart } else { ContainerAction::Start };
        let spec = |id, label, enabled, tone, open| RowButton { id, label, enabled, tone, open };
        let buttons = div()
            .flex()
            .gap(px(6.))
            .flex_none()
            .child({
                let name = name.clone();
                let press = move |_: &mut Self, cx: &mut Context<Self>| {
                    cx.emit(MonitorPanelEvent::ContainerTerminal(name.clone()))
                };
                self.row_button(spec("container-exec", "终端", row.can_exec, Tone::Plain, false), ix, press, p, cx)
            })
            .child({
                let name = name.clone();
                let press = move |this: &mut Self, cx: &mut Context<Self>| this.toggle_container_log(&name, cx);
                self.row_button(spec("container-logs", "日志", true, Tone::Off, log_open), ix, press, p, cx)
            })
            .child({
                let name = name.clone();
                let press = move |_: &mut Self, cx: &mut Context<Self>| {
                    cx.emit(MonitorPanelEvent::Container(run_action, name.clone()))
                };
                self.row_button(spec("container-run", row.run_label, true, Tone::Off, false), ix, press, p, cx)
            })
            .child({
                let name = name.clone();
                let press = move |_: &mut Self, cx: &mut Context<Self>| {
                    cx.emit(MonitorPanelEvent::Container(ContainerAction::Stop, name.clone()))
                };
                self.row_button(spec("container-stop", "停止", row.can_stop, Tone::Off, false), ix, press, p, cx)
            })
            .child({
                let name = name.clone();
                let press = move |_: &mut Self, cx: &mut Context<Self>| {
                    cx.emit(MonitorPanelEvent::Container(ContainerAction::Remove, name.clone()))
                };
                self.row_button(spec("container-rm", "×", row.can_remove, Tone::Err, false), ix, press, p, cx)
            });
        let line = div()
            .flex()
            .items_center()
            .gap(px(14.))
            .h(px(56.))
            .px(px(20.))
            .child(div().size(px(9.)).flex_none().rounded_full().bg(dot_color))
            .child(
                div()
                    .w(px(220.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().truncate().font_weight(FontWeight::BOLD).text_size(px(13.5)).child(row.name.clone()))
                    .child(
                        div()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .text_color(p.tx2)
                            .child(row.image.clone()),
                    ),
            )
            .child(
                div()
                    .w(px(180.))
                    .flex_none()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(match row.status_tone {
                        Tone::Plain => p.tx2,
                        other => tone_text(p, other),
                    })
                    .child(row.status.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .children(row.ports.iter().enumerate().map(|(i, chip)| self.chip_button((ix, i), chip, p, cx))),
            )
            .child(
                div()
                    .w(px(52.))
                    .flex_none()
                    .text_right()
                    .font_family(MONO_FONT)
                    .text_size(px(12.5))
                    .child(row.cpu.clone()),
            )
            .child(
                div()
                    .w(px(120.))
                    .flex_none()
                    .text_right()
                    .font_family(MONO_FONT)
                    .text_size(px(12.5))
                    .text_color(p.tx2)
                    .child(row.memory.clone()),
            )
            .child(buttons);
        let pane = log_open.then(|| {
            let container = self.container(&row.name);
            let caption = container.map_or("", containers::log_caption);
            div()
                .mx(px(20.))
                .mb(px(12.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(p.acc).child(caption))
                .child(
                    log_box(self.container_log.as_ref().map(|(_, pane)| pane), "没有日志", LOG_LINES, p)
                        .h(px(24. + LOG_LINES as f32 * 21.)),
                )
        });
        div().id(("container", ix)).flex().flex_col().child(line).children(pane).into_any_element()
    }

    fn group_card(&self, index: usize, group: &Group, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
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
                    .items_baseline()
                    .gap(px(10.))
                    .px(px(20.))
                    .py(px(8.))
                    .child(div().font_weight(FontWeight::BOLD).text_size(px(14.)).child(group.name.clone()))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .text_color(p.tx2)
                            .child(group.subtitle.clone()),
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
                .items_end()
                .gap(px(1.))
                .child(div().font_weight(FontWeight::BOLD).text_size(px(15.)).child(value))
                .child(div().text_size(px(11.5)).text_color(p.tx2).child(caption))
        };
        let (running, total) = containers::counts(&view);
        let prune_tip: SharedString = header.prune_tooltip.clone().into();
        let top = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(28.))
            .px(px(20.))
            .py(px(14.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().font_weight(FontWeight::BOLD).text_size(px(15.)).child(header.title.clone()))
                    .child(div().text_size(px(12.)).text_color(p.tx2).child(header.subtitle.clone())),
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
                    .text_size(px(12.5))
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
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("读取容器时出错（显示的是上一次的结果）：{why}"))
        });

        div()
            .id("monitor-docker")
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
