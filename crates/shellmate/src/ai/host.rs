//! Send + Sync approval bridge. No UI entity crosses into the worker.
use futures::future::BoxFuture;
use shellmate_ai::{
    agent::{Target, ToolCall, ToolHost, ToolOutcome},
    policy::{Decision, PermissionMode, classify, decide},
};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use tokio::sync::oneshot;

pub enum Approval {
    Approve { command: String, target: Target },
    Deny { reason: Option<String> },
}
#[derive(Clone)]
pub struct Reply(Arc<Mutex<Option<oneshot::Sender<Approval>>>>);
impl Reply {
    pub fn new(sender: oneshot::Sender<Approval>) -> Self {
        Self(Arc::new(Mutex::new(Some(sender))))
    }
    pub fn send(&self, value: Approval) -> Result<(), Approval> {
        match self.0.lock().unwrap().take() {
            Some(sender) => sender.send(value),
            None => Err(value),
        }
    }
    pub fn is_closed(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|s| s.is_closed())
    }
}
pub enum Ui {
    TuiApproval {source:Option<shellmate_ai::SubagentRef>,call:ToolCall,reply:oneshot::Sender<Approval>},
    TerminalInteraction {
        session:usize,lease:Arc<AtomicBool>,owner:Arc<shellmate_core::terminal_interaction::Controller>,operation:super::terminal_interaction::Operation,
        reply:oneshot::Sender<Result<super::terminal_interaction::Reply,String>>,
    },
    ReadSession {
        session: usize,
        lease: Arc<AtomicBool>,
        lines: usize,
        reply: oneshot::Sender<Result<shellmate_ai::ContextSnapshot, String>>,
    },
    Context {
        reply: oneshot::Sender<Option<shellmate_ai::ContextSnapshot>>,
    },
    Approval {
        source: Option<shellmate_ai::SubagentRef>,
        call: ToolCall,
        redline: bool,
        reply: oneshot::Sender<Approval>,
    },
    Running {
        source: Option<shellmate_ai::SubagentRef>,
        call: ToolCall,
        automatic: bool,
        ready: oneshot::Sender<()>,
    },
    CancelApproval(String),
    CancelSource(String),
    Finished(String),
    TerminalStart {
        session: usize,
        command: String,
        nonce: String,
        spool: shellmate_core::ai_output::Spool,
        lease: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<shellmate_term::agent::CaptureHandle, Box<ToolOutcome>>>,
    },
    TerminalFinish {
        session: usize,
        nonce: String,
    },
}
pub trait Executor: Send + Sync {
    fn model_slots(&self) -> Option<Arc<tokio::sync::Semaphore>> { None }
    fn sessions_enabled(&self) -> bool {
        false
    }
    fn routed_host(&self, _call: &mut ToolCall) -> Result<Option<Host>, String> {
        Ok(None)
    }
    fn web_enabled(&self) -> bool {
        false
    }
    fn take_external_steps(&self) -> Vec<shellmate_ai::Step> {
        vec![]
    }
    fn send_agent_message(
        &self,
        _id: &str,
        _text: &str,
        _immediate: bool,
        _resume: bool,
        _image_ids: Vec<String>,
        _call_id: &str,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        Box::pin(async {
            shellmate_ai::agent::LocalResult::error("不支持追加子任务消息")
        })
    }
    fn control(&self) -> Option<shellmate_ai::interaction::Control> {
        None
    }
    fn subagents(&self) -> bool {
        false
    }
    fn subagent(
        &self,
        _request: shellmate_ai::SubagentRequest,
        _call_id: &str,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        Box::pin(async {
            shellmate_ai::agent::LocalResult::error("当前会话不支持子任务")
        })
    }
    fn cancelled(&self) -> bool {
        false
    }
    fn subagent_results(&self) -> Vec<(String, shellmate_ai::LoggedResult)> {
        vec![]
    }
    fn finish_subagent(&self) {}

