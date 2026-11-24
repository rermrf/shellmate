use super::Drawer;
use crate::{
    ai::{
        context_view::{self, CompactAction},
        model::ContextMeter,
    },
    theme::ActivePalette,
};
use gpui::{
    component::{Disableable, button::Button},
    prelude::*,
    *,
};
use shellmate_ai::{ContextBreakdown, ContextBudget, ContextSnapshot, context::ContextCategory};

impl Drawer {
    pub(super) fn open_context(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.context_details().is_none() {
            self.local(
                super::slash_ui::LocalBody::Hint("请先配置 AI 并绑定会话".into()),
                cx,
            );
            return;
        }
        crate::ai::mode_popover::show(
            crate::ai::mode_popover::Kind::Context,
            &mut self.permission_menu,
            &mut self.state.context_popover,
            &mut self.popup,
        );
        w.focus(&self.context_focus, cx);
        cx.notify();
    }
    fn context_details(&self) -> Option<ContextBreakdown> {
        if !context_view::visible(self.session.is_some(), self.configured) {
            return None;
        }
        let settings = self.db.borrow().ai_settings().unwrap_or_default();
        let mut budget = self.state.run_budget.map(|(b, _)| b).unwrap_or_else(|| {
            self.state.run_config.as_ref().map_or_else(
                || settings.context_budget(),
                |c| {
                    ContextBudget::new(
                        shellmate_ai::resolve_window(&c.provider, &c.model, c.context_window).0,
                        c.autocompact_window,
                        c.max_output_tokens.resolve(&c.provider, &c.model),
                    )
                },
            )
        });
        if let Some(meter) = self.state.model.meter {
            if self.state.run_budget.is_none() {
                budget = ContextBudget::new(
                    meter.window,
                    settings.autocompact_window,
                    settings
                        .max_output_tokens
                        .resolve(&settings.provider, &settings.model),
                );
            }
            budget.window = meter.window;
            budget.trigger = meter.trigger;
            budget.reserve = budget.auto.saturating_sub(meter.trigger);
        }
        let limits = self
            .state
            .run_limits
            .unwrap_or_else(|| settings.limits())
            .exec;
        let log = &self.state.model.conversation;
        if log.entries().is_empty() {
            Some(ContextBreakdown::for_session_with_memory(
                budget,
                &self.capabilities,
                &ContextSnapshot {
                    label: self.label.clone(),
                    protocol: self.protocol.clone(),
                    text: String::new(),
                },
                limits,
                &shellmate_ai::memory::Snapshot::load(
                    &self.memory_files(),
                    &self.label,
                    settings.auto_memory,
                ),
            ))
        } else {
            log.breakdown_with_limits(budget, &self.capabilities, limits)
                .ok()
        }
    }
    pub(super) fn context_indicator(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let b = self.context_details()?;
        let p = cx.palette().clone();
        let meter = ContextMeter {
            used: b.total,
            window: b.budget.window,
            trigger: b.budget.trigger,
        };
        let fraction = (b.total as f64 / f64::from(b.budget.window.max(1))).clamp(0., 1.) as f32;
        let anchor_bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<Pixels>>));
        let measured = anchor_bounds.clone();
        let mut anchor = div()
            .relative()
            .flex_none()
            .on_children_prepainted(move |bounds, _, _| {
                measured.set(bounds.first().copied());
            })
            .child(
                div()
                    .id("ai-context-indicator")
                    .h(px(if self.capabilities.available() {
                        26.
                    } else {
                        17.
                    }))
                    .line_height(px(15.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .px(px(4.))
                    .text_size(px(11.5))
                    .text_color(p.tx2)
                    .cursor_pointer()
                    .child(ring(
                        fraction,
                        if meter.amber() { p.warn } else { p.acc },
                        p.card3,
                    ))
                    .child(context_view::percent(b.total, b.budget.window))
                    .on_click(cx.listener(|s, _, w, cx| {
                        s.state.context_popover.toggle();
                        if s.state.context_popover.open {
                            crate::ai::mode_popover::show(
                                crate::ai::mode_popover::Kind::Context,
                                &mut s.permission_menu,
                                &mut s.state.context_popover,
                                &mut s.popup,
                            );
                            w.focus(&s.context_focus, cx);
                        } else {
                            s.input.focus_handle(cx).focus(w, cx);
                        }
                        cx.notify();
                    })),
            );
        if self.state.context_popover.open {
            let mut popover = div()
                .id("ai-context-popover")
                .key_context(super::CONTEXT_POPOVER_KEYS)
                .track_focus(&self.context_focus)
                .absolute()
                .bottom(relative(1.))
                .right_0()
                .mb(px(6.))
                .w(px(310.))
                .p(px(12.))
                .rounded(px(16.))
                .bg(p.card)
                .border_1()
                .border_color(p.line)
                .shadow(crate::ui::card_shadow(&p))
                .flex()
                .flex_col()
                .gap(px(10.))
                .text_size(px(11.5))
                .text_color(p.tx2)
                .on_mouse_down_out(cx.listener(move |s, e: &MouseDownEvent, _, cx| {
                    if !anchor_bounds.get().is_some_and(|b| b.contains(&e.position)) {
                        s.state.context_popover.close();
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.tx)
                                .child("上下文"),
                        )
                        .child(digits().child(format!(
                            "{} / {}",
                            context_view::tokens(b.total),
                            context_view::tokens(b.budget.window as usize)
                        ))),
                );
            let terminal = cx.terminal_style().palette;
            let colors = [
                p.acc,
                shellmate_term::colors::to_hsla(terminal.ansi[5]),
                shellmate_term::colors::to_hsla(terminal.ansi[6]),
                p.ok,
                p.warn,
                p.err,
                shellmate_term::colors::to_hsla(terminal.ansi[4]),
            ];
            let categories = b.categories();
            let denominator = (b.budget.window as usize).max(b.total).max(1) as f32;
            popover = popover.child(
                div()
                    .flex()
                    .h(px(8.))
                    .rounded(px(5.))
                    .overflow_hidden()
                    .bg(p.card3)
                    .children(
                        categories
                            .iter()
                            .enumerate()
                            .filter(|(_, (_, n))| *n > 0)
                            .map(|(i, (_, n))| {
                                div()
                                    .flex_none()
                                    .h_full()
                                    .w(relative(*n as f32 / denominator))
                                    .bg(colors[i])
                            }),
                    ),
            );
            for (i, (kind, n)) in categories
                .into_iter()
                .enumerate()
                .filter(|(_, (_, n))| *n > 0)
            {
                let (label, sub) = match kind {
                    ContextCategory::System => ("系统指令", None),
                    ContextCategory::Tools => {
                        ("工具定义", Some(format!("{} 个工具", b.tool_count)))
                    }
                    ContextCategory::Terminal => ("终端上下文", Some("最近 40 行".into())),
                    ContextCategory::Messages => ("对话消息", None),
                    ContextCategory::Results => ("工具结果", None),
                    ContextCategory::Summary => ("压缩摘要", None),
                    ContextCategory::Memory => ("说明与记忆", None),
                };
                popover = popover.child(usage_row(label, sub, n, b.budget.window, colors[i]));
            }
            popover = popover
                .child(usage_row(
                    "剩余",
                    None,
                    b.remaining(),
                    b.budget.window,
                    p.card3,
                ))
                .when(b.provider_usage, |d| {
                    d.child(
                        div()
                            .text_size(px(10.5))
                            .text_color(p.tx3)
                            .child("各项为估算"),
                    )
                });
            let action = context_view::compact_action(
                self.state.model.conversation.entries().is_empty(),
                self.state.model.compacting || self.state.model.manual_compaction,
                self.state.model.busy || self.state.pending,
            );
            popover = popover.child(
                div()
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(p.line)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.5))
                            .text_color(p.tx3)
                            .whitespace_normal()
                            .child(format!(
                                "将在约 {} 时自动压缩",
                                context_view::tokens(b.budget.trigger as usize)
                            )),
                    )
                    .child(
                        Button::new("ai-context-compact")
                            .label("立即压缩")
                            .h(px(24.))
                            .text_size(px(11.))
                            .disabled(action == CompactAction::Disabled)
                            .on_click(cx.listener(|s, _, w, cx| {
                                let action = context_view::compact_action(
                                    s.state.model.conversation.entries().is_empty(),
                                    s.state.model.compacting || s.state.model.manual_compaction,
                                    s.state.model.busy || s.state.pending,
                                );
                                if action != CompactAction::Disabled {
                                    s.state.context_popover.close();
                                    s.input.focus_handle(cx).focus(w, cx);
                                    s.compact_context(None, w, cx);
                                }
                            })),
                    ),
            );
            anchor = anchor.child(popover);
        }
        Some(anchor.into_any_element())
    }
}
fn digits() -> Div {
    div().font_features(FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)])))
}
fn usage_row(label: &'static str, sub: Option<String>, n: usize, window: u32, color: Hsla) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(7.))
        .child(crate::ui::dot(color, 6.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .whitespace_nowrap()
                .child(div().flex_none().child(label))
                .children(sub.map(|s| {
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .text_ellipsis()
                        .text_size(px(10.))
                        .opacity(0.65)
                        .child(s)
                })),
        )
        .child(
            digits()
                .flex_none()
                .text_right()
                .child(context_view::tokens(n)),
        )
        .child(
            digits()
                .flex_none()
                .w(px(34.))
                .text_right()
                .child(context_view::percent(n, window)),
        )
}
fn ring(fraction: f32, color: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, w, _| {
            for (part, color) in [(1., track), (fraction, color)] {
                if part <= 0. {
                    continue;
                }
                let mut path = PathBuilder::stroke(px(2.));
                let center = bounds.center();
                let radius = 6.;
                let count = (64. * part).ceil() as usize;
                for i in 0..=count {
                    let angle = std::f32::consts::TAU * part * i as f32 / count as f32
                        - std::f32::consts::FRAC_PI_2;
                    let p = point(
                        center.x + px(radius * angle.cos()),
                        center.y + px(radius * angle.sin()),
                    );
                    if i == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                if let Ok(path) = path.build() {
                    w.paint_path(path, color);
                }
            }
        },
    )
    .size(px(16.))
}
