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
use shellmate_core::HostId;
use shellmate_forward::{ForwardKind, ForwardRule, RuleId};
use shellmate_monitor::commands::{ContainerAction, ServiceAction};
use shellmate_monitor::{
    ActionOutcome, ActionReport, ContainerRuntime, Focus, Monitor, MonitorEvent, Options, Probe, Topic,
};
use shellmate_ssh::Connection;

use super::group::{GroupMonitor, PendingAction};
use super::{Panel, Workspace};
use crate::dialogs::confirm::ConfirmDialog;
use crate::monitor::containers::{self, ForwardTarget};
use crate::monitor::model::{ForwardInfo, MonitorData, Page};
use crate::monitor::panel::{MonitorPanel, MonitorPanelEvent};
use crate::monitor::process::{self, Planned, ProcKey, ProcessAction, Run};
use crate::monitor::services;
use crate::runtime;

/// The monitor panel of a tab and what keeps it wired to the workspace.
pub struct MonitorTab {
    pub panel: Entity<MonitorPanel>,
    /// The sub-page the panel shows (what the monitor of the connection asks about follows it).
    pub page: Page,
    pub(super) _subscription: Subscription,
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

    /// Tells every connection whether one of its tabs is showing the monitor page: only then it collects. The
    /// sub-page in front tells which numbers are wanted (container statistics, how often the lists are asked for).
    pub(super) fn sync_monitor_activity(&mut self) {
        let showing = self
            .active
            .and_then(|ix| self.sessions.get(ix))
            .filter(|session| session.panel == Panel::Monitor)
            .and_then(|session| session.ssh().map(|ssh| (ssh.group, session.monitor.as_ref().map(|tab| tab.page))));
        for group in &self.groups {
            if let Some(monitor) = &group.monitor {
                let here = showing.filter(|(id, _)| *id == group.id);
                monitor.monitor.set_active(here.is_some());
                if let Some((_, Some(page))) = here {
                    monitor.monitor.set_focus(match page {
                        Page::Overview => Focus::Overview,
                        Page::Processes => Focus::Processes,
                        Page::Services => Focus::Services,
                        Page::Docker => Focus::Docker,
                    });
                }
            }
        }
    }

    /// Hands the newest to the monitor panels of every connection to `host` (the forwards changed).
    pub(super) fn sync_monitor_host(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let groups: Vec<usize> =
            self.groups.iter().filter(|group| group.key.host() == host).map(|group| group.id).collect();
        for group in groups {
            self.sync_monitor_group(group, cx);
        }
    }

    /// The local forwards of `host`, as the Docker page compares them with the ports of containers.
    fn forward_infos(&self, host: &HostId) -> std::sync::Arc<Vec<ForwardInfo>> {
        let rules = self
            .forward_owner(host)
            .and_then(|owner| self.group(owner))
            .and_then(|group| group.forward.as_ref())
            .map(|forward| forward.rules.as_slice())
            .unwrap_or_default();
        std::sync::Arc::new(
            rules
                .iter()
                .filter(|rule| rule.kind == ForwardKind::Local)
                .filter_map(|rule| {
                    let target = rule.target.as_ref()?;
                    Some(ForwardInfo {
                        listen: rule.listen.to_string(),
                        target_host: target.host.clone(),
                        target_port: target.port,
                        enabled: rule.enabled,
                    })
                })
                .collect(),
        )
    }

    // ---- what the panels show ------------------------------------------------------------------

