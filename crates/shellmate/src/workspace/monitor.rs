//! The 监控 panel inside the workspace: one panel per SSH tab, all the tabs of a connection on the one
//! [`Monitor`] of that connection.
//!
//! Every connection ([`ConnGroup`]) has a monitor next to its forwarder and its SFTP link. It collects only
//! while a tab of that connection is the one in front and shows the monitor page (switching between the
//! sub-pages does not matter), and for 10 seconds after that; the collector's channel is the only thing the
//! monitor holds on the connection, and it goes with the last tab.

use gpui::component::WindowExt as _;
use gpui::component::notification::Notification;
use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use shellmate_monitor::{ActionOutcome, ActionReport, Monitor, MonitorEvent, Options};
use shellmate_ssh::Connection;

use super::group::{GroupMonitor, PendingAction};
use super::{Panel, Workspace};
use crate::dialogs::confirm::ConfirmDialog;
use crate::monitor::model::MonitorData;
use crate::monitor::panel::{MonitorPanel, MonitorPanelEvent};
use crate::monitor::process::{self, ProcKey, ProcessAction, Run};
use crate::runtime;

/// The monitor panel of a tab and what keeps it wired to the workspace.
pub struct MonitorTab {
    pub panel: Entity<MonitorPanel>,
    _subscription: Subscription,
}

/// The title of the toasts of the monitor.
const TOAST_TITLE: &str = "监控";

impl Workspace {
    // ---- the engine of a connection ------------------------------------------------------------

