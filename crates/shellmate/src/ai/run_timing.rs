//! Presentation metadata for logical task runs; never included in model history.
use serde::{Deserialize, Serialize};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
pub(crate) struct Moment {
    pub wall_ms: i64,
    pub monotonic: Instant,
}
impl Moment {
    pub fn now() -> Self {
        Self {
            wall_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64,
            monotonic: Instant::now(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Running,
    Waiting,
    Paused,
    Completed,
    Stopped,
    Failed,
    Interrupted,
}
impl Status {
    fn live(self) -> bool {
        matches!(self, Self::Running | Self::Waiting | Self::Paused)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub turn: usize,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: u64,
    pub status: Status,
    pub start_entry: usize,
    pub observed_calls: Vec<String>,
    pub awaiting: Vec<String>,
    #[serde(default)]
    pub followup: bool,
}
#[derive(Clone, Default)]
pub(crate) struct Timeline {
    records: Vec<Record>,
    segment: Option<Instant>,
}
impl Timeline {
    #[cfg(debug_assertions)]
    pub fn fixture(&mut self, status: Status, turn: usize, now: Moment) {
        self.records = vec![Record {
            id: "fixture-run".into(),
            turn,
            started_at_ms: now.wall_ms - 138_000,
            ended_at_ms: (status == Status::Completed).then_some(now.wall_ms),
            duration_ms: 138_000,
            status,
            start_entry: 0,
            observed_calls: vec![],
            awaiting: vec!["agent-1".into()],
            followup: false,
        }];
        self.segment = None;
    }
    pub fn live(&self) -> bool {
        self.records.last().is_some_and(|r| r.status.live())
    }
    pub fn ticking(&self) -> bool {
        self.segment.is_some()
    }
    pub fn paused(&self) -> bool {
        self.records
            .last()
            .is_some_and(|r| r.status == Status::Paused)
    }
    pub fn start(&mut self, turn: usize, entry: usize, now: Moment) {
        if self.live() {
            let record = self.records.last_mut().unwrap();
            record.status = Status::Running;
            record.turn = turn;
        } else {
            let followup = self.records.last().is_some_and(|r| r.turn == turn);
            self.records.push(Record {
                id: shellmate_core::HostId::generate().to_string(),
                turn,
                start_entry: entry,
                started_at_ms: now.wall_ms,
                ended_at_ms: None,
                duration_ms: 0,
                status: Status::Running,
                observed_calls: vec![],
                awaiting: vec![],
                followup,
            });
        }
        self.segment.get_or_insert(now.monotonic);
    }
    pub fn snapshot(&self, now: Moment) -> Vec<Record> {
        let mut records = self.records.clone();
        if let Some(start) = self.segment
            && let Some(record) = records.last_mut()
        {
            record.duration_ms = record.duration_ms.saturating_add(
                now.monotonic
                    .saturating_duration_since(start)
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
        }
        records
    }
    pub fn restore(&mut self, mut records: Vec<Record>) {
        for record in &mut records {
            if record.status.live() {
                record.status = Status::Interrupted;
                record.ended_at_ms = None;
            }
        }
        self.records = records;
        self.segment = None;
    }
    pub fn finish(&mut self, status: Status, now: Moment) -> bool {
        if !self.live() {
            return false;
        }
        self.records = self.snapshot(now);
        self.segment = None;
        let record = self.records.last_mut().unwrap();
        record.status = status;
        record.ended_at_ms =
            (!matches!(status, Status::Paused | Status::Interrupted)).then_some(now.wall_ms);
        true
    }
    pub fn done(&mut self, now: Moment) {
        if !self.live() {
            return;
        }
        if self.records.last().unwrap().awaiting.is_empty() {
            self.finish(Status::Completed, now);
        } else {
            self.records.last_mut().unwrap().status = Status::Waiting;
        }
    }
    pub fn accept_results(&mut self, ids: &[String]) {
        if self.live() {
            self.records
                .last_mut()
                .unwrap()
                .awaiting
                .retain(|id| !ids.contains(id));
        }
    }
    pub fn await_tasks(
        &mut self,
        ids: &[String],
        tasks: &[crate::ai::tasks::TaskInfo],
        pending: &[String],
    ) {
        if !self.live() {
            return;
        }
        let record = self.records.last_mut().unwrap();
        for id in ids {
            let waiting = tasks.iter().any(|t| &t.id == id) || pending.contains(id);
            if waiting && !record.awaiting.contains(id) {
                record.awaiting.push(id.clone());
            }
        }
    }
    pub fn observe(
        &mut self,
        log: &shellmate_ai::Conversation,
        tasks: &[crate::ai::tasks::TaskInfo],
        pending: &[String],
    ) {
        if !self.live() {
            return;
        }
        let record = self.records.last_mut().unwrap();
        record.turn = log.turns().len().saturating_sub(1);
        for entry in log.entries().iter().skip(record.start_entry) {
            let shellmate_ai::Entry::Step(step) = entry else {
                continue;
            };
            if record.observed_calls.contains(&step.call.id) {
                continue;
            }
            record.observed_calls.push(step.call.id.clone());
            let mut needed = vec![];
            if matches!(step.call.name.as_str(), "agent" | "send_agent_message") {
                if let Some(child) = &step.result.subagent
                    && let Some(id) = &step.result.output_id
                    && (child.status == shellmate_ai::SubagentStatus::Running
                        || pending.contains(id))
                {
                    needed.push(id.clone());
                }
            } else if step.call.name == "run_command"
                && step.call.args_json.get("service").is_none_or(|v| v.is_null())
                && step.call.args_json["run_in_background"].as_bool() != Some(true)
            {
                needed.extend(
                    tasks
                        .iter()
                        .filter(|t| t.call_id == step.call.id && t.background)
                        .map(|t| t.id.clone()),
                );
            }
            for id in needed {
                // An in-flight background result promises a later notification,
                // including while saving that completion is still retrying.
                if !record.awaiting.contains(&id) {
                    record.awaiting.push(id);
                }
            }
        }
    }
}

pub(super) fn duration(ms: u64) -> String {
    if ms < 1000 {
        return "不足 1 秒".into();
    }
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!(
            "{} 小时 {} 分 {} 秒",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}
fn local(ms: i64) -> Option<libc::tm> {
    let time = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid and tm is local to this call.
    (!unsafe { libc::localtime_r(&time, &mut tm) }.is_null()).then_some(tm)
}
pub(super) fn timestamp(ms: i64, now_ms: i64, full: bool) -> String {
    let Some(t) = local(ms) else {
        return "时间不可用".into();
    };
    let today = local(now_ms).is_some_and(|n| n.tm_year == t.tm_year && n.tm_yday == t.tm_yday);
    let time = format!("{:02}:{:02}:{:02}", t.tm_hour, t.tm_min, t.tm_sec);
    if full || !today {
        format!(
            "{}-{:02}-{:02} {time}",
            t.tm_year + 1900,
            t.tm_mon + 1,
            t.tm_mday
        )
    } else {
        time
    }
}
pub(super) fn label(
    record: &Record,
    phase: &shellmate_ai::interaction::Phase,
) -> String {
    use shellmate_ai::interaction::Phase;
    let state = match record.status {
        Status::Running => match phase {
            Phase::Waiting(ids) if ids.iter().all(|id| id.starts_with("agent-")) => "等待子代理",
            Phase::Waiting(_) => "等待任务",
            Phase::Approval => "等待批准",
            Phase::Retrying => "等待重试",
            Phase::Compacting => "正在压缩上下文",
            _ => "正在运行",
        },
        Status::Waiting if record.awaiting.iter().all(|id| id.starts_with("agent-")) => {
            "等待子代理"
        }
        Status::Waiting => "等待任务",
        Status::Paused => "已暂停",
        Status::Completed => "",
        Status::Stopped => "已停止",
        Status::Failed => "失败",
        Status::Interrupted => "已中断",
    };
    let elapsed = format!("用时 {}", duration(record.duration_ms));
    if state.is_empty() {
        elapsed
    } else {
        format!("{elapsed} {state}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::tasks::{Kind, Status as TaskStatus, TaskInfo};
    use shellmate_ai::{Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn};
    use std::time::Duration;
    fn at(base: Moment, seconds: u64) -> Moment {
        Moment {
            wall_ms: base.wall_ms + seconds as i64 * 1000,
            monotonic: base.monotonic + Duration::from_secs(seconds),
        }
    }
    fn base() -> Moment {
        Moment {
            wall_ms: 1_700_000_000_000,
            monotonic: Instant::now(),
        }
    }
    fn task(id: &str, call: &str) -> TaskInfo {
        TaskInfo {
            service: None, destination: None, owner: None,
            kind: Kind::Command,
            call_id: call.into(),
            id: id.into(),
            command: "work".into(),
            target: shellmate_ai::Target::Background,
            started: Instant::now(),
            status: TaskStatus::Running,
            output_id: format!("out-{id}"),
            background: true,
        }
    }
    fn step(id: &str, name: &str, args: serde_json::Value) -> Entry {
        Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: id.into(),
                name: name.into(),
                args_json: args,
            },
            result: LoggedResult {
                inline: "result".into(),
                output_id: None,
                is_error: false,
                cleared: false,
                subagent: None,
            },
            display: None,
        })
    }
    #[test]
    fn parallel_wait_and_automatic_reentry_are_one_elapsed_interval() {
        let t = base();
        let mut runs = Timeline::default();
        runs.start(0, 1, t);
        let tasks = [task("agent-1", "a"), task("agent-2", "b")];
        runs.await_tasks(&["agent-1".into(), "agent-2".into()], &tasks, &[]);
        runs.done(at(t, 5));
        let run_id = runs.snapshot(t)[0].id.clone();
        assert_eq!(runs.snapshot(at(t, 60))[0].duration_ms, 60_000);
        assert_eq!(runs.snapshot(t)[0].ended_at_ms, None);
        runs.accept_results(&["agent-1".into()]);
        runs.start(0, 10, at(t, 60));
        runs.done(at(t, 62));
        assert_eq!(runs.snapshot(t)[0].status, Status::Waiting);
        runs.accept_results(&["agent-2".into()]);
        runs.start(0, 20, at(t, 65));
        runs.done(at(t, 70));
        let saved = runs.snapshot(at(t, 100));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, run_id);
        assert_eq!(saved[0].duration_ms, 70_000);
        assert_eq!(saved[0].ended_at_ms, Some(t.wall_ms + 70_000));
    }
    #[test]
    fn pause_excludes_idle_and_clock_adjustment_does_not_change_duration() {
        let t = base();
        let mut runs = Timeline::default();
        runs.start(0, 1, t);
        runs.finish(Status::Paused, at(t, 10));
        assert_eq!(runs.snapshot(at(t, 100))[0].duration_ms, 10_000);
        assert_eq!(runs.snapshot(at(t, 100))[0].ended_at_ms, None);
        runs.start(0, 5, at(t, 100));
        let mut end = at(t, 120);
        end.wall_ms = t.wall_ms - 5_000;
        runs.done(end);
        assert_eq!(runs.snapshot(at(t, 500))[0].duration_ms, 30_000);
        assert_eq!(runs.snapshot(t)[0].ended_at_ms, Some(end.wall_ms));
    }
    #[test]
    fn stopped_round_is_immutable_when_old_background_results_arrive() {
        let t = base();
        let mut runs = Timeline::default();
        runs.start(0, 1, t);
        runs.await_tasks(&["agent-1".into()], &[task("agent-1", "a")], &[]);
        runs.finish(Status::Stopped, at(t, 12));
        runs.accept_results(&["agent-1".into()]);
        runs.done(at(t, 100));
        assert_eq!(runs.snapshot(t)[0].duration_ms, 12_000);
        runs.start(1, 10, at(t, 200));
        runs.await_tasks(&["agent-2".into()], &[task("agent-2", "b")], &[]);
        runs.accept_results(&["agent-1".into()]);
        runs.done(at(t, 210));
        let saved = runs.snapshot(at(t, 215));
        assert_eq!(saved[0].ended_at_ms, Some(t.wall_ms + 12_000));
        assert_eq!(saved[1].status, Status::Waiting);
        assert_eq!(saved[1].duration_ms, 15_000);
    }
    #[test]
    fn reload_preserves_finished_times_and_marks_unfinished_as_interrupted() {
        let t = base();
        let mut runs = Timeline::default();
        runs.start(0, 1, t);
        runs.done(at(t, 3));
        runs.start(1, 5, at(t, 10));
        let json = serde_json::to_string(&runs.snapshot(at(t, 15))).unwrap();
        let mut restored = Timeline::default();
        restored.restore(serde_json::from_str(&json).unwrap());
        let saved = restored.snapshot(at(t, 1000));
        assert_eq!(saved[0].status, Status::Completed);
        assert_eq!(saved[0].duration_ms, 3000);
        assert_eq!(saved[1].status, Status::Interrupted);
        assert_eq!(saved[1].duration_ms, 5000);
        assert_eq!(saved[1].ended_at_ms, None);
        assert!(!restored.ticking());
        assert!(Timeline::default().snapshot(t).is_empty());
    }
    #[test]
    fn background_service_and_completed_any_wait_do_not_keep_round_open() {
        let t = base();
        let mut runs = Timeline::default();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("start server", Default::default()));
        runs.start(0, log.entries().len(), t);
        log.append(step(
            "server",
            "run_command",
            serde_json::json!({"run_in_background":true}),
        ));
        log.append(step(
            "wait",
            "wait_tasks",
            serde_json::json!({"ids":["task-1","task-2"],"mode":"any"}),
        ));
        runs.observe(&log, &[task("task-1", "server")], &[]);
        runs.done(at(t, 5));
        assert_eq!(runs.snapshot(t)[0].status, Status::Completed);
    }
    #[test]
    fn service_without_explicit_async_flag_does_not_hold_round_open() {
        let t = base();
        let mut runs = Timeline::default();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("start server", Default::default()));
        runs.start(0, log.entries().len(), t);
        log.append(step("server", "run_command", serde_json::json!({"service":{}})));
        runs.observe(&log, &[task("1", "server")], &[]);
        runs.done(at(t, 5));
        assert_eq!(runs.snapshot(t)[0].status, Status::Completed);
        assert_eq!(runs.snapshot(at(t, 100))[0].duration_ms, 5000);
    }
    #[test]
    fn child_notification_remains_owned_until_accepted_and_is_not_registered_twice() {
        let t = base();
        let mut runs = Timeline::default();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("delegate", Default::default()));
        runs.start(0, log.entries().len(), t);
        let mut entry = step("child-call", "agent", serde_json::json!({}));
        if let Entry::Step(step) = &mut entry {
            step.result.output_id = Some("agent-1".into());
            step.result.subagent = Some(Box::new(shellmate_ai::SubagentDisplay {
                description: "inspect".into(),
                kind: shellmate_ai::SubagentKind::General,
                model: "fixture".into(),
                reasoning: Default::default(),
                model_overridden: false,
                reasoning_overridden: false,
                status: shellmate_ai::SubagentStatus::Running,
                steps: 0,
                duration_ms: 0,
                usage: Default::default(),
                output_id: Some("agent-1".into()),
                transcript_id: None,
            }));
        }
        log.append(entry);
        let mut task = task("agent-1", "child-call");
        task.kind = Kind::Agent;
        task.status = TaskStatus::Exited(Some(0));
        runs.observe(&log, &[task.clone()], &["agent-1".into()]);
        runs.done(at(t, 20));
        assert_eq!(runs.snapshot(t)[0].status, Status::Waiting);
        runs.start(0, log.entries().len(), at(t, 22));
        runs.accept_results(&["agent-1".into()]);
        runs.observe(&log, &[task], &["agent-1".into()]);
        runs.done(at(t, 25));
        assert_eq!(runs.snapshot(t)[0].status, Status::Completed);
        assert_eq!(runs.snapshot(t)[0].duration_ms, 25_000);
    }
    #[test]
    fn promoted_command_stays_in_its_run_and_old_calls_do_not_move_to_new_run() {
        let t = base();
        let mut runs = Timeline::default();
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("inspect", Default::default()));
        runs.start(0, log.entries().len(), t);
        log.append(step(
            "command",
            "run_command",
            serde_json::json!({"run_in_background":false}),
        ));
        let tasks = [task("task-1", "command")];
        runs.observe(&log, &tasks, &[]);
        runs.done(at(t, 2));
        assert_eq!(runs.snapshot(t)[0].awaiting, ["task-1"]);
        log.push_user(UserTurn::new("additional instruction", Default::default()));
        runs.observe(&log, &tasks, &[]);
        assert_eq!(runs.snapshot(t)[0].turn, 1);
        runs.finish(Status::Stopped, at(t, 3));
        runs.start(1, log.entries().len(), at(t, 10));
        runs.observe(&log, &tasks, &[]);
        runs.done(at(t, 11));
        assert_eq!(runs.snapshot(t)[1].status, Status::Completed);
        assert_eq!(
            label(
                &runs.snapshot(t)[1],
                &shellmate_ai::interaction::Phase::Idle
            ),
            "用时 1 秒"
        );
    }
    #[test]
    fn duration_labels_cover_short_long_and_unknown_end_without_fake_timestamp() {
        assert_eq!(duration(0), "不足 1 秒");
        assert_eq!(duration(138_000), "2 分 18 秒");
        assert_eq!(duration(3_661_000), "1 小时 1 分 1 秒");
        let t = base();
        let mut runs = Timeline::default();
        runs.start(0, 1, t);
        runs.finish(Status::Interrupted, at(t, 3));
        let r = &runs.snapshot(t)[0];
        assert_eq!(
            label(r, &shellmate_ai::interaction::Phase::Idle),
            "用时 3 秒 已中断"
        );
    }
}
