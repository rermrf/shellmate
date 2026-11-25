#[path = "subagent_ui.rs"]
mod subagent_ui;
#[path = "mode_ui.rs"]
mod mode_ui;
#[path = "agent_ui.rs"]
pub(super) mod agent_ui;
use super::markdown::{self, MarkdownCache};
use super::model::SUGGESTIONS;
#[path = "panel_state.rs"]
mod state;
#[path = "slash_ui.rs"]
mod slash_ui;
#[path = "context_ui.rs"]
mod context_ui;
#[path = "composer_layout.rs"]
mod composer_layout;
use crate::{
    theme::{ActivePalette, MONO_FONT},
    workspace::Workspace,
};
use futures::StreamExt;
use gpui::{
    component::{
        Disableable, Sizable, Size,
        button::Button,
        input::{self, InputEvent, InputState, RopeExt, Textarea, TextareaState},
    },
    prelude::*,
    *,
};
use shellmate_ai::agent::{self, AgentEvent, Capabilities};
use shellmate_ai::policy::PermissionMode;
use shellmate_ai::{
    AiError, CommandCard, Event, ProviderClient, RigClient, Risk, Turn, UserTurn,
};
#[cfg(debug_assertions)]
use shellmate_ai::ContextSnapshot;
use shellmate_store::{CredentialStore, SharedStore, ai::ConversationSummary};
#[cfg(debug_assertions)]
use shellmate_store::ai::SavedConversation;
use state::{NewConversation, new_conversation_tooltip};
use std::sync::Arc;
use std::sync::Mutex;
const CONTEXT_POPOVER_KEYS: &str = "AiContextPopover";
pub fn init(cx: &mut App) {
    // The popover owns focus outside the textarea's Input key context.
    cx.bind_keys([KeyBinding::new("escape", input::Escape, Some(CONTEXT_POPOVER_KEYS))]);
}
pub struct SessionContext {
    pub session: Option<usize>,
    pub host: Option<String>,
    pub label: String,
    pub protocol: String,
    pub binding: Option<super::exec::Binding>,
}
pub struct Drawer {
    state: state::ConversationState,
    popup: super::commands::Popup,
    context_focus: FocusHandle,
    compact_queue: Arc<Mutex<Option<Option<String>>>>,
    data_root: std::path::PathBuf,
    tasks: Option<super::tasks::Tasks>,
    task_ui: Option<async_channel::Sender<super::host::Ui>>,
    main_cancel: shellmate_ai::subagent::Cancellation,
    fixture_tasks: Vec<super::tasks::TaskInfo>,
    fixture_context: Option<shellmate_ai::ContextBreakdown>,
    save_conversations: bool,
    workspace: WeakEntity<Workspace>,
    db: SharedStore,
    credentials: Arc<CredentialStore>,
    input: Entity<TextareaState>,
    _subscription: Subscription,
    _input_layout_subscription: Subscription,
    pub label: String,
    pub protocol: String,
    pub host: Option<String>,
    pub session: Option<usize>,
    provider: String,
    model_id: String,
    epoch: u64,
    abort: Option<tokio::task::AbortHandle>,
    pub history: bool,
    history_rows: Vec<ConversationSummary>,
    pub configured: bool,
    pub fixture: bool,
    scroll: ScrollHandle,
    selection_scroll: Entity<super::selection_scroll::SelectionScroll>,
    _selection_subscription: Subscription,
    fixture_copy: markdown::CopyFixture,
    capabilities: Capabilities,
    mode: Arc<Mutex<PermissionMode>>,
    permission_menu: bool,
    binding: Option<super::exec::Binding>,
    next_context: Option<SessionContext>,
}
impl Drawer {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        db: SharedStore,
        credentials: Arc<CredentialStore>,
        data_root: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .soft_wrap(true)
                .submit_on_enter(true)
                .placeholder("描述要做的事，例如：找出最占内存的 5 个进程")
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |s: &mut Self, _, event: &InputEvent, w, cx| {
                match event {
                    InputEvent::PressEnter { shift, .. } => {
                        if super::commands::submits_enter(*shift, s.input_composing(w, cx)) { s.send_input(w, cx); }
                    }
                    InputEvent::Change => s.input_changed(cx),
                    _ => {}
                }
            },
        );
        let input_layout_subscription = cx.observe(&input, |_, _, cx| cx.notify());
        let scroll = ScrollHandle::new();
        let selection_scroll = cx.new(|_| super::selection_scroll::SelectionScroll::new(scroll.clone()));
        let selection_subscription = cx.subscribe(&selection_scroll, |s: &mut Self, _, _: &super::selection_scroll::DragStarted, cx| {
            s.state.follow_bottom = false;
            cx.notify();
        });
        let configured = db.borrow().ai_settings().is_ok_and(|s| s.configured());
        let save_conversations = saving_enabled(&db);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update_in(cx, |s, _, cx| {
                        if let Some(tasks)=&s.tasks {
                            for view in tasks.agent_views(){
                                if let Some(old)=s.state.subagents.get_mut(&view.id){old.refresh(view);}else{s.state.subagents.insert(view.id.clone(),view);}
                            }
                        }
                        if !s.state.model.busy
                            && let Some(tasks) = &s.tasks
                        {
                            let notices = tasks.take_notices();
                            let changed = tasks.sync_agents(&mut s.state.model.conversation) || !notices.is_empty();
                            for text in notices {
                                s.state
                                    .model
                                    .conversation
                                    .append(shellmate_ai::Entry::Notice {
                                        kind: "task".into(),
                                        text,
                                    });
                            }
                            if changed {
                                s.persist();
                            }
                        }
                        if s.tasks.is_some() {
                            s.release_context(cx);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            popup: Default::default(),
            context_focus: cx.focus_handle(),
            compact_queue: Default::default(),
            data_root,
            tasks: None,
            task_ui: None,
            main_cancel: Default::default(),
            fixture_tasks: Vec::new(),
            fixture_context: None,
            state: state::ConversationState::default(),
            save_conversations,
            workspace,
            db,
            credentials,
            input,
            _subscription: subscription,
            _input_layout_subscription: input_layout_subscription,
            label: "无会话".into(),
            protocol: String::new(),
            host: None,
            session: None,
            provider: String::new(),
            model_id: String::new(),
            epoch: 0,
            abort: None,
            history: false,
            history_rows: vec![],
            configured,
            fixture: false,
            scroll,
            selection_scroll,
            _selection_subscription: selection_subscription,
            fixture_copy: Default::default(),
            capabilities: Capabilities::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
            permission_menu: false,
            binding: None,
            next_context: None,
        }
    }
    pub fn sync_context(&mut self, context: SessionContext, cx: &mut Context<Self>) {
        // Opening the drawer and saving settings both arrive here, even during a run.
        self.save_conversations = saving_enabled(&self.db);
        cx.notify();
        if self.fixture {
            return;
        }
        if self.state.model.busy || self.state.pending || self.state.continuation.is_some() || self.has_running_tasks() {
            self.next_context = Some(context);
            return;
        }
        if self.session != context.session {
            self.state.context_popover.close();
            super::mode_popover::close(&mut self.permission_menu);
            self.retire_tasks();
        }
        self.session = context.session;
        self.host = context.host;
        self.label = context.label;
        self.protocol = context.protocol;
        self.binding = context.binding;
        self.capabilities = self
            .binding
            .as_ref()
            .map(|b| b.capabilities.clone())
            .unwrap_or_default();
        *self.mode.lock().unwrap() = self
            .db
            .borrow()
            .ai_permission(self.permission_key())
            .unwrap_or_default();
        if !self.fixture {
            self.configured = self.db.borrow().ai_settings().is_ok_and(|s| s.configured());
        }
        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub(crate) fn debug_focus_composer(&self, w: &mut Window, cx: &mut Context<Self>) {
        self.input.focus_handle(cx).focus(w,cx);
    }
    #[cfg(debug_assertions)]
    pub(crate) fn debug_viewport_at(&self, position: Point<Pixels>, cx: &App) -> Option<ScrollHandle> {
        self.selection_scroll.read(cx).debug_viewport_at(position,cx)
    }
    fn input_changed(&mut self, cx: &mut Context<Self>) {
        self.popup.changed(&self.input.read(cx).value());
        if self.popup.open { super::mode_popover::show(super::mode_popover::Kind::Slash,&mut self.permission_menu,&mut self.state.context_popover,&mut self.popup); }
        cx.notify();
    }
    fn set_composer_text(&mut self, text: impl Into<SharedString>, w: &mut Window, cx: &mut Context<Self>) {
        let text = text.into();
        self.input.update(cx, |input, cx| {
            input.set_value(text, w, cx);
            // Textarea set_value resets to 0..0 and emits no Change. Use the
            // rope's UTF-16 conversion, including multi-line/Unicode text.
            let end = input.text().offset_to_position(input.text().len());
            input.set_cursor_position(end, w, cx);
        });
        self.input_changed(cx);
    }
    fn input_composing(&self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        self.input.update(cx, |input, cx| input.marked_text_range(w, cx).is_some())
    }
    fn send_input(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        self.dispatch_input(&text, w, cx);
    }

    pub fn ask(&mut self, question: String, w: &mut Window, cx: &mut Context<Self>) {
        if self.state.pending || self.state.model.busy || question.trim().is_empty() {
            return;
        }
        self.state.continuation = None;
        self.state.run_config = None;
        self.state.run_budget = None;
        self.release_context(cx);
        let context = self
            .workspace
            .update(cx, |s, cx| s.ai_context(self.session, cx))
            .unwrap_or_default();
        let user = UserTurn::new(&question, context);
        if user.question.trim().is_empty() {
            return;
        }
        if !self.fixture {
            self.binding = self
                .workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten();
            self.capabilities = self
                .binding
                .as_ref()
                .map(|b| b.capabilities.clone())
                .unwrap_or_default();
        }
        self.input.update(cx, |i, cx| i.set_value("", w, cx));
        self.begin(Some(user), w, cx);
    }
    fn begin(&mut self, user: Option<UserTurn>, w: &mut Window, cx: &mut Context<Self>) {
        if self.state.model.busy || self.state.pending {
            return;
        }
        if let Some(user) = user {
            self.state.queued = Some(user);
            self.state.follow_bottom = true;
        }
        self.state.readonly = false;
        self.state.continuation = None;
        self.state.active_steps.clear_main();
        self.state.pending = true;
        self.history = false;
        self.state.model.error = None;
        cx.notify();
        let epoch = self.epoch;
        let this = cx.entity().downgrade();
        if self.fixture {
            self.state.pending = false;
            if !self.configured {
                self.state.model.error = Some(AiError::NotConfigured);
                cx.notify();
                return;
            }
            let queued = self.state.queued.take();
            self.start_fixture(queued, w, cx);
            return;
        }
        super::authorize(
            self.db.clone(),
            self.credentials.clone(),
            w,
            cx,
            move |result, w, cx| {
                this.update(cx, |s, cx| {
                    if s.epoch != epoch {
                        return;
                    }
                    s.state.pending = false;
                    match result {
                        Ok(config) => {
                            if shellmate_store::SecretStore::is_locked(s.credentials.as_ref()) {
                                s.begin(None, w, cx);
                                return;
                            }
                            s.configured = true;
                            s.provider = config.provider.id().into();
                            s.model_id = config.model.clone();
                            if let Some(user) = s.state.queued.take() {
                                s.state
                                    .model
                                    .conversation
                                    .push_user(user);
                                s.state.model.busy = true;
                            } else if !s.state.model.retry() {
                                return;
                            }
                            s.start(config, w, cx);
                        }
                        Err(e) => {
                            s.compact_queue.lock().unwrap().take();
                            s.state.model.error = Some(e);
                            if e == AiError::NotConfigured {
                                s.configured = false;
                            }
                            cx.notify();
                        }
                    }
                })
                .ok();
            },
        );
    }
    /// M4's /compact entry point; shares authorization and the busy lock with send.
    pub(crate) fn compact_context(&mut self, focus: Option<String>, w: &mut Window, cx: &mut Context<Self>) {
        if self.state.model.busy || self.state.pending {
            *self.compact_queue.lock().unwrap() = Some(focus);
            self.local(slash_ui::LocalBody::Hint(super::commands::QUEUED.into()), cx);
            return;
        }
        if self.state.model.conversation.entries().is_empty() { return; }
        if let Some(config) = self.state.run_config.clone() {
            self.state.compaction_focus = Some(focus);
            self.state.model.manual_compaction = true;
            self.state.model.busy = true;
            self.start(config, w, cx);
            return;
        }
        self.state.pending = true;
        let epoch = self.epoch;
        let this = cx.entity().downgrade();
        super::authorize(self.db.clone(), self.credentials.clone(), w, cx, move |result, w, cx| {
            this.update(cx, |s, cx| {
                if s.epoch != epoch { return; }
                s.state.pending = false;
                match result {
                    Ok(config) => {
                        s.state.compaction_focus = Some(focus);
                        s.state.model.manual_compaction = true;
                        s.state.model.busy = true;
                        s.start(config, w, cx);
                    }
                    Err(e) => s.state.model.error = Some(e),
                }
                cx.notify();
            }).ok();
        });
        cx.notify();
    }
    pub(super) fn open_memory(&mut self, action: Option<shellmate_ai::memory::Action>, w: &mut Window, cx: &mut Context<Self>) {
        let key = if action.is_some() && self.state.readonly { self.state.history_memory_host.clone() }
            else { self.memory_files().has_host().then(||self.permission_key().to_owned()) };
        let focus = match action { Some(action)=>super::memory_model::Focus::from_action(&action,key.as_deref()), None=>Some(super::memory_model::Focus::current(key.as_deref())) };
        let Some(focus)=focus else { return; };
        if let Err(e)=super::memory_manager::MemoryManager::open(self.data_root.clone(),self.db.clone(),key.as_deref(),focus,w,cx) {
            self.local(slash_ui::LocalBody::Hint(e),cx);
        }
    }
    pub(super) fn memory_files(&self) -> shellmate_core::memory::Memory {
        let saved = self.host.as_deref().filter(|id| self.db.borrow().host(&shellmate_core::HostId::from_raw(*id)).ok().flatten().is_some());
        let key = if self.binding.as_ref().is_some_and(|b| b.local) { Some(self.permission_key()) } else { saved };
        shellmate_core::memory::Memory::new(self.data_root.clone(), key)
    }
    fn start(&mut self, config: shellmate_ai::Config, w: &mut Window, cx: &mut Context<Self>) {
        self.epoch += 1;
        if self.state.continuation.is_none() && !self.fixture {
            self.binding = self
                .workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten();
            *self.mode.lock().unwrap() = self
                .db
                .borrow()
                .ai_permission(self.permission_key())
                .unwrap_or_default();
        }
        let Some(runtime) = crate::runtime::get(cx) else {
            self.state.model.event(Event::Error(AiError::Unavailable));
            cx.notify();
            return;
        };
        self.ensure_tasks();
        if let Some(tasks) = &self.tasks {
            tasks.sync_agents(&mut self.state.model.conversation);
            for text in tasks.take_notices() {
                self.state
                    .model
                    .conversation
                    .append(shellmate_ai::Entry::Notice {
                        kind: "task".into(),
                        text,
                    });
            }
        }
        let mut conversation = self.state.model.conversation.clone();
        let (tx, rx) = async_channel::bounded(64);
        let epoch = self.epoch;
        let ui=if let Some(ui)=&self.task_ui {ui.clone()} else {
            let (ui,messages)=async_channel::unbounded();self.bridge(messages,self.state.id.clone(),w,cx);self.task_ui=Some(ui.clone());ui
        };
        let binding = self.binding.clone();
        let capabilities = binding
            .as_ref()
            .map(|b| b.capabilities.clone())
            .unwrap_or_default();
        self.capabilities = capabilities.clone();
        let alive = binding
            .as_ref()
            .map(|b| b.alive.clone())
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let keep_limits = self.state.continuation.is_some() || self.state.compaction_focus.is_some();
        let compact_focus = self.state.compaction_focus.take();
        let continuation = if compact_focus.is_none() { self.state.continuation.take() } else { None };
        let limits = if keep_limits {
            self.state.run_limits.unwrap_or_else(|| self.db.borrow().ai_settings().unwrap_or_default().limits())
        } else {
            self.db.borrow().ai_settings().unwrap_or_default().limits()
        };
        let settings=if keep_limits {self.state.run_subagent_settings.clone().unwrap_or_else(||self.db.borrow().ai_settings().unwrap_or_default())}else{self.db.borrow().ai_settings().unwrap_or_default()};
        self.state.run_subagent_settings=Some(settings.clone());
        let memory_snapshot=conversation.memory_snapshot().cloned().unwrap_or_else(||shellmate_ai::memory::Snapshot::load(&self.memory_files(),&self.label,limits.auto_memory));
        self.main_cancel=Default::default();
        let subagents=binding.as_ref().filter(|b|b.capabilities.background).map(|binding|Arc::new(super::subagents::Service {
            tasks:self.tasks.as_ref().unwrap().clone(),config:config.clone(),settings,limits,db:self.db.clone(),snapshot:Arc::new(Mutex::new(memory_snapshot)),
            context:conversation.latest_user().map(|u|u.context.clone()).unwrap_or_default(),binding:binding.clone(),memory:self.memory_files(),
            ui:ui.clone(),mode:self.mode.clone(),cancel:self.main_cancel.clone(),factory:Arc::new(RigClient::new),
        }));
        let executor = super::exec::AppExecutor {
            subagents,
            memory: Some((self.memory_files(), self.label.clone())),
            tasks: self.tasks.as_ref().unwrap().clone(),
            binding: binding.unwrap_or(super::exec::Binding {
                session: usize::MAX,
                capabilities: Capabilities::default(),
                connection: None,
                local: false,
                alive: alive.clone(),
            }),
            ui: ui.clone(),
        };
        let host = super::host::Host {
            compaction: self.compact_queue.clone(),
            mode: self.mode.clone(),
            ui,
            executor: Box::new(executor),
            alive,
            resolved: Mutex::new(None),
        };
        self.state.run_limits = Some(limits);
        self.state.run_budget = None;
        self.state.run_config = Some(config.clone());
        let join = runtime.spawn(async move {
            let client = match RigClient::new(&config) {
                Ok(client) => client,
                Err(e) => {
                    let _ = worker_event(&tx, AgentEvent::Error(e)).await;
                    return;
                }
            };
            let result = drawer_stream(&client, &mut conversation, capabilities, &host, limits, continuation, compact_focus);
            match result {
                Ok(mut stream) => {
                    let _ = tx.send(Worker::Started(client.context_budget())).await;
                    while let Some(event) = stream.next().await {
                        if worker_update(&tx, event, stream.take_updates()).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = worker_event(&tx, AgentEvent::Error(e)).await;
                }
            }
        });
        self.abort = Some(join.abort_handle());
        cx.spawn_in(w, async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update_in(cx, |s, _, cx| {
                        if s.epoch != epoch {
                            return;
                        }
                        match event {
                            Worker::Started(budget) => {
                                if let Some(config) = &s.state.run_config {
                                    let source = shellmate_ai::resolve_window(&config.provider, &config.model, config.context_window).1;
                                    s.state.run_budget = Some((budget, source));
                                }
                                s.persist();
                            }
                            Worker::Event(event, updates, ready) => {
                                let event = *event;
                                let durable = matches!(&event, AgentEvent::Context { .. }) || updates.iter().any(|u| !matches!(u, shellmate_ai::LogUpdate::Append(e) if matches!(&**e, shellmate_ai::Entry::Assistant { .. })));
                                for update in updates { s.state.model.conversation.apply(update); }
                                s.agent_event(event);
                                if durable { s.persist(); }
                                let _ = ready.send(());
                                if !s.state.model.busy {
                                    s.compact_queue.lock().unwrap().take();
                                    s.abort = None;
                                    s.persist();
                                    if s.state.continuation.is_none() {
                                        s.binding = None;
                                        s.release_context(cx);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
    pub(super) fn has_running_tasks(&self) -> bool {
        self.tasks.as_ref().is_some_and(|tasks| tasks.has_running())
    }
    fn ensure_tasks(&mut self) {
        if self.tasks.is_none() {
            self.tasks = Some(super::tasks::Tasks::new(
                self.data_root.clone(),
                self.state.id.clone(),
            ));
        }
        self.tasks
            .as_ref()
            .unwrap()
            .remember_outputs(&self.state.model.conversation);
    }
    fn retire_tasks(&mut self) {
        self.task_ui=None;
        if let Some(tasks) = self.tasks.take() { tasks.retire(); }
    }
    fn close_outputs(&mut self) {
        if !saving_enabled(&self.db) && let Some(tasks)=&self.tasks {tasks.discard();}
        self.retire_tasks();
        if !self.fixture
            && let Err(e) = shellmate_core::ai_output::close_conversation(
                &self.data_root,
                &self.state.id,
                saving_enabled(&self.db),
            )
        {
            log::warn!("remove AI outputs: {e}");
        }
    }
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        if !saving_enabled(&self.db) {
            self.clear(cx);
        }
    }
    fn persist(&mut self) {
        if self.fixture {
            return;
        }
        self.state.persist(
            &mut self.db.borrow_mut(),
            self.host.clone(),
            &self.provider,
            &self.model_id,
        );
    }
    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        self.main_cancel.cancel();
        if self.tasks.as_ref().is_some_and(|tasks|tasks.stop_foreground()) {
            self.state.active_steps.clear_main();cx.notify();return;
        }
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.epoch += 1;
        self.compact_queue.lock().unwrap().take();
        self.state.continuation = None;
        self.state.run_config = None;
        if let Some(step) = self.state.active_steps.take_main() {
            let display = shellmate_ai::StepDisplay::from_item(
                shellmate_ai::agent::ToolOutcome::Failed("已停止".into()).item(&step.call),
            );
            let output_id = step.started.and_then(|_| {
                self.tasks
                    .as_ref()
                    .and_then(|t| t.output_id_for(&step.call.id))
            });
            let inline = output_id.as_ref().map_or_else(
                || "已停止".into(),
                |id| format!("已停止 · 输出 id {id} · 用 read_output 查看输出"),
            );
            self.state.model.conversation.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
                message_id: self.state.model.conversation.entries().len(),
                call: shellmate_ai::LoggedCall { id: step.call.id, name: "run_command".into(), args_json: serde_json::json!({"command":step.call.command,"target":step.call.target.id(),"run_in_background":step.call.run_in_background}) },
                result: shellmate_ai::LoggedResult { subagent: None, inline, output_id, cleared: false, is_error: true },
                display,
            }));
        }
        self.state.pending = false;
        self.binding = None;
        if self.state.model.busy {
            self.state.model.event(Event::Error(AiError::Cancelled));
            self.persist();
        }
        self.release_context(cx);
        cx.notify();
    }
    pub(crate) fn discard_and_clear(&mut self, cx: &mut Context<Self>) {
        if let Some(tasks)=&self.tasks {tasks.discard();}
        self.clear(cx);
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selection_scroll.update(cx, |s, _| s.stop());
        self.stop(cx);
        if let Some(abort)=self.abort.take(){abort.abort();}self.epoch+=1;
        self.close_outputs();
        self.state.reset_conversation();
        self.popup.escape();
        self.permission_menu = false;
        self.history_rows.clear();
        self.history = false;
        self.release_context(cx);
        cx.notify();
    }
    pub(crate) fn new_conversation(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        match self.state.new_conversation_action(self.history) {
            NewConversation::Disabled => {}
            NewConversation::Start => self.clear(cx),
            NewConversation::Confirm => {
                let drawer = cx.entity().downgrade();
                let id = self.state.id.clone();
                crate::dialogs::confirm::ConfirmDialog::open(
                    "停止并开始新对话？",
                    state::new_conversation_confirmation(self.save_conversations),
                    "停止并新建",
                    true,
                    move |_, cx| {
                        drawer
                            .update(cx, |s, cx| {
                                if s.state.id == id {
                                    s.clear(cx);
                                }
                            })
                            .ok();
                        Ok(())
                    },
                    w,
                    cx,
                );
            }
        }
    }
    fn history(&mut self, cx: &mut Context<Self>) {
        self.history = !self.history;
        if !self.fixture {
            self.history_rows = self.db.borrow().ai_conversations().unwrap_or_default();
        }
        cx.notify();
    }
    fn load(&mut self, id: &str, cx: &mut Context<Self>) {
        let loaded = self.db.borrow().ai_conversation(id);
        match loaded {
            Ok(Some(c)) => {
                self.stop(cx);
                self.close_outputs();
                self.selection_scroll.update(cx, |s, _| s.stop());
                self.state.reset_conversation();
                self.state.id = c.id;
                self.state.created = c.created_at;
                self.state.history_memory_host = c.host_id.clone().filter(|id|self.db.borrow().host(&shellmate_core::HostId::from_raw(id.as_str())).ok().flatten().is_some())
                    .or_else(||c.conversation.latest_user().filter(|u|u.context.protocol=="本机终端").map(|_|"local".into()));
                self.provider = c.provider;
                self.model_id = c.model;
                self.state.model.conversation = c.conversation;
                self.history = false;
                self.state.readonly = true;
                cx.notify();
            }
            _ => {
                self.state.save_error = Some("无法打开这段对话".into());
                cx.notify();
            }
        }
    }
    fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        // Stop can persist the final partial answer; do it before deleting the row.
        if self.state.id == id {
            self.stop(cx);
        }
        let deleted = self.state.delete(&mut self.db.borrow_mut(), id);
        match deleted {
            Ok(current) => {
                if current {
                    if let Some(tasks)=&self.tasks {tasks.discard();}
                    self.retire_tasks();
                }
                if let Err(e) = shellmate_core::ai_output::delete_conversation(&self.data_root, id)
                {
                    self.state.save_error = Some(format!("无法删除输出：{e}"));
                }
                self.history_rows.retain(|r| r.id != id);
                if current {
                    self.permission_menu = false;
                }
            }
            Err(_) => self.state.save_error = Some("无法删除对话".into()),
        }
        cx.notify();
    }
    fn settings(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let ws = self.workspace.clone();
        w.defer(cx, move |w, cx| {
            ws.update(cx, |s, cx| s.open_ai_settings(w, cx)).ok();
        });
    }
    fn insert(&mut self, card: CommandCard, w: &mut Window, cx: &mut Context<Self>) {
        let ws = self.workspace.clone();
        let target = self.session;
        w.defer(cx, move |w, cx| {
            ws.update(cx, |s, cx| s.ai_insert(card, target, w, cx)).ok();
        });
    }
    fn start_fixture(&mut self, user: Option<UserTurn>, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(user) = user {
            self.state
                .model
                .conversation
                .push_user(user);
        } else {
            self.state.model.retry();
        }
        self.state.model.busy = true;
        let mut fixture_log = self.state.model.conversation.clone();
        let mut stream = FixtureProvider
            .start(&mut fixture_log)
            .expect("fixture provider");
        // The fixture stream is immediately ready; no timer, runtime or HTTP client.
        while let Some(event) = futures::executor::block_on(stream.next()) {
            self.state.model.event(event);
        }

        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub fn fixture_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..8 {
            self.state.model.start(
                &format!("第 {} 轮：检查服务状态", i + 1),
                ContextSnapshot::default(),
            );
            self.state
                .model
                .event(Event::TextDelta(crate::proto::ai::CHAT_ANALYSIS.into()));
            self.state.model.event(Event::Done(None));
        }
        self.state.follow_bottom = false;
        crate::proto::scroll_after_layout(
            self.scroll.clone(),
            |handle| f32::from(handle.max_offset().y) / 2.,
            window,
        );
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn scene(&mut self, scene: &str, w: &mut Window, cx: &mut Context<Self>) {
        self.state.id = format!("fixture-{scene}");
        self.state.markdown = MarkdownCache::default();
        self.state.follow_bottom = true;
        self.fixture = true;
        self.configured = scene != "ai-unconfigured";
        self.fixture_tasks.clear();
        self.fixture_context = None;
        self.fixture_copy = Default::default();
        if crate::proto::subagents::SCENES.contains(&scene) {
            self.label="prod-web-01".into();self.protocol="SSH".into();self.history=false;
            self.state.model.clear();self.capabilities=Capabilities {terminal:true,background:true,..Default::default()};
            self.state.model.conversation.push_user(UserTurn::new("并行检查服务和日志",ContextSnapshot::default()));
            self.state.model.conversation.push_text("子任务正在后台处理。 ");
            if let Some(fixture) = crate::proto::subagents::cards(scene) {
                self.state.model.conversation = fixture.conversation;
                self.state.subagents = fixture.views.into_iter().map(|v|(v.id.clone(),v)).collect();
                self.fixture_tasks = fixture.tasks;
                if let Some((id,step)) = fixture.expanded {
                    let card = self.state.agent_cards.entry(id).or_default();card.open=true;card.step=Some(step);
                }
            }
            if scene=="ai-subagent-approval" {
                for (source,call) in crate::proto::subagents::approvals() {
                    let key=super::host::approval_key(Some(&source),&call.id);let mut step=agent_ui::ActiveStep::new(call);step.source=Some(source.clone());
                    self.state.active_steps.insert(key,Some(source.id),step);
                }
            } else if scene=="ai-tasks-subagents" {self.fixture_tasks=crate::proto::subagents::tasks();}
        }
        if scene==crate::proto::mode::SCENE {
            self.label="prod-web-01".into();self.protocol="SSH".into();self.history=false;
            self.state.model.clear();self.state.model.conversation=crate::proto::mode::conversation();
            self.capabilities=Capabilities {terminal:true,background:true,..Default::default()};
            *self.mode.lock().unwrap()=PermissionMode::Full;
            super::mode_popover::show(super::mode_popover::Kind::Mode,&mut self.permission_menu,&mut self.state.context_popover,&mut self.popup);
            crate::proto::focus_after_layout(self.context_focus.clone(),w);
        }
        if scene=="ai-memory-chip" {
            self.label="prod-web-01".into();self.protocol="SSH".into();self.history=false;
            self.state.model.clear();self.state.model.conversation=crate::proto::memory::conversation();
        }
        if crate::proto::ai::COPY_SCENES.contains(&scene) {
            self.fixture_copy = match scene { "ai-code-copied" => markdown::CopyFixture::Copied, "ai-code-copy" => markdown::CopyFixture::Hovered, _ => markdown::CopyFixture::None };
            self.state.model.start("整理生产环境的 Docker Compose 启动命令",ContextSnapshot::default());
            self.state.model.event(Event::TextDelta(crate::proto::ai::copy_answer()));
            self.state.model.event(Event::Done(None));
        }
        if let Some(fixture) = crate::proto::slash::scene(scene) {
            self.state.model = fixture.model;
            self.fixture_tasks = fixture.tasks;
            self.label = "prod-web-01".into();
            self.protocol = "SSH".into();
            self.capabilities = Capabilities { terminal:true,background:true,..Default::default() };
            self.set_composer_text(fixture.input,w,cx);
            if self.popup.open { crate::proto::focus_after_layout(self.input.focus_handle(cx),w); }
            self.fixture_context = fixture.context.clone();
            if let Some(context)=fixture.context {
                self.state.run_budget = Some((context.budget,shellmate_ai::WindowSource::Catalog));
                self.state.model.meter = Some(super::model::ContextMeter { used:context.total,window:context.budget.window,trigger:context.budget.trigger });
            }
            if fixture.context_open { self.dispatch_input("/context",w,cx); }
            if scene=="ai-slash-tasks" { self.local(slash_ui::LocalBody::Tasks,cx); }
        }
        if let Some(fixture) = crate::proto::ai::execution_scene(scene) {
            self.state.model = fixture.model;
            self.fixture_tasks = fixture.tasks;
            self.label = "prod-web-01".into();
            self.protocol = "SSH".into();
            self.history = false;
            self.state.readonly = false;
            self.state.active_steps.clear_main();
            self.capabilities = Capabilities {
                terminal: true,
                background: true,
                ..Default::default()
            };
        } else if let Some(fixture) = crate::proto::ai::compaction_scene(scene) {
            self.state.model = fixture.model;
            self.label = "prod-web-01".into();
            self.protocol = "SSH".into();
            self.history = false;
            self.state.readonly = false;
            self.state.pending = false;
            self.state.active_steps.clear_main();
            self.state.continuation = None;
            self.state.run_config = None;
            self.state.expanded_summaries.clear();
            self.state.expanded_summaries.extend(fixture.expanded_summary);
            self.capabilities = Capabilities { terminal: true, background: true, ..Default::default() };
            *self.mode.lock().unwrap() = PermissionMode::Auto;
        } else if scene == "ai-new-confirm" {
            self.agent_scene("ai-agent-run");
            // Completed steps plus an answer still streaming; exercise the real button path.
            self.state.model.busy = true;
        } else if scene.starts_with("ai-agent") {
            self.agent_scene(scene);
        }
        if matches!(scene, "ai-chat" | "ai-error") {
            self.state.model.start(
                "找出占用 3000 端口的进程\nTOKEN=fixture-sensitive",
                ContextSnapshot {
                    label: "prod-web-01".into(),
                    protocol: "SSH".into(),
                    text: "[error] bind: address already in use".into(),
                },
            );
            if scene == "ai-error" {
                self.state.model.event(Event::Error(AiError::RateLimited));
            } else {
                for e in fixture_events() {
                    self.state.model.event(e);
                }
                self.state
                    .model
                    .start("这个命令会修改系统吗？", ContextSnapshot::default());
                self.state
                    .model
                    .event(Event::TextDelta(crate::proto::ai::CHAT_ANALYSIS.into()));
                self.state.model.event(Event::Done(None));
            }
        }
        if scene == "ai-markdown" {
            self.state.model.start(
                "帮我整理服务检查结果和后续检查步骤",
                ContextSnapshot::default(),
            );
            self.state
                .model
                .event(Event::TextDelta(crate::proto::ai::MARKDOWN.into()));
            self.state.model.event(Event::Done(None));
            self.state.follow_bottom = false;
        }
        if scene == "ai-history" {
            self.history = true;
            self.history_rows = vec![
                ConversationSummary {
                    id: "demo1".into(),
                    label: "prod-web-01".into(),
                    title: "找出占用 3000 端口的进程".into(),
                    updated_at: super::now() - 120,
                },
                ConversationSummary {
                    id: "demo2".into(),
                    label: "ESP32-DevKit".into(),
                    title: "解释 ESP_ERR_TIMEOUT".into(),
                    updated_at: super::now() - 3600,
                },
            ];
            for row in &self.history_rows {
                let saved = SavedConversation {
                    id: row.id.clone(),
                    host_id: None,
                    label: row.label.clone(),
                    title: row.title.clone(),
                    provider: "openai".into(),
                    model: "fixture".into(),
                    created_at: row.updated_at,
                    updated_at: row.updated_at,
                    conversation: shellmate_ai::Conversation::from_turns(vec![Turn {
                            user: UserTurn::new(
                                &row.title,
                                ContextSnapshot {
                                    label: row.label.clone(),
                                    protocol: "SSH".into(),
                                    text: String::new(),
                                },
                            ),
                            answer: Some(shellmate_ai::Answer {
                                items: vec![shellmate_ai::AnswerItem::Text(
                                    "已为你整理建议，请先预览后执行。".into(),
                                )],
                            }),
                        }]),
                };
                self.db
                    .borrow_mut()
                    .save_ai_conversation(&saved)
                    .expect("fixture history");
            }
        }
        cx.notify();
    }
}
fn saving_enabled(db: &SharedStore) -> bool {
    db.borrow()
        .setting(shellmate_store::ai::SAVE_CONVERSATIONS)
        .ok()
        .flatten()
        .as_deref()
        != Some("0")
}
enum Worker {
    Started(shellmate_ai::ContextBudget),
    Event(
        Box<AgentEvent>,
        Vec<shellmate_ai::LogUpdate>,
        tokio::sync::oneshot::Sender<()>,
    ),
}
impl Drop for Drawer {
    fn drop(&mut self) {
        self.close_outputs();
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}
struct FixtureProvider;
impl ProviderClient for FixtureProvider {
    fn start<'a>(
        &'a self,
        _: &'a mut shellmate_ai::Conversation,
    ) -> Result<shellmate_ai::ResponseStream<'a>, AiError> {
        Ok(shellmate_ai::ResponseStream::fixture(fixture_events()))
    }
}
fn fixture_events() -> Vec<Event> {
    vec![
        Event::TextDelta("可以先查看哪个进程正在监听 3000 端口。".into()),
        Event::Command(CommandCard {
            command: "lsof -nP -iTCP:3000 -sTCP:LISTEN".into(),
            explanation: "列出监听该端口的进程及 PID".into(),
            risk: Risk::Medium,
        }),
        Event::Done(None),
    ]
}
fn ago(time: i64) -> String {
    let n = super::now().saturating_sub(time);
    if n < 60 {
        "刚刚".into()
    } else if n < 3600 {
        format!("{} 分钟前", n / 60)
    } else if n < 86400 {
        format!("{} 小时前", n / 3600)
    } else {
        format!("{} 天前", n / 86400)
    }
}
impl Render for Drawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent_items = self.prepare_subagent_cards();
        let p = cx.palette().clone();
        // Only the session label may shrink; the context prefix and line count
        // keep their intrinsic widths below the title/actions row.
        let context = div()
            .flex()
            .items_center()
            .min_w_0()
            .mt(px(2.))
            .text_size(px(11.5))
            .line_height(px(15.))
            .text_color(p.tx2)
            .child(div().flex_none().child("上下文："))
            .when(self.session.is_some(), |d| {
                d.child(
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .text_ellipsis()
                        .child(self.label.clone()),
                )
                .child(div().flex_none().child(self.state.model.context_hint(true)))
            })
            .when(self.session.is_none(), |d| d.child(self.state.model.context_hint(false)));
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .pt(px(14.))
            .pb(px(10.))
            .pl(px(16.))
            .pr(px(12.))
            .child(
                div()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("AI 助手"),
                    )
                    .child(
                        Button::new("ai-history")
                            .icon(crate::icons::AppIcon::History.at(14.))
                            .w(px(26.))
                            .h(px(26.))
                            .p_0()
                            .rounded(px(8.))
                            .tooltip("历史对话")
                            .on_click(cx.listener(|s, _, _, cx| s.history(cx))),
                    )
                    .child(
                        Button::new("ai-new")
                            .icon(crate::icons::AppIcon::Plus.at(14.))
                            .label("新对话")
                            .h(px(26.))
                            .px(px(8.))
                            .rounded(px(8.))
                            .text_size(px(12.))
                            .disabled(
                                self.state.new_conversation_action(self.history)
                                    == NewConversation::Disabled,
                            )
                            .tooltip(new_conversation_tooltip(
                                self.save_conversations,
                                self.state.save_error.is_some(),
                            ))
                            .on_click(cx.listener(|s, _, w, cx| s.new_conversation(w, cx))),
                    )
                    .child(
                        Button::new("ai-close")
                            .label("×")
                            .w(px(26.))
                            .h(px(26.))
                            .rounded(px(8.))
                            .on_click(cx.listener(|s, _, w, cx| {
                                let ws = s.workspace.clone();
                                w.defer(cx, move |_, cx| {
                                    ws.update(cx, |s, cx| s.close_ai(cx)).ok();
                                });
                            })),
                    ),
            )
            .child(context);
        // GPUI applies this request during layout, using the new content height.
        if self.state.follow_bottom && !self.history {
            self.scroll.scroll_to_bottom();
        }
        let mut body = div()
            .id("ai-messages")
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .on_scroll(cx.listener(|_, _, _, cx| {
                // Read the offset after GPUI has applied the user's scroll delta.
                // Nested output scrolling leaves the message offset unchanged.
                let drawer = cx.entity().downgrade();
                cx.defer(move |cx| {
                    drawer
                        .update(cx, |s, cx| {
                            s.state.follow_bottom =
                                s.scroll.offset().y + s.scroll.max_offset().y <= px(4.);
                            cx.notify();
                        })
                        .ok();
                });
            }))
            .px(px(14.))
            .pt(px(4.))
            .pb(px(10.))
            .flex()
            .flex_col()
            .gap(px(12.));
        if self.history {
            for row in self.history_rows.clone() {
                let id = row.id.clone();
                let del = row.id;
                body = body.child(
                    div()
                        .flex_none()
                        .py(px(10.))
                        .border_b_1()
                        .border_color(p.line)
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id(SharedString::from(format!("conversation-{id}")))
                                .flex_1()
                                .min_w_0()
                                .cursor_pointer()
                                .child(div().text_size(px(13.)).text_ellipsis().child(row.title))
                                .child(
                                    div()
                                        .mt(px(4.))
                                        .text_size(px(11.5))
                                        .text_color(p.tx2)
                                        .child(format!("{} · {}", row.label, ago(row.updated_at))),
                                )
                                .on_click(cx.listener(move |s, _, _, cx| s.load(&id, cx))),
                        )
                        .child(
                            Button::new(SharedString::from(format!("delete-{del}")))
                                .label("删除")
                                .h(px(26.))
                                .text_size(px(11.5))
                                .on_click(cx.listener(move |s, _, _, cx| s.delete(&del, cx))),
                        ),
                );
            }
        } else if !self.configured {
            body = body
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(21.))
                        .text_color(p.tx2)
                        .child("尚未配置 AI 助手。设置服务商、模型和 API Key 后即可提问。"),
                )
                .child(
                    Button::new("ai-settings")
                        .label("去设置")
                        .on_click(cx.listener(|s, _, w, cx| s.settings(w, cx))),
                );
        } else if self.state.model.conversation.turns().is_empty() {
            body = body.child(
                div()
                    .flex_none()
                    .pt(px(6.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.8))
                            .text_color(p.tx2)
                            .child("用自然语言描述想做的事，或让我解释终端里的报错。"),
                    )
                    .children(SUGGESTIONS.into_iter().enumerate().map(|(i, label)| {
                        div()
                            .id(("ai-suggestion", i))
                            .py(px(10.))
                            .px(px(12.))
                            .rounded(px(12.))
                            .bg(p.card2)
                            .cursor_pointer()
                            .hover(|d| d.bg(p.card3))
                            .text_size(px(13.))
                            .line_height(px(17.))
                            .child(label)
                            .on_click(cx.listener(move |s, _, w, cx| s.ask(label.into(), w, cx)))
                    })),
            );
        }
        if !self.history {
            for (index, turn) in self.state.model.conversation.turns().iter().enumerate() {
                body = body.children(self.local_cards_at(index, cx));
                body = body.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(px(5.))
                        .child(
                            div()
                                .max_w_full()
                                .px(px(12.))
                                .py(px(9.))
                                .rounded(px(13.))
                                .bg(p.accs)
                                .text_size(px(13.))
                                .line_height(px(20.8))
                                .whitespace_normal()
                                .child(turn.user.question.clone()),
                        ),
                );
                if let Some(answer) = &turn.answer {
                    let commands: Vec<String> = answer
                        .items
                        .iter()
                        .filter_map(|item| {
                            if let shellmate_ai::AnswerItem::Command(card) = item {
                                Some(card.command.trim().to_owned())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let mut reply = div()
                        .flex_none()
                        .min_w_0()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(8.));
                    for (n, item) in answer.items.iter().enumerate() {
                        let card = match item {
                            shellmate_ai::AnswerItem::Subagent { .. } => {
                                if let Some(view) = agent_items.get(&(index,n)).and_then(|id|self.state.subagents.get(id)) {
                                    reply = reply.child(self.subagent_card(view, window, cx));
                                }
                                continue;
                            }
                            shellmate_ai::AnswerItem::Compaction(c) => {
                                let key = (index, n);
                                let expanded = self.state.expanded_summaries.contains(&key);
                                let mut divider = div().flex_none().flex().flex_col().gap(px(8.))
                                    .py(px(8.)).border_t_1().border_color(p.line).text_size(px(11.)).text_color(p.tx3)
                                    .child(div().flex().flex_wrap().items_center().gap(px(6.)).child(super::model::compaction_label(c))
                                        .when(c.summary.is_some(), |d| d.child("·").child(Button::new(("ai-summary", index * 1000 + n))
                                            .label(if expanded { "收起摘要" } else { "查看摘要" }).h(px(24.)).text_size(px(11.5))
                                            .on_click(cx.listener(move |s, _, _, cx| { s.state.toggle_summary(key); cx.notify(); })) )));
                                if expanded && let Some(summary) = &c.summary {
                                    divider = divider.child(agent_ui::step_surface(&p).text_size(px(12.)).text_color(p.tx2).whitespace_normal().child(summary.clone()));
                                }
                                reply = reply.child(divider);
                                continue;
                            }
                            shellmate_ai::AnswerItem::Notice(text) => {
                                let action = self.state.model.conversation.memory_action(index,n).cloned();
                                let line = div().min_w_0().w_full().whitespace_normal().text_size(px(11.)).text_color(p.tx3).child(text.clone());
                                reply = reply.child(if let Some(action)=action {
                                    line.flex().items_center().flex_wrap().gap(px(5.)).child("·").child(agent_ui::step_action(("ai-memory-view",index*1000+n),"查看",cx).on_click(cx.listener(move|s,_,w,cx|s.open_memory(Some(action.clone()),w,cx))))
                                } else { line });
                                continue;
                            }
                            item @ shellmate_ai::AnswerItem::Step { .. } => {
                                reply = reply.child(self.step_card(item, (index, n), cx));
                                continue;
                            }
                            shellmate_ai::AnswerItem::Command(card) => card.clone(),
                            shellmate_ai::AnswerItem::Text(text) => {
                                let text = self.state.markdown.text(
                                    &self.state.id,
                                    (index, n),
                                    text,
                                    &commands,
                                );
                                reply = reply.child(markdown::drawer_view(
                                    markdown::element_id(&self.state.id, index, n),
                                    text,
                                    self.selection_scroll.clone(),
                                    self.fixture_copy,
                                    window,
                                    cx,
                                ));
                                continue;
                            }
                        };
                        let copy = card.command.clone();
                        let insert = card.clone();
                        let risk = card.risk;
                        reply = reply.child(
                            div()
                                .rounded(px(12.))
                                .bg(p.card2)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .px(px(11.))
                                        .py(px(9.))
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
                                        .child(card.command),
                                )
                                .child(
                                    div()
                                        .px(px(10.))
                                        .py(px(7.))
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(px(12.))
                                                .text_color(p.tx2)
                                                .child(card.explanation),
                                        )
                                        .child(
                                            div()
                                                .px(px(6.))
                                                .py(px(1.))
                                                .rounded(px(5.))
                                                .text_size(px(10.5))
                                                .text_color(if risk == Risk::High {
                                                    p.err
                                                } else {
                                                    p.warn
                                                })
                                                .bg(if risk == Risk::High {
                                                    p.errs
                                                } else {
                                                    p.warns
                                                })
                                                .child(risk.label()),
                                        )
                                        .child(
                                            Button::new(("ai-copy", index * 1000 + n))
                                                .label("复制")
                                                .h(px(24.))
                                                .text_size(px(11.5))
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(copy.clone()),
                                                    )
                                                }),
                                        )
                                        .child(
                                            Button::new(("ai-insert", index * 1000 + n))
                                                .label("插入")
                                                .h(px(24.))
                                                .text_size(px(11.5))
                                                .on_click(cx.listener(move |s, _, w, cx| {
                                                    s.insert(insert.clone(), w, cx)
                                                })),
                                        ),
                                ),
                        );
                    }
                    body = body.child(reply);
                }
            }
            // The awaited foreground call has not produced its Step yet. Keep its
            // live card at that position, before pending approvals, without a duplicate.
            for view in self.state.subagents.values().filter(|v| !v.history && !agent_items.values().any(|id|id==&v.id)) {
                body = body.child(self.subagent_card(view,window,cx));
            }
            body = body.children(self.local_cards_at(self.state.model.conversation.turns().len(), cx));
            if self.state.continuation.is_some() {
                body = body.child(
                    crate::dialogs::button(
                        "ai-continue",
                        self.state.model.continuation_label(self.state.resume_steps),
                        crate::dialogs::Tone::Primary,
                        true,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.resume(w, cx))),
                );
            }
            if self.state.model.compacting || self.state.pending
                || (self.state.model.busy
                    && self.state.active_steps.is_empty()
                    && self
                        .state
                        .model
                        .conversation
                        .turns()
                        .last()
                        .and_then(|t| t.answer.as_ref())
                        .is_none_or(|a| a.text().is_empty())
                        || self.state.model.busy && self.state.model.retrying.is_some())
            {
                body = body.child(div().text_size(px(12.5)).text_color(p.tx2).child(
                    self.state.model.status(self.state.pending).to_owned(),
                ));
            }
            if let Some(error) = self.state.model.error {
                body = body.child(
                    div()
                        .flex_none()
                        .p(px(10.))
                        .rounded(px(12.))
                        .bg(p.errs)
                        .text_color(p.err)
                        .text_size(px(12.5))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .line_height(px(19.))
                                .child(error.to_string()),
                        )
                        .when(self.state.continuation.is_none(), |d| d.child(
                            Button::new("ai-retry")
                                .label("重试")
                                .flex_none()
                                .h(px(34.))
                                .px(px(12.))
                                .rounded(px(10.))
                                .on_click(cx.listener(|s, _, w, cx| s.begin(None, w, cx))),
                        )),
                );
            }
        }
        if let Some(e) = &self.state.save_error {
            body = body.child(div().text_size(px(12.)).text_color(p.err).child(e.clone()));
        }
        // Keep the blocking card last so following the bottom reveals all its actions.
        if !self.history {
            for (key,_) in self.state.active_steps.iter() { body=body.child(self.active_card(key,cx)); }
        }
        // M1's single-line Input ignores h(32); the 30px action determines the row.
        // Clip the expanded inner editor at the field edge, before the button gap.
        let line_height = (window.rem_size() * 1.25).round();
        let rows = self.input.read(cx).display_map().wrap_row_count();
        let layout = composer_layout::layout(rows, window.rem_size().into(), line_height.into());
        let field_height = px(layout.viewport_height);
        let inset = px(layout.text_inset);
        let composer = div().relative().w_full().h(field_height).overflow_hidden().child(
            div().absolute().left(-Size::XSmall.input_px()).right(-Size::XSmall.input_px()).top(inset).bottom(inset)
                .child(Textarea::new(&self.input).xsmall().appearance(false).p_0()
                    .h(field_height - inset * 2.).min_h_0().max_h(field_height - inset * 2.).text_size(px(13.)))
        );
        let footer = div()
            .relative()
            .flex_none()
            .pt(px(10.))
            .px(px(12.))
            .pb(px(12.))
            .border_t_1()
            .border_color(p.line)
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(self.task_strip(cx))
            .child(div().flex().items_start().justify_between().gap(px(8.))
                .child(div().flex_1().min_w_0().when(!self.capabilities.available(), |d| d.whitespace_nowrap().overflow_hidden()).child(self.permission_chip(cx)))
                .children(self.context_indicator(cx)))
            .children(self.slash_popup(cx))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .h(px(layout.height))
                    .items_end()
                    .gap(px(8.))
                    .pl(px(12.))
                    .pr(px(4.))
                    .py(px(composer_layout::VERTICAL_PADDING))
                    .rounded(px(13.))
                    .bg(p.card2)
                    .child(
                        div().flex_1().min_w_0().child(
                            composer,
                        ),
                    )
                    .child(
                        crate::dialogs::button(
                            "ai-send",
                            if self.state.model.busy || self.state.pending {
                                "停止"
                            } else {
                                "发送"
                            },
                            crate::dialogs::Tone::Primary,
                            true,
                        )
                        .h(px(composer_layout::ACTION_HEIGHT))
                        .on_click(cx.listener(|s, _, w, cx| {
                            if s.state.model.busy || s.state.pending {
                                s.stop(cx)
                            } else {
                                s.send_input(w, cx)
                            }
                        })),
                    ),
            );
        div()
            .capture_action(cx.listener(Self::slash_action::<input::MoveUp>))
            .capture_action(cx.listener(Self::slash_action::<input::MoveDown>))
            .capture_action(cx.listener(Self::slash_action::<input::IndentInline>))
            .capture_action(cx.listener(Self::slash_action::<input::Escape>))
            .capture_action(cx.listener(Self::slash_action::<input::Enter>))
            .w_full()
            .flex_none()
            .h_full()
            .min_h_0()
            .rounded(px(18.))
            .bg(p.card)
            .shadow(crate::ui::card_shadow(&p))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(super::selection_scroll::layer(self.selection_scroll.clone()))
                    .child(body)
                    .child(crate::ui::scroll::scroll_fades(self.scroll.clone(), p.card)),
            )
            .child(footer)
    }
}