    fn memory_snapshot(&self, auto_memory: bool) -> shellmate_ai::memory::Snapshot {
        shellmate_ai::memory::Snapshot {
            auto_memory,
            ..Default::default()
        }
    }
    fn output_id(&self, _call: &ToolCall) -> Option<String> {
        None
    }
    fn local_tool_from(&self,tool:shellmate_ai::agent::LocalTool,limits:shellmate_ai::agent::ExecLimits,_source:Option<shellmate_ai::SubagentRef>,_call_id:&str)->BoxFuture<'_,shellmate_ai::agent::LocalResult>{self.local_tool(tool,limits)}
    fn local_tool(
        &self,
        _tool: shellmate_ai::agent::LocalTool,
        _limits: shellmate_ai::agent::ExecLimits,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        Box::pin(async { shellmate_ai::agent::LocalResult::text("工具不可用") })
    }
    fn take_notices(&self) -> Vec<String> {
        Vec::new()
    }
    fn task_state(&self) -> (Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    fn execute(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome>;
}
pub struct Host {
    pub compaction: Arc<Mutex<Option<Option<String>>>>,
    pub mode: Arc<Mutex<PermissionMode>>,
    pub ui: async_channel::Sender<Ui>,
    pub executor: Box<dyn Executor>,
    pub alive: Arc<AtomicBool>,
    pub resolved: Mutex<Option<(ToolCall, bool)>>,
}
struct PendingGuard {
    ui: async_channel::Sender<Ui>,
    id: String,
    armed: bool,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.ui.try_send(Ui::CancelApproval(self.id.clone()));
        }
    }
}
pub fn approval_key(source: Option<&shellmate_ai::SubagentRef>, id: &str) -> String {
    source.map_or_else(|| id.into(), |s| format!("{}:{id}", s.id))
}
struct RunningGuard {
    ui: async_channel::Sender<Ui>,
    key: String,
}
impl Drop for RunningGuard {
    fn drop(&mut self) {
        let _ = self.ui.try_send(Ui::Finished(self.key.clone()));
    }
}
impl ToolHost for Host {
    fn model_slots(&self) -> Option<Arc<tokio::sync::Semaphore>> { self.executor.model_slots() }
    fn sessions_enabled(&self) -> bool {
        self.executor.sessions_enabled()
    }
    fn web_enabled(&self) -> bool {
        self.executor.web_enabled()
    }
    fn take_external_steps(&self) -> Vec<shellmate_ai::Step> {
        self.executor.take_external_steps()
    }
    fn send_agent_message(
        &self,
        id: &str,
        text: &str,
        immediate: bool,
        resume: bool,
        image_ids: Vec<String>,
        call_id: &str,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        self.executor
            .send_agent_message(id, text, immediate, resume, image_ids, call_id)
    }
    fn control(&self) -> Option<shellmate_ai::interaction::Control> {
        self.executor.control()
    }
    fn subagents(&self) -> bool {
        self.executor.subagents()
    }
    fn subagent(
        &self,
        request: shellmate_ai::SubagentRequest,
        call_id: &str,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        self.executor.subagent(request, call_id)
    }
    fn cancelled(&self) -> bool {
        self.executor.cancelled()
    }
    fn subagent_results(&self) -> Vec<(String, shellmate_ai::LoggedResult)> {
        self.executor.subagent_results()
    }
    fn finish_subagent(&self, source: &shellmate_ai::SubagentRef) {
        self.executor.finish_subagent();
        let _ = self.ui.try_send(Ui::CancelSource(source.id.clone()));
    }

