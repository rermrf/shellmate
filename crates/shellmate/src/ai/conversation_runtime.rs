//! A conversation owns its worker, receipts and approvals independently of any window.
use super::{AgentEvent, agent, timing};
use crate::ai::{
    exec::{AppExecutor, Binding},
    host::{Host, Reply, Ui, approval_key},
    model::DrawerModel,
    subagents::Service,
    target_router::Router,
    tasks::Tasks,
};
use futures::StreamExt;
use shellmate_ai::{
    AiError, Config, ContextSnapshot, Conversation, Entry, Event, UserTurn,
    interaction::{Control, Intent},
};
use shellmate_store::{
    SharedStore,
    ai::{SavedConversation, Settings},
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
pub struct ApprovalView {
    pub call: shellmate_ai::ToolCall,
    pub source: Option<shellmate_ai::SubagentRef>,
    pub redline: bool,
    pub reply: Option<Reply>,
    pub started: bool,
    pub automatic: bool,
}
#[derive(Clone)]
pub struct Snapshot {
    pub model: DrawerModel,
    pub approvals: BTreeMap<String, ApprovalView>,
    pub tools: Vec<shellmate_ai::LoggedCall>,
    pub tool_ids: crate::ai::tool_model::Identities,
    pub timing: timing::Timeline,
    pub config: Option<Config>,
    pub limits: agent::Limits,
    pub settings: Settings,
    pub paused: bool,
    pub error: Option<String>,
    pub revision: u64,
}
pub struct Runtime {
    pub id: String,
    pub created: i64,
    handle: tokio::runtime::Handle,
    pub data: Mutex<Snapshot>,
    pub control: Control,
    pub tasks: Tasks,
    pub router: Router,
    pub db: SharedStore,
    pub root: PathBuf,
    pub ui: async_channel::Sender<Ui>,
    pub compact: Arc<Mutex<Option<Option<String>>>>,
    running: AtomicBool,
    retired: AtomicBool,
    resume_requested: AtomicBool,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    wake: tokio::sync::Notify,
    pub service: Mutex<Option<Arc<Service>>>,
    pub factory: Mutex<crate::ai::subagents::ClientFactory>,
    persistence: Mutex<()>,
    pending_receipts: Mutex<(Vec<String>, Vec<String>)>,
    pub seen: std::sync::atomic::AtomicU64,
}
impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        created: i64,
        log: Conversation,
        control: Control,
        router: Router,
        db: SharedStore,
        root: PathBuf,
        credentials: Arc<dyn shellmate_store::SecretStore>,
    ) -> Arc<Self> {
        let (ui, rx) = async_channel::unbounded();
        let mut router = router;
        router.ui = ui.clone();
        let tasks = Tasks::with_control(root.clone(), id.clone(), control.clone());
        tasks.set_store(db.clone());
        tasks.shared_web(router.catalog.web(db.clone(), credentials));
        tasks.remember_outputs(&log);
        let runtime = Arc::new(Self {
            id,
            created,
            handle: tokio::runtime::Handle::current(),
            data: Mutex::new(Snapshot {
                model: DrawerModel {
                    conversation: log,
                    ..Default::default()
                },
                approvals: Default::default(),
                tools: vec![],
                tool_ids: Default::default(),
                timing: Default::default(),
                config: None,
                limits: Default::default(),
                settings: Default::default(),
                paused: false,
                error: None,
                revision: 0,
            }),
            control,
            tasks,
            router,
            db,
            root,
            ui,
            compact: Default::default(),
            running: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            resume_requested: AtomicBool::new(false),
            abort: Default::default(),
            wake: Default::default(),
            service: Default::default(),
            factory: Mutex::new(Arc::new(shellmate_ai::RigClient::new)),
            persistence: Default::default(),
            pending_receipts: Default::default(),
            seen: Default::default(),
        });
        let weak = Arc::downgrade(&runtime);
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                let Some(r) = weak.upgrade() else {
                    break;
                };
                match event {
                    Ui::Context { reply } => {
                        let snapshot = if let Ok(e) = r.router.resolve(None) {
                            r.router.catalog.read(&e.target.session_id, 40).await.ok()
                        } else {
                            Some(r.router.context())
                        };
                        let _ = reply.send(snapshot);
                    }
                    Ui::Approval {
                        source,
                        call,
                        redline,
                        reply,
                    } => {
                        let key = approval_key(source.as_ref(), &call.id);
                        let mut d = r.data.lock().unwrap();
                        d.approvals.insert(
                            key,
                            ApprovalView {
                                call,
                                source,
                                redline,
                                reply: Some(Reply::new(reply)),
                                started: false,
                                automatic: false,
                            },
                        );
                        d.revision += 1;
                    }
                    Ui::Running {
                        source,
                        call,
                        automatic,
                        ready,
                    } => {
                        let key = approval_key(source.as_ref(), &call.id);
                        let mut d = r.data.lock().unwrap();
                        if source.is_some() {
                            d.approvals.remove(&key);
                        } else {
                            d.approvals.insert(
                                key,
                                ApprovalView {
                                    call,
                                    source,
                                    redline: false,
                                    reply: None,
                                    started: true,
                                    automatic,
                                },
                            );
                        }
                        d.revision += 1;
                        let _ = ready.send(());
                    }
                    Ui::Finished(key) | Ui::CancelApproval(key) => {
                        let mut d = r.data.lock().unwrap();
                        d.approvals.remove(&key);
                        d.revision += 1;
                    }
                    Ui::CancelSource(id) => {
                        let mut d = r.data.lock().unwrap();
                        d.approvals
                            .retain(|_, a| a.source.as_ref().is_none_or(|s| s.id != id));
                        d.revision += 1;
                    }
                    event => {
                        let _ = r.router.catalog.bridge.send(event).await;
                    }
                }
            }
        });
        runtime
    }
    pub fn snapshot(&self) -> Snapshot {
        self.data.lock().unwrap().clone()
    }
    pub fn active(&self) -> bool {
        let d = self.data.lock().unwrap();
        d.model.busy || d.paused || self.tasks.has_running() || !d.approvals.is_empty()
    }
    pub fn submit(&self, text: String, context: ContextSnapshot) {
        self.submit_turn(UserTurn::new(&text, context).sent_now());
    }
    pub fn submit_turn(&self, user: UserTurn) {
        let _: Result<_, std::convert::Infallible> = self.control.enqueue_turn_from(
            shellmate_core::HostId::generate().to_string(),
            user,
            None,
            |_| Ok(()),
        );
        self.control.resume();
        self.wake.notify_one();
    }
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }
    pub fn stop_reply(&self) {
        self.control.request(Intent::StopReply);
        self.wake.notify_one();
    }
    pub fn stop_all(&self) {
        self.control.request(Intent::StopReply);
        self.tasks.stop_all();
        self.wake.notify_one();
    }
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.control.hold();
        let saving = self
            .db
            .borrow()
            .setting(shellmate_store::ai::SAVE_CONVERSATIONS)
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        if saving {
            self.tasks.retire();
        } else {
            self.tasks.discard();
        }
        if let Some(abort) = self.abort.lock().unwrap().take() {
            abort.abort();
        }
        let mut d = self.data.lock().unwrap();
        d.model.busy = false;
        d.revision += 1;
        d.approvals.clear();
        d.tools.clear();
        d.timing
            .finish(timing::Status::Interrupted, timing::Moment::now());
        drop(d);
        self.save(&[], &[]);
        if !saving && let Ok(dir) = shellmate_core::ai_output::directory(&self.root, &self.id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    pub fn save(&self, inputs: &[String], events: &[String]) -> bool {
        let _save = self.persistence.lock().unwrap();
        let mut receipts = self.pending_receipts.lock().unwrap();
        for id in inputs {
            if !receipts.0.contains(id) {
                receipts.0.push(id.clone());
            }
        }
        for id in events {
            if !receipts.1.contains(id) {
                receipts.1.push(id.clone());
            }
        }
        let (inputs, events) = (&receipts.0, &receipts.1);
        if self.retired.load(Ordering::Acquire)
            && self
                .db
                .borrow()
                .ai_conversation(&self.id)
                .ok()
                .flatten()
                .is_none()
        {
            return false;
        }
        let d = self.data.lock().unwrap();
        let Some(first) = d
            .model
            .conversation
            .turns()
            .first()
            .map(|t| t.user.clone())
            .or_else(|| {
                self.control
                    .snapshot()
                    .inputs
                    .front()
                    .map(|i| i.user_turn())
            })
        else {
            return true;
        };
        let scope = self.router.scope.lock().unwrap().clone();
        let target = scope.default.as_ref().and_then(|id| scope.targets.get(id));
        let config = d.config.as_ref();
        let saved = SavedConversation {
            id: self.id.clone(),
            host_id: target.and_then(|t| t.host_id.clone()),
            label: target
                .map(|t| t.label.clone())
                .unwrap_or_else(|| first.context.label.clone()),
            title: first.question.chars().take(48).collect(),
            provider: config.map(|c| c.provider.id()).unwrap_or("openai").into(),
            model: config.map(|c| c.model.clone()).unwrap_or_default(),
            created_at: self.created,
            updated_at: crate::ai::now(),
            conversation: d.model.conversation.clone(),
        };
        let state = serde_json::json!({"version":2,"targets":scope,"queue":self.control.delivery_snapshot(inputs,events),"timing":d.timing.snapshot(timing::Moment::now())});
        drop(d);
        let result = self
            .db
            .borrow_mut()
            .save_ai_conversation_with_runtime(&saved, Some(&state));
        let mut d = self.data.lock().unwrap();
        match result {
            Ok(_) => {
                self.control.acknowledge(inputs);
                self.control.acknowledge_completions(events);
                receipts.0.clear();
                receipts.1.clear();
                d.error = None;
                true
            }
            Err(_) => {
                d.error = Some("对话保存失败，已暂停，请检查存储后继续".into());
                self.control.hold();
                false
            }
        }
    }
    pub fn launch(self: &Arc<Self>, config: Config, limits: agent::Limits, settings: Settings) {
        {
            let mut d = self.data.lock().unwrap();
            if d.paused || d.model.error.is_some() || d.error.is_some() {
                self.control.reset_budget();
                self.resume_requested.store(true, Ordering::Release);
            }
            d.config = Some(config);
            d.limits = limits;
            d.settings = settings;
            d.error = None;
            d.model.busy = true;
            d.revision += 1;
        }
        self.control.resume();
        self.wake.notify_one();
        self.start_worker(true);
    }
    pub fn watch(self: &Arc<Self>, config: Config, limits: agent::Limits, settings: Settings) {
        {
            let mut d = self.data.lock().unwrap();
            d.config = Some(config);
            d.limits = limits;
            d.settings = settings;
            d.revision += 1;
        }
        self.start_worker(false);
    }
    fn start_worker(self: &Arc<Self>, first: bool) {
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let job = self.handle.spawn(async move {
            let Some(r) = weak.upgrade() else {
                return;
            };
            r.drive(first).await;
            r.running.store(false, Ordering::Release);
        });
        *self.abort.lock().unwrap() = Some(job.abort_handle());
    }
    async fn drive(self: &Arc<Self>, mut first: bool) {
        let mut continuation = None;
        let mut heartbeat = std::time::Instant::now();
        loop {
            if self.retired.load(Ordering::Acquire) {
                break;
            }
            let snap = self.control.snapshot();
            let due = self.control.pending()
                || self.resume_requested.load(Ordering::Acquire)
                || (self.control.auto_continue() && self.control.pending_completions());
            if !first && (!due || snap.held) && self.compact.lock().unwrap().is_none() {
                if self.control.intent() == Intent::StopReply {
                    self.control.take_intent();
                    let mut d = self.data.lock().unwrap();
                    d.timing
                        .finish(timing::Status::Stopped, timing::Moment::now());
                    drop(d);
                    self.save(&[], &[]);
                }
                tokio::select! {
                    _ = self.wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        self.tasks.retry_completions();
                        let mut d = self.data.lock().unwrap();
                        let external = self.tasks.take_external_steps();
                        let mut changed = !external.is_empty();
                        for step in external { d.model.conversation.append(Entry::Step(step)); }
                        changed |= self.tasks.sync_agents(&mut d.model.conversation);
                        let timing = d.timing.ticking();
                        if changed { d.revision += 1; }
                        drop(d);
                        if changed { self.save(&[], &[]); }
                        else if timing && heartbeat.elapsed() >= Duration::from_secs(1) {
                            let _save = self.persistence.lock().unwrap();
                            let timing = self.data.lock().unwrap().timing.snapshot(timing::Moment::now());
                            if let Ok(timing) = serde_json::to_value(timing) {
                                let _ = self.db.borrow_mut().update_ai_run_timing(&self.id, &timing);
                            }
                            heartbeat = std::time::Instant::now();
                        }
                    }
                }
                continue;
            }
            if !first && snap.held {
                tokio::select! {_=self.wake.notified()=>{},_=tokio::time::sleep(Duration::from_millis(250))=>{}}
                continue;
            }
            first = false;
            self.resume_requested.store(false, Ordering::Release);
            let state = self.snapshot();
            let Some(config) = state.config else {
                break;
            };
            let mut log = state.model.conversation;
            self.tasks.sync_agents(&mut log);
            for step in self.tasks.take_external_steps() {
                log.append(Entry::Step(step));
            }
            if log.turns().is_empty() {
                let Some(i) = self.control.claim_first() else {
                    continue;
                };
                log.push_user(i.user_turn());
                {
                    self.data.lock().unwrap().model.conversation = log.clone();
                }
                if !self.save(&[i.id], &[]) {
                    continue;
                }
            }
            if !state.timing.live() {
                self.router.refresh_run();
                self.control.reset_budget();
            }
            let endpoint = self.router.resolve(None).ok();
            let binding = endpoint
                .as_ref()
                .map(|e| e.binding.clone())
                .unwrap_or(Binding {
                    session: usize::MAX,
                    connection: None,
                    local: false,
                    alive: Arc::new(AtomicBool::new(true)),
                    capabilities: Default::default(),
                });
            let scope = self.router.scope.lock().unwrap().clone();
            let caps = shellmate_ai::Capabilities {
                terminal: true,
                background: true,
                sessions: true,
                web: self.tasks.web_enabled(),
                cwd: binding.capabilities.cwd.clone(),
                ..Default::default()
            };
            let memory = shellmate_core::memory::Memory::new(
                &self.root,
                endpoint.as_ref().and_then(|e| {
                    if e.binding.local {
                        Some("local")
                    } else {
                        e.target.host_id.as_deref()
                    }
                }),
            );
            let mut snapshot = shellmate_ai::memory::Snapshot::load(
                &memory,
                endpoint
                    .as_ref()
                    .map(|e| e.target.label.as_str())
                    .unwrap_or("无终端"),
                state.limits.auto_memory,
            );
            snapshot.instructions.push_str(&format!("\n默认终端与已引用终端：{}。这是上下文记录，不是可用资源白名单；所有打开的终端可经 list_sessions 发现并直接使用，不要求用户加入或匹配完整名称。理解用户的简称、目录和列表序号，再把对应 session_id 传给每次调用；不使用当前标签焦点作为回退。主机记忆使用 memory.session_id 指定来源，跨主机内容不要混淆。",serde_json::to_string(&scope).unwrap()));
            let cancellation = Default::default();
            let service = Arc::new(Service {
                tasks: self.tasks.clone(),
                config: config.clone(),
                settings: state.settings.clone(),
                snapshot: Arc::new(Mutex::new(snapshot)),
                limits: state.limits,
                db: self.db.clone(),
                context: log.latest_user().unwrap().context.clone(),
                binding: binding.clone(),
                memory: memory.clone(),
                ui: self.ui.clone(),
                mode: Arc::new(Mutex::new(Default::default())),
                cancel: cancellation,
                factory: self.factory.lock().unwrap().clone(),
                router: Some(self.router.clone()),
            });
            *self.service.lock().unwrap() = Some(service.clone());
            let host = Host {
                compaction: self.compact.clone(),
                mode: Arc::new(Mutex::new(Default::default())),
                ui: self.ui.clone(),
                executor: Box::new(AppExecutor {
                    lease: None,
                    slots: None,
                    router: Some(self.router.clone()),
                    subagents: Some(service),
                    memory: Some((memory, log.latest_user().unwrap().context.label.clone())),
                    binding,
                    tasks: self.tasks.clone(),
                    ui: self.router.catalog.bridge.clone(),
                }),
                alive: Arc::new(AtomicBool::new(true)),
                resolved: Default::default(),
            };
            let compact = self.compact.lock().unwrap().take();
            {
                let mut d = self.data.lock().unwrap();
                d.model.conversation = log.clone();
                d.model.busy = true;
                d.paused = false;
                d.model.error = None;
                d.timing.start(
                    log.turns().len() - 1,
                    log.entries().len(),
                    timing::Moment::now(),
                );
                d.revision += 1;
            }
            if !self.save(&[], &[]) {
                self.data.lock().unwrap().model.busy = false;
                continue;
            }
            let client = match (self.factory.lock().unwrap())(&config) {
                Ok(c) => c,
                Err(e) => {
                    {
                        let mut d = self.data.lock().unwrap();
                        d.model.event(Event::Error(e));
                        d.timing
                            .finish(timing::Status::Failed, timing::Moment::now());
                        d.revision += 1;
                    }
                    self.control.hold();
                    self.save(&[], &[]);
                    continue;
                }
            };
            let limits = agent::Limits {
                max_steps: state
                    .limits
                    .max_steps
                    .map(|max| max.saturating_sub(self.control.tool_steps())),
                ..state.limits
            };
            let result = if let Some(focus) = compact {
                agent::compact_with_limits(&client, &mut log, caps, &host, focus, state.limits)
            } else if let Some(resume) = continuation.take() {
                agent::resume(&client, &mut log, &host, resume, limits)
            } else {
                agent::run(&client, &mut log, caps, &host, limits)
            };
            match result {
                Err(e) => {
                    let mut d = self.data.lock().unwrap();
                    d.model.event(Event::Error(e));
                    d.timing
                        .finish(timing::Status::Failed, timing::Moment::now());
                    self.control.hold();
                }
                Ok(mut stream) => {
                    let mut checkpoint = std::time::Instant::now();
                    while let Some(event) = stream.next().await {
                        let updates = stream.take_updates();
                        let durable=updates.iter().any(|u|!matches!(u,shellmate_ai::LogUpdate::Append(e)if matches!(&**e,Entry::Assistant{..})))||matches!(&event,AgentEvent::Done(_)|AgentEvent::Error(_)|AgentEvent::Paused{..}|AgentEvent::Released(_));
                        let accepted = if let AgentEvent::InputsAccepted(ids) = &event {
                            ids.clone()
                        } else {
                            vec![]
                        };
                        let completed = if let AgentEvent::CompletionsAccepted(ids) = &event {
                            ids.clone()
                        } else {
                            vec![]
                        };
                        {
                            let mut d = self.data.lock().unwrap();
                            for update in updates {
                                d.model.conversation.apply(update);
                            }
                            match &event {
                                AgentEvent::ToolStarted(c) => {
                                    d.tool_ids.start(&c.id);
                                    d.tools.push(c.clone());
                                }
                                AgentEvent::StepProposed(c) => d.tool_ids.start(&c.id),
                                AgentEvent::ToolFinished(id) => {
                                    let saved =
                                        d.model.conversation.entries().iter().rev().find_map(|e| {
                                            if let Entry::Step(s) = e {
                                                Some(s.call.id.clone())
                                            } else {
                                                None
                                            }
                                        });
                                    if let Some(saved) = saved {
                                        d.tool_ids.finish(id, &saved);
                                    }
                                    d.tools.retain(|c| c.id != *id);
                                }
                                AgentEvent::StepFinished { call, .. } => {
                                    let saved =
                                        d.model.conversation.entries().iter().rev().find_map(|e| {
                                            if let Entry::Step(s) = e {
                                                Some(s.call.id.clone())
                                            } else {
                                                None
                                            }
                                        });
                                    if let Some(saved) = saved {
                                        d.tool_ids.finish(&call.id, &saved);
                                    }
                                    d.approvals.retain(|_, a| a.source.is_some());
                                }
                                _ => {}
                            }
                            let inbox = self.control.snapshot();
                            let pending = inbox
                                .completions
                                .iter()
                                .map(|e| e.task_id.clone())
                                .collect::<Vec<_>>();
                            let tasks = self.tasks.list();
                            let conversation = d.model.conversation.clone();
                            d.timing.observe(&conversation, &tasks, &pending);
                            d.timing.await_tasks(
                                &self.tasks.take_released_waits(),
                                &tasks,
                                &pending,
                            );
                            let ids = inbox
                                .completions
                                .iter()
                                .filter(|e| completed.contains(&e.id))
                                .map(|e| e.task_id.clone())
                                .collect::<Vec<_>>();
                            d.timing.accept_results(&ids);
                            match event {
                                AgentEvent::Done(u) => {
                                    d.model.event(Event::Done(u));
                                    d.timing.done(timing::Moment::now());
                                }
                                AgentEvent::Released(reason) => {
                                    d.model.event(Event::Done(None));
                                    if reason == agent::ReleaseReason::Stopped {
                                        d.timing
                                            .finish(timing::Status::Stopped, timing::Moment::now());
                                    } else {
                                        d.timing.done(timing::Moment::now());
                                    }
                                }
                                AgentEvent::Paused {
                                    reason,
                                    resume,
                                    steps: _,
                                } => {
                                    d.model.pause(reason);
                                    d.paused = true;
                                    continuation = Some(*resume);
                                    d.timing
                                        .finish(timing::Status::Paused, timing::Moment::now());
                                    self.control.hold();
                                }
                                AgentEvent::Error(e) => {
                                    d.model.event(Event::Error(e));
                                    d.timing.finish(
                                        if e == AiError::Cancelled {
                                            timing::Status::Stopped
                                        } else {
                                            timing::Status::Failed
                                        },
                                        timing::Moment::now(),
                                    );
                                    self.control.hold();
                                }
                                AgentEvent::Thinking => d.model.thinking = true,
                                AgentEvent::Retrying { error, .. } => {
                                    d.model.retrying = Some(error.to_string())
                                }
                                AgentEvent::Context {
                                    used,
                                    window,
                                    trigger,
                                } => d.model.event(Event::Context {
                                    used,
                                    window,
                                    trigger,
                                }),
                                AgentEvent::Compacting { tier } => {
                                    d.model.event(Event::Compacting { tier })
                                }
                                AgentEvent::Compacted {
                                    before,
                                    after,
                                    summary,
                                } => d.model.event(Event::Compacted {
                                    before,
                                    after,
                                    summary,
                                }),
                                _ => {}
                            }
                            d.revision += 1;
                        }
                        if (durable
                            || !accepted.is_empty()
                            || !completed.is_empty()
                            || checkpoint.elapsed() >= Duration::from_secs(1))
                            && !self.save(&accepted, &completed)
                        {
                            let mut d = self.data.lock().unwrap();
                            d.model.busy = false;
                            d.timing
                                .finish(timing::Status::Interrupted, timing::Moment::now());
                            break;
                        }
                        if durable || checkpoint.elapsed() >= Duration::from_secs(1) {
                            checkpoint = std::time::Instant::now();
                        }
                    }
                }
            }
            {
                let mut d = self.data.lock().unwrap();
                d.model.busy = false;
                d.tools.clear();
                d.revision += 1;
            }
            self.save(&[], &[]);
        }
    }
}
#[cfg(test)]
#[path = "conversation_runtime_tests.rs"]
mod tests;
