use super::Drawer;
use crate::{
    ai::{
        commands::{self, COMMANDS, ComposerKey, Dispatch},
        tasks::Status,
    },
    theme::ActivePalette,
};
use gpui::{
    component::{input, tooltip::Tooltip},
    prelude::*,
    *,
};

pub(super) struct LocalCard {
    pub after_turn: usize,
    pub body: LocalBody,
}
pub(super) enum LocalBody {
    Hint(String),
    Help,
    Tasks,
}
impl Drawer {
    pub(super) fn local(&mut self, body: LocalBody, cx: &mut Context<Self>) {
        self.state.local_cards.push(LocalCard {
            after_turn: self.state.model.conversation.turns().len(),
            body,
        });
        self.state.follow_bottom = true;
        self.history = false;
        cx.notify();
    }
    pub(super) fn dispatch_input(&mut self, text: &str, w: &mut Window, cx: &mut Context<Self>) {
        if !self.draft_images().is_empty() {self.ask(text.into(),w,cx);return;}
        if text.trim().is_empty() {
            return;
        }
        let action =
            commands::dispatch(text, self.state.model.busy || self.state.pending, COMMANDS);
        if !matches!(action, Dispatch::Hint(_)) {
            self.input.update(cx, |i, cx| i.set_value("", w, cx));
            self.popup.escape();
        }
        match action {
            Dispatch::OpenContext => self.open_context(w, cx),
            Dispatch::Question(question) => self.ask(question, w, cx),
            Dispatch::Hint(text) => self.local(LocalBody::Hint(text), cx),
            Dispatch::QueueCompact(focus) => self.compact_context(focus, w, cx),
            Dispatch::Run { name, args } => match name {
                "btw" => self.start_btw(&args, w, cx),
                "compact" => {
                    if self.state.model.conversation.entries().is_empty() {
                        self.local(LocalBody::Hint("当前没有需要压缩的对话".into()), cx);
                    } else {
                        self.compact_context((!args.is_empty()).then_some(args), w, cx);
                    }
                }
                "memory" => self.open_memory(None, w, cx),
                "clear" => self.new_conversation(w, cx),
                "tasks" => self.local(LocalBody::Tasks, cx),
                "help" => self.local(LocalBody::Help, cx),
                "mode" => {
                    if let Some(mode) = commands::mode(&args) {
                        if mode == shellmate_ai::PermissionMode::Full
                            && *self.mode.lock().unwrap() != mode
                        {
                            self.confirm_full(w, cx);
                        } else if self.set_permission(mode, cx) {
                            self.local(
                                LocalBody::Hint(format!(
                                    "审批模式：{} · {}",
                                    mode.id(),
                                    mode.label()
                                )),
                                cx,
                            );
                        }
                    } else {
                        let mode = *self.mode.lock().unwrap();
                        self.local(
                            LocalBody::Hint(format!("审批模式：{} · {}", mode.id(), mode.label())),
                            cx,
                        );
                    }
                }
                _ => {}
            },
        }
    }
    pub(super) fn slash_action<A: Action>(
        &mut self,
        action: &A,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composer_focused = self.input.focus_handle(cx).contains_focused(w, cx);
        if self.state.child_page.selected.is_some()
            && (action as &dyn Action).as_any().is::<input::Escape>()
            && !self.input_composing(w, cx)
        {
            self.back_to_main(w, cx);
            cx.stop_propagation();
            w.prevent_default();
            return;
        }
        if (self.chooser || self.target_picker) && !self.input_composing(w,cx)
            && (action as &dyn Action).as_any().is::<input::Escape>() {
            self.chooser=false;self.target_picker=false;cx.stop_propagation();w.prevent_default();cx.notify();return;
        }
        if !composer_focused && !self.context_focus.contains_focused(w, cx) {
            cx.propagate();
            return;
        }
        if self.state.btw.question.is_some()
            && !self.permission_menu
            && !self.popup.open
            && (action as &dyn Action).as_any().is::<input::Escape>()
            && !self.input_composing(w, cx)
        {
            self.close_btw(cx);
            cx.stop_propagation();
            w.prevent_default();
            return;
        }
        if self.permission_menu && (action as &dyn Action).as_any().is::<input::Escape>() {
            let composing = self.input_composing(w, cx);
            if crate::ai::mode_popover::escape(&mut self.permission_menu, composing) {
                self.input.focus_handle(cx).focus(w, cx);
                cx.stop_propagation();
                w.prevent_default();
                cx.notify();
                return;
            }
        }
        match route_action(
            action,
            composer_focused,
            self.popup.open,
            self.state.context_popover.open,
            self.input_composing(w, cx),
        ) {
            ActionRoute::Native => {
                cx.propagate();
                return;
            }
            ActionRoute::CloseContext => {
                self.state.context_popover.close();
                self.input.focus_handle(cx).focus(w, cx);
            }
            ActionRoute::Composer(key) => match key {
                ComposerKey::Previous => self.popup.navigate(false),
                ComposerKey::Next => self.popup.navigate(true),
                ComposerKey::Dismiss => self.popup.escape(),
                ComposerKey::Complete => {
                    if let Some(text) = self.popup.complete() {
                        self.set_composer_text(text, w, cx);
                    }
                }
                ComposerKey::Submit => {
                    let text = self.popup.submission(&self.input.read(cx).value());
                    self.dispatch_input(&text, w, cx);
                }
                ComposerKey::Native => unreachable!("native routing handled above"),
            },
        }
        // Capture precedes the textarea's bubble-phase action handler. Stopping
        // here also prevents Enter from emitting a second PressEnter submission.
        cx.stop_propagation();
        w.prevent_default();
        cx.notify();
    }
    pub(super) fn slash_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.popup.open {
            return None;
        }
        let p = cx.palette().clone();
        Some(
            div()
                .id("ai-slash-popup")
                .absolute()
                .bottom(relative(1.))
                .left(px(10.))
                .right(px(10.))
                .mb(px(6.))
                .p(px(6.))
                .rounded(px(16.))
                .bg(p.card)
                .border_1()
                .border_color(p.line)
                .shadow(crate::ui::card_shadow(&p))
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(self.popup.matches().iter().enumerate().map(|(i, c)| {
                    let name = c.name;
                    div()
                        .id(("slash-command", i))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .cursor_pointer()
                        .when(i == self.popup.selected, |d| d.bg(p.accs))
                        .hover(|d| d.bg(p.card2))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(12.5))
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(if i == self.popup.selected { p.acc } else { p.tx })
                                        .child(format!("/{}", c.name)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(p.tx3)
                                        .child(c.args_hint),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(3.))
                                .text_size(px(11.))
                                .text_color(p.tx2)
                                .child(c.description),
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |s, _, w, cx| {
                            let text = s.input.read(cx).value().to_string();
                            let token = text.split_whitespace().next().unwrap_or("");
                            let text = if token == format!("/{name}") {
                                text
                            } else {
                                format!("/{name}")
                            };
                            s.dispatch_input(&text, w, cx);
                        }))
                }))
                .into_any_element(),
        )
    }
    pub(super) fn local_cards_at(&self, turn: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = cx.palette().clone();
        self.state
            .local_cards
            .iter()
            .enumerate()
            .filter(|(_, c)| c.after_turn == turn)
            .map(|(i, c)| {
                let title = match &c.body {
                    LocalBody::Hint(_) => "本地提示",
                    LocalBody::Help => "/help · 命令",
                    LocalBody::Tasks => "/tasks · 任务",
                };
                let mut card = super::agent_ui::step_surface(&p)
                    .flex_none()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(9.))
                    .text_size(px(12.))
                    .text_color(p.tx2)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.tx)
                                    .child(title),
                            )
                            .child(super::agent_ui::status_badge("仅本地", p.tx3, p.card3, false))
                            .child(
                                div()
                                    .id(("local-dismiss", i))
                                    .cursor_pointer()
                                    .flex_none()
                                    .size(px(22.))
                                    .rounded(px(8.))
                                    .bg(p.card)
                                    .border_1()
                                    .border_color(p.line)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(12.))
                                    .text_color(p.tx2)
                                    .hover(|d| d.bg(p.card3))
                                    .tooltip(|window, cx| Tooltip::new("关闭本地卡片").build(window, cx))
                                    .child("×")
                                    .on_click(cx.listener(move |s, _, _, cx| {
                                        s.state.local_cards.remove(i);
                                        cx.notify();
                                    })),
                            ),
                    );
                match &c.body {
                    LocalBody::Hint(text) => card = card.child(text.clone()),
                    LocalBody::Help => {
                        for c in COMMANDS {
                            card = card.child(
                                div()
                                    .p(px(8.))
                                    .rounded(px(10.))
                                    .bg(p.card)
                                    .border_1()
                                    .border_color(p.line)
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .px(px(6.))
                                                    .py(px(1.))
                                                    .rounded(px(6.))
                                                    .bg(p.accs)
                                                    .font_family(crate::theme::MONO_FONT)
                                                    .text_size(px(11.5))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(p.acc)
                                                    .child(format!("/{}", c.name)),
                                            )
                                            .when(!c.args_hint.is_empty(), |d| {
                                                d.child(
                                                    div()
                                                        .text_size(px(11.))
                                                        .text_color(p.tx3)
                                                        .child(c.args_hint),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .mt(px(4.))
                                            .text_size(px(11.))
                                            .text_color(p.tx2)
                                            .child(c.description),
                                    ),
                            );
                        }
                    }
                    LocalBody::Tasks => {
                        if self.has_running_tasks() {
                            card = card.child(
                                super::agent_ui::step_actions().child(
                                    super::agent_ui::pill_action(
                                        "stop-all-tasks",
                                        "停止全部任务",
                                        super::agent_ui::ActionTone::Danger,
                                        cx,
                                    )
                                    .on_click(cx.listener(|s, _, _, cx| s.stop_all_tasks(cx))),
                                ),
                            );
                        }
                        let tasks = if self.fixture {
                            self.fixture_tasks.clone()
                        } else {
                            self.tasks.as_ref().map(|t| t.list()).unwrap_or_default()
                        };
                        if tasks.is_empty() {
                            card = card.child("暂无任务");
                        }
                        for (n, task) in tasks.into_iter().enumerate() {
                            let running = task.status == Status::Running;
                            let status = task.status_text();
                            let id = task.id.clone();
                            let output_id = task.output_id.clone();
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(5.))
                                    .when(n > 0, |d| d.pt(px(9.)).border_t_1().border_color(p.line))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(div().flex_1().child(format!(
                                                "任务 #{} · {}",
                                                task.id,
                                                task.target_label()
                                            )))
                                            .when(
                                                task.kind == crate::ai::tasks::Kind::Command,
                                                |d| {
                                                    d.child(
                                                        super::agent_ui::step_action(
                                                            ("local-output", i * 1000 + n),
                                                            "查看输出",
                                                            cx,
                                                        )
                                                        .on_click(cx.listener(
                                                            move |s, _, w, cx| {
                                                                s.reveal_output(&output_id, w, cx)
                                                            },
                                                        )),
                                                    )
                                                },
                                            )
                                            .when(running, |d| {
                                                d.child(
                                                    super::agent_ui::pill_action(
                                                        ("local-stop", i * 1000 + n),
                                                        "停止任务",
                                                        super::agent_ui::ActionTone::Danger,
                                                        cx,
                                                    )
                                                    .on_click(cx.listener(
                                                        move |s, _, _, cx| {
                                                            if s.fixture {
                                                                if let Some(t) = s
                                                                    .fixture_tasks
                                                                    .iter_mut()
                                                                    .find(|t| t.id == id)
                                                                {
                                                                    t.status = Status::Stopped;
                                                                }
                                                            } else if let Some(t) = &s.tasks {
                                                                t.stop(&id);
                                                            }
                                                            cx.notify();
                                                        },
                                                    )),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .font_family(crate::theme::MONO_FONT)
                                            .text_size(px(11.))
                                            .whitespace_normal()
                                            .child(task.command),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(if running { p.acc } else { p.tx3 })
                                            .child(status),
                                    ),
                            );
                        }
                    }
                }
                card.into_any_element()
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ActionRoute {
    Native,
    CloseContext,
    Composer(ComposerKey),
}
/// Match the actual actions exported/bound by gpui-base 0.9.1. In particular,
/// Tab is IndentInline; Indent belongs to cmd-] and must retain its own behavior.
fn route_action(
    action: &dyn Action,
    composer_focused: bool,
    popup: bool,
    context_open: bool,
    composing: bool,
) -> ActionRoute {
    if composing {
        return ActionRoute::Native;
    }
    let action = action.as_any();
    let (key, shift) = if action.is::<input::MoveUp>() {
        ("up", false)
    } else if action.is::<input::MoveDown>() {
        ("down", false)
    } else if action.is::<input::IndentInline>() {
        ("tab", false)
    } else if action.is::<input::Escape>() {
        ("escape", false)
    } else if let Some(enter) = action.downcast_ref::<input::Enter>() {
        ("enter", enter.shift)
    } else {
        return ActionRoute::Native;
    };
    if key == "escape" && context_open {
        return ActionRoute::CloseContext;
    }
    if !composer_focused {
        return ActionRoute::Native;
    }
    match commands::composer_key(popup, key, shift, false) {
        ComposerKey::Native => ActionRoute::Native,
        key => ActionRoute::Composer(key),
    }
}

#[cfg(test)]
mod action_tests {
    use super::{ActionRoute, ComposerKey, commands, input, route_action};
    use gpui::Action;

    #[test]
    fn actual_textarea_actions_preserve_native_ime_shift_and_focus_routes() {
        let enter = input::Enter {
            secondary: false,
            shift: false,
        };
        for (action, expected) in [
            (&input::MoveUp as &dyn Action, ComposerKey::Previous),
            (&input::MoveDown as &dyn Action, ComposerKey::Next),
            (&input::IndentInline as &dyn Action, ComposerKey::Complete),
            (&input::Escape as &dyn Action, ComposerKey::Dismiss),
            (&enter as &dyn Action, ComposerKey::Submit),
        ] {
            assert_eq!(
                route_action(action, true, true, false, false),
                ActionRoute::Composer(expected)
            );
            assert_eq!(
                route_action(action, true, false, false, false),
                ActionRoute::Native
            );
            assert_eq!(
                route_action(action, true, true, false, true),
                ActionRoute::Native
            );
            assert_eq!(
                route_action(action, false, true, false, false),
                ActionRoute::Native
            );
        }
        for popup in [false, true] {
            assert_eq!(
                route_action(
                    &input::Enter {
                        secondary: false,
                        shift: true
                    },
                    true,
                    popup,
                    false,
                    false
                ),
                ActionRoute::Native
            );
            assert_eq!(
                route_action(&input::Indent, true, popup, false, false),
                ActionRoute::Native
            );
            assert_eq!(
                route_action(&input::OutdentInline, true, popup, false, false),
                ActionRoute::Native
            );
        }
        for composer in [false, true] {
            assert_eq!(
                route_action(&input::Escape, composer, false, true, false),
                ActionRoute::CloseContext
            );
            assert_eq!(
                route_action(&input::Escape, composer, true, true, true),
                ActionRoute::Native
            );
        }
    }

    #[test]
    fn down_down_enter_dispatches_third_command_and_tab_completes() {
        let mut popup = commands::Popup::default();
        popup.changed("/");
        for _ in 0..2 {
            assert_eq!(
                route_action(&input::MoveDown, true, popup.open, false, false),
                ActionRoute::Composer(ComposerKey::Next)
            );
            popup.navigate(true);
        }
        assert_eq!(popup.selected, 2);
        let enter = input::Enter {
            secondary: false,
            shift: false,
        };
        assert_eq!(
            route_action(&enter, true, popup.open, false, false),
            ActionRoute::Composer(ComposerKey::Submit)
        );
        assert_eq!(
            commands::dispatch(&popup.submission("/"), false, commands::COMMANDS),
            commands::Dispatch::OpenContext
        );
        assert_eq!(
            route_action(&input::IndentInline, true, popup.open, false, false),
            ActionRoute::Composer(ComposerKey::Complete)
        );
        assert_eq!(popup.complete().as_deref(), Some("/context"));
        popup.changed("/co");
        assert_eq!(popup.complete().as_deref(), Some("/compact "));
        assert_eq!(popup.submission("/co"), "/co");
        assert_eq!(
            popup.submission("/compact keep focus"),
            "/compact keep focus"
        );
        assert_eq!(
            route_action(&input::Escape, true, popup.open, false, false),
            ActionRoute::Composer(ComposerKey::Dismiss)
        );
        popup.escape();
        assert!(!popup.open);
        assert_eq!(popup.submission("/co"), "/co");
    }
}
