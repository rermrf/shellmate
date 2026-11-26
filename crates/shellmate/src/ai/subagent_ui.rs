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
        let key = super::tool_ui::key(
            &format!("{}:main", self.state.id),
            &format!("agent:{}", view.id),
        );
        let awaiting = self.state.active_steps.iter().any(|(_, s)| {
            s.source.as_ref().is_some_and(|source| source.id == view.id) && s.started.is_none()
        });
        let status = if awaiting {
            crate::ai::tool_model::Status::Approval
        } else if view.saving {
            crate::ai::tool_model::Status::Paused
        } else {
            crate::ai::tool_model::child_status(&view.status)
        };
        let elapsed = if running && !self.fixture {
            view.started.elapsed().as_millis().min(u64::MAX as u128) as u64
        } else {
            view.duration_ms
        };
        let activity = if running {
            view.latest_activity.clone()
        } else {
            card::preview(&view.report)
        };
        let model = super::tool_ui::Card {
            key: key.clone(),
            title: "委派子任务".into(),
            status,
            duration: (elapsed > 0).then_some(elapsed),
            summary: view.description.clone(),
            detail: format!(
                "{} 步{} · {}",
                view.steps,
                if view.background {
                    " · 异步任务"
                } else {
                    ""
                },
                activity
            ),
        };
        let open = self.tool_open(&key, status);
        let mut surface = self.tool_shell(&model, cx);
        if open {
            surface = surface.child(super::tool_ui::section("输入", &p));
            let prompt = if !view.prompt.is_empty() {
                Some(view.prompt.clone())
            } else {
                view.transcript.as_ref().and_then(|entries| {
                    entries.iter().find_map(|e| {
                        if let shellmate_ai::Entry::User { text, .. } = e {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                })
            };
            surface = surface.child(
                selectable(
                    format!("{key}:prompt"),
                    prompt
                        .as_deref()
                        .unwrap_or("原始任务说明未保存在此历史记录中"),
                    w,
                    cx,
                )
                .text_size(px(12.5))
                .line_height(px(19.))
                .whitespace_normal(),
            );
            surface = surface.child(super::tool_ui::section(
                if running { "进展" } else { "结果" },
                &p,
            ));
            if running {
                surface = surface.child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.tx2)
                        .whitespace_normal()
                        .child(view.latest_activity.clone()),
                );
            } else {
                surface = surface.child(super::markdown::drawer_view(
                    SharedString::from(format!("{}:{key}:report", self.state.id)).into(),
                    view.report.clone().into(),
                    self.selection_scroll.clone(),
                    self.fixture_copy,
                    w,
                    cx,
                ));
            }
            if let Some(meta) = card::meta(view) {
                surface = surface.child(
                    div()
                        .text_size(px(11.))
                        .text_color(p.tx3)
                        .whitespace_normal()
                        .child(meta),
                );
            }
        }
        let target = view.id.clone();
        let message = view.id.clone();
        let mut actions = agent_ui::step_actions()
            .flex_wrap()
            .child(
                agent_ui::step_action(SharedString::from(format!("{key}:open")), "查看对话 →", cx)
                    .on_click(cx.listener(move |s, _, w, cx| s.open_child(&target, w, cx))),
            )
            .child(
                agent_ui::step_action(SharedString::from(format!("{key}:message")), "追加消息", cx)
                    .on_click(cx.listener(move |s, _, w, cx| {
                        s.open_child(&message, w, cx);
                        s.input.focus_handle(cx).focus(w, cx);
                    })),
            );
        if open {
            let report = view.report.clone();
            actions = actions.child(
                agent_ui::step_action(
                    SharedString::from(format!("{key}:copy-result")),
                    "复制结果",
                    cx,
                )
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(report.clone()))
                }),
            );
        }
        if running && !self.state.readonly {
            let id = view.id.clone();
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{key}:stop")), "停止任务", cx)
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(t) = &s.tasks {
                            t.stop(&id);
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
