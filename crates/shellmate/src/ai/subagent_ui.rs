//! Compact links into independently browsable child conversations.
use super::{Drawer, agent_ui};
use crate::ai::{subagent_card as card, subagent_view::View};
use crate::theme::ActivePalette;
use gpui::{
    base::{SelectableText, TextSelectionHandle},
    prelude::*,
    *,
};
use shellmate_ai::SubagentStatus;
use std::collections::BTreeMap;
impl Drawer {
    pub(super) fn prepare_subagent_cards(&mut self) -> BTreeMap<(usize, usize), String> {
        let ids = card::projected(&self.state.model.conversation, &mut self.state.subagents);
        for view in self.state.subagents.values_mut() {
            view.load_report(&self.data_root, &self.state.id);
        }
        ids
    }
    pub(super) fn subagent_card(
        &self,
        view: &View,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = cx.palette().clone();
        let running = view.status == SubagentStatus::Running;
        let scope = format!("{}:subagent:{}", self.state.id, view.id);
        let seconds = if running && !self.fixture {
            view.started.elapsed().as_secs()
        } else {
            view.duration_ms / 1000
        };
        let (status, color) = card::status(&view.status, view.steps, seconds);
        let color = color.resolve(&p);
        let target = view.id.clone();
        let title = div()
            .id(SharedString::from(format!("{scope}:title-link")))
            .cursor_pointer()
            .child(selectable(
                format!("{scope}:title"),
                &view.description,
                w,
                cx,
            ))
            .on_click(cx.listener(move |s, _, w, cx| s.open_child(&target, w, cx)));
        let chips = div()
            .flex()
            .gap(px(5.))
            .child(agent_ui::target_chip("子任务", color))
            .when(view.background, |d| {
                d.child(agent_ui::neutral_chip("后台", &p))
            });
        let mut surface = agent_ui::step_surface(&p)
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(agent_ui::step_heading(title, chips, &status, color, cx));
        if view.saving {
            surface = surface.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .child("执行记录保存失败，正在重试"),
            );
        }
        let preview = if running {
            card::command_line(&view.latest_activity)
        } else {
            card::preview(&view.report)
        };
        surface = surface.child(
            selectable(format!("{scope}:preview"), &preview, w, cx)
                .text_size(px(12.5))
                .text_color(p.tx2)
                .line_clamp(3)
                .whitespace_normal()
                .overflow_hidden(),
        );
        let target = view.id.clone();
        let message = view.id.clone();
        let mut actions = agent_ui::step_actions()
            .flex_wrap()
            .child(
                agent_ui::step_action(SharedString::from(format!("{scope}:open")), "查看过程", cx)
                    .on_click(cx.listener(move |s, _, w, cx| s.open_child(&target, w, cx))),
            )
            .child(
                agent_ui::step_action(
                    SharedString::from(format!("{scope}:message")),
                    "追加消息",
                    cx,
                )
                .on_click(cx.listener(move |s, _, w, cx| {
                    s.open_child(&message, w, cx);
                    s.input.focus_handle(cx).focus(w, cx);
                })),
            );
        if running && !self.state.readonly {
            let id = view.id.clone();
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{scope}:stop")), "停止任务", cx)
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(tasks) = &s.tasks {
                            tasks.stop(&id);
                        }
                        cx.notify();
                    })),
            );
        }
        surface.child(actions).into_any_element()
    }
}
struct Selection {
    handle: TextSelectionHandle,
    focus: FocusHandle,
    _refresh: Subscription,
}
/// Uses the same window selection participants as Markdown/code; inherits the
/// surrounding typography and native line clamp/ellipsis for compact card text.
pub(super) fn selectable(id: String, text: &str, w: &mut Window, cx: &mut App) -> Div {
    let state = w.use_keyed_state(
        SharedString::from(format!("{id}:selection")),
        cx,
        |w, cx| {
            let handle = TextSelectionHandle::new("", cx);
            let focus = cx.focus_handle();
            let on_focus = focus.clone();
            handle.focus_with(move |w, cx| on_focus.focus(w, cx), cx);
            let refresh = handle.refresh_window_on_change(w, cx);
            Selection {
                handle,
                focus,
                _refresh: refresh,
            }
        },
    );
    let state = state.read(cx);
    div().min_w_0().track_focus(&state.focus).child(
        SelectableText::with_handle(
            SharedString::from(id),
            state.handle.clone(),
            text.to_owned(),
        )
        .selection_color(cx.palette().accs),
    )
}
