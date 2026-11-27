use super::*;
use crate::ai::host::{Approval, Ui};
use crate::ai::scroll::ScrollArea;
use crate::dialogs::{self, DialogBody, Tone};
use gpui::component::button::ButtonVariants;
use gpui::component::input::{Textarea, TextareaState};
use shellmate_ai::{
    Answer, AnswerItem, StepStatus,
    agent::{Target, ToolCall, ToolOutcome},
    policy::{Class, classify},
};
use std::time::{Duration, Instant};


pub(super) struct ActiveStep {
    pub source: Option<shellmate_ai::SubagentRef>,
    pub call: ToolCall,
    pub reply: Option<crate::ai::host::Reply>,
    pub redline: bool,
    pub started: Option<Instant>,
    pub automatic: bool,
    pub edit: Option<Entity<TextareaState>>,
    _edit_subscription: Option<Subscription>,
    pub reason: Option<Entity<InputState>>,
}
impl ActiveStep {
    pub fn new(call: ToolCall) -> Self {
        let redline = call.class == Class::RedLine;
        Self {
            source: None,
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
    pub(super) fn task_strip(&self, cx: &mut Context<Self>) -> Option<Div> {
        let tasks = if self.fixture {
            self.fixture_tasks.clone()
        } else {
            self.tasks.as_ref().map(|t| t.running()).unwrap_or_default()
        };
        let running: Vec<_> = tasks
            .into_iter()
            .filter(|t| t.kind == crate::ai::tasks::Kind::Command && t.status == crate::ai::tasks::Status::Running && t.background)
            .collect();
        if running.is_empty() {
            return None;
        }
        let p = cx.palette();
        let mut strip = div()
            .id("running-command-list")
            .max_h(px(144.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.))
            .pb(px(6.))
            .mb(px(2.))
            .border_b_1()
            .border_color(p.line);
        for task in running {
            let id = task.id.clone();
            let stop_id = id.clone();
            let output_id = task.output_id.clone();
            let summary = format!("{}{}{}", task.command,
                task.destination.as_ref().map(|d| format!(" · {}", d.label)).unwrap_or_default(),
                task.service.map(|s| format!(" · {}", s.label())).unwrap_or_default());
            strip = strip.child(
                div().flex().flex_col().gap(px(3.)).min_w_0().text_size(px(11.5)).text_color(p.tx2)
                    .child(div().flex().items_center().gap(px(6.)).min_w_0()
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(summary))
                        .child(div().flex_none().text_color(p.tx3).child(format!("{}s", task.started.elapsed().as_secs()))))
                    .child(div().flex().flex_wrap().items_center().gap(px(5.))
                        .child(div().flex_none().px(px(5.)).py(px(2.)).rounded(px(5.)).bg(p.card3).child(task.target_label()))
                        .child(step_action(SharedString::from(format!("task-output-{id}")), "查看输出", cx)
                            .on_click(cx.listener(move |s, _, w, cx| s.reveal_output(&output_id, w, cx))))
                        .child(Button::new(SharedString::from(format!("task-stop-{id}"))).label("停止").h(px(24.)).text_size(px(11.5))
                            .on_click(cx.listener(move |s, _, _, cx| {
                                if s.fixture {
                                    if let Some(task) = s.fixture_tasks.iter_mut().find(|t| t.id == stop_id) {
                                        task.status = crate::ai::tasks::Status::Stopped;
                                    }
                                } else if let Some(tasks) = &s.tasks {
                                    tasks.stop(&stop_id);
                                }
                                cx.notify();
                            })))),
            );
        }
        Some(div().child(strip))
    }
    pub(super) fn permission_key(&self) -> &str {
        self.host.as_deref().unwrap_or("local")
    }
    pub(super) fn release_context(&mut self, cx: &mut Context<Self>) {
        if self.runtime.is_some(){return;}
        if !self.state.model.busy
            && self.state.child_page.selected.is_none()
            && !self.state.pending
            && !self.state.timing.live()
            && self.state.continuation.is_none()
            && !self.has_running_tasks()
            && self.state.control.snapshot().inputs.is_empty()
            && !self.state.control.pending_completions()
            && let Some(context) = self.next_context.take()
        {
            self.sync_context(context, cx);
        }
    }

    pub(super) fn agent_event(&mut self, event: AgentEvent) {
        match &event{
            AgentEvent::StepProposed(call)=>self.state.tool_ids.start(&call.id),
            AgentEvent::ToolStarted(call)=>self.state.tool_ids.start(&call.id),
            AgentEvent::StepFinished{call,..}=>{if let Some(shellmate_ai::Entry::Step(step))=self.state.model.conversation.entries().iter().rev().find(|e|matches!(e,shellmate_ai::Entry::Step(_))){self.state.tool_ids.finish(&call.id,&step.call.id);}}
            AgentEvent::ToolFinished(id)=>{if let Some(shellmate_ai::Entry::Step(step))=self.state.model.conversation.entries().iter().rev().find(|e|matches!(e,shellmate_ai::Entry::Step(_))){self.state.tool_ids.finish(id,&step.call.id);}}
            AgentEvent::Done(_)|AgentEvent::Released(_)|AgentEvent::Error(_)|AgentEvent::Paused{..}=>self.state.tool_ids.clear_active(),
            _=>{}
        }
        self.timing_event(&event);
        if !matches!(event, AgentEvent::Retrying { .. }) {
            self.state.model.retrying = None;
        }
        match event {
            AgentEvent::ToolStarted(call)=>self.state.active_tools.push(call),
            AgentEvent::ToolFinished(id)=>self.state.active_tools.retain(|c|c.id!=id),
            AgentEvent::InputsAccepted(ids)=>{self.state.active_tools.clear();self.state.accepted_inputs.extend(ids);},
            AgentEvent::CompletionsAccepted(ids)=>self.state.accepted_events.extend(ids),
            AgentEvent::Retrying { attempt, max, delay, error } => {
                self.state.model.retrying = Some(format!("{error}，{} 秒后重试（第 {attempt}/{max} 次）", delay.as_secs()));
            }
            AgentEvent::Context { used, window, trigger } => {
                if let Some((budget, _)) = &mut self.state.run_budget {
                    budget.window = window;
                    budget.trigger = trigger;
                    budget.reserve = budget.auto.saturating_sub(trigger);
                }
                self.state.model.event(Event::Context { used, window, trigger });
            }
            AgentEvent::Compacting { tier } => self.state.model.event(Event::Compacting { tier }),
            AgentEvent::Compacted { before, after, summary } => self.state.model.event(Event::Compacted { before, after, summary }),
            AgentEvent::Reasoning {block,..} => self.state.model.thinking = !block.complete,
            AgentEvent::Thinking => self.state.model.event(Event::Thinking),
            AgentEvent::TextDelta(_) | AgentEvent::Command(_) => self.state.model.thinking = false,
            AgentEvent::Done(u) => {
                self.state.active_tools.clear();
                self.state.model.event(Event::Done(u));
                self.state.model.manual_compaction = false;
                // Retain the run configuration for queued input and background-result delivery.
            }
            AgentEvent::Released(_) => {
                self.state.active_tools.clear();
                self.state.model.event(Event::Done(None));
                self.state.model.manual_compaction = false;
            }
            AgentEvent::Error(e) => {
                self.state.active_tools.clear();
                self.state.control.hold();self.state.control.release_claims();
                self.state.model.event(Event::Error(e));
                self.state.model.manual_compaction = false;
                if self.state.continuation.is_none() { self.state.run_config = None; }
            }
            // The bridge owns the pending card; separate channels can deliver this after Approval.
            AgentEvent::StepProposed(_) => self.state.model.thinking = false,
            AgentEvent::StepFinished { .. } => { self.state.active_steps.clear_main(); }
            AgentEvent::Paused { reason, steps, resume } => {
                self.state.active_tools.clear();
                self.state.control.hold();
                self.state.model.pause(reason);
                self.state.continuation = Some(resume);
                self.state.resume_steps = steps;
            }
        }
    }
    pub(super) fn resume(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.state.continuation.is_none() || self.state.model.busy {
            return;
        }
        if let Some(config) = self.state.run_config.clone() {
            self.state.control.resume();self.state.control.reset_budget();
            self.state.model.busy = true;
            self.state.model.error = None;
            self.state.model.pause_reason = None;
            self.start(config, w, cx);
        }
    }
    pub(super) fn bridge(
        &self,
        messages: async_channel::Receiver<Ui>,
        conversation: String,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let timer_conversation=conversation.clone();
        cx.spawn_in(w, async move |this, cx| {
            while let Ok(event) = messages.recv().await {
                let finish = matches!(event, Ui::TerminalFinish { .. });
                match event {
                    Ui::Context { reply } => {
                        let snapshot = this.update_in(cx, |s, _, cx| {
                            s.workspace.update(cx, |w, cx| w.ai_context(s.session, cx)).ok()
                        }).ok().flatten();
                        let _ = reply.send(snapshot);
                    }
                    // Cleanup survives drawer cancellation/destruction and ignores the UI epoch.
                    Ui::TerminalFinish { session, nonce } => {
                        workspace
                            .update_in(cx, |s, _, cx| {
                                s.ai_terminal_finish(session, &nonce, finish, cx)
                            })
                            .ok();
                    }
                    Ui::TerminalStart {
                        session,
                        command,
                        nonce,
                        spool,
                        lease,
                        reply,
                    } => {
                        let valid = this
                            .update_in(cx, |s, _, _| s.state.id == conversation)
                            .unwrap_or(false);
                        if reply.is_closed() || !valid {
                            continue;
                        }
                        let result = workspace
                            .update_in(cx, |s, _, cx| {
                                s.ai_terminal_start(session, &command, &nonce, &lease, spool, cx)
                            })
                            .unwrap_or(Err(Box::new(ToolOutcome::Disconnected)));
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
                            if s.state.id != conversation {
                                return;
                            }
                            match event {
                                Ui::Approval {
                                    call,
                                    redline,
                                    reply,
                                    source,
                                } => {
                                    if reply.is_closed(){return;}
                                    let key=crate::ai::host::approval_key(source.as_ref(),&call.id);
                                    let mut step = ActiveStep::new(call);
                                    step.source=source.clone();
                                    step.reply = Some(crate::ai::host::Reply::new(reply));
                                    step.redline = redline;
                                    s.state.active_steps.insert(key,source.map(|s|s.id),step);
                                    if s.state.child_page.selected.is_none(){s.state.follow_bottom = true;}
                                }
                                Ui::Running {
                                    call,
                                    automatic,
                                    ready,
                                    source,
                                } => {
                                    if ready.is_closed(){return;}
                                    let key=crate::ai::host::approval_key(source.as_ref(),&call.id);
                                    if source.is_some(){s.state.active_steps.remove(&key);let _=ready.send(());cx.notify();return;}
                                    let mut step = ActiveStep::new(call);
                                    step.started = Some(Instant::now());
                                    step.automatic = automatic;
                                    s.state.active_steps.insert(key,None,step);
                                    let _ = ready.send(());
                                }
                                Ui::CancelApproval(id) | Ui::Finished(id) => {s.state.active_steps.remove(&id);}
                                Ui::CancelSource(id) => {s.state.active_steps.remove_source(&id);}
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
                    .update_in(cx, |s, window, cx| {
                        if s.state.id != timer_conversation || (!s.state.model.busy && !s.has_running_tasks()) {
                            return false;
                        }
                        if window.is_visible() { cx.notify(); }
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
    pub(super) fn set_permission(&mut self, mode: PermissionMode, cx: &mut Context<Self>) -> bool {
        if super::super::model::save_permission(&mut self.db.borrow_mut(), self.permission_key(), &self.mode, mode).is_err()
        {
            self.state.save_error = Some("无法保存审批设置，请重试".into());
            cx.notify();
            return false;
        }
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
    fn approve(&mut self, key: &str, deny: bool, cx: &mut Context<Self>) {
        let Some(step) = self.state.active_steps.get_mut(key) else {
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
            self.state.active_steps.remove(key);
        }
        cx.notify();
    }
    pub(super) fn active_card(&self, key: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = cx.palette().clone();
        let step = self.state.active_steps.get(key).unwrap();
        let key = key.to_owned();
        let red = step
            .edit
            .as_ref()
            .map(|i| classify(&i.read(cx).value()) == Class::RedLine)
            .unwrap_or(step.redline);
        let scope=step.source.as_ref().map(|s|format!("child:{}",s.id)).unwrap_or_else(||"main".into());
        let display_key=self.tool_key(&scope,&step.call.id,true);
        let status=if step.started.is_some(){crate::ai::tool_model::Status::Running}else{crate::ai::tool_model::Status::Approval};
        let expanded=self.tool_open(&display_key,status);
        let detail=format!("{}执行{}{}{}",step.call.target.label(),if step.call.service.is_some(){" · 持续服务"}else{""},if step.automatic{" · 自动批准"}else{""},step.source.as_ref().map(|s|format!(" · 子任务：{}",s.description)).unwrap_or_default());
        let detail=if let Some(target)=&step.call.destination{format!("{} · {} · {} · {}",target.label,target.protocol,target.cwd.as_deref().unwrap_or("目录未知"),detail)}else{detail};
        let model=super::tool_ui::Card{key:display_key.clone(),title:"执行命令".into(),status,duration:step.started.map(|t|t.elapsed().as_millis().min(u64::MAX as u128)as u64),summary:step.call.command.clone(),detail};
        let mut card=self.tool_shell(&model,cx);
        if red&&step.started.is_none(){card=card.child(div().text_size(px(12.)).text_color(p.err).font_weight(FontWeight::SEMIBOLD).child("高危命令，需要确认"));}
        if expanded {card=card.child(super::tool_ui::section("输入",&p));
        card = if let Some(edit) = &step.edit {
            card.child(
                Textarea::new(edit)
                    .text_size(px(12.5))
                    .font_family(MONO_FONT),
            )
        } else {
            card.child(command_block(
                &step.call.command,
                SharedString::from(format!("{display_key}:command")),
                self.selection_scroll.clone(),
                cx,
            ))
        };
        }
        if step.started.is_some() {
            if expanded && let Some(output)=self.state.output_panes.get(&display_key){card=card.child(super::tool_ui::section("输出",&p)).child(output.clone());}
            let scope=scope.clone();let call_id=step.call.id.clone();
            card=card.child(step_actions().child(step_action(SharedString::from(format!("{display_key}:stop")),"停止任务",cx).on_click(cx.listener(move|s,_,_,cx|{if let Some(tasks)=s.tool_tasks(&scope)&&let Some(task)=s.tool_task(&scope,&call_id,None){tasks.stop(&task.id);}cx.notify();}))));
        } else {
            let caps = step.call.session_id.as_deref().and_then(|id| {
                self.runtime.as_ref()?.router.catalog.resolve(id).ok().map(|e| e.binding.capabilities)
            }).unwrap_or_else(|| self.capabilities.clone());
            if caps.terminal && caps.background {
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
                        .on_click(cx.listener({ let key=key.clone(); move |s, _, _, cx| {
                            if let Some(step) = s.state.active_steps.get_mut(&key) {
                                step.call.target = target;
                            }
                            cx.notify();
                        } })),
                    );
                }
                card = card.child(row);
            }
            if let Some(reason) = &step.reason {
                card = card.child(crate::ui::field_input(reason, crate::ui::Surface::Card2, false)).child(
                    dialogs::button("ai-step-deny-confirm", "确认拒绝", Tone::Plain, true)
                        .on_click(cx.listener({ let key=key.clone(); move |s, _, _, cx| s.approve(&key, true, cx) })),
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
                            .on_click(cx.listener({ let key=key.clone(); move |s, _, _, cx| s.approve(&key, false, cx) })),
                        )
                        .child(
                            dialogs::button("ai-step-edit", "编辑", Tone::Plain, true).on_click(
                                cx.listener({ let key=key.clone(); move |s, _, w, cx| {
                                    if let Some(step) = s.state.active_steps.get_mut(&key) {
                                        let command = step.call.command.clone();
                                        let edit = cx.new(|cx| {
                                            // Approval must expose the entire edited command too.
                                            let mut i =
                                                TextareaState::new(w, cx).auto_grow(1, usize::MAX);
                                            i.set_value(command, w, cx);
                                            i
                                        });
                                        step._edit_subscription =
                                            Some(cx.observe(&edit, |_, _, cx| cx.notify()));
                                        step.edit = Some(edit);
                                    }
                                    cx.notify();
                                } }),
                            ),
                        )
                        .child(
                            dialogs::button("ai-step-deny", "拒绝", Tone::Plain, true).on_click(
                                cx.listener({ let key=key.clone(); move |s, _, w, cx| {
                                    if let Some(step) = s.state.active_steps.get_mut(&key) {
                                        step.reason = Some(cx.new(|cx| {
                                            InputState::new(w, cx).placeholder("拒绝原因（可选）")
                                        }));
                                    }
                                    cx.notify();
                                } }),
                            ),
                        ),
                );
            }
        }
        card.into_any_element()
    }
}

pub(super) fn step_surface(p: &crate::theme::Palette) -> Div {
    div().p(px(10.)).rounded(px(12.)).bg(p.card2)
}

pub(super) fn step_actions() -> Div {
    div()
        .flex_none()
        .w_full()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(12.))
}

pub(super) fn step_action(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Button {
    let label = label.into();
    let p = cx.palette();
    Button::new(id)
        .text()
        .h(px(24.))
        .accessibility_label(label.clone())
        // Button's built-in label applies its own size and hover foreground.
        // Style the content directly to keep these controls at 12.5 px / tx2.
        .child(
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .text_size(px(12.5))
                .text_color(p.tx2)
                .hover(|style| style.text_color(p.acc))
                .child(label),
        )
}

pub(super) fn command_block(command: &str, id: SharedString, selection: Entity<crate::ai::selection_scroll::SelectionScroll>, cx: &App) -> Div {
    let bg = shellmate_term::colors::to_hsla(cx.terminal_style().palette.background);
    command_surface(cx).child(ScrollArea::new(
        id,
        div()
            .flex()
            .flex_col()
            .items_start()
            .whitespace_nowrap()
            .child(div().flex_none().child(crate::ai::markdown::plain("command", command.to_owned()))),
        bg,
    ).selection(selection))
}
pub(super) fn output_scroll(id:SharedString,content:Div,expanded:bool,selection:Entity<crate::ai::selection_scroll::SelectionScroll>,p:&crate::theme::Palette)->ScrollArea{
    ScrollArea::new(id,div().flex().flex_col().items_start().text_size(px(12.5)).line_height(px(18.)).font_family(MONO_FONT).whitespace_nowrap().text_color(p.tx2).child(content),p.card2).selection(selection).height_limit(if expanded{320.}else{8.*18.},expanded)
}
pub(super) fn command_surface(cx: &App) -> Div {
    div()
        .min_w_0()
        .p(px(11.))
        .rounded(px(10.))
        .bg(shellmate_term::colors::to_hsla(
            cx.terminal_style().palette.background,
        ))
        .text_color(shellmate_term::colors::to_hsla(
            cx.terminal_style().palette.foreground,
        ))
        .font_family(MONO_FONT)
        .text_size(px(12.5))
        .line_height(px(18.))
        .whitespace_normal()
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
                    s.state.save_error = Some("无法保存审批设置，请重试".into());
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
        self.state.readonly = false;
        self.state.model.clear();
        self.state.active_steps.clear_main();
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
        self.state.model.start(
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
        let mut answer = Answer::default();
        if !redline {
            answer.items.push(AnswerItem::Step {automatic:true,command:"docker ps \\\n  --no-trunc \\\n  --filter status=running \\\n  --format 'table {{.Names}}\\t{{.Image}}\\t{{.Status}}\\t{{.Ports}}'".into(),target:Target::Background,purpose:"检查生产环境所有容器的运行状态并确认镜像版本和对外端口映射".into(),class:Class::ReadOnly,status:StepStatus::Completed,exit_code:Some(0),duration_ms:1300,output:"标准输出：\nNAMES      IMAGE                STATUS       PORTS\nnginx      nginx:1.27-alpine     Up 12 days   0.0.0.0:443->443/tcp\napi        shellmate/api:2.8.1   Up 3 days    8080/tcp\nworker     shellmate/jobs:2.8.1  Up 3 days    —\npostgres   postgres:16-alpine    Up 12 days   5432/tcp\nredis      redis:7-alpine        Up 12 days   6379/tcp\n标准错误：\n".into()});
            answer.push_text("5 个容器均在运行。接下来检查 nginx 最近日志。\n");
        }
        if scene == "ai-agent-run" {
            answer.items.push(AnswerItem::Step {automatic:false,command:command.into(),target:Target::Terminal,purpose:"检查入口服务日志".into(),class:Class::ReadOnly,status:StepStatus::Completed,exit_code:Some(0),duration_ms:800,output:"标准输出：\n10.0.1.12 GET /health 200 0.002s\n10.0.1.15 GET /api/v1/status 200 0.015s\n10.0.1.12 GET /health 200 0.001s\n标准错误：\n".into()});
            answer.push_text(crate::proto::ai::AGENT_ANALYSIS);
            self.state.model.busy = false;
        } else {
            let step = ActiveStep::new(ToolCall {
                service: None,
                session_id:None,cwd: None, destination: None,
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
                run_in_background: false,
                exec_limits: shellmate_ai::agent::ExecLimits::default(),
                class: classify(command),
            });
            self.state.active_steps.insert(step.call.id.clone(),None,step);
        }
        for item in answer.items { self.state.model.conversation.push_item(item); }
    }
}
