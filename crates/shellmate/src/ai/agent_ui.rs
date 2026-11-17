use super::*;
use crate::ai::host::{Approval, Ui};
use crate::dialogs::{self, DialogBody, Tone};
use shellmate_ai::{
    Answer, AnswerItem, StepStatus,
    agent::{Target, ToolCall, ToolOutcome},
    policy::{Class, classify},
};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

pub(super) struct ActiveStep {
    pub call: ToolCall,
    pub reply: Option<oneshot::Sender<Approval>>,
    pub redline: bool,
    pub started: Option<Instant>,
    pub automatic: bool,
    pub edit: Option<Entity<InputState>>,
    _edit_subscription: Option<Subscription>,
    pub reason: Option<Entity<InputState>>,
}
impl ActiveStep {
    pub fn new(call: ToolCall) -> Self {
        let redline = call.class == Class::RedLine;
        Self {
            call,
            reply: None,
            redline,
            started: None,
            automatic: false,
            edit: None,
            _edit_subscription: None,
            reason: None,
        }
    }
}
impl Drawer {
    pub(super) fn permission_key(&self) -> &str {
        self.host.as_deref().unwrap_or("local")
    }
    pub(super) fn release_context(&mut self, cx: &mut Context<Self>) {
        if !self.model.busy
            && !self.pending
            && self.continuation.is_none()
            && let Some(context) = self.next_context.take()
        {
            self.sync_context(context, cx);
        }
    }

