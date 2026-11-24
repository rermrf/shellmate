use super::Drawer;
use crate::{
    ai::{
        commands::{self, COMMANDS, Dispatch},
        model::tokens,
        tasks::Status,
    },
    theme::ActivePalette,
};
use gpui::{component::button::Button, prelude::*, *};
use shellmate_ai::{ContextBreakdown, ContextBudget, WindowSource, resolve_window};

pub(super) struct LocalCard {
    pub after_turn: usize,
    pub body: LocalBody,
}
pub(super) enum LocalBody {
    Hint(String),
    Help,
    Context(ContextBreakdown, WindowSource),
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
            Dispatch::Question(question) => self.ask(question, w, cx),
            Dispatch::Hint(text) => self.local(LocalBody::Hint(text), cx),
            Dispatch::QueueCompact(focus) => self.compact_context(focus, w, cx),
            Dispatch::Run { name, args } => match name {
                "compact" => {
                    if self.state.model.conversation.entries().is_empty() {
                        self.local(LocalBody::Hint("当前没有需要压缩的对话".into()), cx);
                    } else {
                        self.compact_context((!args.is_empty()).then_some(args), w, cx);
                    }
                }
                "clear" => self.new_conversation(w, cx),
                "context" => {
                    let s = self.db.borrow().ai_settings().unwrap_or_default();
                    let (budget, source) = self.state.run_config.as_ref().map_or_else(
                        || {
                            (
                                s.context_budget(),
                                resolve_window(&s.provider, &s.model, s.context_window).1,
                            )
                        },
                        |c| {
                            let (window, source) =
                                resolve_window(&c.provider, &c.model, c.context_window);
                            (
                                ContextBudget::new(
                                    window,
                                    c.autocompact_window,
                                    c.max_output_tokens.resolve(&c.provider, &c.model),
                                ),
                                source,
                            )
                        },
                    );
                    let (budget, source) = self.state.run_budget.unwrap_or((budget, source));
                    let mut log = self.state.model.conversation.clone();
                    if log.entries().is_empty() {
                        log.push_user(shellmate_ai::UserTurn::new(
                            "",
                            shellmate_ai::ContextSnapshot {
                                label: self.label.clone(),
                                protocol: self.protocol.clone(),
                                text: String::new(),
                            },
                        ));
                    }
                    match log.breakdown_for_budget(budget, &self.capabilities) {
                        Ok(b) => self.local(LocalBody::Context(b, source), cx),
                        Err(e) => self.local(LocalBody::Hint(e.to_string()), cx),
                    }
                }
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
    pub(super) fn slash_key(&mut self, e: &KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        if !self.input.focus_handle(cx).contains_focused(w, cx) || !self.popup.open {
            return;
        }
        match e.keystroke.key.as_str() {
            "up" => self.popup.navigate(false),
            "down" => self.popup.navigate(true),
            "escape" => self.popup.escape(),
            "tab" => {
                if let Some(text) = self.popup.complete() {
                    self.input.update(cx, |i, cx| i.set_value(text, w, cx));
                }
            }
            "enter" => {
                let text = self.input.read(cx).value().to_string();
                // Enter submits literal input; arrow selection explicitly chooses a completion.
                let name = text.split_whitespace().next().unwrap_or("");
                let text = if COMMANDS.iter().any(|c| name == format!("/{}", c.name))
                    || (!self.popup.navigated && text != "/")
                {
                    text
                } else {
                    self.popup.complete().unwrap_or(text)
                };
                self.dispatch_input(&text, w, cx);
            }
            _ => return,
        }
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
                                .child(format!("/{}", c.name))
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
                    LocalBody::Context(..) => "/context · 上下文",
                    LocalBody::Tasks => "/tasks · 后台任务",
                };
                let mut card = div()
                    .flex_none()
                    .min_w_0()
                    .rounded(px(14.))
                    .border_1()
                    .border_color(p.line)
                    .bg(p.card2)
                    .p(px(12.))
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
                            .child(div().flex_1().child(title))
                            .child(div().text_size(px(10.)).text_color(p.tx3).child("仅本地"))
                            .child(
                                Button::new(("local-dismiss", i))
                                    .label("×")
                                    .p_0()
                                    .flex_none()
                                    .w(px(22.))
                                    .h(px(22.))
                                    .rounded(px(7.))
                                    .tooltip("关闭本地卡片")
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
                                div().child(format!("/{} {}", c.name, c.args_hint)).child(
                                    div()
                                        .mt(px(3.))
                                        .text_size(px(11.))
                                        .text_color(p.tx3)
                                        .child(c.description),
                                ),
                            );
                        }
                    }
                    LocalBody::Context(b, source) => {
                        let source = match source {
                            WindowSource::Setting => "手动设置",
                            WindowSource::Catalog => "模型目录",
                            WindowSource::Fallback => "128K 回退",
                        };
                        card = card
                            .child(div().text_size(px(18.)).text_color(p.tx).child(format!(
                                "{} / {}",
                                tokens(b.total),
                                tokens(b.budget.window as usize)
                            )))
                            .child(format!(
                                "上下文窗口　{}（{source}）",
                                tokens(b.budget.window as usize)
                            ))
                            .child(format!("自动压缩窗口　{}", tokens(b.budget.auto as usize)))
                            .child(format!(
                                "自动压缩阈值　{}",
                                tokens(b.budget.trigger as usize)
                            ))
                            .child(div().h(px(1.)).bg(p.line));
                        for (name, n) in [
                            ("系统提示", b.system_prompt),
                            ("压缩摘要", b.summary),
                            ("消息", b.messages),
                            ("工具结果", b.tool_results),
                        ] {
                            card = card.child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .child(format!("{name}（估算）"))
                                    .child(tokens(n)),
                            );
                        }
                        card = card.child(div().text_size(px(10.5)).text_color(p.tx3).child(
                            if b.provider_usage {
                                "总量：服务商用量 + 新增估算；各项为估算"
                            } else {
                                "总量及各项均为估算"
                            },
                        ));
                    }
                    LocalBody::Tasks => {
                        let tasks = if self.fixture {
                            self.fixture_tasks.clone()
                        } else {
                            self.tasks.as_ref().map(|t| t.list()).unwrap_or_default()
                        };
                        if tasks.is_empty() {
                            card = card.child("暂无后台任务");
                        }
                        for (n, task) in tasks.into_iter().enumerate() {
                            let running = task.status == Status::Running;
                            let status = match task.status {
                                Status::Running => {
                                    format!("运行中 · {}s", task.started.elapsed().as_secs())
                                }
                                Status::Exited(code) => format!(
                                    "已完成 · 退出码 {}",
                                    code.map_or_else(|| "未知".into(), |n| n.to_string())
                                ),
                                Status::Stopped => "已停止".into(),
                                Status::Disconnected => "连接已断开".into(),
                            };
                            let id = task.id.clone();
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
                                                task.target.label()
                                            )))
                                            .when(running, |d| {
                                                d.child(
                                                    Button::new(("local-stop", i * 1000 + n))
                                                        .label("停止")
                                                        .h(px(24.))
                                                        .text_size(px(11.))
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
