//! Per-drawer, per-session task ownership. Workers hold only Weak registry refs;
//! dropping the registry cancels every worker and its process/channel/PTY guard.
#[path = "agent_tasks.rs"]
mod agents;
#[path = "task_events.rs"]
mod events;
use futures::future::BoxFuture;
use shellmate_ai::{
    agent::{ExecLimits, LocalResult, LocalTool, Target, ToolCall, ToolOutcome},
    output::{shape, spooled},
};
use shellmate_core::ai_output::{self, Spool};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(Option<i32>),
    Stopped,
    Busy,
    Disconnected,
    Failed,
    StepLimit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command,
    Agent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Starting,
    Ready,
    Unverified,
    Failed,
}
impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "检查启动",
            Self::Ready => "已出现就绪信号",
            Self::Unverified => "就绪待确认",
            Self::Failed => "启动未完成",
        }
    }
}
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub service: Option<ServiceState>,
    pub destination: Option<shellmate_ai::session_tools::Destination>,
    pub owner: Option<String>,
    pub kind: Kind,
    pub call_id: String,
    pub id: String,
    pub command: String,
    pub target: Target,
    pub started: Instant,
    pub status: Status,
    pub output_id: String,
    pub background: bool,
}
impl TaskInfo {
    pub fn target_label(&self) -> &'static str {
        if self.kind == Kind::Agent {
            "子任务"
        } else {
            self.target.label()
        }
    }
    pub fn status_text(&self) -> String {
        let text = match self.status {
            Status::Running => "运行中".into(),
            Status::Exited(code) if self.kind == Kind::Command => {
                return format!(
                    "已退出 · 退出码 {}",
                    code.map_or_else(|| "未知".into(), |n| n.to_string())
                );
            }
            Status::Exited(_) => "已完成".into(),
            Status::Stopped => "已停止".into(),
            Status::Busy => "未执行（终端被占用）".into(),
            Status::Disconnected => "连接已断开".into(),
            Status::Failed => "失败".into(),
            Status::StepLimit => "达到步数上限".into(),
        };
        if self.status == Status::Running || self.kind == Kind::Agent {
            format!("{text} · {}s", self.started.elapsed().as_secs())
        } else {
            text
        }
    }
}
struct Task {
    info: TaskInfo,
    spool: Spool,
    cancel: watch::Sender<Option<Stop>>,
    result: Option<watch::Receiver<Option<ToolOutcome>>>,
}
#[derive(Clone, Copy)]
enum Stop {
    User,
    Timeout,
}
struct State {
    services: BTreeMap<String, Tasks>,
    external_steps: Vec<shellmate_ai::Step>,
    next_agent: u64,
    agents: BTreeMap<u64, agents::AgentTask>,
    retired: bool,
    next: u64,
    tasks: BTreeMap<u64, Task>,
    notices: Vec<String>,
    remembered: Vec<(String, String)>,
}
struct Inner {
    service_parent: std::sync::OnceLock<(Weak<Inner>, String)>,
    web: std::sync::OnceLock<Arc<super::web::Service>>,
    released_waits: Mutex<Vec<String>>,
    events: Mutex<events::Outbox>,
    event_retrying: std::sync::atomic::AtomicBool,
    store: std::sync::OnceLock<shellmate_store::SharedStore>,
    control: shellmate_ai::interaction::Control,
    changed: watch::Sender<u64>,
    discarded: Arc<Mutex<bool>>,
    root: PathBuf,
    conversation: String,
    state: Mutex<State>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for child in self.state.get_mut().unwrap().services.values() {
            child.retire();
        }
        for task in self.state.get_mut().unwrap().agents.values() {
            task.cancel.cancel();
        }
        for task in self.state.get_mut().unwrap().tasks.values() {
            let _ = task.cancel.send(Some(Stop::User));
        }
    }
}
#[derive(Clone)]
pub struct Tasks(Arc<Inner>);
#[derive(Clone)]
pub struct Retained(std::sync::Weak<Inner>);
impl Retained {
    pub fn while_retained<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let inner = self.0.upgrade()?;
        let deleted = inner.discarded.lock().unwrap();
        if *deleted { None } else { Some(work()) }
    }
}
impl Tasks {
    /// Only service processes are retained after their child agent finishes.
    pub fn finish_agent_commands(&self) {
        for task in self.0.state.lock().unwrap().tasks.values() {
            if task.info.service.is_none() && task.info.status == Status::Running {
                let _ = task.cancel.send(Some(Stop::User));
            }
        }
    }
    fn service_task(&self, id: &str) -> Option<(Tasks, String)> {
        let children = self.0.state.lock().unwrap().services.clone();
        for (agent, child) in children {
            let local = id.strip_prefix(&format!("{agent}/"));
            let found = child
                .0
                .state
                .lock()
                .unwrap()
                .tasks
                .values()
                .find(|t| {
                    t.info.service.is_some()
                        && (local == Some(t.info.id.as_str()) || t.info.output_id == id)
                })
                .map(|t| t.info.id.clone());
            if let Some(id) = found {
                return Some((child, id));
            }
        }
        None
    }
    async fn check_service(
        &self,
        call: &ToolCall,
        service: &shellmate_ai::agent::ServiceOptions,
        number: u64,
        oid: &str,
        spool: &Spool,
        result: &mut watch::Receiver<Option<ToolOutcome>>,
    ) -> ToolOutcome {
        let deadline = Instant::now() + Duration::from_secs(service.startup_timeout_secs);
        let mut phase = ServiceState::Unverified;
        loop {
            if let Some(out) = result.borrow().clone() {
                if let Some(task) = self.0.state.lock().unwrap().tasks.get_mut(&number) {
                    task.info.service = Some(ServiceState::Failed);
                }
                return service_exited(out, oid);
            }
            let matched = service.ready_pattern.as_ref().is_some_and(|pattern| {
                spool
                    .window(65536)
                    .is_ok_and(|w| String::from_utf8_lossy(&w.bytes).contains(pattern))
            });
            if matched {
                phase = ServiceState::Ready;
                break;
            }
            if service.ready_pattern.is_none() || Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                _ = self.0.control.urgent() => break,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
                changed = result.changed() => if changed.is_err() { return ToolOutcome::Disconnected; },
            }
        }
        // Completion and readiness share the registry lock. An exit already observed
        // by the worker must never be overwritten with a running/ready state.
        let ended = {
            let mut state = self.0.state.lock().unwrap();
            let task = state.tasks.get_mut(&number).unwrap();
            if task.info.status == Status::Running {
                task.info.service = Some(phase);
                false
            } else {
                task.info.service = Some(ServiceState::Failed);
                true
            }
        };
        if ended {
            return service_exited(wait_result(result).await, oid);
        }
        let mut out = match background(call, oid, number, false) {
            ToolOutcome::Completed(out) => out,
            _ => unreachable!(),
        };
        let evidence = if phase == ServiceState::Ready {
            format!(
                "已匹配就绪输出 {:?}；这不是 HTTP 健康检查",
                service.ready_pattern.as_deref().unwrap()
            )
        } else {
            "尚未确认就绪，需读取输出或在服务所在目标检查端口/HTTP；不能宣称启动成功".into()
        };
        let preview = spool
            .window(8192)
            .map(|w| String::from_utf8_lossy(&w.bytes).into_owned())
            .unwrap_or_default();
        out.message = Some(format!(
            "{}\n持续服务由会话管理，子代理或本轮回复结束不会停止。{evidence}。不要用 wait_tasks 等待服务退出。\n{preview}",
            out.message.unwrap_or_default()
        ));
        ToolOutcome::Completed(out)
    }
    pub fn retained(&self) -> Retained {
        Retained(Arc::downgrade(&self.0))
    }
    pub fn root(&self) -> PathBuf {
        self.0.root.clone()
    }
    pub fn new(root: PathBuf, conversation: String) -> Self {
        Self::with_control(root, conversation, Default::default())
    }
    pub fn with_control(
        root: PathBuf,
        conversation: String,
        control: shellmate_ai::interaction::Control,
    ) -> Self {
        Self(Arc::new(Inner {
            service_parent: Default::default(),
            web: Default::default(),
            released_waits: Default::default(),
            events: Default::default(),
            event_retrying: Default::default(),
            store: Default::default(),
            control,
            changed: watch::channel(0).0,
            discarded: Arc::new(Mutex::new(false)),
            root,
            conversation,
            state: Mutex::new(State {
                services: BTreeMap::new(),
                external_steps: vec![],
                next: 1,
                next_agent: 1,
                agents: BTreeMap::new(),
                retired: false,
                tasks: BTreeMap::new(),
                notices: Vec::new(),
                remembered: Vec::new(),
            }),
        }))
    }
    pub fn list(&self) -> Vec<TaskInfo> {
        let state = self.0.state.lock().unwrap();
        let mut list: Vec<_> = state
            .tasks
            .values()
            .map(|t| t.info.clone())
            .chain(
                state
                    .agents
                    .values()
                    .map(|t| agents::info(&t.view, &t.call_id)),
            )
            .collect();
        let children = state.services.clone();
        drop(state);
        for (agent, child) in children {
            for mut task in child.list().into_iter().filter(|t| t.service.is_some()) {
                task.id = format!("{agent}/{}", task.id);
                task.call_id = format!("{agent}/{}", task.call_id);
                task.owner = Some(agent.clone());
                list.push(task);
            }
        }
        list.sort_by_key(|t| t.started);
        list
    }
    pub fn control(&self) -> shellmate_ai::interaction::Control {
        self.0.control.clone()
    }
    pub fn set_web(
        &self,
        db: shellmate_store::SharedStore,
        credentials: Arc<dyn shellmate_store::SecretStore>,
    ) {
        self.0
            .web
            .get_or_init(|| Arc::new(super::web::Service::new(db, credentials)));
    }
    pub fn shared_web(&self, service: Arc<super::web::Service>) {
        let _ = self.0.web.set(service);
    }
    pub fn web_enabled(&self) -> bool {
        self.0.web.get().is_some_and(|w| w.enabled())
    }
    #[cfg(test)]
    pub fn test_web(&self, service: Arc<super::web::Service>) {
        let _ = self.0.web.set(service);
    }
    pub fn external_step(&self, step: shellmate_ai::Step) {
        if let Some(db) = self.0.store.get()
            && let Err(e) = db
                .borrow_mut()
                .save_ai_external_step(&self.0.conversation, &step)
        {
            log::warn!("save external step: {e}");
        }
        self.0.state.lock().unwrap().external_steps.push(step);
    }
    pub fn take_external_steps(&self) -> Vec<shellmate_ai::Step> {
        std::mem::take(&mut self.0.state.lock().unwrap().external_steps)
    }
    pub fn set_store(&self, store: shellmate_store::SharedStore) {
        if self.0.store.set(store).is_ok() {
            self.retry_completions();
        }
    }
    pub fn promote_foreground(&self) {
        self.0
            .control
            .request(shellmate_ai::interaction::Intent::Detach);
    }
    pub fn take_released_waits(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.released_waits.lock().unwrap())
    }
    pub async fn wait_tasks(&self, ids: Vec<String>, all: bool) -> LocalResult {
        let mut changed = self.0.changed.subscribe();
        self.0
            .control
            .set_phase(shellmate_ai::interaction::Phase::Waiting(ids.clone()));
        loop {
            let list = self.list();
            let Some(selected) = ids
                .iter()
                .map(|id| list.iter().find(|t| &t.id == id))
                .collect::<Option<Vec<_>>>()
            else {
                return LocalResult::error("未找到本会话中的等待任务");
            };
            let ended = selected
                .iter()
                .filter(|t| t.status != Status::Running)
                .count();
            if selected
                .iter()
                .any(|t| t.service.is_some() && t.status == Status::Running)
            {
                return LocalResult::error(
                    "持续服务不会自行结束；用 read_output 查看启动输出，在同一目标检查就绪后完成回复，不要等待服务退出",
                );
            }
            if (all && ended == selected.len()) || (!all && ended > 0) {
                return LocalResult::text(
                    selected
                        .iter()
                        .map(|t| {
                            format!(
                                "任务 {} · {} · 输出 id {}",
                                t.id,
                                t.status_text(),
                                t.output_id
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            tokio::select! {biased;
                ()=self.0.control.urgent()=>{
                    self.0.released_waits.lock().unwrap().extend(selected.iter().filter(|t| t.status==Status::Running).map(|t|t.id.clone()));
                    return LocalResult::text("已结束等待，尚未完成的任务继续在原执行通道运行");
                },
                event=changed.changed()=>if event.is_err(){return LocalResult::error("会话已关闭");},
            }
        }
    }
    pub fn has_running(&self) -> bool {
        self.list().iter().any(|t| t.status == Status::Running)
    }
    pub fn running(&self) -> Vec<TaskInfo> {
        self.list()
            .into_iter()
            .filter(|t| t.status == Status::Running && t.background)
            .collect()
    }
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.0.state.lock().unwrap().notices)
    }
    pub fn state(&self) -> (Vec<String>, Vec<String>) {
        let services = self
            .list()
            .into_iter()
            .filter(|t| t.owner.is_some())
            .collect::<Vec<_>>();
        let state = self.0.state.lock().unwrap();
        let mut running: Vec<String> = state
            .tasks
            .values()
            .filter(|t| t.info.status == Status::Running)
            .map(|t| {
                format!(
                    "#{} · {} · 输出 id {}",
                    t.info.id, t.info.command, t.info.output_id
                )
            })
            .collect();
        running.extend(
            state
                .agents
                .values()
                .filter(|a| a.view.status == shellmate_ai::SubagentStatus::Running)
                .map(|a| format!("{} · 子任务 · {}", a.view.id, a.view.description)),
        );
        running.extend(
            services
                .iter()
                .filter(|t| t.status == Status::Running)
                .map(|t| {
                    format!(
                        "{} · 持续服务 · {} · 输出 id {}",
                        t.id, t.command, t.output_id
                    )
                }),
        );
        let mut outputs: Vec<_> = state
            .tasks
            .values()
            .rev()
            .filter(|t| t.spool.path().exists())
            .take(20)
            .map(|t| (t.info.output_id.clone(), t.info.command.clone()))
            .collect();
        outputs.extend(
            services
                .iter()
                .map(|t| (t.output_id.clone(), t.command.clone())),
        );
        for agent in state.agents.values().rev() {
            if outputs.len() >= 20 {
                break;
            }
            if let Some(id) = &agent.view.output_id {
                outputs.push((id.clone(), agent.view.description.clone()));
            }
        }
        for pair in &state.remembered {
            if outputs.len() == 20 {
                break;
            }
            if !outputs.iter().any(|(id, _)| id == &pair.0)
                && ai_output::path(&self.0.root, &self.0.conversation, &pair.0)
                    .is_ok_and(|p| p.exists())
            {
                outputs.push(pair.clone());
            }
        }
        let readable = outputs
            .iter()
            .map(|(id, command)| format!("{id} · {command}"))
            .collect();
        (running, readable)
    }
    pub fn remember_outputs(&self, conversation: &shellmate_ai::Conversation) {
        self.remember_agents(conversation);
        let mut state = self.0.state.lock().unwrap();
        state.remembered.clear();
        for e in conversation.entries().iter().rev() {
            if let shellmate_ai::Entry::Step(s) = e
                && let Some(id) = &s.result.output_id
            {
                if state.remembered.iter().any(|(old, _)| old == id) {
                    continue;
                }
                if ai_output::path(&self.0.root, &self.0.conversation, id).is_ok_and(|p| p.exists())
                {
                    let command =
                        conversation
                            .entries()
                            .iter()
                            .rev()
                            .find_map(|entry| {
                                if let shellmate_ai::Entry::Step(other) = entry
                                    && other.result.output_id.as_ref() == Some(id)
                                {
                                    other.display.as_ref().map(|d| d.command.clone()).or_else(
                                        || {
                                            other
                                                .call
                                                .args_json
                                                .get("command")
                                                .and_then(|v| v.as_str())
                                                .map(str::to_owned)
                                        },
                                    )
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                    state.remembered.push((id.clone(), command));
                    if state.remembered.len() == 20 {
                        break;
                    }
                }
            }
        }
    }
    pub fn output_id_for(&self, call_id: &str) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap()
            .tasks
            .values()
            .rev()
            .find(|t| t.info.call_id == call_id)
            .map(|t| t.info.output_id.clone())
    }
    pub fn stop_all(&self) {
        let services = self
            .0
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for child in services {
            child.stop_all();
        }
        for task in self.0.state.lock().unwrap().agents.values() {
            if let Some(child) = task.child_outputs.upgrade() {
                child.control.seal();
            }
            task.cancel.cancel();
        }
        for task in self.0.state.lock().unwrap().tasks.values() {
            if task.info.status == Status::Running {
                let _ = task.cancel.send(Some(Stop::User));
            }
        }
    }
    pub fn stop(&self, id: &str) -> bool {
        if let Some((child, local)) = self.service_task(id) {
            return child.stop(&local);
        }
        let state = self.0.state.lock().unwrap();
        if let Some(number) = id
            .strip_prefix("agent-")
            .and_then(|n| n.parse::<u64>().ok())
        {
            return state.agents.get(&number).is_some_and(|t| {
                if t.view.status == shellmate_ai::SubagentStatus::Running {
                    if let Some(child) = t.child_outputs.upgrade() {
                        child.control.seal();
                    }
                    t.cancel.cancel();
                    true
                } else {
                    false
                }
            });
        }
        let Some(task) = id.parse::<u64>().ok().and_then(|n| state.tasks.get(&n)) else {
            return false;
        };
        task.info.status == Status::Running && task.cancel.send(Some(Stop::User)).is_ok()
    }
    pub async fn local_tool(&self, tool: LocalTool, limits: ExecLimits) -> LocalResult {
        let routed = match &tool {
            LocalTool::ReadOutput(a) => self.service_task(&a.id),
            LocalTool::StopTask { id } => self.service_task(id),
            _ => None,
        };
        if let Some((child, id)) = routed {
            let tool = match tool {
                LocalTool::ReadOutput(mut a) => {
                    a.id = id;
                    LocalTool::ReadOutput(a)
                }
                LocalTool::StopTask { .. } => LocalTool::StopTask { id },
                _ => unreachable!(),
            };
            return Box::pin(child.local_tool(tool, limits)).await;
        }
        if let Some(result) = self.agent_tool(&tool).await {
            return result;
        }
        match tool {
            LocalTool::ViewImage {id} => {
                let root=self.root(); let conversation=self.conversation_id().to_owned();
                match tokio::task::spawn_blocking(move || shellmate_ai::media::lookup(&root,&conversation,&id)).await {
                    Ok(Ok(a)) => {let mut result=LocalResult::text(a.label()); result.attachments.push(a); result},
                    _ => LocalResult::error("图片不可用，请重新添加"),
                }
            }
            LocalTool::Web(request) => {
                let Some(web) = self.0.web.get() else {
                    return LocalResult::error("网页工具不可用");
                };
                let result = tokio::select! {biased;()=self.0.control.urgent()=>Err("网页请求已取消，正在处理新指令".into()),result=web.execute(request)=>result};
                match result {
                    Ok(data) => {
                        let mut result = LocalResult::text(serde_json::to_string(&data).unwrap());
                        if let shellmate_ai::web::ResultData::Page { url, .. } = &data
                            && let Some(text) = web.page_text(url)
                        {
                            let id = format!("web-{}", shellmate_core::HostId::generate());
                            if matches!(
                                self.while_retained(|| {
                                    let spool =
                                        Spool::create(&self.0.root, &self.0.conversation, &id)?;
                                    spool.write(text.as_bytes(), false)?;
                                    spool.finish()
                                }),
                                Some(Ok(()))
                            ) {
                                self.0
                                    .state
                                    .lock()
                                    .unwrap()
                                    .remembered
                                    .push((id.clone(), url.clone()));
                                result.output_id = Some(id);
                            }
                        }
                        result
                    }
                    Err(e) => LocalResult::error(e),
                }
            }
            LocalTool::WaitTasks { ids, all } => self.wait_tasks(ids, all).await,
            LocalTool::Memory(_) | LocalTool::ScopedMemory { .. } | LocalTool::Sessions(_) => {
                LocalResult {
                    attachments: Vec::new(), subagent: None,
                    inline: "记忆工具必须由会话主机处理".into(),
                    output_id: None,
                    is_error: true,
                }
            }
            LocalTool::StopTask { id } => {
                let task = {
                    let state = self.0.state.lock().unwrap();
                    id.parse::<u64>()
                        .ok()
                        .and_then(|n| state.tasks.get(&n))
                        .map(|t| (t.cancel.clone(), t.result.clone(), t.info.output_id.clone()))
                };
                let Some((cancel, result, oid)) = task else {
                    return LocalResult {
                        attachments: Vec::new(), subagent: None,
                        inline: format!("未找到任务 #{id}"),
                        output_id: None,
                        is_error: true,
                    };
                };
                let running = result.is_some();
                if let Some(mut result) = result {
                    let _ = cancel.send(Some(Stop::User));
                    wait_result(&mut result).await;
                }
                LocalResult {
                    attachments: Vec::new(), subagent: None,
                    inline: format!(
                        "{} #{id} · 输出 id {oid}",
                        if running {
                            "已停止任务"
                        } else {
                            "任务已结束"
                        }
                    ),
                    output_id: Some(oid),
                    is_error: false,
                }
            }
            LocalTool::ReadOutput(a) => {
                let (oid, spool, info) = {
                    let state = self.0.state.lock().unwrap();
                    let task =
                        a.id.parse::<u64>()
                            .ok()
                            .and_then(|n| state.tasks.get(&n))
                            .or_else(|| state.tasks.values().find(|t| t.info.output_id == a.id));
                    task.map_or((a.id.clone(), None, None), |t| {
                        (t.info.output_id.clone(), Some(t.spool.clone()), Some(t.info.clone()))
                    })
                };
                let path = match ai_output::path(&self.0.root, &self.0.conversation, &oid) {
                    Ok(p) => p,
                    Err(_) => return LocalResult::text(ai_output::CLEARED),
                };
                if !path.exists() {
                    return LocalResult::text(ai_output::CLEARED);
                }
                let bytes = limits.output_chars() * 4 + 4;
                let window = match spool {
                    Some(s) => s.lines(
                        a.offset_line,
                        a.limit_lines.unwrap_or(2000),
                        a.grep.as_deref(),
                        bytes,
                    ),
                    None => ai_output::read_lines(
                        &path,
                        a.offset_line,
                        a.limit_lines.unwrap_or(2000),
                        a.grep.as_deref(),
                        bytes,
                    ),
                };
                match window {
                    Ok(w) => {
                        // Reading a log says nothing about the original process's exit.
                        // Keep the read result successful even when that process failed.
                        let status = info.map_or_else(
                            || "状态未知（仅有历史日志，不能据此判断是否仍在运行或正常退出）".into(),
                            |info| info.status_text(),
                        );
                        let mut preview = shellmate_ai::output::char_excerpt(
                            &shellmate_ai::output::clean(&w.bytes), limits.output_chars(),
                        );
                        if w.truncated {
                            preview.push_str("\n完整输出已保存，用 read_output 查看或搜索");
                        }
                        LocalResult {
                            attachments: Vec::new(), subagent: None,
                            inline: format!("输出 id {oid} · 总计 {} 字节 · 日志读取成功 · 原命令：{status}\n{preview}", w.total),
                            output_id: Some(oid),
                            is_error: false,
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        LocalResult::text(ai_output::CLEARED)
                    }
                    Err(e) => LocalResult {
                        attachments: Vec::new(), subagent: None,
                        inline: format!("读取输出失败：{e}"),
                        output_id: Some(oid),
                        is_error: true,
                    },
                }
            }
        }
    }
    pub async fn run<F>(
        &self,
        mut call: ToolCall,
        ready: Option<tokio::sync::oneshot::Receiver<()>>,
        work: F,
    ) -> ToolOutcome
    where
        F: FnOnce(Spool) -> BoxFuture<'static, ToolOutcome>,
    {
        if call.service.is_some() {
            call.run_in_background = true;
            if let Some((parent, agent)) = self.0.service_parent.get() {
                let Some(parent) = parent.upgrade() else {
                    return ToolOutcome::Disconnected;
                };
                let mut state = parent.state.lock().unwrap();
                if state.retired {
                    return ToolOutcome::Disconnected;
                }
                state.services.insert(agent.clone(), self.clone());
            }
        }
        let oid = shellmate_core::HostId::generate().to_string();
        let spool =
            match self.while_retained(|| Spool::create(&self.0.root, &self.0.conversation, &oid)) {
                Some(Ok(s)) => s,
                Some(Err(e)) => return ToolOutcome::Failed(format!("无法保存输出：{e}")),
                None => return ToolOutcome::Disconnected,
            };
        let (cancel, mut cancelled) = watch::channel(None);
        let (result_tx, mut result) = watch::channel(None);
        let started = Instant::now();
        let number = {
            let mut state = self.0.state.lock().unwrap();
            if state.retired {
                return ToolOutcome::Disconnected;
            }
            let number = state.next;
            state.next += 1;
            state.tasks.insert(
                number,
                Task {
                    info: TaskInfo {
                        service: call.service.as_ref().map(|_| ServiceState::Starting),
                        destination: call.destination.clone(),
                        owner: None,
                        kind: Kind::Command,
                        call_id: call.id.clone(),
                        id: number.to_string(),
                        command: call.command.clone(),
                        target: call.target,
                        started,
                        status: Status::Running,
                        output_id: oid.clone(),
                        background: call.run_in_background,
                    },
                    spool: spool.clone(),
                    cancel: cancel.clone(),
                    result: Some(result.clone()),
                },
            );
            number
        };
        let mut guard = Foreground {
            cancel,
            detached: false,
        };
        let weak = Arc::downgrade(&self.0);
        let future = work(spool.clone());
        let worker_call = call.clone();
        let worker_oid = oid.clone();
        let service_spool = spool.clone();
        tokio::spawn(async move {
            let (raw, stopped) = tokio::select! {
                biased;
                _ = cancelled.changed() => (None, Some(cancelled.borrow().unwrap_or(Stop::User))),
                out = future => (Some(out), None),
            };
            let status = match (&raw, stopped) {
                (_, Some(_)) => Status::Stopped,
                (Some(ToolOutcome::Disconnected), _) => Status::Disconnected,
                (Some(ToolOutcome::Busy), _) => Status::Busy,
                (Some(ToolOutcome::Failed(_)), _) => Status::Failed,
                (Some(ToolOutcome::Completed(out)), _) => Status::Exited(out.exit_code),
                _ => Status::Exited(None),
            };
            let finish = spool.finish();
            let mut out = match spool.window(worker_call.exec_limits.output_chars() * 4 + 4) {
                Ok(w) => spooled(
                    &worker_oid,
                    &worker_call.command,
                    &w.bytes,
                    w.total,
                    match &status {
                        Status::Exited(code) => *code,
                        _ => None,
                    },
                    started.elapsed(),
                    worker_call.exec_limits,
                ),
                Err(e) => spooled(
                    &worker_oid,
                    &worker_call.command,
                    e.to_string().as_bytes(),
                    spool.total(),
                    None,
                    started.elapsed(),
                    worker_call.exec_limits,
                ),
            };
            out.disconnected = status == Status::Disconnected;
            if out.disconnected {
                out.message = Some(format!(
                    "会话已断开 · 输出 id {worker_oid}\n{}",
                    out.stdout()
                ));
            }
            if let Some(Stop::Timeout) = stopped {
                out.timed_out = true;
            }
            let error = match &raw {
                Some(ToolOutcome::Failed(e)) => Some(e.clone()),
                _ => finish.err().map(|e| e.to_string()),
            };
            if let Some(error) = error {
                out.message = Some(format!(
                    "执行失败：{error} · 输出 id {worker_oid}\n{}",
                    out.stdout()
                ));
                out.successful = false;
            }
            // Busy/disconnected before execution remain their original outcomes. The
            // registry still owns the output id for a partially executed disconnect.
            let outcome = if matches!(raw, Some(ToolOutcome::Busy)) {
                ToolOutcome::Busy
            } else {
                ToolOutcome::Completed(out)
            };
            finish_task(&weak, number, status, &outcome);
            let _ = result_tx.send(Some(outcome));
        });
        if call.run_in_background {
            // Terminal registration must answer Busy before we promise a background task.
            if let Some(ready) = ready
                && ready.await.is_err()
            {
                let out = wait_result(&mut result).await;
                guard.detached = true;
                return out;
            }
            guard.detached = true;
            if let Some(service) = &call.service {
                return self
                    .check_service(&call, service, number, &oid, &service_spool, &mut result)
                    .await;
            }
            return background(&call, &oid, number, false);
        }
        tokio::select! {
            out = wait_result(&mut result) => { guard.detached = true; out }
            () = self.0.control.urgent() => {
                let mut state=self.0.state.lock().unwrap();
                let task=state.tasks.get_mut(&number).unwrap();task.info.background=true;
                if task.info.status!=Status::Running {
                    let text=notice(&task.info);
                    self.0.publish(format!("output:{}",task.info.output_id),task.info.id.clone(),text.clone());
                    state.notices.push(text);
                }
                guard.detached=true;
                if let Some(out)=result.borrow().clone(){out}else{background(&call,&oid,number,false)}
            }
            () = tokio::time::sleep(call.timeout) => {
                if shellmate_ai::policy::command_words(&call.command).first().is_some_and(|w| w == "sleep") {
                    let _ = guard.cancel.send(Some(Stop::Timeout));
                    let out = wait_result(&mut result).await;
                    guard.detached = true;
                    out
                } else {
                    // Finish and promotion share the same lock, preventing lost notices.
                    let mut state = self.0.state.lock().unwrap();
                    let task = state.tasks.get_mut(&number).unwrap();
                    task.info.background = true;
                    if task.info.status != Status::Running {
                        let notice = notice(&task.info);
                        self.0.publish(format!("output:{}",task.info.output_id),task.info.id.clone(),notice.clone());
                        state.notices.push(notice);
                    }
                    guard.detached = true;
                    background(&call, &oid, number, true)
                }
            }
        }
    }
}
fn service_exited(out: ToolOutcome, oid: &str) -> ToolOutcome {
    match out {
        ToolOutcome::Completed(mut out) => {
            out.successful = false;
            let detail = out
                .message
                .clone()
                .unwrap_or_else(|| out.stdout().to_owned());
            out.message = Some(format!(
                "服务在启动检查期间已退出，尚未确认可用 · 输出 id {oid}\n{detail}"
            ));
            ToolOutcome::Completed(out)
        }
        other => other,
    }
}

fn background(call: &ToolCall, oid: &str, number: u64, timeout: bool) -> ToolOutcome {
    let mut out = shape([], [], None, Duration::ZERO, false, false);
    out.output_id = Some(oid.into());
    out.task_id = Some(number.to_string());
    out.successful = true;
    let mut text = if timeout {
        format!(
            "命令在 {} 秒内未完成，已结束等待，命令在原通道继续运行 · 任务 #{number} · 输出 id {oid}",
            call.timeout.as_secs()
        )
    } else {
        format!("命令已异步启动 · 任务 #{number} · 输出 id {oid}")
    };
    if call.target == Target::Terminal {
        text.push_str(
            " · 命令仍在终端中运行，结束后终端状态（包括目录变化）会保留；在此之前终端处于占用状态",
        );
    } else if shellmate_ai::policy::changes_directory(&call.command) {
        text.push_str(" · 其中的 cd 不会影响后续命令");
    }
    out.message = Some(text);
    ToolOutcome::Completed(out)
}
async fn wait_result(result: &mut watch::Receiver<Option<ToolOutcome>>) -> ToolOutcome {
    loop {
        if let Some(out) = result.borrow().clone() {
            return out;
        }
        if result.changed().await.is_err() {
            return ToolOutcome::Disconnected;
        }
    }
}
fn notice(info: &TaskInfo) -> String {
    format!(
        "异步任务 #{} · {} · 用 read_output 查看输出",
        info.id, info.status_text()
    )
}
fn finish_task(weak: &Weak<Inner>, number: u64, status: Status, _: &ToolOutcome) {
    if let Some(inner) = weak.upgrade() {
        let mut state = inner.state.lock().unwrap();
        let mut service_notice = None;
        if let Some(task) = state.tasks.get_mut(&number) {
            task.info.status = status;
            if task.info.service == Some(ServiceState::Starting) {
                task.info.service = Some(ServiceState::Failed);
            }
            task.result = None;
            if task.info.background {
                let text = notice(&task.info);
                if task.info.service.is_some() {
                    service_notice = Some((text.clone(), task.info.output_id.clone()));
                }
                inner.publish(
                    format!("output:{}", task.info.output_id),
                    task.info.id.clone(),
                    text.clone(),
                );
                state.notices.push(text);
            }
        }
        inner.changed.send_modify(|n| *n = n.wrapping_add(1));
        drop(state);
        if let Some((text, oid)) = service_notice
            && let Some((parent, agent)) = inner.service_parent.get()
            && let Some(parent) = parent.upgrade()
        {
            let id = format!("{agent}/{number}");
            let text = format!("{agent} 创建的持续服务：{text}；任务引用 {id}");
            parent.publish(format!("output:{oid}"), id, text.clone());
            parent.state.lock().unwrap().notices.push(text);
            parent.changed.send_modify(|n| *n = n.wrapping_add(1));
        }
    }
}
struct Foreground {
    cancel: watch::Sender<Option<Stop>>,
    detached: bool,
}
impl Drop for Foreground {
    fn drop(&mut self) {
        if !self.detached {
            let _ = self.cancel.send(Some(Stop::User));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_age_only_delete_conversation_and_save_off_close() {
        use std::fs::{File, FileTimes};
        use std::time::SystemTime;
        let temp = tempfile::tempdir().unwrap();
        let old = Spool::create(temp.path(), "keep-conversation", "old").unwrap();
        old.write(b"old", false).unwrap();
        old.finish().unwrap();
        let fresh = Spool::create(temp.path(), "keep-conversation", "fresh").unwrap();
        fresh.write(b"new", false).unwrap();
        fresh.finish().unwrap();
        let now = SystemTime::now();
        for path in [old.path(), old.path().with_extension("meta")] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(now - Duration::from_secs(31 * 86400)))
                .unwrap();
        }
        assert_eq!(ai_output::sweep(temp.path(), now).unwrap(), 2);
        assert!(!old.path().exists() && fresh.path().exists());
        ai_output::close_conversation(temp.path(), "keep-conversation", true).unwrap();
        assert!(fresh.path().exists());
        ai_output::delete_conversation(temp.path(), "keep-conversation").unwrap();
        assert!(!fresh.path().parent().unwrap().exists());
        let private = Spool::create(temp.path(), "unsaved", "one").unwrap();
        private.write(b"private", false).unwrap();
        ai_output::close_conversation(temp.path(), "unsaved", false).unwrap();
        assert!(!private.path().exists());
        assert!(ai_output::path(temp.path(), "../escape", "x").is_err());
        assert!(ai_output::path(temp.path(), "safe", "../../escape").is_err());
    }
    #[test]
    fn running_ring_readback_and_stderr_fragments_and_line_limit() {
        let temp = tempfile::tempdir().unwrap();
        let spool = Spool::create(temp.path(), "test", "ring").unwrap();
        spool.write(b"HEAD\n", false).unwrap();
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..65 {
            spool.write(&block, false).unwrap();
        }
        spool.write(b"\nTAIL\n", false).unwrap();
        assert_eq!(
            std::fs::metadata(spool.path()).unwrap().len(),
            ai_output::KEEP_BYTES
        );
        let live = spool.window(200).unwrap();
        assert!(live.bytes.starts_with(b"HEAD\n") && live.bytes.ends_with(b"TAIL\n"));
        let selected = spool.lines(1, 2000, Some("TAIL"), 30000).unwrap();
        assert_eq!(selected.bytes, b"TAIL\n");
        spool.finish().unwrap();
        let ended = ai_output::read_window(&spool.path(), 200).unwrap();
        assert_eq!(live.bytes, ended.bytes);
        let text = Spool::create(temp.path(), "test", "lines").unwrap();
        text.write(b"out\n", false).unwrap();
        text.write(b"er", true).unwrap();
        text.write(b"ror\nsecond\n", true).unwrap();
        text.write(b"done\n", false).unwrap();
        assert_eq!(
            text.window(1000).unwrap().bytes,
            b"out\n[stderr] error\n[stderr] second\ndone\n"
        );
        text.write("line\n".repeat(2500).as_bytes(), false).unwrap();
        assert_eq!(
            text.lines(0, 9000, Some("line"), 30000).unwrap().bytes,
            "line\n".repeat(2000).as_bytes()
        );
        assert!(
            text.lines(0, 2000, Some(".*"), 30000)
                .unwrap()
                .bytes
                .is_empty()
        );
    }
    #[tokio::test]
    async fn retired_tool_full_output_is_readable_without_its_executor() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::create(root.path(), "history", "legacy-comparison").unwrap();
        spool
            .write(
                "config.toml\t内容不同\nREADME.md\t仅左侧存在\n".as_bytes(),
                false,
            )
            .unwrap();
        spool.finish().unwrap();
        let mut log = shellmate_ai::Conversation::default();
        log.push_user(shellmate_ai::UserTurn::new(
            "旧目录比较",
            Default::default(),
        ));
        log.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
            message_id: 1,
            call: shellmate_ai::LoggedCall {
                id: "old".into(),
                name: "compare_directories".into(),
                args_json: serde_json::json!({}),
            },
            result: shellmate_ai::LoggedResult {
                attachments: vec![],
                subagent: None,
                inline: "旧结果摘要".into(),
                output_id: Some("legacy-comparison".into()),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        let tasks = Tasks::new(root.path().into(), "history".into());
        tasks.remember_outputs(&log);
        assert!(
            tasks
                .state()
                .1
                .iter()
                .any(|s| s.contains("legacy-comparison"))
        );
        let result = tasks
            .local_tool(
                LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                    id: "legacy-comparison".into(),
                    offset_line: 0,
                    limit_lines: None,
                    grep: None,
                }),
                ExecLimits::default(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.inline.contains("config.toml") && result.inline.contains("README.md"));
        assert!(
            tasks.list().is_empty(),
            "reading saved output starts no terminal task"
        );
    }
    #[tokio::test]
    async fn reopened_log_restores_readable_ids_with_commands() {
        let temp = tempfile::tempdir().unwrap();
        let spool = Spool::create(temp.path(), "saved", "old-output").unwrap();
        spool.write(b"old output", false).unwrap();
        spool.finish().unwrap();
        let mut c = shellmate_ai::Conversation::default();
        c.push_user(shellmate_ai::UserTurn::new(
            "old question",
            Default::default(),
        ));
        let mut out = shape([], [], Some(0), Duration::ZERO, false, false);
        out.output_id = Some("old-output".into());
        c.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
            message_id: 1,
            call: shellmate_ai::LoggedCall {
                id: "old-call".into(),
                name: "run_command".into(),
                args_json: serde_json::json!({"command":"pwd"}),
            },
            result: shellmate_ai::LoggedResult {
                attachments: Vec::new(), subagent: None,
                inline: out.wire_text(),
                output_id: out.output_id,
                cleared: true,
                is_error: false,
            },
            display: None,
        }));
        c.append(shellmate_ai::Entry::Step(shellmate_ai::Step {
            message_id: 2,
            call: shellmate_ai::LoggedCall {
                id: "read-old".into(),
                name: "read_output".into(),
                args_json: serde_json::json!({"id":"old-output"}),
            },
            result: shellmate_ai::LoggedResult {
                attachments: Vec::new(), subagent: None,
                inline: "old output".into(),
                output_id: Some("old-output".into()),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        let tasks = Tasks::new(temp.path().into(), "saved".into());
        tasks.remember_outputs(&c);
        assert!(tasks.state().1[0].contains("pwd"));
        assert!(tasks.state().1[0].contains("old-output"));
        let result = tasks
            .local_tool(
                LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                    id: "old-output".into(),
                    offset_line: 0,
                    limit_lines: None,
                    grep: None,
                }),
                ExecLimits::default(),
            )
            .await;
        assert!(result.inline.contains("old output"));
        assert!(result.inline.contains("原命令：状态未知"));
        assert!(!result.inline.contains("退出码"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn unreadable_execution_state_never_becomes_success_from_log_reading() {
        for (raw, expected) in [
            (ToolOutcome::Busy, "未执行（终端被占用）"),
            (ToolOutcome::Disconnected, "连接已断开"),
            (ToolOutcome::Failed("cannot spawn".into()), "失败"),
            (ToolOutcome::Completed(shape([], [], None, Duration::ZERO, false, false)), "已退出 · 退出码 未知"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let tasks = Tasks::new(temp.path().into(), "read-states".into());
            let call = ToolCall {
                service: None, session_id: None, cwd: None, destination: None,
                id: "attempt".into(), command: "sleep 30".into(), purpose: "test".into(),
                target: Target::Terminal, timeout: Duration::from_secs(3), run_in_background: false,
                exec_limits: ExecLimits::default(), class: shellmate_ai::policy::classify("sleep 30"),
            };
            tasks.run(call, None, |_| Box::pin(async move { raw })).await;
            let read = tasks.local_tool(LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                id: "1".into(), offset_line: 0, limit_lines: None, grep: None,
            }), ExecLimits::default()).await;
            assert!(!read.is_error);
            assert!(read.inline.contains(&format!("原命令：{expected}")), "{}", read.inline);
            assert!(!read.inline.contains("退出码 0"));
            let view = super::super::output_view::display(&read.inline);
            assert_eq!(view.preview, "");
        }
    }
}
