//! App subagent service. Commands reuse AppExecutor/Tasks in a child-owned registry.
use super::{
    exec::{AppExecutor, Binding},
    host::{Host, Ui},
    tasks::Tasks,
};
use futures::future::BoxFuture;
use shellmate_ai::{
    Config, RigClient, SubagentRequest, SubagentStatus,
    agent::{Limits, LocalResult},
    subagent::{self, Cancellation, SubagentContext},
};
use shellmate_store::{SharedStore, ai::Settings};
use std::sync::{Arc, Mutex, atomic::Ordering};
pub type ClientFactory =
    Arc<dyn Fn(&Config) -> Result<RigClient, shellmate_ai::AiError> + Send + Sync>;
#[derive(Clone)]
pub struct Service {
    pub tasks: Tasks,
    pub config: Config,
    pub settings: Settings,
    pub snapshot: Arc<Mutex<shellmate_ai::memory::Snapshot>>,
    pub limits: Limits,
    pub db: SharedStore,
    pub context: shellmate_ai::ContextSnapshot,
    pub binding: Binding,
    pub memory: shellmate_core::memory::Memory,
    pub ui: async_channel::Sender<Ui>,
    pub mode: Arc<Mutex<shellmate_ai::PermissionMode>>,
    pub cancel: Cancellation,
    pub factory: ClientFactory,
}
pub fn supported(background: bool, local: bool, ssh: bool) -> bool {
    background && (local || ssh)
}
impl Service {
    pub fn supported(&self) -> bool {
        supported(
            self.binding.capabilities.background,
            self.binding.local,
            self.binding.connection.is_some(),
        )
    }
    pub fn start(&self, request: SubagentRequest, call_id: &str) -> BoxFuture<'_, LocalResult> {
        let call_id = call_id.to_owned();
        Box::pin(async move {
            if !self.supported() {
                return LocalResult::error("当前会话不支持子任务");
            }
            let config = match self.db.borrow().subagent_config(&self.config, &request) {
                Ok(c) => c,
                Err(_) => return LocalResult::error("无法读取子任务模型设置"),
            };
            let ticket = match self.tasks.reserve_agent(
                &call_id,
                &request,
                &config,
                self.settings.subagent_limit,
            ) {
                Ok(t) => t,
                Err(e) => return LocalResult::error(e),
            };
            let client = match (self.factory)(&config) {
                Ok(c) => c,
                Err(error) => {
                    return ticket.fail(error, self.limits.exec);
                }
            };
            let source = ticket.source();
            let child_tasks = self.tasks.child_tasks();
            let mut binding = self.binding.clone();
            binding.capabilities.terminal = false;
            let host = Host {
                compaction: Default::default(),
                mode: self.mode.clone(),
                ui: self.ui.clone(),
                executor: Box::new(AppExecutor {
                    subagents: None,
                    memory: Some((self.memory.clone(), self.context.label.clone())),
                    binding: binding.clone(),
                    tasks: child_tasks.clone(),
                    ui: self.ui.clone(),
                }),
                alive: binding.alive.clone(),
                resolved: Mutex::new(None),
            };
            let snapshot = self.memory_snapshot();
            let context = SubagentContext {
                id: source.id.clone(),
                snapshot: self.context.clone(),
                cwd: binding.capabilities.cwd.clone(),
            };
            let limits = self.limits;
            let background = request.background;
            let mut receive = ticket.result.clone();
            let token = ticket.cancel.clone();
            let immediate = ticket.start_result();
            let weak = ticket.view_handle();
            let ui = self.ui.clone();
            let source_id = source.id.clone();
            let alive = binding.alive.clone();
            let foreground_token = token.clone();
            let db = self.db.clone();
            let conversation = self.tasks.conversation_id().to_owned();
            tokio::spawn(async move {
                let monitor = token.clone();
                let run = subagent::run_with_cancel(
                    &client,
                    request,
                    context,
                    snapshot,
                    &host,
                    limits,
                    &token,
                    |event| weak.event(event),
                );
                tokio::pin!(run);
                let mut out = tokio::select! {biased; ()=super::exec::disconnected(&alive)=>{monitor.cancel();let mut out=run.await;out.status=SubagentStatus::Failed(shellmate_ai::AiError::Disconnected);out},out=&mut run=>out};
                if !alive.load(Ordering::Acquire) {
                    out.status = SubagentStatus::Failed(shellmate_ai::AiError::Disconnected);
                }
                child_tasks.stop_all();
                let _ = ui.try_send(Ui::CancelSource(source_id));
                let result = ticket.finish(out, limits.exec);
                let logged = shellmate_ai::LoggedResult {
                    inline: result.inline,
                    output_id: result.output_id,
                    subagent: result.subagent,
                    cleared: false,
                    is_error: result.is_error,
                };
                if let Err(error) =
                    db.borrow_mut()
                        .finish_ai_subagent(&conversation, &call_id, &logged)
                {
                    log::warn!("save subagent completion: {error}");
                }
            });
            if background {
                return immediate;
            }
            struct CancelOnDrop(Cancellation);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.cancel();
                }
            }
            let _guard = CancelOnDrop(foreground_token);
            loop {
                if let Some(result) = receive.borrow().clone() {
                    return result;
                }
                tokio::select! { biased; ()=self.cancel.cancelled()=>{_guard.0.cancel(); if receive.changed().await.is_err(){return LocalResult::error("子任务已停止");}},changed=receive.changed()=>{if changed.is_err(){return LocalResult::error("子任务已停止");}} }
            }
        })
    }
    fn memory_snapshot(&self) -> shellmate_ai::memory::Snapshot {
        self.snapshot.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[path = "subagents_tests.rs"]
mod tests;