    fn memory_snapshot(&self, auto_memory: bool) -> shellmate_ai::memory::Snapshot {
        self.executor.memory_snapshot(auto_memory)
    }
    fn take_compaction(&self) -> Option<Option<String>> {
        self.compaction.lock().unwrap().take()
    }
    fn output_id(&self, call: &ToolCall) -> Option<String> {
        self.executor.output_id(call)
    }
    fn local_tool_from(&self,tool:shellmate_ai::agent::LocalTool,limits:shellmate_ai::agent::ExecLimits,source:Option<shellmate_ai::SubagentRef>,call_id:&str)->BoxFuture<'_,shellmate_ai::agent::LocalResult>{self.executor.local_tool_from(tool,limits,source,call_id)}
    fn local_tool(
        &self,
        tool: shellmate_ai::agent::LocalTool,
        limits: shellmate_ai::agent::ExecLimits,
    ) -> BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        self.executor.local_tool(tool, limits)
    }
    fn take_notices(&self) -> Vec<String> {
        self.executor.take_notices()
    }
    fn task_state(&self) -> (Vec<String>, Vec<String>) {
        self.executor.task_state()
    }

    fn permission_mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap()
    }
    fn automatic(&self, call: &ToolCall) -> bool {
        self.resolved
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(c, automatic)| c.id == call.id && *automatic)
    }
    fn context_snapshot(&self) -> BoxFuture<'_, Option<shellmate_ai::ContextSnapshot>> {
        Box::pin(async move {
            let (reply, receive) = oneshot::channel();
            self.ui.send(Ui::Context { reply }).await.ok()?;
            receive.await.ok().flatten()
        })
    }
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.run_from(call, None)
    }
    fn run_from(
        &self,
        mut call: ToolCall,
        source: Option<shellmate_ai::SubagentRef>,
    ) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            match self.executor.routed_host(&mut call) {
                Ok(Some(host)) => {
                    *self.resolved.lock().unwrap() = Some((call.clone(), false));
                    let result = host.run_from(call.clone(), source).await;
                    if let Some(resolved) = host.resolved.lock().unwrap().clone() {
                        *self.resolved.lock().unwrap() = Some(resolved);
                    }
                    return result;
                }
                Err(e) => return ToolOutcome::Failed(e),
                Ok(None) => {}
            }
            tokio::select! {
                biased;
                () = super::exec::disconnected(&self.alive) => ToolOutcome::Disconnected,
                out = async {
                    call.class = classify(&call.command);
                    let key=approval_key(source.as_ref(),&call.id);
                    let decision = decide(*self.mode.lock().unwrap(), call.class);
                    if decision != Decision::Run {
                        if let Some(control)=self.control(){control.set_phase(shellmate_ai::interaction::Phase::Approval);}
                        let mut guard = PendingGuard { ui: self.ui.clone(), id: key.clone(), armed: true };
                        let (reply, receive) = oneshot::channel();
                        if self.ui.send(Ui::Approval { source: source.clone(), call: call.clone(), redline: decision == Decision::AskRedLine, reply }).await.is_err() { return ToolOutcome::Disconnected; }
                        let approval = tokio::select!{biased;
                            ()=async{match self.control(){Some(c)=>c.urgent().await,None=>std::future::pending().await}}=>{
                                return ToolOutcome::Denied{reason:Some("用户更新了指令，本步骤未执行".into())};
                            },
                            approval=receive=>approval,
                        };
                        guard.armed = false;
                        match approval {
                            Ok(Approval::Approve { command, target }) => {
                                if command.trim().is_empty() { return ToolOutcome::Failed("命令不能为空".into()); }
                                call.command = command;
                                call.target = target;
                                call.timeout = call.exec_limits.timeout(Some(call.timeout.as_secs()));
                                call.class = classify(&call.command);
                            }
                            Ok(Approval::Deny { reason }) => return ToolOutcome::Denied { reason },
                            Err(_) => return ToolOutcome::Denied { reason: Some("审批已取消".into()) },
                        }
                    }
                    *self.resolved.lock().unwrap() = Some((call.clone(), decision == Decision::Run));
                    if let Some(control)=self.control(){control.set_phase(shellmate_ai::interaction::Phase::Executing);}
                    let (ready, receive) = oneshot::channel();
                    if self.ui.send(Ui::Running { source: source.clone(), call: call.clone(), automatic: decision == Decision::Run, ready }).await.is_err() || receive.await.is_err() { return ToolOutcome::Disconnected; }
                    let _guard=source.as_ref().map(|_|RunningGuard {ui:self.ui.clone(),key});
                    self.executor.execute(call).await
                } => out,
            }
        })
    }
    fn resolved_call(&self, call: &ToolCall) -> ToolCall {
        self.resolved
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(c, _)| c.id == call.id)
            .map(|(c, _)| c.clone())
            .unwrap_or_else(|| call.clone())
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
