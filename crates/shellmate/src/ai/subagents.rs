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
    pub router: Option<super::target_router::Router>,
    // The registry owns live state; no extra strong task-registry references are stored here.
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
    fn resolve_images(&self, ids: &[String]) -> Result<Vec<shellmate_ai::Attachment>,String> {
        if ids.len()>shellmate_ai::media::MAX_IMAGES {return Err("最多传递 8 张图片".into());}
        ids.iter().map(|id|shellmate_ai::media::lookup(&self.tasks.root(),self.tasks.conversation_id(),id)).collect()
    }
    #[cfg(test)]
    pub fn message(
        &self,
        id: &str,
        text: &str,
        immediate: bool,
        resume: bool,
        call_id: &str,
    ) -> BoxFuture<'_, LocalResult> {
        self.message_from(
            id,
            text,
            immediate,
            resume,
            call_id,
            shellmate_ai::MessageOrigin::Parent,
        )
    }
    #[cfg(test)]
    pub fn message_from(
        &self,
        id: &str,
        text: &str,
        immediate: bool,
        resume: bool,
        call_id: &str,
        origin: shellmate_ai::MessageOrigin,
    ) -> BoxFuture<'_, LocalResult> {
        self.message_images(id, text, immediate, resume, call_id, vec![], origin)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn message_images(&self, id: &str, text: &str, immediate: bool, resume: bool, call_id: &str, image_ids: Vec<String>, origin: shellmate_ai::MessageOrigin) -> BoxFuture<'_, LocalResult> {
        let id = id.to_owned();
        let text = text.to_owned();
        let call_id = call_id.to_owned();
        Box::pin(async move {
            if text.trim().is_empty() && image_ids.is_empty() {
                return LocalResult::error("追加消息不能为空");
            }
            let attachments=match self.resolve_images(&image_ids) {Ok(a)=>a,Err(e)=>return LocalResult::error(e)};
            let Some((view, stable, children)) = self.tasks.latest_agent(&id) else {
                return LocalResult::error("未找到本会话的子任务");
            };
            if view.status == SubagentStatus::Running {
                let Some(children) = children else {
                    return LocalResult::error("子任务正在启动，请稍后重试");
                };
                let control = children.control();
                let accepted = self.tasks.while_retained(|| {
                    control.enqueue_turn_from(
                        call_id.clone(),
                        shellmate_ai::UserTurn::new(&text,self.context.clone()).with_attachments(attachments).sent_now(),
                        Some(origin),
                        |input| {
                            super::subagent_files::append_input(
                                &self.tasks.root(),
                                self.tasks.conversation_id(),
                                &view.id,
                                input,
                            )
                        },
                    )
                });
                match accepted {
                    Some(Ok(true)) => {}
                    Some(Ok(false)) => {
                        return LocalResult::error(
                            "子任务正在结束，本条消息尚未接收，请稍后重试并继续该任务",
                        );
                    }
                    _ => return LocalResult::error("追加消息未能保存，请重试"),
                }
                control.resume();
                if immediate {
                    control.request(shellmate_ai::interaction::Intent::SendNow);
                }
                return LocalResult::text("追加消息已送达子任务，将在下一处理边界读取");
            }
            if !resume {
                return LocalResult::error("子任务已结束；明确继续时请设置 resume=true");
            }
            let Some(mut record) =
                load_record(&self.tasks.root(), self.tasks.conversation_id(), &view.id)
            else {
                return LocalResult::error("子任务恢复信息已清理，无法继续原上下文");
            };
            if record.provider != self.config.provider.id()
                || record.base_url != self.config.provider.base_url()
            {
                return LocalResult::error("服务商已变化，请恢复原配置后继续子任务");
            }
            if let (Some(saved), Some(router)) = (&record.scope, &self.router) {
                let targets=saved.targets.keys().map(|id|router.resumed_reference(id)).collect::<Result<Vec<_>,_>>();
                record.request.session_ids=match targets {Ok(ids)=>ids,Err(e)=>return LocalResult::error(e)};
                record.request.default_session_id=match saved.default.as_ref().map(|id|router.resumed_reference(id)).transpose(){Ok(id)=>id,Err(e)=>return LocalResult::error(e)};
            }
            let checkpoint = super::subagent_files::read_checkpoint(
                &self.tasks.root(),
                self.tasks.conversation_id(),
                &view.id,
            );
            let restored_control=shellmate_ai::interaction::Control::default();
            if let Some(saved)=&checkpoint {restored_control.restore(saved.inbox.clone());}
            let pending=checkpoint.as_ref().map(|c|c.inbox.inputs.iter().filter(|i|!i.claimed || i.question_id.is_some()).cloned().collect::<Vec<_>>()).unwrap_or_default();
            let Some(transcript) = view
                .transcript
                .clone()
                .or_else(|| {
                    view.transcript_id.as_deref().and_then(|id| {
                        super::subagent_files::read(
                            &self.tasks.root(),
                            self.tasks.conversation_id(),
                            id,
                        )
                    })
                })
                .or_else(|| checkpoint.map(|c| c.entries))
            else {
                return LocalResult::error("执行记录已清理，无法继续原上下文");
            };
            let mut transcript = shellmate_ai::Conversation::from_entries(transcript);
            restored_control.restore_questions(transcript.entries());
            for input in pending {
                if input.question_id.as_ref().is_some_and(|id|transcript.entries().iter().any(|e|matches!(e,shellmate_ai::Entry::QuestionAnswer{request_id,..}if request_id==id))){continue;}
                transcript.append(restored_control.input_entry(&input));
            }
            let mut request = record.request;
            request.prompt = text;
            request.image_ids = image_ids;
            request.background = true;
            request.model = Some(record.model);
            request.reasoning = Some(record.reasoning);
            self.start_history(
                request,
                &call_id,
                Some((transcript, record.context, stable)),
                origin,
            )
            .await
        })
    }
    pub fn answer_question(
        &self,
        id: &str,
        question: &str,
        response: shellmate_ai::questions::Response,
    ) -> BoxFuture<'_, LocalResult> {
        let id = id.to_owned();
        let question = question.to_owned();
        Box::pin(async move {
            let Some((view, _, children)) = self.tasks.latest_agent(&id) else {
                return LocalResult::error("子任务不可用");
            };
            let control =
                if let Some(children) = children.filter(|_| view.status == SubagentStatus::Running) {
                    children.control()
                } else {
                    let Some(saved) = super::subagent_files::read_checkpoint(
                        &self.tasks.root(),
                        self.tasks.conversation_id(),
                        &view.id,
                    ) else {
                        return LocalResult::error("子任务恢复记录不可用");
                    };
                    let c = shellmate_ai::interaction::Control::default();
                    c.restore(saved.inbox);
                    c.restore_questions(&saved.entries);
                    c
                };
            if let Some(entries) = &view.transcript {
                control.restore_questions(entries);
            }
            let retry_saved = view.status != SubagentStatus::Running
                && control
                    .question(&question)
                    .is_some_and(|q| q.response.as_ref() == Some(&response))
                && control
                    .snapshot()
                    .inputs
                    .iter()
                    .any(|i| i.question_id.as_deref() == Some(&question));
            if !retry_saved {
                let saved = self.tasks.while_retained(|| {
                    control.answer_question(&question, response, |snapshot| {
                        super::subagent_files::save_question_answers(
                            &self.tasks.root(),
                            self.tasks.conversation_id(),
                            &view.id,
                            snapshot,
                        )
                        .map_err(|_| "答案未能保存，请重试".into())
                    })
                });
                match saved {
                    Some(Ok(())) => {}
                    Some(Err(e)) => return LocalResult::error(e),
                    None => return LocalResult::error("对话已删除"),
                }
            }
            if view.status == SubagentStatus::Running {
                control.resume();
                return LocalResult::text("答案已送达子任务");
            }
            self.message_images(
                &id,
                "请根据用户刚提交的问题反馈继续原任务。",
                false,
                true,
                &format!("answer-{question}"),
                vec![],
                shellmate_ai::MessageOrigin::User,
            )
            .await
        })
    }
    pub fn supported(&self) -> bool {
        if self.router.is_some() {
            return true;
        }
        supported(
            self.binding.capabilities.background,
            self.binding.local,
            self.binding.connection.is_some(),
        )
    }
    pub fn start(&self, request: SubagentRequest, call_id: &str) -> BoxFuture<'_, LocalResult> {
        self.start_history(
            request,
            call_id,
            None,
            shellmate_ai::MessageOrigin::Delegation,
        )
    }
    fn start_history(
        &self,
        request: SubagentRequest,
        call_id: &str,
        history: Option<(
            shellmate_ai::Conversation,
            subagent::SubagentContext,
            String,
        )>,
        origin: shellmate_ai::MessageOrigin,
    ) -> BoxFuture<'_, LocalResult> {
        let call_id = call_id.to_owned();
        let sent_at_ms = Some(shellmate_ai::UserTurn::now_ms());
        Box::pin(async move {
            if !self.supported() {
                return LocalResult::error("当前会话不支持子任务");
            }
            let config = match self.db.borrow().subagent_config(&self.config, &request) {
                Ok(c) => c,
                Err(_) => return LocalResult::error("无法读取子任务模型设置"),
            };
            let router = match &self.router {
                Some(router) => match router
                    .subset(&request.session_ids, request.default_session_id.as_deref())
                {
                    Ok(r) => Some(r),
                    Err(e) => return LocalResult::error(e),
                },
                None => None,
            };
            let attachments=match self.resolve_images(&request.image_ids) {Ok(a)=>a,Err(e)=>return LocalResult::error(e)};
            let ticket = match self.tasks.reserve_agent_family(
                &call_id,
                &request,
                &config,
                self.settings.subagent_limit,
                history.as_ref().map(|(_, _, stable)| stable.as_str()),
            ) {
                Ok(t) => t,
                Err(e) => return LocalResult::error(e),
            };
            let client = match (self.factory)(&config) {
                Ok(c) => c.with_image_root(self.tasks.root()),
                Err(error) => {
                    return ticket.fail(error, self.limits.exec);
                }
            };
            let source = ticket.source();
            let restored = history.as_ref().map(|(_, context, _)| context.clone());
            let stable = history
                .as_ref()
                .map(|(_, _, stable)| stable.clone())
                .unwrap_or_else(|| source.id.clone());
            let child_tasks = self.tasks.child_tasks();
            self.tasks.attach_child_outputs(&source.id, &child_tasks);
            let mut binding = router
                .as_ref()
                .and_then(|r| r.resolve(None).ok())
                .map(|e| e.binding)
                .unwrap_or_else(|| self.binding.clone());
            if let Some(context) = &restored {
                binding.capabilities.cwd = context.cwd.clone();
            }
            let host = Host {
                compaction: Default::default(),
                mode: self.mode.clone(),
                ui: self.ui.clone(),
                executor: Box::new(AppExecutor {
                    lease: None,
            slots: None,
                    router: router.clone(),
                    subagents: None,
                    memory: Some((self.memory.clone(), self.context.label.clone())),
                    binding: binding.clone(),
                    tasks: child_tasks.clone(),
                    ui: self.ui.clone(),
                }),
                alive: binding.alive.clone(),
                resolved: Mutex::new(None),
            };
            let snapshot = if let Some(router) = &router {
                let scope = router.scope.lock().unwrap().clone();
                let endpoint = router.resolve(None).ok();
                let memory = shellmate_core::memory::Memory::new(
                    &router.root,
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
                        .unwrap_or("多终端任务"),
                    self.limits.auto_memory,
                );
                snapshot.instructions.push_str(&format!(
                    "\n子任务默认终端与已引用终端：{}。全部应用终端均可通过 list_sessions 发现和使用；这些记录不是资源限制。根据委派要求选择目标和执行通道，每次调用指定准确 session_id。",
                    serde_json::to_string(&scope).unwrap()
                ));
                snapshot
            } else {
                self.memory_snapshot()
            };
            if !attachments.is_empty() && let Err(error)=client.validate_images() {return ticket.fail(error,self.limits.exec);}
            let context = SubagentContext {
                attachments,
                sent_at_ms,
                origin: Some(origin),
                id: source.id.clone(),
                snapshot: restored
                    .as_ref()
                    .map(|c| c.snapshot.clone())
                    .unwrap_or_else(|| self.context.clone()),
                cwd: binding.capabilities.cwd.clone(),
            };
            let record = AgentRecord {
                scope: router.as_ref().map(|r| r.scope.lock().unwrap().clone()),
                stable_id: stable,
                request: request.clone(),
                context: context.clone(),
                provider: config.provider.id().into(),
                base_url: config.provider.base_url().into(),
                model: config.model.clone(),
                reasoning: config.reasoning,
            };
            if !matches!(
                self.tasks.while_retained(|| save_record(
                    &self.tasks.root(),
                    self.tasks.conversation_id(),
                    &source.id,
                    &record,
                )),
                Some(Ok(()))
            ) {
                return ticket.fail(shellmate_ai::AiError::Persistence, self.limits.exec);
            }
            let transcript = history.map(|(history, _, _)| history).unwrap_or_default();
            let mut checkpoint_log = transcript.clone();
            subagent::prepare_history(
                &mut checkpoint_log,
                &request.prompt,
                context.snapshot.clone(),
                context.origin,
                context.sent_at_ms,
                context.attachments.clone(),
            );
            child_tasks.control().restore_questions(checkpoint_log.entries());
            let saved = self.tasks.while_retained(|| {
                super::subagent_files::checkpoint(
                    &self.tasks.root(),
                    self.tasks.conversation_id(),
                    &source.id,
                    Some(checkpoint_log.entries()),
                    child_tasks.control().snapshot(),
                )
            });
            if !matches!(saved, Some(Ok(()))) {
                return ticket.fail(shellmate_ai::AiError::Persistence, self.limits.exec);
            }
            let checkpoint_owner = self.tasks.retained();
            let checkpoint_root = self.tasks.root();
            let checkpoint_id = source.id.clone();
            let checkpoint_conversation = self.tasks.conversation_id().to_owned();
            let limits = self.limits;
            let background = request.background;
            let mut receive = ticket.result.clone();
            let token = ticket.cancel.clone();
            let immediate = ticket.start_result();
            let weak = ticket.view_handle();
            weak.configure(&checkpoint_log, &binding.capabilities.cwd);
            let ui = self.ui.clone();
            let source_id = source.id.clone();
            let alive = if router.is_some() {
                Arc::new(std::sync::atomic::AtomicBool::new(true))
            } else {
                binding.alive.clone()
            };
            let foreground_token = token.clone();
            let db = self.db.clone();
            let conversation = self.tasks.conversation_id().to_owned();
            tokio::spawn(async move {
                let monitor = token.clone();
                let run = subagent::run_from_history(
                    &client,
                    request,
                    context,
                    snapshot,
                    &host,
                    limits,
                    &token,
                    transcript,
                    |mut event| {
                        if let subagent::SubagentEvent::Updates(updates) = &event {
                            for update in *updates {
                                checkpoint_log.apply(update.clone());
                            }
                        }
                        if let subagent::SubagentEvent::Checkpoint(saved) = &mut event {
                            **saved = match checkpoint_owner.while_retained(|| {
                                super::subagent_files::checkpoint(
                                    &checkpoint_root,
                                    &checkpoint_conversation,
                                    &checkpoint_id,
                                    Some(checkpoint_log.entries()),
                                    child_tasks.control().snapshot(),
                                )
                            }) {
                                Some(Ok(())) => Ok(()),
                                _ => Err(shellmate_ai::AiError::Persistence),
                            };
                        }
                        weak.event(event)
                    },
                );
                tokio::pin!(run);
                let mut out = tokio::select! {biased; ()=super::exec::disconnected(&alive)=>{monitor.cancel();let mut out=run.await;out.status=SubagentStatus::Failed(shellmate_ai::AiError::Disconnected);out},out=&mut run=>out};
                if !alive.load(Ordering::Acquire) {
                    out.status = SubagentStatus::Failed(shellmate_ai::AiError::Disconnected);
                }
                child_tasks.finish_agent_commands();
                let _ = ui.try_send(Ui::CancelSource(source_id));
                let result = ticket.finish(out, limits.exec);
                let logged = shellmate_ai::LoggedResult {
                    attachments: Vec::new(), inline: result.inline,
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
            struct CancelOnDrop(Option<Cancellation>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    if let Some(cancel) = &self.0 {
                        cancel.cancel();
                    }
                }
            }
            let mut guard = CancelOnDrop(Some(foreground_token));
            self.tasks
                .control()
                .set_phase(shellmate_ai::interaction::Phase::Waiting(vec![
                    immediate.output_id.clone().unwrap(),
                ]));
            loop {
                if let Some(result) = receive.borrow().clone() {
                    return result;
                }
                tokio::select! { biased;
                    ()=self.cancel.cancelled()=>{if let Some(c)=&guard.0{c.cancel();}
                        if receive.changed().await.is_err(){return LocalResult::error("子任务已停止");}},
                    ()=async {self.tasks.control().urgent().await}=>{
                        self.tasks.promote_agent(immediate.output_id.as_deref().unwrap());guard.0=None;
                        return receive.borrow().clone().unwrap_or(immediate);
                    },
                    changed=receive.changed()=>{if changed.is_err(){return LocalResult::error("子任务已停止");}}
                }
            }
        })
    }
    fn memory_snapshot(&self) -> shellmate_ai::memory::Snapshot {
        self.snapshot.lock().unwrap().clone()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(crate = "serde")]
struct AgentRecord {
    #[serde(default)]
    scope: Option<super::target_router::Scope>,
    stable_id: String,
    request: SubagentRequest,
    context: SubagentContext,
    provider: String,
    base_url: String,
    model: String,
    reasoning: shellmate_ai::ReasoningLevel,
}
fn save_record(
    root: &std::path::Path,
    conversation: &str,
    id: &str,
    record: &AgentRecord,
) -> std::io::Result<()> {
    use std::io::Write;
    let path =
        shellmate_core::ai_output::path(root, conversation, id)?.with_extension("agent.json");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(file.as_file_mut(), record).map_err(std::io::Error::other)?;
    file.flush()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn load_record(root: &std::path::Path, conversation: &str, id: &str) -> Option<AgentRecord> {
    use std::io::Read;
    let path = shellmate_core::ai_output::path(root, conversation, id)
        .ok()?
        .with_extension("agent.json");
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = vec![];
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
pub fn saved_family(root: &std::path::Path, conversation: &str, id: &str) -> Option<String> {
    load_record(root, conversation, id).map(|r| r.stable_id)
}
pub fn saved_cwd(root: &std::path::Path, conversation: &str, id: &str) -> Option<String> {
    load_record(root, conversation, id).and_then(|r| r.context.cwd)
}

#[cfg(test)]
#[path = "subagents_tests.rs"]
mod tests;
