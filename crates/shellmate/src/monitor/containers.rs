//! What the Docker page says, without a window: the header card, the containers grouped by compose project,
//! how a status reads, the chips of the ports and what a click on one does, the buttons of a row and how each
//! would run, the questions before a stop, a removal and the clean-up, and where the log of a container comes
//! from.

use shellmate_monitor::Probe;
use shellmate_monitor::commands::{self, ContainerAction, Plan, container_privilege};
use shellmate_monitor::docker::{self, Container, ContainerState, DockerView, PortMapping};
use shellmate_monitor::probe::{ContainerAccess, ContainerRuntime, RuntimeKind};

use super::format;
use super::model::{ForwardInfo, Tone};
use super::process::{Confirm, Planned, Run};
use super::services::LogSource;

/// The container tool and how the page says it is used.
pub fn engine_title(runtime: &ContainerRuntime) -> String {
    let version = runtime.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
    match runtime.kind {
        RuntimeKind::Docker => format!("Docker Engine{version}"),
        RuntimeKind::Podman => format!("Podman{version}"),
    }
}

/// The line under the title: how the tool is asked, and that events come in by themselves.
pub fn engine_subtitle(runtime: &ContainerRuntime) -> String {
    let tool = runtime.kind.command();
    match runtime.access {
        ContainerAccess::Sudo => format!("经 sudo {tool} 命令 · 事件实时推送"),
        _ => format!("经 {tool} 命令 · 事件实时推送"),
    }
}

/// The header card of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// `Docker Engine 29.1.3`
    pub title: String,
    /// `经 docker 命令 · 事件实时推送`
    pub subtitle: String,
    /// `4 / 6 个容器运行中`
    pub summary: String,
    /// `14 · 3.2 GB` and the caption `镜像`.
    pub images: String,
    /// `6` and the caption `数据卷`.
    pub volumes: String,
    /// Why the 清理 button says what it says.
    pub prune_tooltip: String,
    /// Something went wrong the last time the containers were asked: the reason.
    pub error: Option<String>,
}

/// How many containers run, and how many there are.
pub fn counts(view: &DockerView) -> (usize, usize) {
    (view.containers.iter().filter(|c| c.state == ContainerState::Running).count(), view.containers.len())
}

/// The header of `view`.
pub fn header(view: &DockerView, runtime: &ContainerRuntime) -> Header {
    let (running, total) = counts(view);
    let images = match (view.usage.images, view.usage.images_size) {
        (Some(count), Some(size)) => format!("{count} · {}", format::decimal_size(size)),
        (Some(count), None) => count.to_string(),
        _ => "—".into(),
    };
    let prune_tooltip = if view.dangling.count == 0 {
        "没有悬空镜像可清理".to_string()
    } else {
        format!("清理 {} 个悬空镜像，可回收 {}", view.dangling.count, format::decimal_size(view.dangling.bytes))
    };
    Header {
        title: engine_title(runtime),
        subtitle: engine_subtitle(runtime),
        summary: format!("{running} / {total} 个容器运行中"),
        images,
        volumes: view.usage.volumes.map_or_else(|| "—".into(), |count| count.to_string()),
        prune_tooltip,
        error: view.error.clone(),
    }
}

// ---- the status --------------------------------------------------------------------------------

/// docker's English status (`Up 11 hours`, `Exited (0) 2 weeks ago`) in the words of the page.
pub fn translate_status(status: &str) -> String {
    let words: Vec<&str> = status.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let lower = word.to_ascii_lowercase();
        let next = words.get(i + 1).map(|w| w.to_ascii_lowercase());
        let unit = |name: &str| match name.trim_end_matches('s') {
            "second" => Some("秒"),
            "minute" => Some("分钟"),
            "hour" => Some("小时"),
            "day" => Some("天"),
            "week" => Some("周"),
            "month" => Some("个月"),
            "year" => Some("年"),
            _ => None,
        };
        match (lower.as_str(), next.as_deref()) {
            ("about", Some("a" | "an")) if words.get(i + 2).is_some_and(|w| unit(w).is_some()) => {
                out.push(format!("约 1 {}", unit(words[i + 2]).expect("checked")));
                i += 3;
            }
            ("less", Some("than"))
                if words.get(i + 2).is_some_and(|w| w.eq_ignore_ascii_case("a"))
                    && words.get(i + 3).is_some_and(|w| unit(w).is_some()) =>
            {
                out.push(format!("不到 1 {}", unit(words[i + 3]).expect("checked")));
                i += 4;
            }
            (n, Some(u)) if n.chars().all(|c| c.is_ascii_digit()) && unit(u).is_some() => {
                out.push(format!("{n} {}", unit(u).expect("checked")));
                i += 2;
            }
            ("a" | "an", Some(u)) if unit(u).is_some() => {
                out.push(format!("1 {}", unit(u).expect("checked")));
                i += 2;
            }
            ("ago", _) => {
                out.push("前".into());
                i += 1;
            }
            _ => {
                out.push(word.to_string());
                i += 1;
            }
        }
    }
    // "2 周 前" reads better as "2 周前".
    out.join(" ").replace(" 前", "前")
}