    /// Gives a new connection its monitor and starts following what it says.
    pub(super) fn attach_monitor(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = runtime::get(cx) else { return };
        let (events, changes) = async_channel::unbounded();
        let monitor = Monitor::with_options(runtime.handle(), events, Options::default());
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = changes.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_monitor_event(group_id, event, window, cx)).is_err() {
                    break;
                }
            }
        });
        if let Some(group) = self.group_mut(group_id) {
            group.monitor = Some(GroupMonitor { monitor, pending: Default::default(), _task: Some(task) });
        }
    }

    /// The connection came up (the first time, or after a reconnect).
    pub(super) fn monitor_connected(&mut self, group_id: usize, connection: &Connection, cx: &mut Context<Self>) {
        if let Some(monitor) = self.group(group_id).and_then(|group| group.monitor.as_ref()) {
            monitor.monitor.connection_restored(connection);
        }
        self.sync_monitor_group(group_id, cx);
    }

    /// The connection was lost: the numbers stay, grey, until it is back.
    pub(super) fn monitor_lost(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if let Some(monitor) = self.group(group_id).and_then(|group| group.monitor.as_ref()) {
            monitor.monitor.connection_lost();
        }
        self.sync_monitor_group(group_id, cx);
    }

    /// Tells every connection whether one of its tabs is showing the monitor page: only then it collects.
    pub(super) fn sync_monitor_activity(&mut self) {
        let showing = self
            .active
            .and_then(|ix| self.sessions.get(ix))
            .filter(|session| session.panel == Panel::Monitor)
            .and_then(|session| session.ssh())
            .map(|ssh| ssh.group);
        for group in &self.groups {
            if let Some(monitor) = &group.monitor {
                monitor.monitor.set_active(Some(group.id) == showing);
            }
        }
    }

    // ---- what the panels show ------------------------------------------------------------------

    /// What the monitor of connection `group_id` knows now.
    fn monitor_data(&self, group_id: usize) -> MonitorData {
        match self.group(group_id).and_then(|group| group.monitor.as_ref()) {
            Some(group) => MonitorData {
                phase: group.monitor.phase(),
                probe: group.monitor.probe(),
                snapshot: group.monitor.snapshot(),
            },
            None => MonitorData::empty(),
        }
    }

    /// Hands what the monitor knows to every monitor panel of the connection's tabs.
    pub(super) fn sync_monitor_group(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if self.monitor_frozen {
            return;
        }
        let data = self.monitor_data(group_id);
        let panels: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| s.ssh().is_some_and(|ssh| ssh.group == group_id))
            .filter_map(|s| s.monitor.as_ref().map(|tab| tab.panel.clone()))
            .collect();
        for panel in panels {
            panel.update(cx, |panel, cx| panel.set_data(data.clone(), cx));
        }
    }

    /// Creates the monitor panel of the tab at `ix` if it has none yet. `false` if it cannot be made.
    pub(super) fn ensure_monitor_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.sessions[ix].monitor.is_some() {
            return true;
        }
        let Some((session_id, host, group_id)) =
            self.sessions[ix].ssh().map(|ssh| (self.sessions[ix].id, ssh.host.clone(), ssh.group))
        else {
            return false;
        };
        let data = self.monitor_data(group_id);
        let panel = cx.new(|cx| MonitorPanel::new(host.name.clone(), data, window, cx));
        let subscription =
            cx.subscribe_in(&panel, window, move |this: &mut Self, _, event: &MonitorPanelEvent, window, cx| {
                this.on_monitor_panel_event(session_id, event, window, cx)
            });
        self.sessions[ix].monitor = Some(MonitorTab { panel, _subscription: subscription });
        true
    }

    /// What the engine of connection `group_id` says.
    fn on_monitor_event(&mut self, group_id: usize, event: MonitorEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            MonitorEvent::Phase(phase) => log::debug!("监控 {phase:?}"),
            MonitorEvent::Probe(_) | MonitorEvent::Snapshot(_) => {}
            MonitorEvent::Action(report) => self.on_action_report(group_id, report, window, cx),
        }
        self.sync_monitor_group(group_id, cx);
    }

    /// An action finished: the toast shows the command line and how it went.
    fn on_action_report(&mut self, group_id: usize, report: ActionReport, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .group_mut(group_id)
            .and_then(|group| group.monitor.as_mut())
            .and_then(|monitor| monitor.pending.remove(&report.id));
        let line = &report.command;
        let notification = match &report.outcome {
            ActionOutcome::Success { .. } => {
                let done = pending.map_or("已执行", |action| action.done);
                Notification::success(format!("{line} · {done}"))
            }
            ActionOutcome::Failure { code, message } => {
                let code = code.map(|code| format!("（退出码 {code}）")).unwrap_or_default();
                Notification::error(format!("{line} · 失败{code}：{}", first_lines(message)))
            }
            ActionOutcome::Error(why) => Notification::error(format!("{line} · 没有执行：{why}")),
        };
        window.push_notification(notification.title(TOAST_TITLE), cx);
    }

    // ---- what the user does --------------------------------------------------------------------

    /// What the panel of tab `session_id` asked for.
    fn on_monitor_panel_event(
        &mut self,
        session_id: usize,
        event: &MonitorPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            MonitorPanelEvent::Process(action, key) => {
                self.run_process_action(session_id, *action, *key, false, window, cx)
            }
            MonitorPanelEvent::AnalyzeDisk(mount) => match shellmate_monitor::commands::analyze_directory(mount) {
                Ok(line) => self.open_command_tab(session_id, &format!("分析 {mount}"), line, true, window, cx),
                Err(err) => window.push_notification(Notification::error(err.to_string()).title(TOAST_TITLE), cx),
            },
            MonitorPanelEvent::Retry => {
                let group =
                    self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group);
                if let Some(monitor) = group.and_then(|id| self.group(id)).and_then(|group| group.monitor.as_ref()) {
                    monitor.monitor.restart();
                }
            }
        }
    }

    /// A button of the bar under the process table. `confirmed`: the question (for KILL) was answered.
    fn run_process_action(
        &mut self,
        session_id: usize,
        action: ProcessAction,
        key: ProcKey,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group_id) =
            self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group)
        else {
            return;
        };
        let Some(monitor) = self.group(group_id).and_then(|group| group.monitor.as_ref()).map(|m| m.monitor.clone())
        else {
            return;
        };
        let (Some(snapshot), Some(probe)) = (monitor.snapshot(), monitor.probe()) else { return };
        let Some(process) = process::resolve(&snapshot, key) else {
            window.push_notification(
                Notification::warning(format!("进程 {} 已经不存在", key.pid)).title(TOAST_TITLE),
                cx,
            );
            return;
        };
        let Some(planned) = process::plan(action, process, &probe) else { return };
        if let (Some(confirm), false) = (planned.confirm.clone(), confirmed) {
            let workspace = cx.entity().downgrade();
            ConfirmDialog::open(
                confirm.title,
                confirm.text,
                confirm.label,
                true,
                move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.run_process_action(session_id, action, key, true, window, cx)
                        })
                        .ok();
                    Ok(())
                },
                window,
                cx,
            );
            return;
        }
        match planned.run {
            Run::Exec { line, done } => {
                let id = monitor.run_action(line);
                if let Some(group) = self.group_mut(group_id).and_then(|group| group.monitor.as_mut()) {
                    group.pending.insert(id, PendingAction { done });
                }
            }
            Run::Terminal { line, enter, title } => self.open_command_tab(session_id, &title, line, enter, window, cx),
        }
    }

    // ---- review hooks ----------------------------------------------------------------------------

    /// Debug review hook: one step of a monitor scenario (see `demo::run`, `monitor:`).
    #[cfg(debug_assertions)]
    pub(crate) fn demo_monitor_step(&mut self, step: &str, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        use crate::monitor::model::Page;
        use crate::monitor::process::SortKey;

        if step == "show" {
            return self.show_panel(Panel::Monitor, window, cx);
        }
        if step == "terminal" {
            return self.show_panel(Panel::Terminal, window, cx);
        }
        let Some(ix) = self.active else { return };
        let Some(session) = self.sessions.get(ix) else { return };
        let session_id = session.id;
        let panel = session.monitor.as_ref().map(|tab| tab.panel.clone());
        let group_id = session.ssh().map(|ssh| ssh.group);
        match step {
            // Made-up data instead of the server's (and the server's numbers stop being shown).
            "fake" => {
                self.monitor_frozen = true;
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.set_data(crate::monitor::demo::data(arg), cx));
                }
            }
            "page" => {
                let page = match arg {
                    "proc" => Page::Processes,
                    "svc" => Page::Services,
                    "dk" => Page::Docker,
                    _ => Page::Overview,
                };
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.set_page(page, cx));
                }
            }
            // `query|sort|tree`
            "table" => {
                let mut parts = arg.splitn(3, '|');
                let query = parts.next().unwrap_or_default().to_string();
                let sort = match parts.next() {
                    Some("mem") => SortKey::Mem,
                    Some("pid") => SortKey::Pid,
                    _ => SortKey::Cpu,
                };
                let tree = parts.next() == Some("tree");
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.demo_table(&query, sort, tree, window, cx));
                }
            }
            // Selects the process with this command line (or this number).
            "select" => {
                let pid = arg.parse::<u32>().ok().or_else(|| self.demo_pid(group_id, arg));
                if let (Some(panel), Some(pid)) = (panel, pid) {
                    panel.update(cx, |panel, cx| panel.show_process(pid, window, cx));
                }
            }
            // `<action>|<command or pid>`: what pressing the button does; `kill-now` skips the question.
            "act" | "act-now" => {
                let (kind, target) = arg.split_once('|').unwrap_or((arg, ""));
                let action = match kind {
                    "term" => ProcessAction::Term,
                    "hup" => ProcessAction::Hup,
                    "nice" => ProcessAction::Nice,
                    "strace" => ProcessAction::Strace,
                    _ => ProcessAction::Kill,
                };
                let key = self.demo_pid(group_id, target).and_then(|pid| {
                    let snapshot = self.group(group_id?)?.monitor.as_ref()?.monitor.snapshot()?;
                    snapshot.process(pid).map(ProcKey::of)
                });
                if let Some(key) = key {
                    self.run_process_action(session_id, action, key, step == "act-now", window, cx);
                } else {
                    log::warn!("监控演示：没有找到进程 {target:?}");
                }
            }
            // 分析目录 on a mount point.
            "analyze" => {
                self.on_monitor_panel_event(session_id, &MonitorPanelEvent::AnalyzeDisk(arg.to_string()), window, cx)
            }
            "retry" => self.on_monitor_panel_event(session_id, &MonitorPanelEvent::Retry, window, cx),
            "close-now" => self.close_session(session_id, window, cx),
            "dismiss" => window.close_all_dialogs(cx),
            // Logs how things stand (no process names or command lines).
            "log" => {
                let Some(monitor) = group_id.and_then(|id| self.group(id)).and_then(|g| g.monitor.as_ref()) else {
                    return;
                };
                let (phase, snapshot) = (monitor.monitor.phase(), monitor.monitor.snapshot());
                match snapshot {
                    Some(s) => {
                        let cpu = s.cpu.as_ref().and_then(|c| c.percent);
                        let top: Vec<String> = s
                            .top_by_cpu(3)
                            .iter()
                            .map(|p| format!("{}:{:?}", p.pid, p.cpu_percent.map(|v| v.round())))
                            .collect();
                        log::info!(
                            "监控 [{arg}] {phase:?} 核{} CPU{cpu:?} 内存{:?} 进程{} 僵尸{} 磁盘{} 网络{:?} 前三{top:?} 标签{}",
                            s.host.cores,
                            s.memory.map(|m| m.used_percent().round()),
                            s.processes.len(),
                            s.zombies,
                            s.disks.len(),
                            s.network.as_ref().map(|n| (n.rx_per_sec.map(f64::round), n.tx_per_sec.map(f64::round))),
                            self.sessions.len(),
                        );
                    }
                    None => log::info!("监控 [{arg}] {phase:?} 还没有数据，标签{}", self.sessions.len()),
                }
            }
            other => log::warn!("unknown monitor step {other:?}"),
        }
    }

    /// The pid of the process whose command line is `target` (or the number `target`), in the newest snapshot.
    #[cfg(debug_assertions)]
    fn demo_pid(&self, group_id: Option<usize>, target: &str) -> Option<u32> {
        let snapshot = self.group(group_id?)?.monitor.as_ref()?.monitor.snapshot()?;
        target
            .parse::<u32>()
            .ok()
            .filter(|pid| snapshot.process(*pid).is_some())
            .or_else(|| snapshot.processes.iter().find(|p| p.command == target).map(|p| p.pid))
    }
}

/// The first two lines of what a command said, so that a toast stays short.
fn first_lines(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty()).take(2).collect::<Vec<_>>().join(" ");
    if lines.chars().count() > 160 {
        lines = lines.chars().take(160).collect::<String>() + "…";
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_keeps_the_first_lines_of_what_a_command_said() {
        assert_eq!(
            first_lines("bash: line 1: kill: (1) - Operation not permitted\n"),
            "bash: line 1: kill: (1) - Operation not permitted"
        );
        assert_eq!(first_lines("a\n\n  b  \nc\nd"), "a b");
        assert_eq!(first_lines(""), "");
        assert!(first_lines(&"x".repeat(500)).ends_with('…'));
    }
}
