//! What the 服务 page says, without a window: the list with its filter and search, the detail of the selected
//! service with its buttons, how each button would run (directly, with `sudo -n`, or in a terminal tab) and the
//! question before a stop, and where the log of the service comes from.

use shellmate_monitor::commands::{self, BuildError, Command, Plan, Privilege, ServiceAction};
use shellmate_monitor::services::{ActiveState, Autostart, Service, ServiceList};
use shellmate_monitor::{JournalAccess, Probe, Sudo};

use super::format;
use super::model::Tone;
use super::process::{Confirm, Planned, Run, Via};

/// The filter above the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceFilter {
    /// 全部
    All,
    /// 运行中
    Running,
    /// 失败
    Failed,
    /// 未运行
    Stopped,
}

impl ServiceFilter {
    /// The segments, in the order of the prototype.
    pub const ALL: [ServiceFilter; 4] =
        [ServiceFilter::All, ServiceFilter::Running, ServiceFilter::Failed, ServiceFilter::Stopped];

    /// The label of the segment.
    pub fn label(self) -> &'static str {
        match self {
            ServiceFilter::All => "全部",
            ServiceFilter::Running => "运行中",
            ServiceFilter::Failed => "失败",
            ServiceFilter::Stopped => "未运行",
        }
    }

    fn accepts(self, service: &Service) -> bool {
        match self {
            ServiceFilter::All => true,
            ServiceFilter::Running => service.active.is_running(),
            ServiceFilter::Failed => service.is_failed(),
            ServiceFilter::Stopped => !service.active.is_running() && !service.is_failed(),
        }
    }
}

/// A row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRow {
    pub key: String,
    /// `nginx.service`
    pub name: String,
    /// What it is for.
    pub description: String,
    /// The word in the badge (`running`, `failed`, `dead`, `exited`).
    pub state: String,
    /// The color of the badge.
    pub tone: Tone,
    /// `自启`, `—` or `屏蔽`.
    pub autostart: &'static str,
    /// Whether the unit starts at boot (drawn darker).
    pub autostart_on: bool,
    /// `38 MB`, or `—`.
    pub memory: String,
}

/// The color of a state badge: green for what runs, red for what failed, grey for the rest.
pub fn state_tone(service: &Service) -> Tone {
    match service.active {
        ActiveState::Active | ActiveState::Reloading | ActiveState::Activating => Tone::Ok,
        ActiveState::Failed => Tone::Err,
        ActiveState::LastRunFailed => Tone::Warn,
        _ => Tone::Off,
    }
}

/// The rows of `list` for a filter and a search text (name or description, any case), in the order of the
/// list: failed first, then by name.
pub fn rows(list: &ServiceList, query: &str, filter: ServiceFilter) -> Vec<ServiceRow> {
    filtered_rows(list, query, filter, false)
}

pub fn filtered_rows(list: &ServiceList, query: &str, filter: ServiceFilter, show_apple: bool) -> Vec<ServiceRow> {
    let query = query.trim().to_lowercase();
    list.services
        .iter()
        .filter(|service| show_apple || !service.apple())
        .filter(|service| filter.accepts(service))
        .filter(|service| {
            query.is_empty()
                || service.name.to_lowercase().contains(&query)
                || service.description.to_lowercase().contains(&query)
                || service.key().to_lowercase().contains(&query)
        })
        .map(|service| ServiceRow {
            key: service.key().into(),
            name: display_name(service),
            description: service.launchd.as_ref().filter(|job| job.formula().is_some()).map_or_else(|| service.description.clone(), |job| job.label().into()),
            state: if service.sub.is_empty() { state_word(&service.active) } else { service.sub.clone() },
            tone: state_tone(service),
            autostart: match service.autostart {
                Autostart::Enabled if service.launchd.is_some() => "开机自启",
                Autostart::Disabled if service.launchd.is_some() => "已禁用",
                Autostart::Unknown if service.launchd.is_some() => "状态未知",
                Autostart::Manual => "手动",
                Autostart::Enabled => "自启",
                Autostart::Masked => "屏蔽",
                _ => "—",
            },
            autostart_on: service.autostart == Autostart::Enabled,
            memory: service.memory.map_or_else(|| "—".into(), format::bytes),
        })
        .collect()
}