/// What the status column says and how loud: `Up 11 小时`, `重启中 · 第 6 次 · OOMKilled`, `Exited (0) 2 周前`.
pub fn status_text(container: &Container) -> (String, Tone) {
    if container.is_restart_looping() {
        let mut text = "重启中".to_string();
        if container.restart_count > 0 {
            text += &format!(" · 第 {} 次", container.restart_count);
        }
        if container.oom_killed {
            text += " · OOMKilled";
        }
        return (text, Tone::Warn);
    }
    let text = translate_status(&container.status);
    (if text.is_empty() { "—".into() } else { text }, Tone::Plain)
}

/// The dot before the name.
pub fn dot_tone(container: &Container) -> Tone {
    if container.is_restart_looping() {
        Tone::Warn
    } else if container.state == ContainerState::Running {
        Tone::Ok
    } else {
        Tone::Off
    }
}

/// `412 MB / 1 GB` for a container with a memory limit, `24 MB` without, `—` when it does not run (a container in a
/// restart loop only says 0 B) or nothing has been measured yet.
pub fn memory_text(container: &Container) -> String {
    if container.state != ContainerState::Running {
        return "—".into();
    }
    match (container.mem_used, container.memory_limit) {
        (Some(used), Some(limit)) => format!("{} / {}", format::bytes(used), format::bytes_short(limit)),
        (Some(used), None) => format::bytes(used),
        (None, _) => "—".into(),
    }
}

/// `4.8%`, or `—`.
pub fn cpu_text(container: &Container) -> String {
    match container.cpu_percent {
        Some(cpu) if container.state == ContainerState::Running => format!("{cpu:.1}%"),
        _ => "—".into(),
    }
}

// ---- the ports ---------------------------------------------------------------------------------

/// Where a forward made from a port chip goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTarget {
    /// The name of the rule: `shellmate-web 容器`.
    pub name: String,
    /// The local port: the container's port number (the page may find another one when it is taken).
    pub local_port: u16,
    /// The target host as the server sees it: `127.0.0.1` for a published port, the container's address
    /// for one that is not published.
    pub host: String,
    /// The target port.
    pub port: u16,
}

impl ForwardTarget {
    /// `127.0.0.1:18080`
    pub fn target_text(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Why a port cannot be forwarded.
pub const NO_ADDRESS: &str = "这个容器没有可用的 IP 地址（没有运行，或使用 host 网络）：无法转发没有发布的端口";

/// Where a click on the chip of `port` of `container` forwards to. A published port is reached on the server's
/// loopback (or the address it is bound to); a port that is not published through the container's own address.
/// The local port is the same number.
pub fn forward_target(container: &Container, port: &PortMapping) -> Result<ForwardTarget, &'static str> {
    let name = format!("{} 容器", container.name);
    if let Some((host, host_port)) = port.published_target() {
        return Ok(ForwardTarget { name, local_port: host_port, host, port: host_port });
    }
    let ip = container.ip().ok_or(NO_ADDRESS)?;
    Ok(ForwardTarget { name, local_port: port.container_port, host: ip.to_string(), port: port.container_port })
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// The rule that already goes where `target` goes (any local port).
pub fn existing_forward<'a>(forwards: &'a [ForwardInfo], target: &ForwardTarget) -> Option<&'a ForwardInfo> {
    forwards.iter().find(|rule| {
        rule.target_port == target.port
            && (rule.target_host == target.host || (is_loopback(&rule.target_host) && is_loopback(&target.host)))
    })
}

/// The chip of a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortChip {
    /// `127.0.0.1:3000→3000`, `6379/tcp`, with `✓ ` in front when a forward to it exists.
    pub label: String,
    /// A forward to it exists.
    pub forwarded: bool,
    /// Where a click goes, or why it cannot.
    pub target: Result<ForwardTarget, &'static str>,
    /// What a hover says.
    pub tooltip: String,
}

