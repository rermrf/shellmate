//! Subagent entries of the conversation task registry. agent-N uses a separate monotonic sequence.
use super::*;
use crate::ai::subagent_view::{View, status_label};
use shellmate_ai::{
    SubagentOutcome, SubagentRef, SubagentRequest, SubagentStatus,
    subagent::{Cancellation, SubagentEvent},
};
pub(super) struct AgentTask {
    pub view: View,
    pub call_id: String,
    pub cancel: Cancellation,
    pub result: watch::Receiver<Option<LocalResult>>,
    pub child_outputs: Weak<Inner>,
}
pub struct Ticket {
    root: PathBuf,
    conversation: String,
    discarded: Arc<Mutex<bool>>,
    registry: Weak<Inner>,
    pub id: u64,
    pub cancel: Cancellation,
    pub result: watch::Receiver<Option<LocalResult>>,
    sender: watch::Sender<Option<LocalResult>>,
    initial: View,
}
#[derive(Clone)]
pub struct ViewHandle {
    registry: Weak<Inner>,
    id: u64,
}
impl ViewHandle {
    pub fn event(&self, event: SubagentEvent<'_>) {
        // Publish terminal status only after reports/transcripts and final result are ready.
        if matches!(event, SubagentEvent::Finished(_)) {
            return;
        }
        if let Some(registry) = self.registry.upgrade()
            && let Some(task) = registry.state.lock().unwrap().agents.get_mut(&self.id)
        {
            task.view.event(event);
        }
    }
}
impl Ticket {
    pub fn source(&self) -> SubagentRef {
        SubagentRef {
            id: self.initial.id.clone(),
            description: self.initial.description.clone(),
            kind: self.initial.kind,
        }
    }
    pub fn view_handle(&self) -> ViewHandle {
        ViewHandle {
            registry: self.registry.clone(),
            id: self.id,
        }
    }
    pub fn start_result(&self) -> LocalResult {
        LocalResult {
            inline: format!(
                "子任务已在后台启动：{}（{}）",
                self.initial.id, self.initial.description
            ),
            subagent: Some(Box::new(self.initial.display())),
            output_id: Some(self.initial.id.clone()),
            is_error: false,
        }
    }
    pub fn fail(self, error: shellmate_ai::AiError, limits: ExecLimits) -> LocalResult {
        let outcome = SubagentOutcome {
            status: SubagentStatus::Failed(error),
            report: "尚未完成可报告的工作。".into(),
            steps: 0,
            duration: self.initial.started.elapsed(),
            usage: Default::default(),
            model: self.initial.model.clone(),
            reasoning: self.initial.reasoning,
            transcript: Default::default(),
            source: self.source(),
            model_overridden: self.initial.model_overridden,
            reasoning_overridden: self.initial.reasoning_overridden,
        };
        self.finish(outcome, limits)
    }
    pub fn finish(self, out: SubagentOutcome, limits: ExecLimits) -> LocalResult {
        let mut result =
            shellmate_ai::subagent::subagent_result(&out, limits, Some(self.initial.id.clone()));
        if out.status == SubagentStatus::Failed(shellmate_ai::AiError::Disconnected) {
            result.inline =
                result
                    .inline
                    .replacen("子任务失败：会话已断开", "子任务失败：连接已断开", 1);
        }
        let discarded = self.discarded.lock().unwrap();
        if !*discarded {
            let saved = (|| {
                let spool = Spool::create(&self.root, &self.conversation, &self.initial.id)?;
                spool.write(out.report.as_bytes(), false)?;
                spool.finish()?;
                crate::ai::subagent_files::write(
                    &self.root,
                    &self.conversation,
                    &self.initial.id,
                    out.transcript.entries(),
                )
            })();
            if let Err(error) = saved {
                result
                    .inline
                    .push_str(&format!("\n子任务结果保存失败：{error}"));
                result.output_id = None;
                if let Some(display) = result.subagent.as_mut() {
                    display.output_id = None;
                }
            } else if let Some(display) = result.subagent.as_mut() {
                display.transcript_id = Some(self.initial.id.clone());
            }
        }
        drop(discarded);
        if let Some(registry) = self.registry.upgrade() {
            let mut state = registry.state.lock().unwrap();
            if let Some(task) = state.agents.get_mut(&self.id) {
                task.view.event(SubagentEvent::Finished(&out));
                task.view.output_id = result.output_id.clone();
                task.view.transcript_id = result
                    .subagent
                    .as_ref()
                    .and_then(|d| d.transcript_id.clone());
                self.sender.send_replace(Some(result.clone()));
                if task.view.background {
                    let notice = format!("子任务 {} {}", task.view.id, status_label(&out.status));
                    state.notices.push(notice);
                }
            }
        }
        self.sender.send_replace(Some(result.clone()));
        result
    }
}
impl Tasks {
    pub fn reserve_agent(
        &self,
        call_id: &str,
        request: &SubagentRequest,
        config: &shellmate_ai::Config,
        limit: usize,
    ) -> Result<Ticket, String> {
        let mut state = self.0.state.lock().unwrap();
        if state.retired {
            return Err("会话已关闭".into());
        }
        let running = state
            .agents
            .values()
            .filter(|a| a.view.status == SubagentStatus::Running)
            .count();
        if running >= limit.clamp(1, 8) {
            return Err(format!(
                "已有 {running} 个子任务在运行，请等待或停止其中一个"
            ));
        }
        while ai_output::path(
            &self.0.root,
            &self.0.conversation,
            &format!("agent-{}", state.next_agent),
        )
        .is_ok_and(|p| p.exists() || p.with_extension("json").exists())
        {
            state.next_agent += 1;
        }
        let id = state.next_agent;
        state.next_agent += 1;
        let mut view = View::new(format!("agent-{id}"), request, config);
        view.call_id = call_id.into();
        view.output_id = Some(view.id.clone());
        let cancel = Cancellation::default();
        let (sender, result) = watch::channel(None);
        state.agents.insert(
            id,
            AgentTask {
                view: view.clone(),
                call_id: call_id.into(),
                cancel: cancel.clone(),
                result: result.clone(),
                child_outputs: Weak::new(),
            },
        );
        Ok(Ticket {
            root: self.0.root.clone(),
            conversation: self.0.conversation.clone(),
            discarded: self.0.discarded.clone(),
            registry: Arc::downgrade(&self.0),
            id,
            cancel,
            result,
            sender,
            initial: view,
        })
    }
    pub(super) fn remember_agents(&self, log: &shellmate_ai::Conversation) {
        let mut state = self.0.state.lock().unwrap();
        for entry in log.entries() {
            if let shellmate_ai::Entry::Step(step) = entry
                && let Some(display) = &step.result.subagent
            {
                let Some(id) = display
                    .transcript_id
                    .as_ref()
                    .or(display.output_id.as_ref())
                else {
                    continue;
                };
                let Some(n) = id
                    .strip_prefix("agent-")
                    .and_then(|s| s.parse::<u64>().ok())
                else {
                    continue;
                };
                state.next_agent = state.next_agent.max(n + 1);
                if state.agents.contains_key(&n) {
                    continue;
                }
                let view = View::reopened(
                    id.clone(),
                    display,
                    step.call.args_json["run_in_background"]
                        .as_bool()
                        .unwrap_or(false),
                    step.result.inline.clone(),
                );
                let (sender, result) = watch::channel(None);
                drop(sender);
                state.agents.insert(
                    n,
                    AgentTask {
                        view,
                        call_id: step.call.id.clone(),
                        cancel: Default::default(),
                        result,
                        child_outputs: Weak::new(),
                    },
                );
            }
        }
    }
    pub fn child_tasks(&self) -> Self {
        Self::new(self.0.root.clone(), self.0.conversation.clone())
    }
    pub fn attach_child_outputs(&self, id: &str, tasks: &Self) {
        if let Some(task) = self
            .0
            .state
            .lock()
            .unwrap()
            .agents
            .values_mut()
            .find(|t| t.view.id == id)
        {
            task.child_outputs = Arc::downgrade(&tasks.0);
        }
    }
    pub fn agent_views(&self) -> Vec<View> {
        let views: Vec<_> = self
            .0
            .state
            .lock()
            .unwrap()
            .agents
            .values()
            .map(|t| (t.view.clone(), t.child_outputs.upgrade()))
            .collect();
        views
            .into_iter()
            .map(|(mut view, children)| {
                if let Some(children) = children {
                    let state = children.state.lock().unwrap();
                    for step in &mut view.step_list {
                        if let Some(task) = state.tasks.values().find(|t| t.info.call_id == step.id)
                        {
                            step.output_id = Some(task.info.output_id.clone());
                            step.running = task.info.status == Status::Running;
                        }
                    }
                }
                view
            })
            .collect()
    }
    pub fn agent_updates(&self) -> Vec<(String, shellmate_ai::LoggedResult)> {
        self.0
            .state
            .lock()
            .unwrap()
            .agents
            .values()
            .filter_map(|task| {
                task.result.borrow().clone().map(|r| {
                    (
                        task.call_id.clone(),
                        shellmate_ai::LoggedResult {
                            inline: r.inline,
                            output_id: r.output_id,
                            is_error: r.is_error,
                            cleared: false,
                            subagent: r.subagent,
                        },
                    )
                })
            })
            .collect()
    }
    pub fn sync_agents(&self, log: &mut shellmate_ai::Conversation) -> bool {
        let mut changed = false;
        for (call_id, result) in self.agent_updates() {
            if log.entries().iter().any(|e|matches!(e,shellmate_ai::Entry::Step(s) if s.call.id==call_id&&!s.result.same_content(&result))) {log.apply(shellmate_ai::LogUpdate::SubagentResult {call_id,result});changed=true;}
        }
        changed
    }
    pub fn stop_foreground(&self) -> bool {
        let state = self.0.state.lock().unwrap();
        let mut agent = false;
        for task in state.tasks.values() {
            if task.info.status == Status::Running && !task.info.background {
                let _ = task.cancel.send(Some(Stop::User));
            }
        }
        for task in state.agents.values() {
            if task.view.status == SubagentStatus::Running && !task.view.background {
                task.cancel.cancel();
                agent = true;
            }
        }
        agent
    }
    pub fn conversation_id(&self) -> &str {
        &self.0.conversation
    }
    pub fn discard(&self) {
        *self.0.discarded.lock().unwrap() = true;
        self.retire();
    }
    pub fn retire(&self) {
        self.0.state.lock().unwrap().retired = true;
        self.stop_all();
    }
    pub(super) async fn agent_tool(&self, tool: &LocalTool) -> Option<LocalResult> {
        let id = match tool {
            LocalTool::ReadOutput(a) => &a.id,
            LocalTool::StopTask { id } => id,
            _ => return None,
        };
        let n = id.strip_prefix("agent-")?.parse::<u64>().ok()?;
        let found = {
            let mut state = self.0.state.lock().unwrap();
            state.agents.get_mut(&n).map(|t| {
                if t.view.status != SubagentStatus::Running {
                    t.view.load_transcript(&self.0.root, &self.0.conversation);
                }
                (t.view.clone(), t.cancel.clone(), t.result.clone())
            })
        };
        let (view, cancel, mut receive) = found?;
        if matches!(tool, LocalTool::StopTask { .. }) {
            cancel.cancel();
            loop {
                if receive.borrow().is_some() {
                    break;
                }
                if receive.changed().await.is_err() {
                    break;
                }
            }
            return Some(LocalResult::text(format!("子任务 {id} 已停止")));
        }
        if view.status == SubagentStatus::Running {
            return Some(LocalResult::text(view.progress()));
        }
        // Full report is retained in the view; P11 paging parameters still apply.
        if let LocalTool::ReadOutput(args) = tool {
            let report = view
                .output_id
                .as_ref()
                .and_then(|id| ai_output::path(&self.0.root, &self.0.conversation, id).ok())
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or(view.report);
            let text = if args.offset_line == 0 && args.limit_lines.is_none() && args.grep.is_none()
            {
                report
            } else {
                report
                    .lines()
                    .skip(args.offset_line)
                    .filter(|line| args.grep.as_ref().is_none_or(|s| line.contains(s)))
                    .take(args.limit_lines.unwrap_or(usize::MAX))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Some(LocalResult {
                inline: text,
                output_id: view.output_id,
                is_error: false,
                subagent: None,
            });
        }
        None
    }
}
pub(super) fn info(view: &View, call_id: &str) -> TaskInfo {
    TaskInfo {
        kind: Kind::Agent,
        call_id: call_id.into(),
        id: view.id.clone(),
        command: view.description.clone(),
        target: Target::Background,
        started: if view.status == SubagentStatus::Running {
            view.started
        } else {
            Instant::now()
                .checked_sub(Duration::from_millis(view.duration_ms))
                .unwrap_or_else(Instant::now)
        },
        status: match view.status {
            SubagentStatus::Running => Status::Running,
            SubagentStatus::Completed => Status::Exited(Some(0)),
            SubagentStatus::Stopped => Status::Stopped,
            SubagentStatus::StepLimit => Status::StepLimit,
            SubagentStatus::Failed(shellmate_ai::AiError::Disconnected) => Status::Disconnected,
            SubagentStatus::Failed(_) => Status::Failed,
        },
        output_id: view.output_id.clone().unwrap_or_default(),
        background: view.background,
    }
}
