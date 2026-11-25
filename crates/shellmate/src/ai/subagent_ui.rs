//! Subagent cards reuse command-step surfaces, header, output viewport and text actions.
use super::{Drawer, agent_ui, markdown};
use crate::ai::{
    subagent_card as card,
    subagent_view::{View, saved_output},
};
use crate::theme::ActivePalette;
use gpui::{
    base::{SelectableText, TextSelectionHandle},
    prelude::*,
    *,
};
use shellmate_ai::{SubagentKind, SubagentStatus};
use std::collections::BTreeMap;

impl Drawer {
    /// Join semantic Steps with registry identities before borrowing the projected turns.
    /// A foreground call has no result Step yet; it is shown at the pending Step position.
    pub(super) fn prepare_subagent_cards(&mut self) -> BTreeMap<(usize, usize), String> {
        let ids = card::projected(&self.state.model.conversation, &mut self.state.subagents);
        for view in self.state.subagents.values_mut() {
            view.load_report(&self.data_root, &self.state.id);
        }
        ids
    }

    fn toggle_subagent(&mut self, id: &str, cx: &mut Context<Self>) {
        let card = self.state.agent_cards.entry(id.into()).or_default();
        card.open = !card.open;
        if card.open
            && let Some(view) = self.state.subagents.get_mut(id)
        {
            view.load_transcript(&self.data_root, &self.state.id);
        }
        cx.notify();
    }
    fn toggle_subagent_step(&mut self, id: &str, step_id: &str, cx: &mut Context<Self>) {
        let card = self.state.agent_cards.entry(id.into()).or_default();
        card.toggle_step(step_id);
        if card.step.is_some()
            && !card.outputs.contains_key(step_id)
            && let Some(step) = self
                .state
                .subagents
                .get(id)
                .and_then(|v| v.step_list.iter().find(|s| s.id == step_id))
        {
            let output = step
                .output_id
                .as_deref()
                .and_then(|id| saved_output(&self.data_root, &self.state.id, id))
                .unwrap_or_else(|| step.output.clone());
            // A running command may acquire more output; don't cache an incomplete read.
            if !step.running {
                card.outputs.insert(step_id.into(), output);
            }
        }
        cx.notify();
    }
    fn stop_subagent(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tasks) = &self.tasks {
            tasks.stop(id);
        }
        cx.notify();
    }

    pub(super) fn subagent_card(
        &self,
        view: &View,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = cx.palette().clone();
        let id = &view.id;
        let scope = format!("{}:subagent:{id}", self.state.id);
        let running = view.status == SubagentStatus::Running;
        let seconds = if running && !self.fixture {
            view.started.elapsed().as_secs()
        } else {
            view.duration_ms / 1000
        };
        let (status, color) = card::status(&view.status, view.steps, seconds);
        let color = color.resolve(&p);
        let expansion = self.state.agent_cards.get(id);
        let expanded = expansion.is_some_and(|c| c.open);
        let chips = div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(agent_ui::target_chip("子任务", color))
            .child(agent_ui::neutral_chip(
                match view.kind {
                    SubagentKind::Explore => "只读",
                    SubagentKind::General => "通用",
                },
                &p,
            ))
            .when(view.background, |d| {
                d.child(agent_ui::neutral_chip("后台", &p))
            });
        let title = selectable(format!("{scope}:title"), &view.description, window, cx);
        let mut surface = agent_ui::step_surface(&p)
            .flex_none()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(agent_ui::step_heading(title, chips, &status, color, cx));
        if let Some(meta) = card::meta(view) {
            surface = surface.child(div().text_size(px(11.)).text_color(p.tx3).child(selectable(
                format!("{scope}:meta"),
                &meta,
                window,
                cx,
            )));
        }
        if expanded {
            surface = surface.child(note(format!("执行记录 · {} 步", view.steps), &p));
            if let Some(note_text) = card::transcript_note(view) {
                surface = surface.child(note(note_text, &p));
            } else {
                let mut steps = agent_ui::command_surface(cx)
                    .px(px(12.))
                    .py(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.));
                for step in &view.step_list {
                    let step_key = format!("{scope}:step:{}", step.id);
                    let (status, color) = card::step_status(step);
                    let row_open =
                        expansion.and_then(|c| c.step.as_deref()) == Some(step.id.as_str());
                    let agent_id = id.clone();
                    let step_id = step.id.clone();
                    let row = div()
                        .id(SharedString::from(step_key.clone()))
                        .min_w_0()
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .cursor_pointer()
                        .child(
                            div()
                                .flex_none()
                                .size(px(6.))
                                .rounded_full()
                                .bg(color.resolve(&p)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(selectable(
                                    format!("{step_key}:command"),
                                    &card::command_line(&step.command),
                                    window,
                                    cx,
                                )),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(crate::theme::UI_FONT)
                                .text_size(px(11.))
                                .text_color(
                                    if step.running
                                        || matches!(
                                            step.status,
                                            shellmate_ai::StepStatus::Denied { .. }
                                                | shellmate_ai::StepStatus::Background
                                        )
                                    {
                                        color.resolve(&p)
                                    } else {
                                        p.tx3
                                    },
                                )
                                .child(status),
                        )
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.toggle_subagent_step(&agent_id, &step_id, cx)
                        }));
                    steps = steps.child(row);
                    if row_open {
                        let output = expansion
                            .and_then(|c| c.outputs.get(&step.id))
                            .unwrap_or(&step.output);
                        let output_view = crate::ai::output_view::display_step(output, false);
                        let (stdout, stderr) = agent_ui::output_sections(output_view.preview);
                        let content = agent_ui::output_content(
                            stdout,
                            stderr,
                            SharedString::from(format!("{step_key}:stdout")).into(),
                            SharedString::from(format!("{step_key}:stderr")).into(),
                            &p,
                        );
                        let copy = output.clone();
                        steps = steps.child(
                            div()
                                .font_family(crate::theme::UI_FONT)
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(agent_ui::output_scroll(
                                    format!("{step_key}:output").into(),
                                    div().flex_none().child(content),
                                    true,
                                    self.selection_scroll.clone(),
                                    &p,
                                ))
                                .children(output_view.saved.map(|text| note(text, &p)))
                                .child(
                                    agent_ui::step_actions().child(
                                        agent_ui::step_action(
                                            SharedString::from(format!("{step_key}:copy")),
                                            "复制",
                                            cx,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy.clone(),
                                                ))
                                            },
                                        ),
                                    ),
                                ),
                        );
                    }
                }
                surface = surface.child(steps);
            }
            surface = surface.child(note("报告", &p));
            surface = surface.child(markdown::drawer_view(
                SharedString::from(format!("{scope}:report")).into(),
                markdown::preprocess(
                    if view.report.trim().is_empty() {
                        card::EMPTY_REPORT
                    } else {
                        &view.report
                    },
                    &[],
                )
                .into(),
                self.selection_scroll.clone(),
                self.fixture_copy,
                window,
                cx,
            ));
        } else if running {
            let command = view
                .step_list
                .last()
                .map(|s| card::command_line(&s.command));
            surface = surface.child(
                agent_ui::command_surface(cx)
                    .when(command.is_none(), |d| {
                        d.font_family(crate::theme::UI_FONT).text_color(p.tx3)
                    })
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(selectable(
                        format!("{scope}:activity"),
                        command.as_deref().unwrap_or("正在分析任务…"),
                        window,
                        cx,
                    )),
            );
        } else {
            let preview = card::preview(&view.report);
            surface = surface.child(
                div()
                    .min_w_0()
                    .w_full()
                    .text_size(px(13.))
                    .line_height(px(20.8))
                    .text_color(if preview == card::EMPTY_REPORT {
                        p.tx3
                    } else {
                        p.tx
                    })
                    .text_ellipsis()
                    .whitespace_normal()
                    .line_clamp(3)
                    .child(selectable(format!("{scope}:preview"), &preview, window, cx)),
            );
        }
        let agent_id = id.clone();
        let mut actions = agent_ui::step_actions().child(
            agent_ui::step_action(
                SharedString::from(format!("{scope}:expand")),
                if expanded { "收起" } else { "展开" },
                cx,
            )
            .on_click(cx.listener(move |s, _, _, cx| s.toggle_subagent(&agent_id, cx))),
        );
        if expanded || !running {
            let copy = card::copy_report(view);
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{scope}:copy")), "复制报告", cx)
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                    }),
            );
        }
        if running && !self.state.readonly {
            let agent_id = id.clone();
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{scope}:stop")), "停止", cx)
                    .on_click(cx.listener(move |s, _, _, cx| s.stop_subagent(&agent_id, cx))),
            );
        }
        surface.child(actions).into_any_element()
    }
}

fn note(text: impl Into<SharedString>, p: &crate::theme::Palette) -> Div {
    div()
        .text_size(px(11.))
        .text_color(p.tx3)
        .child(text.into())
}
struct Selection {
    handle: TextSelectionHandle,
    focus: FocusHandle,
    _refresh: Subscription,
}
/// Uses the same window selection participants as Markdown/code; inherits the
/// surrounding typography and native line clamp/ellipsis for compact card text.
fn selectable(id: String, text: &str, w: &mut Window, cx: &mut App) -> Div {
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