async fn worker_event(tx: &async_channel::Sender<Worker>, event: AgentEvent) -> Result<(), ()> {
    worker_update(tx, event, vec![]).await
}
async fn worker_update(tx: &async_channel::Sender<Worker>, event: AgentEvent, updates: Vec<shellmate_ai::LogUpdate>) -> Result<(), ()> {
    let (ready, receive) = tokio::sync::oneshot::channel();
    tx.send(Worker::Event(Box::new(event), updates, ready))
        .await
        .map_err(|_| ())?;
    receive.await.map_err(|_| ())
}

/// The run/resume factory shared by the drawer worker and its offline integration tests.
fn drawer_stream<'a>(
    client: &'a RigClient,
    conversation: &'a mut shellmate_ai::Conversation,
    capabilities: Capabilities,
    host: &'a dyn agent::ToolHost,
    limits: agent::Limits,
    continuation: Option<Box<agent::Resume>>,
    compact_focus: Option<Option<String>>,
) -> Result<agent::AgentStream<'a>, AiError> {
    if let Some(focus) = compact_focus {
        agent::compact_with_limits(client, conversation, capabilities, host, focus, limits)
    } else if let Some(resume) = continuation {
        agent::resume(client, conversation, host, *resume, limits)
    } else {
        agent::run(client, conversation, capabilities, host, limits)
    }
}
#[cfg(test)]
#[path = "slash_tests.rs"]
mod slash_tests;
