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
        call: ToolCall,
        redline: bool,
        reply: oneshot::Sender<Approval>,
    },
    Running {
        call: ToolCall,
        automatic: bool,
        ready: oneshot::Sender<()>,
    },
    CancelApproval(String),
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
impl ToolHost for Host {
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
    fn run(&self, mut call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            tokio::select! {
                biased;
                () = super::exec::disconnected(&self.alive) => ToolOutcome::Disconnected,
                out = async {
                    call.class = classify(&call.command);
                    let decision = decide(*self.mode.lock().unwrap(), call.class);
                    if decision != Decision::Run {
                        let mut guard = PendingGuard { ui: self.ui.clone(), id: call.id.clone(), armed: true };
                        let (reply, receive) = oneshot::channel();
                        if self.ui.send(Ui::Approval { call: call.clone(), redline: decision == Decision::AskRedLine, reply }).await.is_err() { return ToolOutcome::Disconnected; }
                        let approval = receive.await;
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
                    let (ready, receive) = oneshot::channel();
                    if self.ui.send(Ui::Running { call: call.clone(), automatic: decision == Decision::Run, ready }).await.is_err() || receive.await.is_err() { return ToolOutcome::Disconnected; }
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