    /// What the monitor of connection `group_id` knows now.
    fn monitor_data(&self, group_id: usize) -> MonitorData {
        let Some(group) = self.group(group_id) else { return MonitorData::empty() };
        match group.monitor.as_ref() {
            Some(monitor) => MonitorData {
                phase: monitor.monitor.phase(),
                probe: monitor.monitor.probe(),
                snapshot: monitor.monitor.snapshot(),
                services: monitor.monitor.services(),
                docker: monitor.monitor.docker(),
                forwards: self.forward_infos(group.key.host()),
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
        let monitor = self.group(group_id).and_then(|group| group.monitor.as_ref()).map(|m| m.monitor.clone());
        let panel = cx.new(|cx| MonitorPanel::new(host.name.clone(), data, monitor, window, cx));
        let subscription =
            cx.subscribe_in(&panel, window, move |this: &mut Self, _, event: &MonitorPanelEvent, window, cx| {
                this.on_monitor_panel_event(session_id, event, window, cx)
            });
        self.sessions[ix].monitor = Some(MonitorTab { panel, page: Page::Overview, _subscription: subscription });
        true
    }

    /// What the engine of connection `group_id` says.
    fn on_monitor_event(&mut self, group_id: usize, event: MonitorEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            MonitorEvent::Phase(phase) => log::debug!("监控 {phase:?}"),
            MonitorEvent::Probe(_)
            | MonitorEvent::Snapshot(_)
            | MonitorEvent::Services(_)
            | MonitorEvent::Docker(_) => {}
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
        // What the action changed is asked about again at once, whether it worked or not.
        if let (Some(topic), Some(monitor)) = (
            pending.as_ref().and_then(|action| action.topic),
            self.group(group_id).and_then(|group| group.monitor.as_ref()),
        ) {
            monitor.monitor.refresh(topic);
        }
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
                if self.retry_mosh_panel(session_id,window,cx){return;}
                let group =
                    self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.group);
                if let Some(monitor) = group.and_then(|id| self.group(id)).and_then(|group| group.monitor.as_ref()) {
                    monitor.monitor.restart();
                }
            }
            MonitorPanelEvent::PageChanged(page) => {
                if let Some(tab) =
                    self.sessions.iter_mut().find(|s| s.id == session_id).and_then(|s| s.monitor.as_mut())
                {
                    tab.page = *page;
                }
                self.sync_monitor_activity();
            }
            MonitorPanelEvent::Service(action, unit) => self.run_service_action(session_id, *action, unit, window, cx),
            MonitorPanelEvent::Container(action, name) => {
                self.run_container_action(session_id, *action, name, window, cx)
            }
            MonitorPanelEvent::ContainerTerminal(name) => self.open_container_terminal(session_id, name, window, cx),
            MonitorPanelEvent::PruneImages => self.run_prune(session_id, window, cx),
            MonitorPanelEvent::ForwardPort(target) => {
                self.forward_container_port(session_id, target.clone(), window, cx)
            }
        }
    }

    /// The group and monitor of tab `session_id`, and the host it is on.
    fn monitor_of(&self, session_id: usize) -> Option<(usize, Monitor, HostId)> {
        let ssh = self.sessions.iter().find(|s| s.id == session_id)?.ssh()?;
        let monitor = self.group(ssh.group)?.monitor.as_ref()?.monitor.clone();
        Some((ssh.group, monitor, ssh.host.id.clone()))
    }

    /// Asks the question of `planned` (if it has one) and then does it.
    fn run_planned(
        &mut self,
        session_id: usize,
        planned: Planned,
        topic: Option<Topic>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match planned.confirm {
            Some(confirm) => {
                let workspace = cx.entity().downgrade();
                let run = planned.run;
                ConfirmDialog::open(
                    confirm.title,
                    confirm.text,
                    confirm.label,
                    true,
                    move |window, cx| {
                        let run = run.clone();
                        workspace
                            .update(cx, |workspace, cx| workspace.execute_run(session_id, run, topic, window, cx))
                            .ok();
                        Ok(())
                    },
                    window,
                    cx,
                );
            }
            None => self.execute_run(session_id, planned.run, topic, window, cx),
        }
    }

    /// Runs a command on its own channel (the toast tells how it went), or types it in a new tab.
    fn execute_run(
        &mut self,
        session_id: usize,
        run: Run,
        topic: Option<Topic>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match run {
            Run::Exec { line, done } => {
                let Some((group_id, monitor, _)) = self.monitor_of(session_id) else { return };
                let id = monitor.run_action(line);
                if let Some(group) = self.group_mut(group_id).and_then(|group| group.monitor.as_mut()) {
                    group.pending.insert(id, PendingAction { done, topic });
                }
            }
            Run::Terminal { line, enter, title } => self.open_command_tab(session_id, &title, line, enter, window, cx),
        }
    }

    fn toast(&self, notification: Notification, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(notification.title(TOAST_TITLE), cx);
    }

    /// A button of the detail of a service, or its autostart switch.
    fn run_service_action(
        &mut self,
        session_id: usize,
        action: ServiceAction,
        unit: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, monitor, _)) = self.monitor_of(session_id) else { return };
        let (Some(list), Some(probe)) = (monitor.services(), monitor.probe()) else { return };
        let Some(service) = list.get(unit) else {
            return self.toast(Notification::warning(format!("服务 {unit} 已经不存在")), window, cx);
        };
        let Some(planned) = services::plan(action, service, &probe) else { return };
        self.run_planned(session_id, planned, Some(Topic::Services), window, cx);
    }

    /// The probe, the container tool and what is known about the container `name`.
    fn container_context(
        &self,
        session_id: usize,
        name: &str,
    ) -> Option<(Probe, ContainerRuntime, shellmate_monitor::docker::Container)> {
        let (_, monitor, _) = self.monitor_of(session_id)?;
        let probe = monitor.probe()?;
        let runtime = probe.usable_container_runtime()?.clone();
        let container = monitor.docker()?.containers.iter().find(|c| c.name == name)?.clone();
        Some(((*probe).clone(), runtime, container))
    }

    /// A button of a container row (start, restart, stop, remove).
    fn run_container_action(
        &mut self,
        session_id: usize,
        action: ContainerAction,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((probe, runtime, container)) = self.container_context(session_id, name) else {
            return self.toast(Notification::warning(format!("容器 {name} 已经不存在")), window, cx);
        };
        let Some(planned) = containers::plan(action, &container, &runtime, &probe) else {
            return self.toast(Notification::warning("请先停止容器再删除"), window, cx);
        };
        self.run_planned(session_id, planned, Some(Topic::Docker), window, cx);
    }

    /// 终端 on a container.
    fn open_container_terminal(&mut self, session_id: usize, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((probe, runtime, container)) = self.container_context(session_id, name) else { return };
        match containers::exec_plan(&container, &runtime, &probe) {
            Some(run) => self.execute_run(session_id, run, None, window, cx),
            None => self.toast(Notification::warning("容器未运行，无法进入"), window, cx),
        }
    }

    /// 清理: removes the dangling images after the question.
    fn run_prune(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, monitor, _)) = self.monitor_of(session_id) else { return };
        let (Some(probe), Some(view)) = (monitor.probe(), monitor.docker()) else { return };
        let Some(runtime) = probe.usable_container_runtime().cloned() else { return };
        match containers::prune_plan(&view, &runtime, &probe) {
            Some(planned) => self.run_planned(session_id, planned, Some(Topic::Docker), window, cx),
            None => self.toast(Notification::info("没有悬空镜像可清理"), window, cx),
        }
    }

    /// A click on a port chip: a local forward to the container's port, saved with the host's rules and
    /// started; or the note that it exists already.
    fn forward_container_port(
        &mut self,
        session_id: usize,
        target: Result<ForwardTarget, &'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match target {
            Ok(target) => target,
            Err(why) => return self.toast(Notification::warning(why), window, cx),
        };
        let Some((_, _, host)) = self.monitor_of(session_id) else { return };
        let forwards = self.forward_infos(&host);
        if let Some(rule) = containers::existing_forward(&forwards, &target) {
            return self.toast(Notification::info(containers::existing_text(rule)), window, cx);
        }
        let Some(local_port) = shellmate_forward::pick_local_port("127.0.0.1", target.local_port) else {
            return self.toast(Notification::error("本机没有可用的端口"), window, cx);
        };
        let rule = ForwardRule::parse(
            RuleId::generate(),
            &target.name,
            ForwardKind::Local,
            &format!("127.0.0.1:{local_port}"),
            &target.target_text(),
            true,
        );
        match rule {
            Ok(rule) => {
                log::debug!("监控：容器端口转发 本地 127.0.0.1:{local_port} → {}", target.target_text());
                self.add_rule(&host, rule, window, cx);
                self.toast(Notification::success(containers::created_text(local_port, &target)), window, cx);
            }
            Err(err) => self.toast(Notification::error(format!("无法创建转发：{err}")), window, cx),
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
                    group.pending.insert(id, PendingAction { done, topic: None });
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
            // The journal of the shown service and the log under a container, with made-up lines.
            "svc-log" => {
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.demo_service_log(crate::monitor::demo::service_log(), cx));
                }
            }
            "dk-log" => {
                if let Some(panel) = panel {
                    let lines = crate::monitor::demo::container_log();
                    panel.update(cx, |panel, cx| panel.demo_container_log(arg, lines, cx));
                }
            }
            // The overview scrolled to its end (to see that nothing is cut off).
            "scroll-end" => {
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.demo_scroll_end(cx));
                }
            }
            // `<query>|<filter>|<unit>`: the search and the filter of the 服务 page, and the unit shown.
            "services" => {
                let mut parts = arg.splitn(3, '|');
                let query = parts.next().unwrap_or_default().to_string();
                let filter = match parts.next() {
                    Some("run") => crate::monitor::services::ServiceFilter::Running,
                    Some("fail") => crate::monitor::services::ServiceFilter::Failed,
                    Some("off") => crate::monitor::services::ServiceFilter::Stopped,
                    _ => crate::monitor::services::ServiceFilter::All,
                };
                let select = parts.next().filter(|name| !name.is_empty()).map(str::to_string);
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.demo_services(&query, filter, select.as_deref(), window, cx));
                }
            }
            // The 查看服务 / 查看容器 of 需要关注.
            "show-service" => {
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.show_service(arg, window, cx));
                }
            }
            "show-container" => {
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.show_container(arg, cx));
                }
            }
            // `<action>|<name>`: a button of a service (`start`, `stop`, `restart`, `reload`, `enable`, `disable`) or of a
            // container (`c-start`, `c-stop`, `c-restart`, `c-rm`, `c-exec`), `prune`, or a port chip (`port|<name>|<n>`).
            "svc-act" => {
                let (now, arg) = arg.strip_prefix('!').map_or((false, arg), |rest| (true, rest));
                let (kind, unit) = arg.split_once('|').unwrap_or((arg, ""));
                let action = match kind {
                    "start" => ServiceAction::Start,
                    "stop" => ServiceAction::Stop,
                    "restart" => ServiceAction::Restart,
                    "reload" => ServiceAction::Reload,
                    "enable" => ServiceAction::Enable,
                    _ => ServiceAction::Disable,
                };
                if now {
                    let planned = self
                        .monitor_of(session_id)
                        .and_then(|(_, monitor, _)| Some((monitor.services()?, monitor.probe()?)))
                        .and_then(|(list, probe)| services::plan(action, list.get(unit)?, &probe));
                    if let Some(planned) = planned {
                        self.execute_run(session_id, planned.run, Some(Topic::Services), window, cx);
                    }
                } else {
                    self.run_service_action(session_id, action, unit, window, cx);
                }
            }
            "ctr-act" => {
                let (now, arg) = arg.strip_prefix('!').map_or((false, arg), |rest| (true, rest));
                let (kind, name) = arg.split_once('|').unwrap_or((arg, ""));
                match kind {
                    "exec" => self.open_container_terminal(session_id, name, window, cx),
                    "prune" if now => {
                        let planned = self.monitor_of(session_id).and_then(|(_, monitor, _)| {
                            let probe = monitor.probe()?;
                            let runtime = probe.usable_container_runtime()?.clone();
                            containers::prune_plan(&*monitor.docker()?, &runtime, &probe)
                        });
                        if let Some(planned) = planned {
                            self.execute_run(session_id, planned.run, Some(Topic::Docker), window, cx);
                        }
                    }
                    "prune" => self.run_prune(session_id, window, cx),
                    "port" => {
                        // `port|<name>:<index of the port>`
                        let (name, index) = name.rsplit_once(':').unwrap_or((name, "0"));
                        let target = self.container_context(session_id, name).and_then(|(_, _, c)| {
                            c.ports.get(index.parse::<usize>().unwrap_or(0)).map(|p| containers::forward_target(&c, p))
                        });
                        if let Some(target) = target {
                            self.forward_container_port(session_id, target, window, cx);
                        }
                    }
                    other => {
                        let action = match other {
                            "start" => ContainerAction::Start,
                            "stop" => ContainerAction::Stop,
                            "restart" => ContainerAction::Restart,
                            _ => ContainerAction::Remove,
                        };
                        if now {
                            let planned =
                                self.container_context(session_id, name).and_then(|(probe, runtime, container)| {
                                    containers::plan(action, &container, &runtime, &probe)
                                });
                            if let Some(planned) = planned {
                                self.execute_run(session_id, planned.run, Some(Topic::Docker), window, cx);
                            }
                        } else {
                            self.run_container_action(session_id, action, name, window, cx);
                        }
                    }
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
                if let Some(list) = monitor.monitor.services() {
                    let mine: Vec<String> = list
                        .services
                        .iter()
                        .filter(|s| s.name.starts_with("shellmate"))
                        .map(|s| format!("{}:{:?}/{}/{}", s.name, s.active, s.sub, s.memory.is_some()))
                        .collect();
                    log::info!(
                        "监控 [{arg}] 服务 共{} 运行{} 失败{} {mine:?}",
                        list.services.len(),
                        list.running(),
                        list.failed()
                    );
                }
                if let Some(view) = monitor.monitor.docker() {
                    let mine: Vec<String> = view
                        .containers
                        .iter()
                        .filter(|c| c.name.starts_with("shellmate"))
                        .map(|c| format!("{}:{:?}", c.name, c.state))
                        .collect();
                    log::info!("监控 [{arg}] 容器 共{} {mine:?} 悬空{}", view.containers.len(), view.dangling.count);
                }
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