fn display_name(service: &Service) -> String {
    if service.launchd.as_ref().is_some_and(|job| job.formula().is_some()) { format!("{}（Homebrew）", service.name) } else { service.name.clone() }
}

fn state_word(active: &ActiveState) -> String {
    match active {
        ActiveState::Active => "active".into(),
        ActiveState::Reloading => "reloading".into(),
        ActiveState::Activating => "activating".into(),
        ActiveState::Deactivating => "deactivating".into(),
        ActiveState::Inactive => "inactive".into(),
        ActiveState::LastRunFailed => "上次运行失败".into(),
        ActiveState::Failed => "failed".into(),
        ActiveState::Other(other) => other.clone(),
    }
}

/// The service that is shown: the one that is selected if it is still there, else the first of the rows
/// (the prototype always has one selected).
pub fn shown<'a>(list: &'a ServiceList, rows: &[ServiceRow], selected: Option<&str>) -> Option<&'a Service> {
    selected
        .and_then(|name| list.get(name))
        .filter(|service| rows.iter().any(|row| row.key == service.key()))
        .or_else(|| rows.first().and_then(|row| list.get(&row.key)))
}

// ---- the detail --------------------------------------------------------------------------------

/// How long a service has been in its state, and since when.
pub fn since_text(service: &Service, uptime_secs: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(text) = &service.since_text {
        parts.push(format!("自 {text}"));
    }
    if let (Some(micros), Some(uptime)) = (service.since_monotonic_us, uptime_secs) {
        let elapsed = uptime - micros as f64 / 1e6;
        if elapsed >= 0.0 {
            parts.push(format!("已 {}", format::uptime(elapsed)));
        }
    }
    parts.join(" · ")
}

/// CPU time as the detail writes it: `340 ms`, `12.3 s`, `3 分 21 秒`, `2 小时 5 分`.
pub fn cpu_time(nsec: u64) -> String {
    let ms = nsec / 1_000_000;
    let secs = nsec as f64 / 1e9;
    if ms < 1000 {
        format!("{ms} ms")
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else if secs < 3600.0 {
        format!("{} 分 {} 秒", secs as u64 / 60, secs as u64 % 60)
    } else {
        format!("{} 小时 {} 分", secs as u64 / 3600, secs as u64 % 3600 / 60)
    }
}

/// A button of the detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceButton {
    /// What it does.
    pub action: ServiceAction,
    /// `启动`, `重启`, … (with ` · 终端` when it runs in a terminal).
    pub label: String,
    /// Whether it can be pressed.
    pub enabled: bool,
    /// How it will run.
    pub via: Via,
    /// What a hover says: how it runs, or why it cannot.
    pub tooltip: Option<String>,
}

/// The switch 开机自启.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutostartSwitch {
    /// On: the unit starts at boot.
    pub on: bool,
    /// Whether it can be switched.
    pub enabled: bool,
    /// `开机自启 enable`, `开机自启 disable`; what the unit allows when it cannot be switched.
    pub label: String,
    /// What a hover says.
    pub tooltip: Option<String>,
    /// How switching runs.
    pub via: Via,
}

