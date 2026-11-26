use super::*;
use shellmate_ai::interaction::{Intent, Phase};
actions!(live_interaction, [SendNow]);
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "cmd-enter",
        SendNow,
        Some("AiLiveInteraction"),
    )]);
}
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn live_fixture(&mut self, scene: &str, w: &mut Window, cx: &mut Context<Self>) {
        if scene.starts_with("ai-multi-"){self.multi_fixture(scene,cx);return;}
        if scene.starts_with("ai-tools-") {
            self.unified_fixture(scene, cx);
            return;
        }
        if scene.starts_with("ai-web-") {
            self.web_fixture(scene, cx);
            return;
        }
        if scene.starts_with("ai-child-") {
            self.child_fixture(scene, w, cx);
            return;
        }
        if scene.starts_with("ai-timing-") {
            self.timing_fixture(scene, cx);
            return;
        }
        if scene == "ai-service-child" {
            let mut view = crate::proto::subagents::view("agent-1", "启动前端开发服务");
            view.status = shellmate_ai::SubagentStatus::Completed;
            view.prompt = "检查启动脚本，启动项目并核实地址，服务保持运行。".into();
            view.transcript = Some(self.state.model.conversation.entries().to_vec());
            view.report = "开发服务已启动：http://localhost:5173/".into();
            view.steps = 1;
            let mut log = shellmate_ai::Conversation::default();
            log.push_user(UserTurn::new("让子代理帮我启动项目", Default::default()));
            log.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
                message_id: 1,
                call: shellmate_ai::LoggedCall { id: view.call_id.clone(), name: "agent".into(), args_json:serde_json::json!({"prompt":view.prompt}) },
                result: shellmate_ai::LoggedResult { inline:view.report.clone(), output_id:view.output_id.clone(), subagent:Some(Box::new(view.display())), cleared:false, is_error:false },
                display: None,
            }));
            log.push_text("服务已交由当前会话管理，可以继续操作。");
            self.state.model.conversation = log;
            self.state.model.busy = false;
            for task in &mut self.fixture_tasks {
                task.owner = Some(view.id.clone());
                task.id = format!("{}/{}", view.id, task.id);
            }
            self.state.subagents.insert(view.id.clone(), view);
            cx.notify();
            return;
        }
        self.state.control.resume();
        self.state.model.busy = !matches!(scene, "ai-live-restored" | "ai-live-target");
        self.state.control.set_phase(match scene {
            "ai-live-waiting" => Phase::Waiting(vec!["agent-1".into()]),
            "ai-live-approval" => Phase::Approval,
            _ => Phase::Executing,
        });
        if scene == "ai-live-waiting" {
            self.state.active_tools.push(shellmate_ai::LoggedCall {
                id: "fixture-wait".into(), name: "wait_tasks".into(),
                args_json: serde_json::json!({"ids":["agent-1"],"mode":"all"}),
            });
            self.state.tool_ids.start("fixture-wait");
            self.fixture_tasks = crate::proto::subagents::tasks().into_iter()
                .filter(|t| t.id == "agent-1").collect();
        }
        if scene == "ai-live-target" {
            self.state.message_target = Some("agent-1".into());
            self.set_composer_text("继续检查测试和启动方式", w, cx);
        } else if scene == "ai-live-btw" {
            self.fixture_btw();
        } else {
            for (n, text) in [
                "先告诉我目前查到的结论",
                "也检查一下错误日志，先不要修改配置",
            ]
            .into_iter()
            .enumerate()
            {
                self.state
                    .control
                    .enqueue(format!("fixture-{n}"), text.into(), Default::default());
            }
            if scene == "ai-live-restored" {
                self.state.control.hold();
            }
        }
        cx.notify();
    }
    pub(super) fn queue_message(&mut self, text: String, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.state.editing_message.clone() {
            if self.state.control.edit(&id, Some(text)).is_some() {
                self.state.editing_message = None;
                self.input.update(cx, |i, cx| i.set_value("", w, cx));
                self.persist();
                cx.notify();
            }
            return;
        }
        let context = self
            .workspace
            .update(cx, |s, cx| s.ai_context(self.session, cx))
            .unwrap_or_default();
        let context=self.message_context(context);
        let id = shellmate_core::HostId::generate().to_string();
        if self.state.control.enqueue(id, text, context) {
            self.input.update(cx, |i, cx| i.set_value("", w, cx));
            self.persist();
            cx.notify();
        }
    }
    pub(super) fn send_to_child(
        &mut self,
        text: String,
        immediate: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.state.message_target.clone() else {
            return;
        };
        if text.trim().is_empty() && immediate {
            if let Some(children) = self
                .tasks
                .as_ref()
                .and_then(|t| t.latest_agent(&id))
                .and_then(|(_, _, t)| t)
            {
                children.control().resume();
                children.control().request(Intent::SendNow);
                cx.notify();
            }
            return;
        }
        let Some(service) = self.subagent_service.clone() else {
            if let Some(scope) = &self.state.interaction_scope
                && (scope["host"].as_str() != self.host.as_deref()
                    || scope["local"].as_bool() != self.binding.as_ref().map(|b| b.local))
            {
                self.child_error("请回到原主机会话后继续子任务".into(), cx);
                return;
            }
            if self.state.readonly && self.runtime.is_none() {
                let local = self
                    .state
                    .model
                    .conversation
                    .latest_user()
                    .is_some_and(|u| u.context.protocol == "本机终端");
                let valid = if local {
                    self.binding.as_ref().is_some_and(|b| b.local)
                } else {
                    self.host == self.state.history_memory_host
                };
                if !valid {
                    self.child_error("请回到原主机会话后继续子任务".into(), cx);
                    return;
                }
            }
            let this = cx.entity().downgrade();
            let conversation = self.state.id.clone();
            super::super::authorize(
                self.db.clone(),
                self.credentials.clone(),
                w,
                cx,
                move |result, w, cx| {
                    this.update(cx, |s, cx| {
                        if s.state.id != conversation
                            || s.state.message_target.as_deref() != Some(&id)
                        {
                            return;
                        }
                        match result {
                            Ok(config) => {
                                if s.install_subagent_service(config, w, cx) {
                                    s.send_to_child(text, immediate, w, cx);
                                }
                            }
                            Err(e) => s.child_error(e.to_string(), cx),
                        }
                    })
                    .ok();
                },
            );
            return;
        };
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let call_id = shellmate_core::HostId::generate().to_string();
        let saved_call = call_id.clone();
        let conversation = self.state.id.clone();
        let label = text.clone();
        let target = id.clone();
        let child_page = self.state.child_page.selected.clone();
        if let Some(id) = &child_page {
            self.state
                .child_page
                .pages
                .entry(id.clone())
                .or_default()
                .error = None;
        }
        let timing_started = timing::Moment::now();
        self.input.update(cx, |i, cx| i.set_value("", w, cx));
        self.state.message_target = self.state.child_page.selected.clone();
        self.state.control.resume();
        let job = runtime.spawn(async move {
            service
                .message_from(
                    &id,
                    &text,
                    immediate,
                    true,
                    &call_id,
                    shellmate_ai::MessageOrigin::User,
                )
                .await
        });
        cx.spawn_in(w,async move|this,cx|{
            let Ok(result)=job.await else{return;};
            this.update_in(cx,|s,w,cx|{
                if s.state.id!=conversation{return;}
                if result.is_error{
                    if let Some(id)=&child_page{
                        let same=s.state.child_page.selected.as_ref()==Some(id);
                        if same&&s.input.read(cx).value().trim().is_empty(){s.set_composer_text(label.clone(),w,cx);}
                        else{s.state.child_page.pages.entry(id.clone()).or_default().unsent.push(label.clone());}
                        s.state.child_page.pages.entry(id.clone()).or_default().error=Some(result.inline);cx.notify();return;
                    }
                    if s.input.read(cx).value().trim().is_empty(){s.state.message_target=Some(target.clone());s.set_composer_text(label.clone(),w,cx);}
                    s.local(slash_ui::LocalBody::Hint(result.inline),cx);return;
                }
                s.state.timing.start(s.state.model.conversation.turns().len().saturating_sub(1),s.state.model.conversation.entries().len(),timing_started);
                if let Some(r)=&s.runtime{let mut d=r.data.lock().unwrap();let turn=d.model.conversation.turns().len().saturating_sub(1);let entry=d.model.conversation.entries().len();d.timing.start(turn,entry,timing_started);d.revision+=1;}
                if result.subagent.is_some(){
                    let step=shellmate_ai::Step{
                        message_id:s.state.model.conversation.entries().len(),call:shellmate_ai::LoggedCall{id:saved_call,name:"send_agent_message".into(),args_json:serde_json::json!({"id":target,"message":label,"resume":true})},
                        result:shellmate_ai::LoggedResult{inline:result.inline,output_id:result.output_id,subagent:result.subagent,is_error:false,cleared:false},display:None,
                    };
                    if s.runtime.is_some()||s.state.model.busy {if let Some(tasks)=&s.tasks{tasks.external_step(step);}}
                    else{s.state.model.conversation.append(shellmate_ai::Entry::Step(step));}
                }else{s.local(slash_ui::LocalBody::Hint("已向子任务追加消息".into()),cx);}
                if let Some(tasks)=&s.tasks && let Some((view,_,_))=tasks.latest_agent(&target) {
                    let pending=s.state.control.snapshot().completions.iter().map(|e|e.task_id.clone()).collect::<Vec<_>>();
                    if view.status==shellmate_ai::SubagentStatus::Running || pending.contains(&view.id) {
                        s.state.timing.await_tasks(&[view.id],&tasks.list(),&pending);
                    }
                }
                if !s.state.model.busy {s.state.timing.done(timing::Moment::now());}
                s.persist();cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }
    fn install_subagent_service(
        &mut self,
        config: shellmate_ai::Config,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.fixture {
            let Some(runtime)=self.prepare_runtime(cx) else{return false;};
            let binding=match runtime.router.resolve(None){Ok(e)=>e.binding,Err(e)=>{self.child_error(e,cx);return false;}};
            let settings=self.db.borrow().ai_settings().unwrap_or_default();let limits=settings.limits();
            let service=Arc::new(crate::ai::subagents::Service {
                router:Some(runtime.router.clone()),tasks:runtime.tasks.clone(),config:config.clone(),settings:settings.clone(),limits,
                db:self.db.clone(),snapshot:Default::default(),context:runtime.router.context(),binding,
                memory:self.memory_files(),ui:runtime.ui.clone(),mode:self.mode.clone(),cancel:Default::default(),factory:runtime.factory.lock().unwrap().clone(),
            });
            *runtime.service.lock().unwrap()=Some(service.clone());self.subagent_service=Some(service);
            runtime.watch(config,limits,settings);return true;
        }
        let binding = self.binding.clone().or_else(|| {
            self.workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten()
        });
        let Some(binding) = binding.filter(|b| {
            b.capabilities.background && b.alive.load(std::sync::atomic::Ordering::Acquire)
        }) else {
            self.child_error("原会话连接不可用".into(), cx);
            return false;
        };
        self.ensure_tasks();
        let ui = if let Some(ui) = &self.task_ui {
            ui.clone()
        } else {
            let (ui, messages) = async_channel::unbounded();
            self.bridge(messages, self.state.id.clone(), w, cx);
            self.task_ui = Some(ui.clone());
            ui
        };
        let settings = self.db.borrow().ai_settings().unwrap_or_default();
        let limits = settings.limits();
        self.subagent_service = Some(Arc::new(crate::ai::subagents::Service {
            router: None,
            tasks: self.tasks.as_ref().unwrap().clone(),
            config: config.clone(),
            settings,
            limits,
            db: self.db.clone(),
            snapshot: Arc::new(Mutex::new(shellmate_ai::memory::Snapshot::load(
                &self.memory_files(),
                &self.label,
                limits.auto_memory,
            ))),
            context: self
                .state
                .model
                .conversation
                .latest_user()
                .map(|u| u.context.clone())
                .unwrap_or_default(),
            binding: binding.clone(),
            memory: self.memory_files(),
            ui,
            mode: self.mode.clone(),
            cancel: self.main_cancel.clone(),
            factory: Arc::new(RigClient::new),
        }));
        self.binding = Some(binding);
        self.state.run_config = Some(config);
        true
    }
    pub(super) fn send_now_action(&mut self, _: &SendNow, w: &mut Window, cx: &mut Context<Self>) {
        if !self.input.focus_handle(cx).contains_focused(w, cx) || self.input_composing(w, cx) {
            cx.propagate();
            return;
        }
        self.send_now(w, cx);
    }
    fn send_now(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if let Some(id) = self.state.child_page.selected.clone() {
            self.state.message_target = Some(id);
        }
        if self.state.message_target.is_some() {
            self.send_to_child(text, true, w, cx);
            return;
        }
        if !text.trim().is_empty() {
            self.queue_message(text, w, cx);
        }
        if self.state.control.snapshot().inputs.is_empty() {
            return;
        }
        self.state.control.resume();
        if self.state.model.busy {
            self.state.control.request(Intent::SendNow);
        } else if self.state.continuation.is_some() {
            self.resume(w, cx);
        } else {
            self.drive_interaction(w, cx);
        }
        self.persist();
        cx.notify();
    }
    pub(super) fn drive_interaction(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(runtime) = &self.runtime {
            let snap = runtime.snapshot();
            if let Some(config) = snap.config {
                runtime.launch(config, snap.limits, snap.settings);
            }
            return;
        }
        if self.fixture
            || self.state.model.busy
            || self.state.pending
            || self.state.continuation.is_some()
        {
            return;
        }
        let pending = self.state.control.pending();
        let completed =
            self.state.control.auto_continue() && self.state.control.pending_completions();
        if !pending && !completed {
            return;
        }
        if self.state.readonly {
            if let Some(scope) = &self.state.interaction_scope {
                let same_host = scope["host"].as_str() == self.host.as_deref();
                let local = self.binding.as_ref().is_some_and(|b| b.local);
                if !same_host || scope["local"].as_bool() != Some(local) {
                    self.state.control.hold();
                    self.local(
                        slash_ui::LocalBody::Hint("请回到原主机会话后恢复待处理消息".into()),
                        cx,
                    );
                    return;
                }
                if let Some(binding) = &mut self.binding {
                    binding.capabilities.cwd = scope["cwd"].as_str().map(str::to_owned);
                }
            }
            self.state.reenter = true;
        }
        if self.binding.is_none() {
            let binding = self
                .workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten();
            if let Some(mut binding) = binding {
                if let Some(scope) = &self.state.interaction_scope {
                    if scope["host"].as_str() != self.host.as_deref()
                        || scope["local"].as_bool() != Some(binding.local)
                    {
                        self.state.control.hold();
                        return;
                    }
                    binding.capabilities.cwd = scope["cwd"].as_str().map(str::to_owned);
                }
                self.binding = Some(binding);
            }
        }
        if let Some(config) = self.state.run_config.clone() {
            if self
                .binding
                .as_ref()
                .is_none_or(|b| !b.alive.load(std::sync::atomic::Ordering::Acquire))
            {
                self.state.control.hold();
                self.local(
                    slash_ui::LocalBody::Hint(
                        "原会话不可用，待处理消息和结果已保留，请恢复原会话后继续".into(),
                    ),
                    cx,
                );
                self.state
                    .timing
                    .finish(timing::Status::Interrupted, timing::Moment::now());
                self.persist();
                return;
            }
            self.state.reenter = true;
            self.state.model.busy = true;
            self.start(config, w, cx);
        } else if pending {
            self.begin(None, w, cx);
        }
    }
    fn stop_reply(&mut self, cx: &mut Context<Self>) {
        if let Some(runtime) = &self.runtime {
            runtime.stop_reply();
            cx.notify();
            return;
        }
        if self.state.pending {
            self.stop(cx);
            return;
        }
        if !self.state.model.busy {
            self.state
                .timing
                .finish(timing::Status::Stopped, timing::Moment::now());
        }
        self.state.control.request(Intent::StopReply);
        self.persist();
        cx.notify();
    }
    pub(super) fn stop_all_tasks(&mut self, cx: &mut Context<Self>) {
        if let Some(runtime) = &self.runtime {
            runtime.stop_all();
            cx.notify();
            return;
        }
        self.state.control.hold();
        if let Some(tasks) = &self.tasks {
            tasks.stop_all();
        }
        self.stop(cx);
        cx.notify();
    }
    pub(super) fn interaction_bar(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let p = cx.palette();
        let snapshot = self.state.control.snapshot();
        let running = self.state.model.busy
            || self.state.pending
            || (self.state.timing.live() && !self.state.timing.paused());
        if !running && snapshot.inputs.is_empty() && self.state.message_target.is_none() {
            return None;
        }
        let mut bar = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .text_size(px(11.5))
            .text_color(p.tx2);
        if let Some(id) = &self.state.message_target {
            let label = self
                .state
                .subagents
                .get(id)
                .map(|v| v.description.clone())
                .unwrap_or_else(|| "子任务".into());
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(format!("发送给：{label}")),
                    )
                    .child(
                        agent_ui::step_action("clear-message-target", "取消", cx)
                            .flex_none()
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.state.message_target = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        for input in snapshot.inputs.iter().take(if self.state.queue_expanded {
            usize::MAX
        } else {
            4
        }) {
            let id = input.id.clone();
            let edit = id.clone();
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().flex_none().child(if input.claimed {
                    "已接收"
                } else if input.editing {
                    "编辑中"
                } else if snapshot.held {
                    "待恢复"
                } else {
                    "待处理"
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_ellipsis()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(input.text.clone()),
                );
            if !input.claimed {
                row = row.child(
                    agent_ui::step_action(
                        SharedString::from(format!("input-edit-{id}")),
                        "编辑",
                        cx,
                    )
                    .on_click(cx.listener(move |s, _, w, cx| {
                        if let Some(text) = s.state.control.edit(&edit, None) {
                            let draft = s.input.read(cx).value().to_string();
                            if !draft.trim().is_empty() {
                                s.queue_message(draft, w, cx);
                            }
                            s.state.editing_message = Some(edit.clone());
                            s.set_composer_text(text, w, cx);
                            s.persist();
                            cx.notify();
                        }
                    })),
                );
                row = row.child(
                    agent_ui::step_action(
                        SharedString::from(format!("input-remove-{id}")),
                        "撤回",
                        cx,
                    )
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.state.control.withdraw(&id);
                        if s.state.editing_message.as_deref() == Some(&id) {
                            s.state.editing_message = None;
                        }
                        s.persist();
                        cx.notify();
                    })),
                );
            }
            bar = bar.child(row);
        }
        if snapshot.inputs.len() > 4 {
            bar = bar.child(
                agent_ui::step_action(
                    "queue-expand",
                    if self.state.queue_expanded {
                        "收起队列"
                    } else {
                        "展开队列"
                    },
                    cx,
                )
                .on_click(cx.listener(|s, _, _, cx| {
                    s.state.queue_expanded = !s.state.queue_expanded;
                    cx.notify();
                })),
            );
        }
        let mut actions = agent_ui::step_actions();
        if !snapshot.inputs.is_empty() {
            actions = actions.child(
                agent_ui::step_action("input-now", "立即处理", cx)
                    .on_click(cx.listener(|s, _, w, cx| s.send_now(w, cx))),
            );
        }
        if running {
            let phase = self.state.control.phase();
            bar = bar.child(div().text_color(p.tx3).child(
                if self.state.model.busy || self.state.pending {
                    phase.label()
                } else {
                    "等待异步任务"
                },
            ));
            if matches!(phase, Phase::Executing | Phase::Waiting(_)) {
                actions = actions.child(
                    agent_ui::step_action("input-detach", "结束等待", cx).on_click(cx.listener(
                        |s, _, _, cx| {
                            if let Some(tasks) = &s.tasks {
                                tasks.promote_foreground();
                            }
                            cx.notify();
                        },
                    )),
                );
            }
            actions = actions.child(
                agent_ui::step_action("input-stop-reply", "停止回复", cx)
                    .on_click(cx.listener(|s, _, _, cx| s.stop_reply(cx))),
            );
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .id("input-queue-scroll")
                        .max_h(px(if window.viewport_size().height < px(720.) {
                            80.
                        } else {
                            160.
                        }))
                        .overflow_y_scroll()
                        .child(bar),
                )
                .child(actions),
        )
    }
}