    pub(super) fn agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking => self.model.event(Event::Thinking),
            AgentEvent::TextDelta(t) => self.model.event(Event::TextDelta(t)),
            AgentEvent::Command(c) => self.model.event(Event::Command(c)),
            AgentEvent::Done(u) => {
                self.model.event(Event::Done(u));
                self.run_config = None;
            }
            AgentEvent::Error(e) => {
                self.model.event(Event::Error(e));
                self.run_config = None;
            }
            // The bridge owns the pending card; separate channels can deliver this after Approval.
            AgentEvent::StepProposed(_) => self.model.thinking = false,
            AgentEvent::StepFinished { call, outcome } => {
                if let Some(turn) = self.model.conversation.turns.last_mut() {
                    let answer = turn.answer.get_or_insert_with(Answer::default);
                    let mut item = outcome.item(&call);
                    if let AnswerItem::Step { automatic, .. } = &mut item {
                        *automatic = self.active_step.as_ref().is_some_and(|s| s.automatic);
                    }
                    answer.items.push(item);
                }
                self.active_step = None;
            }
            AgentEvent::StepLimit { steps, resume } => {
                self.model.busy = false;
                self.model.thinking = false;
                self.continuation = Some(resume);
                self.resume_steps = steps;
            }
        }
    }
    pub(super) fn resume(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.continuation.is_none() || self.model.busy {
            return;
        }
        if let Some(config) = self.run_config.clone() {
            self.model.busy = true;
            self.start(config, w, cx);
        }
    }
    pub(super) fn bridge(
        &self,
        messages: async_channel::Receiver<Ui>,
        epoch: u64,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.spawn_in(w, async move |this, cx| {
            while let Ok(event) = messages.recv().await {
                let finish = matches!(event, Ui::TerminalFinish { .. });
                match event {
                    // Cleanup survives drawer cancellation/destruction and ignores the UI epoch.
                    Ui::TerminalFinish { session, nonce }
                    | Ui::TerminalInterrupt { session, nonce } => {
                        workspace
                            .update_in(cx, |s, _, cx| {
                                s.ai_terminal_finish(session, &nonce, finish, cx)
                            })
                            .ok();
                    }
                    Ui::LocalCwd { session, reply } => {
                        if !reply.is_closed() {
                            let directory = workspace
                                .update_in(cx, |s, _, cx| {
                                    s.ai_binding(Some(session), cx)
                                        .and_then(|b| b.capabilities.cwd)
                                })
                                .ok()
                                .flatten();
                            let _ = reply.send(directory);
                        }
                    }
                    Ui::TerminalStart {
                        session,
                        command,
                        nonce,
                        lease,
                        reply,
                    } => {
                        let valid = this
                            .update_in(cx, |s, _, _| s.epoch == epoch)
                            .unwrap_or(false);
                        if reply.is_closed() || !valid {
                            continue;
                        }
                        let result = workspace
                            .update_in(cx, |s, _, cx| {
                                s.ai_terminal_start(session, &command, &nonce, &lease, cx)
                            })
                            .unwrap_or(Err(ToolOutcome::Disconnected));
                        if reply.send(result).is_err() {
                            workspace
                                .update_in(cx, |s, _, cx| {
                                    s.ai_terminal_finish(session, &nonce, true, cx)
                                })
                                .ok();
                        }
                    }
                    event => {
                        this.update_in(cx, |s, _, cx| {
                            if s.epoch != epoch {
                                return;
                            }
                            match event {
                                Ui::Approval {
                                    call,
                                    redline,
                                    reply,
                                } => {
                                    let mut step = ActiveStep::new(call);
                                    step.reply = Some(reply);
                                    step.redline = redline;
                                    s.active_step = Some(step);
                                    s.follow_bottom = true;
                                }
                                Ui::Running {
                                    call,
                                    automatic,
                                    ready,
                                } => {
                                    let mut step = ActiveStep::new(call);
                                    step.started = Some(Instant::now());
                                    step.automatic = automatic;
                                    s.active_step = Some(step);
                                    let _ = ready.send(());
                                }
                                Ui::CancelApproval(id)
                                    if s.active_step.as_ref().is_some_and(|step| {
                                        step.call.id == id && step.started.is_none()
                                    }) =>
                                {
                                    s.active_step = None;
                                }
                                _ => {}
                            }
                            cx.notify();
                        })
                        .ok();
                    }
                }
            }
        })
        .detach();
        cx.spawn_in(w, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if !this
                    .update_in(cx, |s, _, cx| {
                        if s.epoch != epoch || !s.model.busy {
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
        .detach();
    }
    fn set_permission(&mut self, mode: PermissionMode, cx: &mut Context<Self>) -> bool {
        if self
            .db
            .borrow_mut()
            .set_ai_permission(self.permission_key(), mode)
            .is_err()
        {
            self.save_error = Some("无法保存审批设置，请重试".into());
            cx.notify();
            return false;
        }
        *self.mode.lock().unwrap() = mode;
        self.permission_menu = false;
        cx.notify();
        true
    }
    pub(crate) fn confirm_full(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.permission_menu = false;
        let drawer = cx.entity().downgrade();
        let label = self.label.clone();
        let key = self.permission_key().to_owned();
        let body = cx.new(|_| FullAccess { drawer, label, key });
        dialogs::open(w, cx, "开启完全访问", 500., body);
    }
    pub(super) fn permission_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = cx.palette().clone();
        if !self.capabilities.available() {
            return div()
                .text_size(px(11.))
                .line_height(px(17.))
                .text_color(p.tx2)
                .child("此会话不支持 AI 执行命令，仅提供建议")
                .into_any_element();
        }
        let mode = *self.mode.lock().unwrap();
        let full = mode == PermissionMode::Full;
        let mut chip = div().flex().flex_col().gap(px(4.)).child(
            div().flex().items_center().child(
                div()
                    .id("ai-permission")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(26.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .bg(if full { p.errs } else { p.card2 })
                    .text_color(if full { p.err } else { p.tx2 })
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .hover(|d| d.bg(if full { p.err.opacity(0.2) } else { p.card3 }))
                    .child(crate::icons::AppIcon::Shield.icon())
                    .child(mode.label())
                    .child("▾")
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.permission_menu = !s.permission_menu;
                        cx.notify();
                    })),
            ),
        );
        if self.permission_menu {
            for (i, (mode, description)) in [
                (PermissionMode::Ask, "每条命令都需要你批准"),
                (PermissionMode::Auto, "只读命令自动执行，其他命令需批准"),
                (PermissionMode::Full, "所有命令自动执行，高危命令仍需确认"),
            ]
            .into_iter()
            .enumerate()
            {
                chip = chip.child(
                    div()
                        .id(("ai-permission-option", i))
                        .p(px(8.))
                        .rounded(px(9.))
                        .bg(p.card2)
                        .cursor_pointer()
                        .hover(|d| d.bg(p.card3))
                        .text_size(px(12.))
                        .child(
                            div()
                                .text_color(if mode == PermissionMode::Full {
                                    p.err
                                } else {
                                    p.tx
                                })
                                .child(mode.label()),
                        )
                        .child(
                            div()
                                .mt(px(3.))
                                .text_size(px(10.5))
                                .text_color(p.tx2)
                                .child(description),
                        )
                        .on_click(cx.listener(move |s, _, w, cx| {
                            if mode == PermissionMode::Full && *s.mode.lock().unwrap() != mode {
                                s.confirm_full(w, cx);
                            } else {
                                s.set_permission(mode, cx);
                            }
                        })),
                );
            }
        }
        chip.into_any_element()
    }
    fn approve(&mut self, deny: bool, cx: &mut Context<Self>) {
        let Some(step) = &mut self.active_step else {
            return;
        };
        let command = step
            .edit
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_else(|| step.call.command.clone());
        if !deny && command.trim().is_empty() {
            return;
        }
        let approval = if deny {
            Approval::Deny {
                reason: step
                    .reason
                    .as_ref()
                    .map(|i| i.read(cx).value().to_string())
                    .filter(|s| !s.trim().is_empty()),
            }
        } else {
            Approval::Approve {
                command,
                target: step.call.target,
            }
        };
        if let Some(reply) = step.reply.take() {
            let _ = reply.send(approval);
        }
        cx.notify();
    }
    pub(super) fn active_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = cx.palette().clone();
        let step = self.active_step.as_ref().unwrap();
        let red = step
            .edit
            .as_ref()
            .map(|i| classify(&i.read(cx).value()) == Class::RedLine)
            .unwrap_or(step.redline);
        let mut card = div()
            .flex_none()
            .rounded(px(12.))
            .p(px(10.))
            .bg(if red { p.errs } else { p.card2 })
            .flex()
            .flex_col()
            .gap(px(8.));
        if red {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("高危命令，需要确认"),
            );
        }
        let state = step
            .started
            .map(|t| format!("运行中 · {} s", t.elapsed().as_secs()))
            .unwrap_or_else(|| "等待批准".into());
        card = card.child(step_header(
            &step.call.purpose,
            step.call.target,
            step.automatic,
            &state,
            p.tx2,
            cx,
        ));
        card = if let Some(edit) = &step.edit {
            card.child(Input::new(edit).text_size(px(12.)).font_family(MONO_FONT))
        } else {
            card.child(command_block(&step.call.command, cx))
        };
        if step.started.is_some() {
            card = card.child(gpui::component::spinner::Spinner::new()).child(
                dialogs::button("ai-step-stop", "停止", Tone::Plain, true)
                    .on_click(cx.listener(|s, _, _, cx| s.stop(cx))),
            );
        } else {
            if self.capabilities.terminal && self.capabilities.background {
                let mut row = div().flex().gap(px(4.));
                for target in [Target::Terminal, Target::Background] {
                    row = row.child(
                        dialogs::button(
                            if target == Target::Terminal {
                                "ai-target-terminal"
                            } else {
                                "ai-target-background"
                            },
                            target.label(),
                            if target == step.call.target {
                                Tone::Primary
                            } else {
                                Tone::Plain
                            },
                            true,
                        )
                        .h(px(25.))
                        .text_size(px(11.))
                        .on_click(cx.listener(move |s, _, _, cx| {
                            if let Some(step) = &mut s.active_step {
                                step.call.target = target;
                            }
                            cx.notify();
                        })),
                    );
                }
                card = card.child(row);
            }
            if let Some(reason) = &step.reason {
                card = card.child(Input::new(reason).text_size(px(12.))).child(
                    dialogs::button("ai-step-deny-confirm", "确认拒绝", Tone::Plain, true)
                        .on_click(cx.listener(|s, _, _, cx| s.approve(true, cx))),
                );
            } else {
                card = card.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(
                            dialogs::button(
                                "ai-step-run",
                                "运行",
                                if red { Tone::Danger } else { Tone::Primary },
                                true,
                            )
                            .on_click(cx.listener(|s, _, _, cx| s.approve(false, cx))),
                        )
                        .child(
                            dialogs::button("ai-step-edit", "编辑", Tone::Plain, true).on_click(
                                cx.listener(|s, _, w, cx| {
                                    if let Some(step) = &mut s.active_step {
                                        let command = step.call.command.clone();
                                        let edit = cx.new(|cx| {
                                            let mut i = InputState::new(w, cx);
                                            i.set_value(command, w, cx);
                                            i
                                        });
                                        step._edit_subscription =
                                            Some(cx.observe(&edit, |_, _, cx| cx.notify()));
                                        step.edit = Some(edit);
                                    }
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            dialogs::button("ai-step-deny", "拒绝", Tone::Plain, true).on_click(
                                cx.listener(|s, _, w, cx| {
                                    if let Some(step) = &mut s.active_step {
                                        step.reason = Some(cx.new(|cx| {
                                            InputState::new(w, cx).placeholder("拒绝原因（可选）")
                                        }));
                                    }
                                    cx.notify();
                                }),
                            ),
                        ),
                );
            }
        }
        card.into_any_element()
    }
    pub(super) fn step_card(
        &self,
        item: &AnswerItem,
        key: (usize, usize),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let AnswerItem::Step {
            command,
            target,
            purpose,
            status,
            exit_code,
            duration_ms,
            automatic,
            output,
            ..
        } = item
        else {
            unreachable!()
        };
        let p = cx.palette().clone();
        let state = match status {
            StepStatus::Completed => format!(
                "退出码 {} · {:.1} s",
                exit_code.map_or_else(|| "未知".into(), |n| n.to_string()),
                *duration_ms as f64 / 1000.
            ),
            StepStatus::TimedOut => "超时".into(),
            StepStatus::Denied { .. } => "已拒绝".into(),
            StepStatus::Busy => "终端正忙".into(),
            StepStatus::Disconnected => "会话已断开".into(),
            StepStatus::Failed(_) => "失败".into(),
        };
        let color = if *status == StepStatus::Completed && *exit_code == Some(0) {
            p.ok
        } else {
            p.err
        };
        let mut card = div()
            .flex_none()
            .p(px(10.))
            .rounded(px(12.))
            .bg(p.card2)
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(step_header(purpose, *target, *automatic, &state, color, cx))
            .child(command_block(command, cx));
        match status {
            StepStatus::Denied {
                reason: Some(reason),
            }
            | StepStatus::Failed(reason) => {
                card = card.child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.tx2)
                        .child(reason.clone()),
                );
            }
            _ => {}
        }
        if !output.is_empty() || matches!(status, StepStatus::Completed | StepStatus::TimedOut) {
            let (stdout, stderr) = output_sections(output);
            let has_output = !stdout.is_empty() || !stderr.is_empty();
            let expanded = self.expanded.contains(&key);
            let overflowing = self.overflowing_output.contains(&key);
            let drawer = cx.entity().downgrade();
            let epoch = self.epoch;
            let scrolls_vertically = std::rc::Rc::new(std::cell::Cell::new(false));
            let measured_scroll = scrolls_vertically.clone();
            let content = div()
                .flex_none()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .when(!stdout.is_empty(), |d| {
                    d.child(div().child(stdout.to_owned()))
                })
                .when(!stderr.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(div().text_color(p.err).child("标准错误"))
                            .child(div().child(stderr.to_owned())),
                    )
                })
                .when(!has_output, |d| d.child("（无输出）"));
            // Measure the complete content, including section labels and gaps. The cap
            // clips by rendered height; it never slices or reflows command output.
            let measured =
                div()
                    .flex_none()
                    .child(content)
                    .on_children_prepainted(move |bounds, _, cx| {
                        measured_scroll
                            .set(bounds.first().is_some_and(|b| b.size.height > px(320.)));
                        let over = bounds.first().is_some_and(|b| b.size.height > px(8. * 17.));
                        if over != overflowing {
                            let drawer = drawer.clone();
                            cx.defer(move |cx| {
                                drawer
                                    .update(cx, |s, cx| {
                                        if s.epoch != epoch {
                                            return;
                                        }
                                        let changed = if over {
                                            s.overflowing_output.insert(key)
                                        } else {
                                            s.overflowing_output.remove(&key)
                                        };
                                        if changed {
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            });
                        }
                    });
            card = card.child(
                div()
                    // Distinct scroll state resets vertical position when collapsing.
                    .id(SharedString::from(format!(
                        "ai-step-output-{}-{}-{expanded}",
                        key.0, key.1
                    )))
                    .flex_none()
                    .min_w_0()
                    .max_w_full()
                    .max_h(px(if expanded { 320. } else { 8. * 17. }))
                    .overflow_x_scroll()
                    .overflow_y_hidden()
                    .when(expanded, |d| d.overflow_y_scroll())
                    .restrict_scroll_to_axis()
                    .on_scroll(move |event, _, cx| {
                        // GPUI has already scrolled this viewport. Keep an expanded
                        // output's vertical gesture from also moving the message list.
                        if expanded
                            && scrolls_vertically.get()
                            && event.delta.pixel_delta(px(17.)).y != px(0.)
                        {
                            cx.stop_propagation();
                        }
                    })
                    .flex()
                    .flex_col()
                    .items_start()
                    .text_size(px(11.))
                    .line_height(px(17.))
                    .font_family(MONO_FONT)
                    .whitespace_nowrap()
                    .text_color(p.tx2)
                    .child(measured),
            );
            if has_output && !self.readonly {
                let copy = output.clone();
                let mut actions = div().flex_none().flex().gap(px(8.));
                if overflowing {
                    actions = actions.child(
                        Button::new(("ai-step-expand", key.0 * 1000 + key.1))
                            .label(if expanded { "收起" } else { "展开" })
                            .h(px(24.))
                            .text_size(px(11.))
                            .on_click(cx.listener(move |s, _, _, cx| {
                                if !s.expanded.remove(&key) {
                                    s.expanded.insert(key);
                                }
                                cx.notify();
                            })),
                    );
                }
                actions = actions.child(
                    Button::new(("ai-step-copy", key.0 * 1000 + key.1))
                        .label("复制")
                        .h(px(24.))
                        .text_size(px(11.))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                );
                card = card.child(actions);
            }
        }
        card.into_any_element()
    }
}
// Keep the persisted v9 output representation compatible; labels are presentation only.
fn output_sections(output: &str) -> (&str, &str) {
    match output.strip_prefix("标准输出：\n") {
        Some(output) => output.split_once("\n标准错误：\n").unwrap_or((output, "")),
        None => (output, ""),
    }
}
fn step_header(
    purpose: &str,
    target: Target,
    automatic: bool,
    state: &str,
    color: Hsla,
    cx: &App,
) -> Div {
    let p = cx.palette();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(5.))
        .text_size(px(11.))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .px(px(5.))
                .py(px(2.))
                .rounded(px(5.))
                .bg(color.opacity(0.1))
                .child(target.label()),
        )
        .when(automatic, |d| {
            d.child(
                div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(2.))
                    .rounded(px(5.))
                    .bg(p.card3)
                    .text_size(px(10.5))
                    .text_color(p.tx2)
                    .child("自动执行"),
            )
        })
        .child(div().flex_1().min_w_0().child(purpose.to_owned()))
        .child(div().flex_none().text_color(color).child(state.to_owned()))
}
fn command_block(command: &str, cx: &App) -> Div {
    div()
        .p(px(9.))
        .rounded(px(8.))
        .bg(shellmate_term::colors::to_hsla(
            cx.terminal_style().palette.background,
        ))
        .text_color(shellmate_term::colors::to_hsla(
            cx.terminal_style().palette.foreground,
        ))
        .font_family(MONO_FONT)
        .text_size(px(12.))
        .line_height(px(18.))
        .whitespace_normal()
        .child(command.to_owned())
}
struct FullAccess {
    drawer: WeakEntity<Drawer>,
    label: String,
    key: String,
}
impl DialogBody for FullAccess {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let key = self.key.clone();
        self.drawer
            .update(cx, |s, cx| {
                // A modal is bound to the host named in its disclosure, even if context changes.
                if s.db
                    .borrow_mut()
                    .set_ai_permission(&key, PermissionMode::Full)
                    .is_err()
                {
                    s.save_error = Some("无法保存审批设置，请重试".into());
                    cx.notify();
                    return false;
                }
                if s.permission_key() == key {
                    *s.mode.lock().unwrap() = PermissionMode::Full;
                }
                cx.notify();
                true
            })
            .unwrap_or(true)
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}
impl Render for FullAccess {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = dialogs::palette(cx);
        dialogs::stack().child(dialogs::hint(format!("AI 将无需批准即可在「{}」上执行命令（rm -rf /、mkfs、reboot 等高危命令仍会请你确认）。此设置会记住，仅对这台主机生效。",self.label),&p)).child(dialogs::actions(vec![dialogs::button("ai-full-cancel","取消",Tone::Plain,true).on_click(cx.listener(|s,_,w,cx|s.dismiss(w,cx))),dialogs::button("ai-full-confirm","开启完全访问",Tone::Danger,true).on_click(cx.listener(|s,_,w,cx|s.submit(w,cx)))]))
    }
}