/// The detail of the selected service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub key: String,
    pub plist: Option<String>,
    pub log: Option<String>,
    /// `php8.2-fpm.service`
    pub name: String,
    /// What it is for.
    pub description: String,
    /// `failed`, or `active (running)`.
    pub state: String,
    /// The color of the badge.
    pub tone: Tone,
    /// `自 2026-11-16 01:09:15 UTC · 已 3 小时`
    pub since: String,
    /// The little cards.
    pub main_pid: String,
    /// `38 MB`, or `—`.
    pub memory: String,
    /// `12.3 s`, or `—`.
    pub cpu: String,
    /// Why a unit failed, when it did: the last line of its journal, or what systemd says.
    pub failure: Option<String>,
    /// 启动 重启 重载 停止.
    pub buttons: Vec<ServiceButton>,
    /// The autostart switch.
    pub autostart: AutostartSwitch,
}

/// What a failed unit says about its failure: the last error line of the journal when there was one, else
/// the exit status and what systemd calls it (`退出码 1（exit-code）`).
pub fn failure_text(service: &Service) -> Option<String> {
    if service.active == ActiveState::LastRunFailed {
        return service.exit_status.map(|code| format!("上次退出码 {code}"));
    }
    if !service.is_failed() {
        return None;
    }
    if let Some(line) = &service.last_error {
        return Some(line.clone());
    }
    Some(match (service.exit_status, &service.result) {
        (Some(code), Some(result)) if code != 0 => format!("退出码 {code}（{result}）"),
        (Some(code), None) if code != 0 => format!("退出码 {code}"),
        (_, Some(result)) => format!("systemd 报告：{result}"),
        _ => "systemd 报告该服务失败".to_string(),
    })
}

const TERMINAL_NOTE: &str = "需要 sudo 密码，应用不会收集：在新会话里打好命令，由你回车并输入密码";

fn command_of(action: ServiceAction, service: &Service) -> Result<Command, BuildError> {
    if let Some(job) = &service.launchd { return job.command(action); }
    commands::systemctl(action, &service.name)
}

/// How the command of `action` gets to run, and the line that will be run. Managing services needs root.
fn route(action: ServiceAction, service: &Service, probe: &Probe) -> Result<(Via, String, bool), BuildError> {
    let command = command_of(action, service)?;
    if let Some(job) = &service.launchd {
        return Ok(if job.system() { (Via::Terminal, command.prefixed(&["sudo"]).line(), false) } else { (Via::Direct, command.line(), true) });
    }
    Ok(match commands::plan(&command, Privilege::Root, probe) {
        Plan::Exec { line } if line.starts_with("sudo -n ") => (Via::Sudo, line, true),
        Plan::Exec { line } => (Via::Direct, line, true),
        Plan::Terminal { line } => (Via::Terminal, line, false),
    })
}

/// The label of the button of an action.
pub fn action_label(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Start => "启动",
        ServiceAction::Restart => "重启",
        ServiceAction::Reload => "重载",
        ServiceAction::Stop => "停止",
        ServiceAction::Enable => "开启自启",
        ServiceAction::Disable => "关闭自启",
    }
}

/// What the toast says after the command line when it worked.
fn done_text(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Start => "已启动",
        ServiceAction::Restart => "已重启",
        ServiceAction::Reload => "已重载",
        ServiceAction::Stop => "已停止",
        ServiceAction::Enable => "已设为开机自启",
        ServiceAction::Disable => "已取消开机自启",
    }
}

/// Why a button cannot be pressed for this service, if it cannot.
fn unavailable(action: ServiceAction, service: &Service) -> Option<&'static str> {
    if service.autostart == Autostart::Masked && !matches!(action, ServiceAction::Stop) {
        return Some("这个服务被屏蔽（masked），不能启动：需要先 unmask");
    }
    match action {
        ServiceAction::Reload | ServiceAction::Stop if !service.active.is_running() => Some("服务没有在运行"),
        // launchd's kickstart of a running job does nothing; say so instead of offering it.
        ServiceAction::Start if service.launchd.is_some() && service.active.is_running() => Some("服务已在运行"),
        _ => None,
    }
}