fn port_chips(container: &Container, forwards: &[ForwardInfo]) -> Vec<PortChip> {
    container
        .ports
        .iter()
        .map(|port| {
            let target = forward_target(container, port);
            let existing = target.as_ref().ok().and_then(|target| existing_forward(forwards, target));
            let tooltip = match (&target, existing) {
                (_, Some(rule)) => format!(
                    "已有转发 {} → {}{}",
                    rule.listen,
                    existing_target(rule),
                    if rule.enabled { " · 运行中" } else { " · 已停用" }
                ),
                (Ok(target), None) => format!("点击建立转发 -L {} → {}", target.local_port, target.target_text()),
                (Err(why), None) => (*why).to_string(),
            };
            PortChip {
                label: format!("{}{}", if existing.is_some() { "✓ " } else { "" }, port.label()),
                forwarded: existing.is_some(),
                target,
                tooltip,
            }
        })
        .collect()
}

fn existing_target(rule: &ForwardInfo) -> String {
    format!("{}:{}", rule.target_host, rule.target_port)
}

/// The toast after a click on a chip whose forward exists: `已有转发 127.0.0.1:6379 → 172.18.0.3:6379 · 运行中`.
pub fn existing_text(rule: &ForwardInfo) -> String {
    format!(
        "已有转发 {} → {}{}",
        rule.listen,
        existing_target(rule),
        if rule.enabled { " · 运行中" } else { " · 已停用" }
    )
}

/// The toast after a forward was made.
pub fn created_text(local_port: u16, target: &ForwardTarget) -> String {
    format!("已创建转发 -L {local_port} → {} · 在「端口转发」中管理", target.target_text())
}

// ---- the rows ----------------------------------------------------------------------------------

/// A container of the list.
#[derive(Clone, Debug, PartialEq)]
pub struct ContainerRow {
    /// Its name.
    pub name: String,
    /// The image.
    pub image: String,
    /// The dot.
    pub dot: Tone,
    /// `Up 11 小时`, …
    pub status: String,
    /// How loud the status is.
    pub status_tone: Tone,
    /// The port chips.
    pub ports: Vec<PortChip>,
    /// `4.8%`, or `—`.
    pub cpu: String,
    /// `412 MB / 1 GB`, …
    pub memory: String,
    /// It runs (the terminal needs that).
    pub running: bool,
    /// 终端 can be pressed.
    pub can_exec: bool,
    /// 停止 can be pressed.
    pub can_stop: bool,
    /// 删除 can be pressed (only for a container that is not running).
    pub can_remove: bool,
    /// `重启` for a container that runs, `启动` for one that does not.
    pub run_label: &'static str,
}

/// A compose project (or the containers that belong to none).
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// The project name, or `其他`.
    pub name: String,
    /// `compose · /srv/app/docker-compose.yml`, or `docker run · 独立容器`.
    pub subtitle: String,
    /// The containers.
    pub rows: Vec<ContainerRow>,
}

/// The name of the group of containers that belong to no compose project.
pub const OTHERS: &str = "其他";

/// The containers of `view` by compose project (alphabetical), the ones without a project last.
pub fn groups(view: &DockerView, forwards: &[ForwardInfo]) -> Vec<Group> {
    let mut names: Vec<Option<&str>> = view.containers.iter().map(|c| c.project.as_deref()).collect();
    names.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    });
    names.dedup();
    names
        .into_iter()
        .map(|project| {
            let members: Vec<&Container> = view.containers.iter().filter(|c| c.project.as_deref() == project).collect();
            let subtitle = match project {
                Some(_) => {
                    let files = members.iter().find_map(|c| c.config_files.clone());
                    match files {
                        Some(files) => format!("compose · {}", files.replace(',', "  ")),
                        None => "compose".to_string(),
                    }
                }
                None => "docker run · 独立容器".to_string(),
            };
            Group {
                name: project.map_or_else(|| OTHERS.to_string(), str::to_string),
                subtitle,
                rows: members.into_iter().map(|c| row(c, forwards)).collect(),
            }
        })
        .collect()
}

fn row(container: &Container, forwards: &[ForwardInfo]) -> ContainerRow {
    let (status, status_tone) = status_text(container);
    let running = container.state.is_running();
    ContainerRow {
        name: container.name.clone(),
        image: container.image.clone(),
        dot: dot_tone(container),
        status,
        status_tone,
        ports: port_chips(container, forwards),
        cpu: cpu_text(container),
        memory: memory_text(container),
        running,
        can_exec: container.state == ContainerState::Running,
        can_stop: running,
        can_remove: !running,
        run_label: if running { "重启" } else { "启动" },
    }
}

