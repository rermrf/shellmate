use super::*;
use timing::{Moment, Record, Status};
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn timing_fixture(&mut self, scene: &str, cx: &mut Context<Self>) {
        let status = match scene {
            "ai-timing-running" => Status::Running,
            "ai-timing-waiting" => Status::Waiting,
            "ai-timing-paused" => Status::Paused,
            "ai-timing-interrupted" => Status::Interrupted,
            _ => Status::Completed,
        };
        self.state.timing.fixture(
            status,
            self.state
                .model
                .conversation
                .turns()
                .len()
                .saturating_sub(1),
            Moment::now(),
        );
        self.state.model.busy = status == Status::Running;
        self.state.control.set_phase(if status == Status::Waiting {
            shellmate_ai::interaction::Phase::Waiting(vec!["agent-1".into()])
        } else {
            shellmate_ai::interaction::Phase::Generating
        });
        cx.notify();
    }
    pub(super) fn timing_event(&mut self, event: &AgentEvent) {
        if self.state.model.manual_compaction {
            return;
        }
        let snapshot = self.state.control.snapshot();
        let pending: Vec<_> = snapshot
            .completions
            .iter()
            .map(|e| e.task_id.clone())
            .collect();
        let tasks = self.tasks.as_ref().map(|t| t.list()).unwrap_or_default();
        self.state
            .timing
            .observe(&self.state.model.conversation, &tasks, &pending);
        if let Some(registry) = &self.tasks {
            self.state
                .timing
                .await_tasks(&registry.take_released_waits(), &tasks, &pending);
        }
        let now = Moment::now();
        match event {
            AgentEvent::Done(_) | AgentEvent::Released(agent::ReleaseReason::Detached) => {
                self.state.timing.done(now)
            }
            AgentEvent::Released(agent::ReleaseReason::Stopped)
            | AgentEvent::Error(AiError::Cancelled) => {
                self.state.timing.finish(Status::Stopped, now);
            }
            AgentEvent::Error(AiError::Disconnected) => {
                self.state.timing.finish(Status::Interrupted, now);
            }
            AgentEvent::Error(_) => {
                self.state.timing.finish(Status::Failed, now);
            }
            AgentEvent::Paused { .. } => {
                self.state.timing.finish(Status::Paused, now);
            }
            AgentEvent::CompletionsAccepted(ids) => {
                let tasks: Vec<_> = snapshot
                    .completions
                    .iter()
                    .filter(|e| ids.contains(&e.id))
                    .map(|e| e.task_id.clone())
                    .collect();
                self.state.timing.accept_results(&tasks);
            }
            _ => {}
        }
    }
    pub(super) fn checkpoint_timing(&mut self) {
        if self.fixture || !self.state.timing.ticking() {
            return;
        }
        let value = serde_json::json!(self.state.timing.snapshot(Moment::now()));
        const ERROR: &str = "运行时间未能保存到本机，正在重试";
        match self
            .db
            .borrow_mut()
            .update_ai_run_timing(&self.state.id, &value)
        {
            Err(_) => self.state.save_error = Some(ERROR.into()),
            Ok(_) if self.state.save_error.as_deref() == Some(ERROR) => {
                self.state.save_error = None
            }
            _ => {}
        }
    }
}
pub(super) fn footer(
    record: &Record,
    now: Moment,
    phase: &shellmate_ai::interaction::Phase,
    reply_group: &SharedString,
    copy: Option<AnyElement>,
    cx: &App,
) -> Stateful<Div> {
    let started = timing::timestamp(record.started_at_ms, now.wall_ms, true);
    let ended = record
        .ended_at_ms
        .map(|at| timing::timestamp(at, now.wall_ms, true))
        .unwrap_or_else(|| {
            if record.status == Status::Interrupted {
                "未知".into()
            } else {
                "尚未结束".into()
            }
        });
    let tip = format!(
        "开始时间：{started}\n回复时间：{ended}\n用时：{}\n按本地时区显示；不含排队及手动暂停时间",
        timing::duration(record.duration_ms)
    );
    div()
        .id(SharedString::from(format!("run-timing-{}", record.id)))
        .flex_none()
        .min_w_0()
        .text_size(px(11.5))
        .line_height(px(17.))
        .text_color(cx.palette().tx3)
        .whitespace_normal()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.))
        .child(timing::label(record, phase))
        .when(record.ended_at_ms.is_some() || copy.is_some(), |footer| {
            footer.child(div().flex().flex_none().items_center().gap(px(4.))
                .children(record.ended_at_ms.map(|at| {
                    div().opacity(0.)
                        .group_hover(reply_group.clone(), |time| time.opacity(1.))
                        .child(timing::timestamp(at, now.wall_ms, false))
                }))
                .children(copy))
        })
        .tooltip(move |window, cx| {
            gpui::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
}