fn button(action: ServiceAction, service: &Service, probe: Option<&Probe>) -> ServiceButton {
    let label = if keep_alive_stop(action, service) { KEEP_ALIVE_STOP } else { action_label(action) };
    let off = |why: &str| ServiceButton {
        action,
        label: label.into(),
        enabled: false,
        via: Via::Direct,
        tooltip: Some(why.into()),
    };
    let Some(probe) = probe else { return off("还没有读到这台主机的信息") };
    if let Some(why) = unavailable(action, service) {
        return off(why);
    }
    match route(action, service, probe) {
        Err(_) => off("这个服务名不能用于操作"),
        Ok((via, line, _)) => ServiceButton {
            action,
            label: if via == Via::Terminal {
                format!("{label} · 终端")
            } else {
                label.to_string()
            },
            enabled: true,
            via,
            tooltip: match via {
                Via::Sudo => Some(format!("以 sudo -n 执行：{line}")),
                Via::Terminal => Some(terminal_note(service, &line)),
                Via::Direct => None,
            },
        },
    }
}

const KEEP_ALIVE_STOP: &str = "停止（本次登录内不再自动启动）";

fn keep_alive_stop(action: ServiceAction, service: &Service) -> bool {
    action == ServiceAction::Stop && service.launchd.as_ref().is_some_and(|job| job.keep_alive == Some(true))
}

fn terminal_note(service: &Service, line: &str) -> String {
    if service.launchd.is_some() { format!("在当前本机终端填入命令，由你回车：{line}") } else { format!("{TERMINAL_NOTE}：{line}") }
}

/// The detail of `service`. `uptime_secs`: the server's uptime now (for how long it has been in its state).
pub fn detail(service: &Service, uptime_secs: Option<f64>, probe: Option<&Probe>) -> Detail {
    let state = if service.launchd.is_some() { service.sub.clone() } else if service.sub.is_empty() || state_word(&service.active) == service.sub {
        state_word(&service.active)
    } else {
        format!("{} ({})", state_word(&service.active), service.sub)
    };
    let buttons = [ServiceAction::Start, ServiceAction::Restart, ServiceAction::Reload, ServiceAction::Stop]
        .into_iter()
        .filter(|action| service.launchd.is_none() || *action != ServiceAction::Reload)
        .map(|action| button(action, service, probe))
        .collect();
    let (on, toggle) = match service.autostart {
        Autostart::Enabled => (true, Some(ServiceAction::Disable)),
        Autostart::Disabled | Autostart::Manual => (false, Some(ServiceAction::Enable)),
        _ => (false, None),
    };
    let autostart = match (toggle, probe) {
        (Some(action), Some(probe)) => {
            let (via, line) =
                route(action, service, probe).map(|(via, line, _)| (via, line)).unwrap_or((Via::Direct, String::new()));
            AutostartSwitch {
                on,
                enabled: !line.is_empty(),
                label: if service.launchd.is_some() { if on { "取消开机自启" } else { "设为开机自启" }.into() } else { format!("开机自启 {}", action.verb()) },
                tooltip: match via {
                    Via::Sudo => Some(format!("以 sudo -n 执行：{line}")),
                    Via::Terminal => Some(terminal_note(service, &line)),
                    Via::Direct => None,
                },
                via,
            }
        }
        _ => AutostartSwitch {
            on,
            enabled: false,
            label: match service.autostart {
                Autostart::Masked => "开机自启：已屏蔽 masked".into(),
                Autostart::Fixed => "开机自启：由其他单元带起（static）".into(),
                _ => "开机自启".into(),
            },
            tooltip: Some("这个单元没有可以开关的安装配置".into()),
            via: Via::Direct,
        },
    };
    Detail {
        name: display_name(service),
        key: service.key().into(),
        plist: service.launchd.as_ref().and_then(|job| job.plist.clone()),
        log: service.launchd.as_ref().and_then(|job| job.log.clone()),
        description: service.description.clone(),
        state,
        tone: state_tone(service),
        since: since_text(service, uptime_secs),
        main_pid: service.main_pid.map_or_else(|| "—".into(), |pid| pid.to_string()),
        memory: service.memory.map_or_else(|| "—".into(), format::bytes),
        cpu: service.cpu_nsec.map_or_else(|| "—".into(), cpu_time),
        failure: failure_text(service),
        buttons,
        autostart,
    }
}