// ---- the actions -------------------------------------------------------------------------------

/// How the commands of a container are run: `docker`, as the login user or with `sudo -n`.
fn privilege(runtime: &ContainerRuntime) -> commands::Privilege {
    container_privilege(runtime)
}

fn done_text(action: ContainerAction) -> &'static str {
    match action {
        ContainerAction::Start => "已启动",
        ContainerAction::Stop => "已停止",
        ContainerAction::Restart => "已重启",
        ContainerAction::Remove => "已删除",
    }
}

/// What pressing a button does to `container`.
pub fn plan(
    action: ContainerAction,
    container: &Container,
    runtime: &ContainerRuntime,
    probe: &Probe,
) -> Option<Planned> {
    if action == ContainerAction::Remove && container.state.is_running() {
        return None;
    }
    let command = commands::container(runtime.kind.command(), action, &container.name).ok()?;
    let run = match commands::plan(&command, privilege(runtime), probe) {
        Plan::Exec { line } => Run::Exec { line, done: done_text(action) },
        Plan::Terminal { line } => {
            Run::Terminal { line, enter: false, title: format!("{} {}", action.verb(), container.name) }
        }
    };
    let confirm = match action {
        ContainerAction::Stop => Some(Confirm {
            title: format!("停止容器 {}？", container.name),
            text: format!("{} · {}\n容器内正在处理的请求会中断。", container.image, status_text(container).0),
            label: "停止容器",
        }),
        ContainerAction::Remove => Some(Confirm {
            title: format!("删除容器 {}？", container.name),
            text: format!("镜像 {} 会保留，未挂载到卷的容器数据将被删除。", container.image),
            label: "删除容器",
        }),
        _ => None,
    };
    Some(Planned { run, confirm })
}

/// 终端: a new tab running `docker exec -it NAME sh -c '…bash or sh'`.
pub fn exec_plan(container: &Container, runtime: &ContainerRuntime, probe: &Probe) -> Option<Run> {
    if container.state != ContainerState::Running {
        return None;
    }
    let command = docker::exec_shell(runtime.kind.command(), &container.name).ok()?;
    let line = commands::terminal_line(&command, privilege(runtime), probe);
    Some(Run::Terminal { line, enter: true, title: format!("{} · 容器", container.name) })
}

/// 清理: the question (it says how much is given back), and the command. `None` when there is nothing to remove.
pub fn prune_plan(view: &DockerView, runtime: &ContainerRuntime, probe: &Probe) -> Option<Planned> {
    if view.dangling.count == 0 {
        return None;
    }
    let command = commands::prune_dangling_images(runtime.kind.command()).ok()?;
    let run = match commands::plan(&command, privilege(runtime), probe) {
        Plan::Exec { line } => Run::Exec { line, done: "已清理" },
        Plan::Terminal { line } => Run::Terminal { line, enter: false, title: "image prune".into() },
    };
    Some(Planned {
        run,
        confirm: Some(Confirm {
            title: "清理悬空镜像？".into(),
            text: format!(
                "将删除 {} 个悬空镜像（没有名称、也没有被容器使用的镜像），可回收约 {}。\n不影响容器、数据卷和有名称的镜像。",
                view.dangling.count,
                format::decimal_size(view.dangling.bytes)
            ),
            label: "清理",
        }),
    })
}

/// What the log of a container follows: `docker logs -f --tail 100 -t NAME` for one that runs, without `-f` for
/// one that does not (it would never end).
pub fn log_source(container: &Container, runtime: &ContainerRuntime) -> LogSource {
    let Ok(command) = docker::logs(runtime.kind.command(), &container.name, container.state.is_running()) else {
        return LogSource::Denied("这个容器名不能用于查看日志");
    };
    LogSource::Follow(if runtime.access == ContainerAccess::Sudo {
        command.sudo_n().line_merged()
    } else {
        command.line_merged()
    })
}

