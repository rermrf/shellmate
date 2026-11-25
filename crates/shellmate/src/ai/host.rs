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
pub enum Ui {
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
    LocalCwd {
        session: usize,
        reply: oneshot::Sender<Option<String>>,
    },
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
            tokio::select! {
                biased;
                () = super::exec::disconnected(&self.alive) => ToolOutcome::Disconnected,
                out = async {
                    if source.is_some(){call.target=Target::Background;}
                    call.class = classify(&call.command);
                    let key=approval_key(source.as_ref(),&call.id);
                    let decision = decide(*self.mode.lock().unwrap(), call.class);
                    if decision != Decision::Run {
                        let mut guard = PendingGuard { ui: self.ui.clone(), id: key.clone(), armed: true };
                        let (reply, receive) = oneshot::channel();
                        if self.ui.send(Ui::Approval { source: source.clone(), call: call.clone(), redline: decision == Decision::AskRedLine, reply }).await.is_err() { return ToolOutcome::Disconnected; }
                        let approval = receive.await;
                        guard.armed = false;
                        match approval {
                            Ok(Approval::Approve { command, target }) => {
                                if command.trim().is_empty() { return ToolOutcome::Failed("命令不能为空".into()); }
                                call.command = command;
                                call.target = target;
                                call.timeout = call.exec_limits.timeout(Some(call.timeout.as_secs()));
                                call.class = classify(&call.command);
                                if source.is_some() {
                                    call.target=Target::Background;
                                }
                            }
                            Ok(Approval::Deny { reason }) => return ToolOutcome::Denied { reason },
                            Err(_) => return ToolOutcome::Denied { reason: Some("审批已取消".into()) },
                        }
                    }
                    *self.resolved.lock().unwrap() = Some((call.clone(), decision == Decision::Run));
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