/// What pressing `action` does to `service` on a server that was probed as `probe`.
pub fn plan(action: ServiceAction, service: &Service, probe: &Probe) -> Option<Planned> {
    if unavailable(action, service).is_some() {
        return None;
    }
    let (via, line, enter) = route(action, service, probe).ok()?;
    let run = if let Some(job) = &service.launchd {
        Run::LocalService { key: service.key().into(), action, words: job.command(action).ok()?, done: done_text(action) }
    } else { match via {
        Via::Terminal => Run::Terminal { line, enter, title: format!("{} {}", action.verb(), service.name) },
        _ => Run::Exec { line, done: done_text(action) },
    } };
    let confirm = (action == ServiceAction::Stop).then(|| Confirm {
        title: format!("停止 {}？", service.name),
        text: if keep_alive_stop(action, service) { format!("{}\n将卸载此服务，本次登录内不再自动启动。可用「启动」重新加载。", service.key()) } else { format!("{}\n依赖它的服务和正在处理的请求会立即中断。", service.description) },
        label: if keep_alive_stop(action, service) { KEEP_ALIVE_STOP } else { "停止服务" },
    });
    Some(Planned { run, confirm })
}

// ---- the log -----------------------------------------------------------------------------------

/// What the journal pane does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// Follow an explicitly selected local job's file.
    File { key: String, path: String },
    /// Follow this command line.
    Follow(String),
    /// The journal cannot be read: the pane says so.
    Denied(&'static str),
}

/// The text of the pane for a user who cannot read the journal.
pub const JOURNAL_DENIED: &str = "没有权限读取日志（需要 adm 或 systemd-journal 组，或免密 sudo）";

/// Where the log of `unit` comes from: `journalctl -u UNIT -n 50 -f -o short-iso --no-pager` as the login
/// user, with `sudo -n` when only that can read the journal, or nowhere.
pub fn log_source(unit: &str, probe: &Probe) -> LogSource {
    let Ok(command) = commands::journal_follow(unit, 50) else {
        return LogSource::Denied("这个服务名不能用于查看日志");
    };
    match probe.journal_access() {
        JournalAccess::Direct => LogSource::Follow(command.line_merged()),
        JournalAccess::Sudo => LogSource::Follow(command.sudo_n().line_merged()),
        JournalAccess::None => LogSource::Denied(JOURNAL_DENIED),
    }
}