/// The label over the log: live or the last lines of a container that does not run.
pub fn log_caption(container: &Container) -> &'static str {
    match container.state {
        ContainerState::Running => "实时 · docker logs -f",
        ContainerState::Restarting => "重启中 · 实时 · docker logs -f",
        _ => "容器未运行 · 显示最近日志",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_monitor::Sudo;
    use shellmate_monitor::docker::{ContainerNetwork, Dangling, Usage};

    fn runtime(access: ContainerAccess) -> ContainerRuntime {
        ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access,
            version: Some("27.3.1".into()),
        }
    }

    fn probe(sudo: Sudo) -> Probe {
        Probe { sudo, uid: Some(1000), user: "deploy".into(), ..Probe::unknown() }
    }

    fn container(name: &str, project: Option<&str>, state: ContainerState, status: &str) -> Container {
        Container {
            id: format!("{name}-id"),
            name: name.into(),
            image: "nginx:alpine".into(),
            state,
            status: status.into(),
            ports: Vec::new(),
            project: project.map(str::to_string),
            config_files: project.map(|p| format!("/srv/{p}/docker-compose.yml")),
            service: None,
            restart_count: 0,
            oom_killed: false,
            restarting: false,
            memory_limit: None,
            networks: vec![ContainerNetwork { name: "bridge".into(), ip: "172.18.0.3".into() }],
            up_secs: None,
            cpu_percent: None,
            mem_used: None,
        }
    }

    fn port(host_ip: Option<&str>, host_port: Option<u16>, container_port: u16) -> PortMapping {
        PortMapping { host_ip: host_ip.map(str::to_string), host_port, container_port, protocol: "tcp".into() }
    }

    fn view(containers: Vec<Container>) -> DockerView {
        DockerView {
            runtime: RuntimeKind::Docker,
            version: Some("27.3.1".into()),
            containers,
            usage: Usage { images: Some(14), images_size: Some(3_200_000_000), volumes: Some(6) },
            dangling: Dangling { count: 4, bytes: 1_240_000_000 },
            error: None,
        }
    }

    #[test]
    fn docker_says_its_status_in_english() {
        assert_eq!(translate_status("Up 11 hours"), "Up 11 小时");
        assert_eq!(translate_status("Up 6 days"), "Up 6 天");
        assert_eq!(translate_status("Up About a minute (healthy)"), "Up 约 1 分钟 (healthy)");
        assert_eq!(translate_status("Up About an hour"), "Up 约 1 小时");
        assert_eq!(translate_status("Up Less than a second"), "Up 不到 1 秒");
        assert_eq!(translate_status("Up 3 seconds"), "Up 3 秒");
        assert_eq!(translate_status("Exited (0) 2 weeks ago"), "Exited (0) 2 周前");
        assert_eq!(translate_status("Exited (137) 3 minutes ago"), "Exited (137) 3 分钟前");
        assert_eq!(translate_status("Exited (1) About an hour ago"), "Exited (1) 约 1 小时前");
        assert_eq!(translate_status("Restarting (137) Less than a second ago"), "Restarting (137) 不到 1 秒前");
        assert_eq!(translate_status("Created"), "Created");
        assert_eq!(translate_status("Up 2 months (unhealthy)"), "Up 2 个月 (unhealthy)");
        assert_eq!(translate_status(""), "");
        assert_eq!(translate_status("Up an hour"), "Up 1 小时");
    }

    #[test]
    fn a_restart_loop_is_told_with_its_count_and_its_cause() {
        let mut oomer = container("worker", Some("app"), ContainerState::Restarting, "Restarting (137) 3 seconds ago");
        oomer.restart_count = 6;
        oomer.oom_killed = true;
        oomer.restarting = true;
        assert_eq!(status_text(&oomer), ("重启中 · 第 6 次 · OOMKilled".to_string(), Tone::Warn));
        assert_eq!(dot_tone(&oomer), Tone::Warn);
        oomer.oom_killed = false;
        assert_eq!(status_text(&oomer).0, "重启中 · 第 6 次");
        oomer.restart_count = 0;
        assert_eq!(status_text(&oomer).0, "重启中");
        let up = container("app", Some("app"), ContainerState::Running, "Up 11 hours");
        assert_eq!((status_text(&up), dot_tone(&up)), (("Up 11 小时".to_string(), Tone::Plain), Tone::Ok));
        let exited = container("old", None, ContainerState::Exited, "Exited (0) 2 weeks ago");
        assert_eq!((status_text(&exited).0.as_str(), dot_tone(&exited)), ("Exited (0) 2 周前", Tone::Off));
    }

    #[test]
    fn cpu_and_memory_columns() {
        let mut c = container("app", None, ContainerState::Running, "Up 1 hour");
        assert_eq!((cpu_text(&c), memory_text(&c)), ("—".to_string(), "—".to_string()), "nothing measured yet");
        c.cpu_percent = Some(4.8);
        c.mem_used = Some(412 << 20);
        c.memory_limit = Some(1 << 30);
        assert_eq!((cpu_text(&c), memory_text(&c)), ("4.8%".to_string(), "412 MB / 1 GB".to_string()));
        c.memory_limit = Some(256 << 20);
        c.mem_used = Some(256 << 20);
        assert_eq!(memory_text(&c), "256 MB / 256 MB");
        c.memory_limit = None;
        c.mem_used = Some(24 << 20);
        assert_eq!(memory_text(&c), "24 MB");
        c.state = ContainerState::Restarting;
        assert_eq!(
            (cpu_text(&c), memory_text(&c)),
            ("—".to_string(), "—".to_string()),
            "a container in a restart loop has no numbers of its own"
        );
        c.state = ContainerState::Exited;
        assert_eq!(cpu_text(&c), "—", "a stopped container has no CPU");
    }

    #[test]
    fn the_header_counts_and_sizes() {
        let v = view(vec![
            container("a", None, ContainerState::Running, "Up 1 hour"),
            container("b", None, ContainerState::Exited, "Exited (0) 1 hour ago"),
        ]);
        let h = header(&v, &runtime(ContainerAccess::User));
        assert_eq!(h.title, "Docker Engine 27.3.1");
        assert_eq!(h.subtitle, "经 docker 命令 · 事件实时推送");
        assert_eq!(h.summary, "1 / 2 个容器运行中");
        assert_eq!((h.images.as_str(), h.volumes.as_str()), ("14 · 3.2 GB", "6"));
        assert_eq!(h.prune_tooltip, "清理 4 个悬空镜像，可回收 1.2 GB");
        assert_eq!(header(&v, &runtime(ContainerAccess::Sudo)).subtitle, "经 sudo docker 命令 · 事件实时推送");
        let empty = DockerView { usage: Usage::default(), dangling: Dangling::default(), ..v };
        let h = header(&empty, &runtime(ContainerAccess::User));
        assert_eq!((h.images.as_str(), h.volumes.as_str(), h.prune_tooltip.as_str()), ("—", "—", "没有悬空镜像可清理"));
        let podman = ContainerRuntime { kind: RuntimeKind::Podman, ..runtime(ContainerAccess::User) };
        assert_eq!(header(&empty, &podman).title, "Podman 27.3.1");
    }

    #[test]
    fn containers_are_grouped_by_compose_project_with_the_others_last() {
        let v = view(vec![
            container("worker", Some("app"), ContainerState::Running, "Up 1 hour"),
            container("old-api", None, ContainerState::Exited, "Exited (0) 2 weeks ago"),
            container("grafana", Some("monitoring"), ContainerState::Running, "Up 6 days"),
            container("app", Some("app"), ContainerState::Running, "Up 11 hours"),
        ]);
        let groups = groups(&v, &[]);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["app", "monitoring", "其他"]);
        assert_eq!(groups[0].subtitle, "compose · /srv/app/docker-compose.yml");
        assert_eq!(groups[2].subtitle, "docker run · 独立容器");
        assert_eq!(groups[0].rows.len(), 2);
        let mut with_two_files = container("x", Some("multi"), ContainerState::Running, "Up 1 hour");
        with_two_files.config_files = Some("/a/compose.yml,/a/override.yml".into());
        assert_eq!(
            super::groups(&view(vec![with_two_files]), &[])[0].subtitle,
            "compose · /a/compose.yml  /a/override.yml"
        );
        assert!(super::groups(&view(Vec::new()), &[]).is_empty());
    }

    #[test]
    fn the_buttons_of_a_row_follow_the_state() {
        let v = view(vec![
            container("up", None, ContainerState::Running, "Up 1 hour"),
            container("old", None, ContainerState::Exited, "Exited (0) 1 hour ago"),
            container("loop", None, ContainerState::Restarting, "Restarting (1) 2 seconds ago"),
        ]);
        let rows = &groups(&v, &[])[0].rows;
        let by = |name: &str| rows.iter().find(|r| r.name == name).unwrap();
        let up = by("up");
        assert!((up.can_exec, up.can_stop, up.can_remove, up.run_label) == (true, true, false, "重启"));
        let old = by("old");
        assert!((old.can_exec, old.can_stop, old.can_remove, old.run_label) == (false, false, true, "启动"));
        let looping = by("loop");
        assert!(
            (looping.can_exec, looping.can_stop, looping.can_remove) == (false, true, false),
            "a restarting container cannot be entered, but can be stopped"
        );
    }

    #[test]
    fn a_chip_forwards_a_published_port_to_the_loopback_and_another_through_the_address_of_the_container() {
        let mut web = container("app", Some("app"), ContainerState::Running, "Up 1 hour");
        web.ports = vec![
            port(Some("127.0.0.1"), Some(3000), 3000),
            port(Some("0.0.0.0"), Some(8080), 80),
            port(None, None, 6379),
        ];
        let t0 = forward_target(&web, &web.ports[0]).unwrap();
        assert_eq!((t0.name.as_str(), t0.local_port, t0.host.as_str(), t0.port), ("app 容器", 3000, "127.0.0.1", 3000));
        let t1 = forward_target(&web, &web.ports[1]).unwrap();
        assert_eq!(
            (t1.local_port, t1.host.as_str(), t1.port),
            (8080, "127.0.0.1", 8080),
            "published on every address: through the loopback, on the host port"
        );
        let t2 = forward_target(&web, &web.ports[2]).unwrap();
        assert_eq!(
            (t2.local_port, t2.host.as_str(), t2.port, t2.target_text().as_str()),
            (6379, "172.18.0.3", 6379, "172.18.0.3:6379")
        );
        web.networks.clear();
        assert_eq!(forward_target(&web, &web.ports[2]), Err(NO_ADDRESS), "no address, no forward");
        assert!(forward_target(&web, &web.ports[0]).is_ok(), "a published port needs none");
        assert_eq!(created_text(16379, &t2), "已创建转发 -L 16379 → 172.18.0.3:6379 · 在「端口转发」中管理");
    }

    #[test]
    fn a_chip_shows_a_tick_when_a_rule_with_that_target_exists() {
        let mut web = container("app", Some("app"), ContainerState::Running, "Up 1 hour");
        web.ports = vec![port(Some("127.0.0.1"), Some(3000), 3000), port(None, None, 6379)];
        let rules = vec![
            ForwardInfo {
                listen: "127.0.0.1:3000".into(),
                target_host: "localhost".into(),
                target_port: 3000,
                enabled: true,
            },
            ForwardInfo {
                listen: "127.0.0.1:16379".into(),
                target_host: "172.18.0.9".into(),
                target_port: 6379,
                enabled: true,
            },
        ];
        let chips = port_chips(&web, &rules);
        assert!(
            chips[0].forwarded && chips[0].label == "✓ 127.0.0.1:3000→3000",
            "localhost is the loopback: {}",
            chips[0].label
        );
        assert_eq!(chips[0].tooltip, "已有转发 127.0.0.1:3000 → localhost:3000 · 运行中");
        assert!(!chips[1].forwarded, "another address is another target");
        assert_eq!(chips[1].label, "6379/tcp");
        assert_eq!(chips[1].tooltip, "点击建立转发 -L 6379 → 172.18.0.3:6379");
        let rules = vec![ForwardInfo {
            listen: "127.0.0.1:16379".into(),
            target_host: "172.18.0.3".into(),
            target_port: 6379,
            enabled: false,
        }];
        let chips = port_chips(&web, &rules);
        assert!(chips[1].forwarded);
        assert_eq!(existing_text(&rules[0]), "已有转发 127.0.0.1:16379 → 172.18.0.3:6379 · 已停用");
        assert!(!chips[0].forwarded);
    }

    #[test]
    fn the_plans_of_the_buttons() {
        let up = container("shop-web", None, ContainerState::Running, "Up 2 hours");
        let sudo = probe(Sudo::Passwordless);
        let user = runtime(ContainerAccess::User);
        let restart = plan(ContainerAction::Restart, &up, &user, &sudo).unwrap();
        assert_eq!(restart.run, Run::Exec { line: "docker restart shop-web".into(), done: "已重启" });
        assert!(restart.confirm.is_none());
        let stop = plan(ContainerAction::Stop, &up, &user, &sudo).unwrap();
        assert_eq!(stop.run, Run::Exec { line: "docker stop shop-web".into(), done: "已停止" });
        let confirm = stop.confirm.unwrap();
        assert_eq!((confirm.title.as_str(), confirm.label), ("停止容器 shop-web？", "停止容器"));
        assert_eq!(confirm.text, "nginx:alpine · Up 2 小时\n容器内正在处理的请求会中断。");
        // Only sudo gets the tool through: with `sudo -n`.
        let only_sudo = runtime(ContainerAccess::Sudo);
        assert_eq!(
            plan(ContainerAction::Restart, &up, &only_sudo, &sudo).unwrap().run,
            Run::Exec { line: "sudo -n docker restart shop-web".into(), done: "已重启" }
        );
        // A running container is not removed; an exited one is, after a question.
        assert!(plan(ContainerAction::Remove, &up, &user, &sudo).is_none());
        let old = container("old-api", None, ContainerState::Exited, "Exited (0) 2 weeks ago");
        let remove = plan(ContainerAction::Remove, &old, &user, &sudo).unwrap();
        assert_eq!(remove.run, Run::Exec { line: "docker rm old-api".into(), done: "已删除" });
        assert_eq!(remove.confirm.unwrap().text, "镜像 nginx:alpine 会保留，未挂载到卷的容器数据将被删除。");
        assert_eq!(
            plan(ContainerAction::Start, &old, &user, &sudo).unwrap().run,
            Run::Exec { line: "docker start old-api".into(), done: "已启动" }
        );
        // A name that looks like an option is refused.
        let mut odd = old.clone();
        odd.name = "-f".into();
        assert!(plan(ContainerAction::Start, &odd, &user, &sudo).is_none());
        // A name with a quote or a space reaches the shell as one word.
        let mut tricky = old;
        tricky.name = "it's a name".into();
        assert_eq!(
            plan(ContainerAction::Start, &tricky, &user, &sudo).unwrap().run,
            Run::Exec { line: "docker start 'it'\\''s a name'".into(), done: "已启动" }
        );
    }

    #[test]
    fn the_terminal_opens_a_shell_for_a_running_container() {
        let up = container("shop-web", None, ContainerState::Running, "Up 2 hours");
        let line = "docker exec -it shop-web sh -c 'command -v bash >/dev/null && exec bash || exec sh'";
        assert_eq!(
            exec_plan(&up, &runtime(ContainerAccess::User), &probe(Sudo::NeedsPassword)),
            Some(Run::Terminal { line: line.into(), enter: true, title: "shop-web · 容器".into() })
        );
        let sudo = exec_plan(&up, &runtime(ContainerAccess::Sudo), &probe(Sudo::Passwordless)).unwrap();
        assert_eq!(sudo, Run::Terminal { line: format!("sudo {line}"), enter: true, title: "shop-web · 容器".into() });
        let looping = container("loop", None, ContainerState::Restarting, "Restarting (1) 1 second ago");
        assert!(exec_plan(&looping, &runtime(ContainerAccess::User), &probe(Sudo::Root)).is_none());
        assert!(
            exec_plan(
                &container("old", None, ContainerState::Exited, "Exited (0) 1 hour ago"),
                &runtime(ContainerAccess::User),
                &probe(Sudo::Root)
            )
            .is_none()
        );
    }

    #[test]
    fn the_clean_up_asks_how_much_it_gives_back() {
        let v = view(vec![container("a", None, ContainerState::Running, "Up 1 hour")]);
        let planned = prune_plan(&v, &runtime(ContainerAccess::User), &probe(Sudo::Root)).unwrap();
        assert_eq!(planned.run, Run::Exec { line: "docker image prune -f".into(), done: "已清理" });
        let confirm = planned.confirm.unwrap();
        assert_eq!((confirm.title.as_str(), confirm.label), ("清理悬空镜像？", "清理"));
        assert!(
            confirm.text.starts_with("将删除 4 个悬空镜像") && confirm.text.contains("可回收约 1.2 GB"),
            "{}",
            confirm.text
        );
        let none = DockerView { dangling: Dangling::default(), ..v };
        assert!(prune_plan(&none, &runtime(ContainerAccess::User), &probe(Sudo::Root)).is_none());
    }

    #[test]
    fn the_log_follows_a_running_container_and_shows_the_end_of_a_stopped_one() {
        let up = container("shop-web", None, ContainerState::Running, "Up 2 hours");
        assert_eq!(
            log_source(&up, &runtime(ContainerAccess::User)),
            LogSource::Follow("docker logs -f --tail 100 -t shop-web 2>&1".into())
        );
        assert_eq!(log_caption(&up), "实时 · docker logs -f");
        let looping = container("loop", None, ContainerState::Restarting, "Restarting (1) 1 second ago");
        assert_eq!(log_caption(&looping), "重启中 · 实时 · docker logs -f");
        let old = container("old", None, ContainerState::Exited, "Exited (0) 2 weeks ago");
        assert_eq!(
            log_source(&old, &runtime(ContainerAccess::Sudo)),
            LogSource::Follow("sudo -n docker logs --tail 100 -t old 2>&1".into())
        );
        assert_eq!(log_caption(&old), "容器未运行 · 显示最近日志");
        let mut odd = old;
        odd.name = "-x".into();
        assert!(matches!(log_source(&odd, &runtime(ContainerAccess::User)), LogSource::Denied(_)));
    }
}