#[cfg(debug_assertions)]
impl Drawer {
    pub(super) fn agent_scene(&mut self, scene: &str) {
        self.capabilities = Capabilities {
            terminal: true,
            background: true,
            ..Default::default()
        };
        self.label = "prod-web-01".into();
        self.protocol = "SSH".into();
        self.readonly = false;
        self.model.clear();
        self.active_step = None;
        self.expanded.clear();
        self.overflowing_output.clear();
        let redline = scene == "ai-agent-redline";
        *self.mode.lock().unwrap() = if redline {
            PermissionMode::Full
        } else {
            PermissionMode::Ask
        };
        let command = if redline {
            "systemctl reboot"
        } else {
            "docker logs --tail 50 nginx"
        };
        self.model.start(
            if redline {
                "重启这台服务器"
            } else {
                "帮我看一下这台机器的 docker 运行了哪些服务"
            },
            ContextSnapshot {
                label: self.label.clone(),
                protocol: self.protocol.clone(),
                text: String::new(),
            },
        );
        let answer = self
            .model
            .conversation
            .turns
            .last_mut()
            .unwrap()
            .answer
            .get_or_insert_with(Answer::default);
        if !redline {
            answer.items.push(AnswerItem::Step {automatic:true,command:"docker ps --format 'table {{.Names}}\\t{{.Image}}\\t{{.Status}}\\t{{.Ports}}'".into(),target:Target::Background,purpose:"查看正在运行的容器".into(),class:Class::ReadOnly,status:StepStatus::Completed,exit_code:Some(0),duration_ms:1300,output:"标准输出：\nNAMES      IMAGE                STATUS       PORTS\nnginx      nginx:1.27-alpine     Up 12 days   0.0.0.0:443->443/tcp\napi        shellmate/api:2.8.1   Up 3 days    8080/tcp\nworker     shellmate/jobs:2.8.1  Up 3 days    —\npostgres   postgres:16-alpine    Up 12 days   5432/tcp\nredis      redis:7-alpine        Up 12 days   6379/tcp\n标准错误：\n".into()});
            answer.push_text("5 个容器均在运行。接下来检查 nginx 最近日志。\n");
        }
        if scene == "ai-agent-run" {
            answer.items.push(AnswerItem::Step {automatic:false,command:command.into(),target:Target::Terminal,purpose:"检查入口服务日志".into(),class:Class::ReadOnly,status:StepStatus::Completed,exit_code:Some(0),duration_ms:800,output:"标准输出：\n10.0.1.12 GET /health 200 0.002s\n10.0.1.15 GET /api/v1/status 200 0.015s\n10.0.1.12 GET /health 200 0.001s\n标准错误：\n".into()});
            answer.push_text("检查完成：\n• 5 个容器均在运行。\n• nginx 最近请求正常，没有发现 5xx。\n• API、数据库和缓存通过内部网络提供服务。");
            self.model.busy = false;
        } else {
            self.active_step = Some(ActiveStep::new(ToolCall {
                id: "fixture-step-2".into(),
                command: command.into(),
                purpose: if redline {
                    "重启生产服务器"
                } else {
                    "查看 nginx 最近 50 行日志"
                }
                .into(),
                target: Target::Background,
                timeout: Duration::from_secs(30),
                class: classify(command),
            }));
        }
    }
}