/// What the right of the log header says: the command as the user would type it.
pub fn log_hint(probe: &Probe) -> &'static str {
    if probe.journal_access() == JournalAccess::Sudo || probe.sudo == Sudo::Root {
        "sudo journalctl -u -f"
    } else {
        "journalctl -u -f"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_monitor::services::{merge, parse_list_units, parse_show, parse_unit_files};

    fn probe(sudo: Sudo, groups: &[&str]) -> Probe {
        Probe {
            sudo,
            uid: Some(1000),
            user: "deploy".into(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            ..Probe::unknown()
        }
    }

    fn list() -> ServiceList {
        let rows = parse_list_units(
            "php8.2-fpm.service loaded failed failed The PHP 8.2 FastCGI Process Manager\nnginx.service loaded active running A high performance web server\ncron.service loaded active running Regular background program processing daemon\nsnapd.service loaded inactive dead Snap Daemon\n",
        );
        let files = parse_unit_files(
            "php8.2-fpm.service enabled\nnginx.service enabled\ncron.service enabled\nsnapd.service disabled\n",
        );
        let props = parse_show(
            "Id=nginx.service\nMainPID=1185\nMemoryCurrent=40000000\nCPUUsageNSec=201000000000\nStateChangeTimestamp=Mon 2026-11-16 01:09:15 UTC\nStateChangeTimestampMonotonic=100000000\n\nId=php8.2-fpm.service\nResult=exit-code\nExecMainStatus=78\n",
        );
        ServiceList { services: merge(&rows, &files, &props) }
    }

    #[test]
    fn the_filter_and_the_search_choose_the_rows() {
        let list = list();
        let names = |rows: Vec<ServiceRow>| rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(
            names(rows(&list, "", ServiceFilter::All)),
            ["php8.2-fpm.service", "cron.service", "nginx.service", "snapd.service"],
            "failed first"
        );
        assert_eq!(names(rows(&list, "", ServiceFilter::Running)), ["cron.service", "nginx.service"]);
        assert_eq!(names(rows(&list, "", ServiceFilter::Failed)), ["php8.2-fpm.service"]);
        assert_eq!(names(rows(&list, "", ServiceFilter::Stopped)), ["snapd.service"]);
        assert_eq!(names(rows(&list, "  REVERSE ", ServiceFilter::All)), Vec::<String>::new());
        assert_eq!(
            names(rows(&list, "web server", ServiceFilter::All)),
            ["nginx.service"],
            "the description is searched too, in any case"
        );
        assert_eq!(
            names(rows(&list, "PHP", ServiceFilter::Running)),
            Vec::<String>::new(),
            "the filter and the search both apply"
        );
    }

    #[test]
    fn a_row_says_state_autostart_and_memory() {
        let list = list();
        let all = rows(&list, "", ServiceFilter::All);
        let php = &all[0];
        assert_eq!(
            (php.state.as_str(), php.tone, php.autostart, php.memory.as_str()),
            ("failed", Tone::Err, "自启", "—")
        );
        let nginx = all.iter().find(|r| r.name == "nginx.service").unwrap();
        assert_eq!((nginx.state.as_str(), nginx.tone, nginx.memory.as_str()), ("running", Tone::Ok, "38 MB"));
        let snapd = all.iter().find(|r| r.name == "snapd.service").unwrap();
        assert_eq!(
            (snapd.state.as_str(), snapd.tone, snapd.autostart, snapd.autostart_on),
            ("dead", Tone::Off, "—", false)
        );
    }

    #[test]
    fn the_shown_service_is_the_selected_one_or_the_first() {
        let list = list();
        let all = rows(&list, "", ServiceFilter::All);
        assert_eq!(shown(&list, &all, Some("cron.service")).unwrap().name, "cron.service");
        assert_eq!(
            shown(&list, &all, Some("gone.service")).unwrap().name,
            "php8.2-fpm.service",
            "a selection that is gone: the first"
        );
        assert_eq!(shown(&list, &all, None).unwrap().name, "php8.2-fpm.service");
        let running = rows(&list, "", ServiceFilter::Running);
        assert_eq!(
            shown(&list, &running, Some("php8.2-fpm.service")).unwrap().name,
            "cron.service",
            "one the filter hides is not shown"
        );
        assert!(shown(&list, &[], None).is_none());
    }

    #[test]
    fn the_detail_of_a_running_and_a_failed_service() {
        let list = list();
        let p = probe(Sudo::Passwordless, &[]);
        let nginx = detail(list.get("nginx.service").unwrap(), Some(4000.0), Some(&p));
        assert_eq!(
            (nginx.state.as_str(), nginx.tone, nginx.main_pid.as_str(), nginx.memory.as_str(), nginx.cpu.as_str()),
            ("active (running)", Tone::Ok, "1185", "38 MB", "3 分 21 秒")
        );
        assert_eq!(nginx.since, "自 2026-11-16 01:09:15 UTC · 已 1 小时 5 分", "{}", nginx.since);
        assert!(nginx.failure.is_none());
        assert!(nginx.buttons.iter().all(|b| b.enabled && b.via == Via::Sudo));
        assert_eq!(nginx.buttons[3].tooltip.as_deref(), Some("以 sudo -n 执行：sudo -n systemctl stop nginx.service"));
        assert!(nginx.autostart.on && nginx.autostart.enabled);
        assert_eq!(nginx.autostart.label, "开机自启 disable");
        let php = detail(list.get("php8.2-fpm.service").unwrap(), Some(4000.0), Some(&p));
        assert_eq!((php.state.as_str(), php.tone), ("failed", Tone::Err), "failed (failed) is just failed");
        assert_eq!(php.failure.as_deref(), Some("退出码 78（exit-code）"), "no journal line: what systemd says");
        assert!(!php.buttons[2].enabled && !php.buttons[3].enabled, "reload and stop need a running service");
        assert_eq!(php.buttons[2].tooltip.as_deref(), Some("服务没有在运行"));
        assert!(php.buttons[0].enabled, "a failed service can be started again");
        let mut with_line = list.get("php8.2-fpm.service").unwrap().clone();
        with_line.last_error = Some("ERROR: [pool www] cannot get uid".into());
        assert_eq!(detail(&with_line, None, Some(&p)).failure.as_deref(), Some("ERROR: [pool www] cannot get uid"));
    }

    #[test]
    fn buttons_follow_what_the_server_allows() {
        let list = list();
        let nginx = list.get("nginx.service").unwrap();
        let direct = detail(nginx, None, Some(&probe(Sudo::Root, &[])));
        assert!(
            direct
                .buttons
                .iter()
                .all(|b| b.enabled && b.via == Via::Direct && b.tooltip.is_none() && !b.label.contains("终端"))
        );
        let ask = detail(nginx, None, Some(&probe(Sudo::NeedsPassword, &[])));
        assert!(ask.buttons.iter().all(|b| b.via == Via::Terminal && b.label.ends_with("· 终端")));
        assert!(ask.buttons[0].tooltip.as_deref().unwrap().contains("sudo systemctl start nginx.service"));
        assert!(ask.autostart.via == Via::Terminal && ask.autostart.enabled);
        let unknown = detail(nginx, None, None);
        assert!(unknown.buttons.iter().all(|b| !b.enabled), "before the probe nothing can be said");
        assert!(!unknown.autostart.enabled);
    }

    #[test]
    fn a_unit_that_cannot_be_enabled_or_started() {
        let rows = parse_list_units(
            "auditd.service masked inactive dead auditd.service\nsystemd-journald.service loaded active running Journal Service\n",
        );
        let files = parse_unit_files("auditd.service masked\nsystemd-journald.service static\n");
        let list = ServiceList { services: merge(&rows, &files, &[]) };
        let p = probe(Sudo::Passwordless, &[]);
        let masked = detail(list.get("auditd.service").unwrap(), None, Some(&p));
        assert!(!masked.buttons[0].enabled && !masked.buttons[1].enabled);
        assert!(masked.buttons[0].tooltip.as_deref().unwrap().contains("masked"));
        assert!(!masked.autostart.enabled && masked.autostart.label.contains("masked"));
        let fixed = detail(list.get("systemd-journald.service").unwrap(), None, Some(&p));
        assert!(!fixed.autostart.enabled && fixed.autostart.label.contains("static"), "{}", fixed.autostart.label);
        assert!(fixed.buttons[0].enabled, "but it can be started and stopped");
        assert!(plan(ServiceAction::Start, list.get("auditd.service").unwrap(), &p).is_none());
    }

    #[test]
    fn the_plans_of_the_buttons() {
        let list = list();
        let nginx = list.get("nginx.service").unwrap();
        let sudo = probe(Sudo::Passwordless, &[]);
        let restart = plan(ServiceAction::Restart, nginx, &sudo).unwrap();
        assert_eq!(restart.run, Run::Exec { line: "sudo -n systemctl restart nginx.service".into(), done: "已重启" });
        assert!(restart.confirm.is_none());
        let stop = plan(ServiceAction::Stop, nginx, &sudo).unwrap();
        assert_eq!(stop.run, Run::Exec { line: "sudo -n systemctl stop nginx.service".into(), done: "已停止" });
        let confirm = stop.confirm.unwrap();
        assert_eq!((confirm.title.as_str(), confirm.label), ("停止 nginx.service？", "停止服务"));
        assert_eq!(confirm.text, "A high performance web server\n依赖它的服务和正在处理的请求会立即中断。");
        assert_eq!(
            plan(ServiceAction::Enable, nginx, &probe(Sudo::Root, &[])).unwrap().run,
            Run::Exec { line: "systemctl enable nginx.service".into(), done: "已设为开机自启" }
        );
        // Needs a password: a terminal tab, the line typed and not run.
        let ask = plan(ServiceAction::Start, nginx, &probe(Sudo::NeedsPassword, &[])).unwrap();
        assert_eq!(
            ask.run,
            Run::Terminal {
                line: "sudo systemctl start nginx.service".into(),
                enter: false,
                title: "start nginx.service".into()
            }
        );
        // A name that must not reach a shell as an option.
        let mut odd = nginx.clone();
        odd.name = "-rf.service".into();
        assert!(plan(ServiceAction::Start, &odd, &sudo).is_none());
        // Nothing to stop or reload on a service that does not run.
        assert!(plan(ServiceAction::Stop, list.get("snapd.service").unwrap(), &sudo).is_none());
    }

    #[test]
    fn the_log_comes_from_the_journal_as_the_user_may_read_it() {
        let unit = "nginx.service";
        assert_eq!(
            log_source(unit, &probe(Sudo::NeedsPassword, &["adm"])),
            LogSource::Follow("journalctl -u nginx.service -n 50 -f -o short-iso --no-pager 2>&1".into())
        );
        assert_eq!(
            log_source(unit, &probe(Sudo::Passwordless, &["docker"])),
            LogSource::Follow("sudo -n journalctl -u nginx.service -n 50 -f -o short-iso --no-pager 2>&1".into())
        );
        assert_eq!(log_source(unit, &probe(Sudo::NeedsPassword, &[])), LogSource::Denied(JOURNAL_DENIED));
        assert_eq!(log_source("-x", &probe(Sudo::Root, &[])), LogSource::Denied("这个服务名不能用于查看日志"));
        assert_eq!(JOURNAL_DENIED, "没有权限读取日志（需要 adm 或 systemd-journal 组，或免密 sudo）");
        assert_eq!(
            log_source("my unit.service", &probe(Sudo::NeedsPassword, &["adm"])),
            LogSource::Follow("journalctl -u 'my unit.service' -n 50 -f -o short-iso --no-pager 2>&1".into())
        );
    }

    #[test]
    fn cpu_times_and_since() {
        assert_eq!(cpu_time(340_000_000), "340 ms");
        assert_eq!(cpu_time(12_340_000_000), "12.3 s");
        assert_eq!(cpu_time(201_000_000_000), "3 分 21 秒");
        assert_eq!(cpu_time(7_500_000_000_000), "2 小时 5 分");
        let mut service = list().get("cron.service").unwrap().clone();
        service.since_text = None;
        service.since_monotonic_us = Some(1_000_000);
        assert_eq!(since_text(&service, Some(3601.0)), "已 1 小时 0 分");
        service.since_monotonic_us = None;
        assert_eq!(since_text(&service, Some(3601.0)), "");
        service.since_text = Some("2026-11-16 01:09:15 UTC".into());
        assert_eq!(since_text(&service, None), "自 2026-11-16 01:09:15 UTC");
        service.since_monotonic_us = Some(9_000_000_000);
        assert_eq!(
            since_text(&service, Some(10.0)),
            "自 2026-11-16 01:09:15 UTC",
            "a clock that does not fit is not shown as a duration"
        );
    }
}
