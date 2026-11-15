//! What the 监控 page says, without a window: the header with its chips and sub-page tabs, what the page
//! shows when there is nothing to show (waiting, failed, not Linux), the cards of the overview, the items
//! that need attention, and the empty states of the pages that come with the next version.

use std::sync::Arc;

use shellmate_monitor::probe::{ContainerAccess, ContainerRuntime, RuntimeKind};
use shellmate_monitor::snapshot::{DISK_ATTENTION_PERCENT, HISTORY_POINTS};
use shellmate_monitor::{Phase, Probe, Snapshot, Sudo};

use super::format;

/// How loud something is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Nothing special.
    Plain,
    /// Good (green).
    Ok,
    /// Look at it (orange).
    Warn,
    /// Wrong (red).
    Err,
    /// Not there (grey).
    Off,
}

/// The sub-pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// 概览
    Overview,
    /// 进程
    Processes,
    /// 服务
    Services,
    /// Docker
    Docker,
}

impl Page {
    /// All of them, in the order of the header.
    pub const ALL: [Page; 4] = [Page::Overview, Page::Processes, Page::Services, Page::Docker];

    /// The label of the tab.
    pub fn label(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Processes => "进程",
            Page::Services => "服务",
            Page::Docker => "Docker",
        }
    }
}

/// Everything the page knows about the server it shows.
#[derive(Clone, Debug)]
pub struct MonitorData {
    /// Where the monitor stands.
    pub phase: Phase,
    /// What the probe found out (once it has run).
    pub probe: Option<Arc<Probe>>,
    /// The newest numbers (once the first frames are in).
    pub snapshot: Option<Arc<Snapshot>>,
}

impl MonitorData {
    /// A monitor that has not said anything yet.
    pub fn empty() -> Self {
        Self { phase: Phase::Idle, probe: None, snapshot: None }
    }
}

// ---- the header ---------------------------------------------------------------------------------

/// A little label under the system name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    /// What it says.
    pub text: String,
    /// How it looks.
    pub tone: Tone,
}

/// A tab of the sub-page control, with its badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageTab {
    /// Which page.
    pub page: Page,
    /// Whether it is the one shown.
    pub active: bool,
    /// The number or word beside the label, if there is one.
    pub badge: Option<(String, Tone)>,
}

/// The header that all sub-pages share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderView {
    /// The system name (`Ubuntu 24.04.5 LTS`).
    pub os: String,
    /// ` · 内核 6.8.0-45 · 已运行 41 天 · 负载 0.42 0.38 0.31`: what follows it, in lighter type.
    pub meta: String,
    /// What the user can do on this server.
    pub chips: Vec<Chip>,
    /// The tabs.
    pub tabs: Vec<PageTab>,
}

/// The chip about sudo.
pub fn sudo_chip(sudo: Sudo) -> Chip {
    match sudo {
        Sudo::Root => Chip { text: "root 登录".into(), tone: Tone::Ok },
        Sudo::Passwordless => Chip { text: "sudo 免密".into(), tone: Tone::Ok },
        Sudo::NeedsPassword => Chip { text: "sudo 需密码".into(), tone: Tone::Warn },
        Sudo::Unavailable => Chip { text: "无 sudo".into(), tone: Tone::Off },
    }
}

/// The chip about containers: the first tool that works, or why none does.
pub fn docker_chip(probe: &Probe) -> Chip {
    if let Some(runtime) = probe.usable_container_runtime() {
        return match (runtime.kind, runtime.access) {
            (RuntimeKind::Docker, ContainerAccess::User) => Chip { text: "docker 组".into(), tone: Tone::Ok },
            (RuntimeKind::Podman, ContainerAccess::User) => Chip { text: "podman".into(), tone: Tone::Ok },
            (RuntimeKind::Docker, _) => Chip { text: "Docker 需 sudo".into(), tone: Tone::Warn },
            (RuntimeKind::Podman, _) => Chip { text: "podman 需 sudo".into(), tone: Tone::Warn },
        };
    }
    match probe.containers.first() {
        None => Chip { text: "无 Docker".into(), tone: Tone::Off },
        Some(runtime) => {
            let name = match runtime.kind {
                RuntimeKind::Docker => "Docker",
                RuntimeKind::Podman => "podman",
            };
            match runtime.access {
                ContainerAccess::Denied => Chip { text: format!("{name} 无权限"), tone: Tone::Warn },
                ContainerAccess::NoDaemon => Chip { text: format!("{name} 未运行"), tone: Tone::Warn },
                _ => Chip { text: format!("{name} 异常"), tone: Tone::Warn },
            }
        }
    }
}

/// The chip about systemd: only a system that systemd booted can be managed.
pub fn systemd_chip(probe: &Probe) -> Chip {
    match &probe.systemd {
        Some(systemd) if systemd.running => Chip {
            text: systemd.version.map_or_else(|| "systemd".into(), |v| format!("systemd {v}")),
            tone: Tone::Plain,
        },
        _ => Chip { text: "无 systemd".into(), tone: Tone::Off },
    }
}

/// How many things need attention (what the badge of 概览 says).
pub fn attention_count(snapshot: Option<&Snapshot>) -> usize {
    attention(snapshot).len()
}

/// The header for what is known now. `host` names the connection until the system says its own name.
pub fn header(data: &MonitorData, page: Page, host: &str) -> HeaderView {
    let probe = data.probe.as_deref();
    let snapshot = data.snapshot.as_deref();
    let os = probe
        .and_then(|p| p.os_name.clone())
        .or_else(|| probe.map(|p| p.kernel.clone()).filter(|k| !k.is_empty()))
        .unwrap_or_else(|| host.to_string());

    let mut meta = Vec::new();
    // When the kernel is what the system is called (no os-release), it is not said twice.
    let named = probe.is_some_and(|p| p.os_name.is_some());
    if let Some(kernel) = probe.map(|p| p.kernel.trim_start_matches("Linux").trim()).filter(|k| named && !k.is_empty())
    {
        meta.push(format!("内核 {kernel}"));
    }
    let uptime =
        snapshot.map(|s| s.host.uptime_secs).filter(|secs| *secs > 0.0).or_else(|| probe.and_then(|p| p.uptime_secs));
    if let Some(secs) = uptime {
        meta.push(format!("已运行 {}", format::uptime(secs)));
    }
    if let Some(load) = snapshot.and_then(|s| s.host.load) {
        meta.push(format!("负载 {}", format::load(load.one, load.five, load.fifteen)));
    }
    let meta = if meta.is_empty() { String::new() } else { format!(" · {}", meta.join(" · ")) };

    let mut chips = Vec::new();
    if let Some(probe) = probe.filter(|p| p.can_collect()) {
        chips.push(sudo_chip(probe.sudo));
        chips.push(docker_chip(probe));
        chips.push(systemd_chip(probe));
        chips.push(Chip { text: "独立 exec 通道 · 2s".into(), tone: Tone::Plain });
    }

    let attention = attention_count(snapshot);
    let tabs = Page::ALL
        .iter()
        .map(|candidate| PageTab {
            page: *candidate,
            active: *candidate == page,
            badge: match candidate {
                // Nothing to look at is no number: a badge that says 0 is noise.
                Page::Overview => snapshot.filter(|_| attention > 0).map(|_| (attention.to_string(), Tone::Err)),
                Page::Processes => snapshot.map(|s| (s.processes.len().to_string(), Tone::Plain)),
                Page::Services => probe.and_then(services_badge),
                Page::Docker => probe.and_then(docker_badge),
            },
        })
        .collect();
    HeaderView { os, meta, chips, tabs }
}

fn services_badge(probe: &Probe) -> Option<(String, Tone)> {
    match &probe.systemd {
        Some(systemd) if systemd.running => None,
        _ if probe.can_collect() => Some(("无".into(), Tone::Off)),
        _ => None,
    }
}

fn docker_badge(probe: &Probe) -> Option<(String, Tone)> {
    if !probe.can_collect() || probe.usable_container_runtime().is_some() {
        return None;
    }
    Some(match probe.containers.first().map(|c| c.access) {
        None => ("未安装".into(), Tone::Off),
        Some(ContainerAccess::Denied) => ("无权限".into(), Tone::Warn),
        Some(ContainerAccess::NoDaemon) => ("未运行".into(), Tone::Warn),
        Some(_) => ("异常".into(), Tone::Warn),
    })
}

// ---- what the page shows ------------------------------------------------------------------------

/// A line above the data of a page that is not quite current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    /// What it says.
    pub text: String,
    /// How loud.
    pub tone: Tone,
    /// Whether it has a 重试 button.
    pub retry: bool,
}

/// A page that has nothing to show but a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The big line.
    pub title: String,
    /// The explanation under it (may have several lines).
    pub detail: String,
    /// Whether it has a 重试 button.
    pub retry: bool,
}

/// The body of the overview and the process page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Cards and tables. `stale`: the numbers are the last ones from before the connection was lost; they
    /// are drawn grey. Without a snapshot the cards show placeholders.
    Data {
        /// The numbers are not current.
        stale: bool,
        /// A line above them.
        banner: Option<Banner>,
    },
    /// Nothing to show but a sentence.
    Message(Message),
}

/// What the overview and the process page show for `data`.
pub fn body(data: &MonitorData) -> Body {
    let has_data = data.snapshot.is_some();
    match &data.phase {
        Phase::Unsupported(why) => Body::Message(Message {
            title: "暂只支持 Linux 主机的监控".into(),
            detail: format!("{}\n监控读取 /proc，只在 Linux 上可用。", unsupported_system(data, why)),
            retry: false,
        }),
        Phase::Failed(reason) if !has_data => {
            Body::Message(Message {
                title: "监控无法读取这台主机".into(), detail: reason.clone(), retry: true
            })
        }
        Phase::Failed(reason) => Body::Data {
            stale: true,
            banner: Some(Banner { text: format!("监控已停止：{reason}"), tone: Tone::Err, retry: true }),
        },
        Phase::WaitingReconnect if !has_data => Body::Message(Message {
            title: "等待重连".into(),
            detail: "连接恢复后会自动继续读取。".into(),
            retry: false,
        }),
        Phase::WaitingReconnect => Body::Data {
            stale: true,
            banner: Some(Banner {
                text: "等待重连 · 显示的是断线前的最后一次数据".into(),
                tone: Tone::Warn,
                retry: false,
            }),
        },
        Phase::Idle | Phase::Starting | Phase::Collecting => Body::Data { stale: false, banner: None },
    }
}

/// The system a server that cannot be monitored says it is.
fn unsupported_system(data: &MonitorData, why: &str) -> String {
    match data.probe.as_deref() {
        Some(probe) if !probe.kernel.is_empty() => match &probe.os_name {
            Some(os) => format!("{os} · {}", probe.kernel),
            None => probe.kernel.clone(),
        },
        _ => why.to_string(),
    }
}

// ---- the overview -------------------------------------------------------------------------------

/// The CPU card.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuView {
    /// `CPU · 8 核`
    pub title: String,
    /// `负载 0.42`
    pub load: String,
    /// `19`, or `—` until there are two samples.
    pub percent: String,
    /// The last two minutes, 0 to 100.
    pub history: Vec<f32>,
}

/// The memory card.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryView {
    /// `9.6 / 15.6 GB`
    pub total: String,
    /// `61`, or `—`.
    pub percent: String,
    /// The share of the bar that is used memory, 0 to 1.
    pub used: f32,
    /// The share after it that is cache, 0 to 1.
    pub cache: f32,
    /// `已用 9.6 GB`, `缓存 2.8 GB`, `Swap 128 MB` (or `无 Swap`).
    pub legend: [String; 3],
}

/// A mount point of the disk card.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskView {
    /// `/data`
    pub mount: String,
    /// `164 / 200 GB`
    pub text: String,
    /// 0 to 1.
    pub fraction: f32,
    /// Orange from 80 %, red from 95 %.
    pub tone: Tone,
}

/// The network card.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkView {
    /// `网络 · eth0`
    pub title: String,
    /// `MB/s`
    pub unit: &'static str,
    /// `0.4`, or `—`.
    pub down: String,
    /// `2.4`, or `—`.
    pub up: String,
    /// Received, as fractions of `peak` units… see [`NetworkView::down_history`].
    pub down_history: Vec<f64>,
    /// Sent.
    pub up_history: Vec<f64>,
}

/// The four cards.
#[derive(Clone, Debug, PartialEq)]
pub struct Cards {
    /// CPU.
    pub cpu: CpuView,
    /// Memory.
    pub memory: MemoryView,
    /// Disks (at most five; the rest are in the attention list when they are full).
    pub disks: Vec<DiskView>,
    /// Network (`None` when the server has no interface to speak of).
    pub network: Option<NetworkView>,
}

/// How many disks the card has room for.
pub const DISK_ROWS: usize = 5;

/// The cards for `snapshot`, with placeholders where there is nothing yet.
pub fn cards(snapshot: Option<&Snapshot>) -> Cards {
    let cores = snapshot.map(|s| s.host.cores).filter(|n| *n > 0);
    let cpu_card = snapshot.and_then(|s| s.cpu.as_ref());
    let cpu = CpuView {
        title: cores.map_or_else(|| "CPU".into(), |n| format!("CPU · {n} 核")),
        load: snapshot.and_then(|s| s.host.load).map_or_else(|| "负载 —".into(), |l| format!("负载 {:.2}", l.one)),
        percent: format::percent_whole(cpu_card.and_then(|c| c.percent)),
        history: cpu_card.map(|c| c.history.clone()).unwrap_or_default(),
    };

    let memory = match snapshot.and_then(|s| s.memory) {
        Some(mem) => MemoryView {
            total: format::gb_pair(mem.used, mem.total, 100.0),
            percent: format::percent_whole(Some(mem.used_percent())),
            used: (mem.used_percent() / 100.0).clamp(0.0, 1.0),
            cache: ((mem.used_and_cache_percent() - mem.used_percent()) / 100.0).clamp(0.0, 1.0),
            legend: [
                format!("已用 {}", format::bytes(mem.used)),
                format!("缓存 {}", format::bytes(mem.cache)),
                if mem.swap_total == 0 { "无 Swap".into() } else { format!("Swap {}", format::bytes(mem.swap_used)) },
            ],
        },
        None => MemoryView {
            total: "—".into(),
            percent: "—".into(),
            used: 0.0,
            cache: 0.0,
            legend: ["已用 —".into(), "缓存 —".into(), "Swap —".into()],
        },
    };

    let disks = snapshot
        .map(|s| {
            s.disks
                .iter()
                .take(DISK_ROWS)
                .map(|disk| DiskView {
                    mount: disk.mount.clone(),
                    text: format::gb_pair(disk.used, disk.total, 10.0),
                    fraction: f32::from(disk.percent.min(100)) / 100.0,
                    tone: disk_tone(disk.percent),
                })
                .collect()
        })
        .unwrap_or_default();

    let network = match snapshot.and_then(|s| s.network.as_ref()) {
        Some(net) => {
            let (down, up, unit) = match (net.rx_per_sec, net.tx_per_sec) {
                (Some(rx), Some(tx)) => format::rate_pair(rx, tx),
                _ => ("—".into(), "—".into(), "B/s"),
            };
            Some(NetworkView {
                title: format!("网络 · {}", net.interface),
                unit,
                down,
                up,
                down_history: net.rx_history.clone(),
                up_history: net.tx_history.clone(),
            })
        }
        None => None,
    };
    Cards { cpu, memory, disks, network }
}

/// The color of a disk's bar.
pub fn disk_tone(percent: u8) -> Tone {
    match percent {
        95.. => Tone::Err,
        p if p >= DISK_ATTENTION_PERCENT => Tone::Warn,
        _ => Tone::Plain,
    }
}

/// What a sparkline of `values` is scaled to: the largest value, never less than `floor` (so that a
/// trickle of traffic does not fill the card).
pub fn scale_to(values: &[f64], floor: f64) -> f64 {
    values.iter().copied().fold(floor, f64::max).max(f64::MIN_POSITIVE)
}

// ---- what needs attention -----------------------------------------------------------------------

/// What an item of the attention list does when it is clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttentionAction {
    /// 分析目录: a new terminal tab with `du` on this mount point.
    AnalyzeDisk(String),
    /// 查看进程: the process page with this process selected.
    ShowProcess(u32),
}

/// One item of 需要关注.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionItem {
    /// The tag (`磁盘`, `进程`).
    pub tag: &'static str,
    /// How loud.
    pub tone: Tone,
    /// The line.
    pub title: String,
    /// The line under it.
    pub detail: String,
    /// The label of what clicking does (`分析目录`).
    pub action_label: &'static str,
    /// What clicking does.
    pub action: AttentionAction,
}

/// The items that need attention in `snapshot`: volumes that are 80 % full or more (the fullest first)
/// and zombie processes. Services and containers come with the next version.
pub fn attention(snapshot: Option<&Snapshot>) -> Vec<AttentionItem> {
    let Some(snapshot) = snapshot else { return Vec::new() };
    let mut items = Vec::new();
    let mut disks = snapshot.disks_over(DISK_ATTENTION_PERCENT);
    disks.sort_by(|a, b| b.percent.cmp(&a.percent).then_with(|| a.mount.cmp(&b.mount)));
    for disk in disks {
        items.push(AttentionItem {
            tag: "磁盘",
            tone: if disk.percent >= 95 { Tone::Err } else { Tone::Warn },
            title: format!("{} 已用 {}%", disk.mount, disk.percent),
            detail: format!(
                "已用 {} / {} · 可用 {}",
                format::gigabytes(disk.used),
                format::gigabytes(disk.total),
                format::gigabytes(disk.available)
            ),
            action_label: "分析目录",
            action: AttentionAction::AnalyzeDisk(disk.mount.clone()),
        });
    }
    if snapshot.zombies > 0 {
        let zombies: Vec<_> = snapshot.processes.iter().filter(|p| p.state == 'Z').collect();
        let names = zombies.iter().take(3).map(|p| format!("{} {}", p.pid, p.name)).collect::<Vec<_>>().join(" · ");
        let more = if zombies.len() > 3 { format!(" 等 {} 个", zombies.len()) } else { String::new() };
        items.push(AttentionItem {
            tag: "进程",
            tone: Tone::Warn,
            title: format!("{} 个僵尸进程", snapshot.zombies),
            detail: format!("{names}{more} · 已结束但父进程没有回收"),
            action_label: "查看进程",
            action: AttentionAction::ShowProcess(zombies.first().map_or(0, |p| p.pid)),
        });
    }
    items
}

/// The line of the attention card when there is nothing to report.
pub const ALL_WELL: &str = "一切正常";

// ---- the cores ----------------------------------------------------------------------------------

/// A bar of the 各核占用 card.
#[derive(Clone, Debug, PartialEq)]
pub struct CoreBar {
    /// `cpu0`, or `cpu0–1` where cores are merged.
    pub name: String,
    /// 0 to 1, or `None` before there is a rate.
    pub fraction: Option<f32>,
    /// `52%`, or `—`.
    pub text: String,
}

/// How many bars the card shows at the most; beyond that cores are merged in groups.
pub const CORE_BARS: usize = 8;

/// The bars for the per-core load: one per core up to [`CORE_BARS`], then groups of neighbours (their
/// average).
pub fn core_bars(per_core: &[Option<f32>]) -> Vec<CoreBar> {
    let n = per_core.len();
    let group = n.div_ceil(CORE_BARS).max(1);
    per_core
        .chunks(group)
        .enumerate()
        .map(|(i, chunk)| {
            let first = i * group;
            let name = if chunk.len() == 1 {
                format!("cpu{first}")
            } else {
                format!("cpu{first}–{}", first + chunk.len() - 1)
            };
            let known: Vec<f32> = chunk.iter().flatten().copied().collect();
            let average = (!known.is_empty()).then(|| known.iter().sum::<f32>() / known.len() as f32);
            CoreBar {
                name,
                fraction: average.map(|v| (v / 100.0).clamp(0.0, 1.0)),
                text: average.map_or_else(|| "—".into(), |v| format!("{:.0}%", v.clamp(0.0, 100.0))),
            }
        })
        .collect()
}

/// `CPU 温度 52°C`, if the server has a sensor.
pub fn temperature_text(snapshot: Option<&Snapshot>) -> Option<String> {
    snapshot.and_then(|s| s.temperature).map(|t| format!("CPU 温度 {t:.0}°C"))
}

// ---- the processes that use the most -------------------------------------------------------------

/// A line of 占用最高.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopProcess {
    /// Process id.
    pub pid: u32,
    /// The command line.
    pub command: String,
    /// `13.8%`, or `—`.
    pub cpu: String,
}

/// The `n` processes that use the most CPU.
pub fn top_processes(snapshot: Option<&Snapshot>, n: usize) -> Vec<TopProcess> {
    snapshot
        .map(|s| {
            s.top_by_cpu(n)
                .into_iter()
                .map(|p| TopProcess {
                    pid: p.pid,
                    command: p.command.clone(),
                    cpu: p.cpu_percent.map_or_else(|| "—".into(), |v| format!("{v:.1}%")),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---- the pages of the next version ---------------------------------------------------------------

/// What the 服务 and Docker pages say until they have more to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// The big line.
    pub title: String,
    /// The explanation.
    pub detail: String,
}

/// The 服务 page: what the server has, or a note that the page comes with the next version.
pub fn services_page(probe: Option<&Probe>) -> Placeholder {
    let Some(probe) = probe else {
        return Placeholder { title: "正在检测这台主机".into(), detail: "稍候…".into() };
    };
    match &probe.systemd {
        Some(systemd) if systemd.running => Placeholder {
            title: "服务管理将在下一版本提供".into(),
            detail: format!(
                "这台主机由 {} 管理服务。\n服务列表、启停与日志会在下一版本加入。",
                systemd.version.map_or_else(|| "systemd".to_string(), |v| format!("systemd {v}"))
            ),
        },
        _ => Placeholder {
            title: "此主机没有 systemd".into(),
            detail: "没有检测到由 systemd 启动的系统（/run/systemd/system 不存在），\n所以没有服务可以管理。".into(),
        },
    }
}

/// The Docker page: the prototype's text when there is no Docker, an explanation when there is but the user
/// cannot use it, and a note that the page comes with the next version when they can.
pub fn docker_page(probe: Option<&Probe>) -> Placeholder {
    let Some(probe) = probe else {
        return Placeholder { title: "正在检测这台主机".into(), detail: "稍候…".into() };
    };
    if let Some(runtime) = probe.usable_container_runtime() {
        return Placeholder {
            title: "Docker 管理将在下一版本提供".into(),
            detail: format!("{}\n容器列表、日志与端口转发会在下一版本加入。", runtime_line(runtime)),
        };
    }
    match probe.containers.first() {
        None => Placeholder {
            title: "此主机未安装 Docker".into(),
            detail: "未检测到 docker / podman 命令，也没有 /var/run/docker.sock\n安装后重新进入此页会自动识别".into(),
        },
        Some(runtime) => match runtime.access {
            ContainerAccess::Denied => Placeholder {
                title: "没有权限使用 Docker".into(),
                detail: "当前用户不能访问 Docker（不在 docker 组），也没有免密 sudo。\n加入 docker 组并重新连接后，重新进入此页会自动识别。".into(),
            },
            ContainerAccess::NoDaemon => Placeholder {
                title: "Docker 没有在运行".into(),
                detail: "找到了 docker 命令，但连不上守护进程。\n启动后重新进入此页会自动识别。".into(),
            },
            _ => Placeholder {
                title: "无法使用 Docker".into(),
                detail: "docker 命令存在，但检查它的版本时出错。\n修好后重新进入此页会自动识别。".into(),
            },
        },
    }
}

fn runtime_line(runtime: &ContainerRuntime) -> String {
    let name = match runtime.kind {
        RuntimeKind::Docker => "Docker",
        RuntimeKind::Podman => "podman",
    };
    let version = runtime.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
    match runtime.access {
        ContainerAccess::Sudo => format!("这台主机有 {name}{version}，需要 sudo -n 才能使用。"),
        _ => format!("这台主机有 {name}{version}，当前用户可以直接使用。"),
    }
}

/// How many points a sparkline has room for (the engine keeps this many).
pub const SPARK_POINTS: usize = HISTORY_POINTS;

#[cfg(test)]
mod tests {
    use shellmate_monitor::probe::{ContainerRuntime, Systemd};
    use shellmate_monitor::snapshot::{CpuCard, Disk, HostFacts, MemoryCard, NetworkCard, Process};

    use super::*;

    fn probe(sudo: Sudo) -> Probe {
        Probe {
            os_name: Some("Ubuntu 24.04.5 LTS".into()),
            kernel: "Linux 6.8.0-45-generic".into(),
            is_linux: true,
            uptime_secs: Some(3.0 * 86_400.0),
            uid: Some(1000),
            user: "tester".into(),
            sudo,
            ..Probe::unknown()
        }
    }

    fn data(probe: Probe, snapshot: Option<Snapshot>, phase: Phase) -> MonitorData {
        MonitorData { phase, probe: Some(Arc::new(probe)), snapshot: snapshot.map(Arc::new) }
    }

    fn process(pid: u32, state: char) -> Process {
        Process {
            pid,
            ppid: 1,
            user: "root".into(),
            name: format!("p{pid}"),
            command: format!("/bin/p{pid}"),
            state,
            cpu_percent: Some(1.0),
            mem_percent: Some(1.0),
            rss_bytes: Some(1 << 20),
            threads: 1,
            nice: 0,
            start_time: 5,
            is_collector: false,
        }
    }

    fn disk(mount: &str, percent: u8) -> Disk {
        Disk {
            mount: mount.into(),
            filesystem: "/dev/sda1".into(),
            total: 200 << 30,
            used: u64::from(percent) * (2 << 30),
            available: u64::from(100 - percent) * (2 << 30),
            percent,
        }
    }

    #[test]
    fn the_chips_say_what_the_user_may_do() {
        assert_eq!(sudo_chip(Sudo::Root), Chip { text: "root 登录".into(), tone: Tone::Ok });
        assert_eq!(sudo_chip(Sudo::Passwordless), Chip { text: "sudo 免密".into(), tone: Tone::Ok });
        assert_eq!(sudo_chip(Sudo::NeedsPassword), Chip { text: "sudo 需密码".into(), tone: Tone::Warn });
        assert_eq!(sudo_chip(Sudo::Unavailable), Chip { text: "无 sudo".into(), tone: Tone::Off });

        let runtime =
            |kind, access| ContainerRuntime { kind, path: "/usr/bin/x".into(), access, version: Some("29.1.3".into()) };
        let with = |containers| Probe { containers, ..probe(Sudo::Root) };
        assert_eq!(docker_chip(&with(vec![])), Chip { text: "无 Docker".into(), tone: Tone::Off });
        assert_eq!(docker_chip(&with(vec![runtime(RuntimeKind::Docker, ContainerAccess::User)])).text, "docker 组");
        assert_eq!(
            docker_chip(&with(vec![runtime(RuntimeKind::Docker, ContainerAccess::Sudo)])).text,
            "Docker 需 sudo"
        );
        assert_eq!(docker_chip(&with(vec![runtime(RuntimeKind::Podman, ContainerAccess::User)])).text, "podman");
        assert_eq!(
            docker_chip(&with(vec![runtime(RuntimeKind::Docker, ContainerAccess::Denied)])).text,
            "Docker 无权限"
        );
        assert_eq!(
            docker_chip(&with(vec![runtime(RuntimeKind::Docker, ContainerAccess::NoDaemon)])).text,
            "Docker 未运行"
        );
        // docker is not usable but podman is: the chip is about the one that works.
        let both = with(vec![
            runtime(RuntimeKind::Docker, ContainerAccess::Denied),
            runtime(RuntimeKind::Podman, ContainerAccess::User),
        ]);
        assert_eq!(docker_chip(&both).text, "podman");

        let mut p = probe(Sudo::Root);
        assert_eq!(systemd_chip(&p), Chip { text: "无 systemd".into(), tone: Tone::Off });
        p.systemd = Some(Systemd { version: Some(255), running: true });
        assert_eq!(systemd_chip(&p).text, "systemd 255");
        p.systemd = Some(Systemd { version: Some(255), running: false });
        assert_eq!(
            systemd_chip(&p).text,
            "无 systemd",
            "systemctl is installed but systemd did not boot the system (a container)"
        );
    }

    #[test]
    fn the_header_of_a_server_with_data() {
        let mut p = probe(Sudo::Passwordless);
        p.systemd = Some(Systemd { version: Some(255), running: true });
        p.containers = vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::User,
            version: Some("29.1.3".into()),
        }];
        let snapshot = Snapshot {
            host: HostFacts {
                uptime_secs: 41.0 * 86_400.0,
                load: Some(shellmate_monitor::parse::LoadAvg {
                    one: 0.42,
                    five: 0.38,
                    fifteen: 0.31,
                    running: None,
                    threads: None,
                }),
                cores: 4,
                ..HostFacts::default()
            },
            processes: vec![process(1, 'S'); 15],
            disks: vec![disk("/data", 82)],
            ..Snapshot::default()
        };
        let header = header(&data(p, Some(snapshot), Phase::Collecting), Page::Overview, "prod");
        assert_eq!(header.os, "Ubuntu 24.04.5 LTS");
        assert_eq!(header.meta, " · 内核 6.8.0-45-generic · 已运行 41 天 · 负载 0.42 0.38 0.31");
        assert_eq!(
            header.chips.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["sudo 免密", "docker 组", "systemd 255", "独立 exec 通道 · 2s"]
        );
        let badges: Vec<_> = header.tabs.iter().map(|t| (t.page, t.active, t.badge.clone())).collect();
        assert_eq!(
            badges,
            [
                (Page::Overview, true, Some(("1".to_string(), Tone::Err))),
                (Page::Processes, false, Some(("15".to_string(), Tone::Plain))),
                (Page::Services, false, None),
                (Page::Docker, false, None),
            ],
            "the pages that exist only in the next version have no number yet, and Ubuntu does not say 未安装"
        );
    }

    #[test]
    fn the_header_before_there_is_anything_and_for_servers_without_systemd_or_docker() {
        let none = header(&MonitorData::empty(), Page::Processes, "prod-web-01");
        assert_eq!(none.os, "prod-web-01");
        assert_eq!(none.meta, "");
        assert!(none.chips.is_empty(), "no chips before the probe");
        assert!(none.tabs.iter().all(|t| t.badge.is_none()));
        assert!(none.tabs[1].active);

        let alpine = data(probe(Sudo::NeedsPassword), None, Phase::Starting);
        let header = header(&alpine, Page::Overview, "x");
        assert_eq!(
            header.chips.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["sudo 需密码", "无 Docker", "无 systemd", "独立 exec 通道 · 2s"]
        );
        assert_eq!(header.tabs[2].badge, Some(("无".into(), Tone::Off)));
        assert_eq!(header.tabs[3].badge, Some(("未安装".into(), Tone::Off)));
        assert_eq!(
            header.meta, " · 内核 6.8.0-45-generic · 已运行 3 天",
            "the uptime of the probe until the first frame"
        );

        // Nothing needs attention: 概览 has no badge (and 0 is not shown), the process count stays.
        let quiet = Snapshot { processes: vec![process(1, 'S'); 3], disks: vec![disk("/", 40)], ..Snapshot::default() };
        let header = super::header(&data(probe(Sudo::Root), Some(quiet), Phase::Collecting), Page::Overview, "x");
        assert_eq!(
            (header.tabs[0].badge.clone(), header.tabs[1].badge.clone()),
            (None, Some(("3".into(), Tone::Plain)))
        );

        let mut denied = probe(Sudo::Unavailable);
        denied.containers = vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::Denied,
            version: None,
        }];
        assert_eq!(
            super::header(&data(denied, None, Phase::Starting), Page::Docker, "x").tabs[3].badge,
            Some(("无权限".into(), Tone::Warn))
        );
        // A server that is not Linux says what it is and has no chips.
        let mac = Probe { is_linux: false, kernel: "Darwin 23.1.0".into(), os_name: None, ..probe(Sudo::Unavailable) };
        let header = super::header(&data(mac, None, Phase::Unsupported("x".into())), Page::Overview, "mbp");
        assert_eq!((header.os.as_str(), header.chips.len()), ("Darwin 23.1.0", 0));
        assert!(!header.meta.contains("内核"), "the kernel is the name already: {}", header.meta);
    }

    #[test]
    fn the_body_follows_the_phase() {
        let snapshot = Some(Snapshot { zombies: 0, ..Snapshot::default() });
        let p = probe(Sudo::Root);
        let live = |phase| body(&data(p.clone(), snapshot.clone(), phase));
        assert_eq!(live(Phase::Collecting), Body::Data { stale: false, banner: None });
        assert_eq!(live(Phase::Starting), Body::Data { stale: false, banner: None });
        let Body::Data { stale: true, banner: Some(waiting) } = live(Phase::WaitingReconnect) else { panic!() };
        assert_eq!((waiting.tone, waiting.retry), (Tone::Warn, false));
        assert!(waiting.text.starts_with("等待重连"), "{}", waiting.text);
        let Body::Data { stale: true, banner: Some(failed) } = live(Phase::Failed("采集脚本意外结束".into()))
        else {
            panic!()
        };
        assert_eq!((failed.tone, failed.retry), (Tone::Err, true));
        assert!(failed.text.contains("采集脚本意外结束"));

        // Without data there are placeholders, or a message.
        let empty = |phase| body(&data(p.clone(), None, phase));
        assert_eq!(empty(Phase::Starting), Body::Data { stale: false, banner: None });
        assert_eq!(
            empty(Phase::WaitingReconnect),
            Body::Message(Message {
                title: "等待重连".into(),
                detail: "连接恢复后会自动继续读取。".into(),
                retry: false
            })
        );
        let Body::Message(message) = empty(Phase::Failed("探测没有得到结果".into())) else { panic!() };
        assert_eq!((message.retry, message.detail.as_str()), (true, "探测没有得到结果"));

        let mac = Probe { is_linux: false, kernel: "Darwin 23.1.0".into(), os_name: None, ..probe(Sudo::Unavailable) };
        let Body::Message(message) =
            body(&data(mac, None, Phase::Unsupported("暂只支持 Linux 主机的监控（Darwin 23.1.0）".into())))
        else {
            panic!()
        };
        assert_eq!(message.title, "暂只支持 Linux 主机的监控");
        assert!(message.detail.contains("Darwin 23.1.0") && !message.retry);
    }

    #[test]
    fn the_cards_have_placeholders_until_there_are_numbers() {
        let cards = cards(None);
        assert_eq!(
            (cards.cpu.title.as_str(), cards.cpu.percent.as_str(), cards.cpu.load.as_str()),
            ("CPU", "—", "负载 —")
        );
        assert_eq!(cards.memory.percent, "—");
        assert!(cards.disks.is_empty() && cards.network.is_none());

        // One sample: the card knows its cores and load but has no rate yet.
        let first = Snapshot {
            host: HostFacts {
                cores: 8,
                load: Some(shellmate_monitor::parse::LoadAvg { one: 1.5, ..Default::default() }),
                ..HostFacts::default()
            },
            cpu: Some(CpuCard { percent: None, per_core: vec![None; 8], history: vec![] }),
            network: Some(NetworkCard { interface: "eth0".into(), ..NetworkCard::default() }),
            ..Snapshot::default()
        };
        let cards = cards_of(&first);
        assert_eq!(
            (cards.cpu.title.as_str(), cards.cpu.percent.as_str(), cards.cpu.load.as_str()),
            ("CPU · 8 核", "—", "负载 1.50")
        );
        let net = cards.network.unwrap();
        assert_eq!((net.title.as_str(), net.down.as_str(), net.up.as_str()), ("网络 · eth0", "—", "—"));
    }

    fn cards_of(snapshot: &Snapshot) -> Cards {
        cards(Some(snapshot))
    }

    #[test]
    fn the_cards_with_numbers() {
        let gib = 1u64 << 30;
        let snapshot = Snapshot {
            host: HostFacts {
                cores: 4,
                load: Some(shellmate_monitor::parse::LoadAvg {
                    one: 0.42,
                    five: 0.4,
                    fifteen: 0.3,
                    running: None,
                    threads: None,
                }),
                ..HostFacts::default()
            },
            cpu: Some(CpuCard { percent: Some(18.6), per_core: vec![Some(52.0); 4], history: vec![10.0, 20.0] }),
            memory: Some(MemoryCard {
                total: 16 * gib,
                used: 10 * gib,
                cache: 3 * gib,
                available: 5 * gib,
                swap_total: 2 * gib,
                swap_used: 128 << 20,
            }),
            disks: vec![disk("/", 64), disk("/data", 82), disk("/boot", 99)],
            network: Some(NetworkCard {
                interface: "eth0".into(),
                rx_per_sec: Some(0.4 * 1_048_576.0),
                tx_per_sec: Some(2.4 * 1_048_576.0),
                rx_history: vec![1.0, 2.0],
                tx_history: vec![3.0],
            }),
            ..Snapshot::default()
        };
        let cards = cards_of(&snapshot);
        assert_eq!((cards.cpu.percent.as_str(), cards.cpu.history.clone()), ("19", vec![10.0, 20.0]));
        assert_eq!(cards.memory.total, "10 / 16 GB");
        assert_eq!(cards.memory.percent, "62");
        assert!(
            (cards.memory.used - 0.625).abs() < 1e-6 && (cards.memory.cache - 0.1875).abs() < 1e-6,
            "{:?}",
            cards.memory
        );
        assert_eq!(cards.memory.legend, ["已用 10.0 GB", "缓存 3.0 GB", "Swap 128 MB"]);
        assert_eq!(
            cards.disks.iter().map(|d| (d.mount.as_str(), d.tone)).collect::<Vec<_>>(),
            [("/", Tone::Plain), ("/data", Tone::Warn), ("/boot", Tone::Err)]
        );
        assert_eq!(cards.disks[1].text, "164 / 200 GB");
        assert!((cards.disks[1].fraction - 0.82).abs() < 1e-6);
        let net = cards.network.unwrap();
        assert_eq!((net.unit, net.down.as_str(), net.up.as_str()), ("MB/s", "0.4", "2.4"));

        let no_swap = Snapshot {
            memory: Some(MemoryCard { total: gib, swap_total: 0, ..Default::default() }),
            ..Snapshot::default()
        };
        assert_eq!(cards_of(&no_swap).memory.legend[2], "无 Swap");
        // Never more rows than the card has room for.
        let many = Snapshot { disks: (0..9).map(|i| disk(&format!("/d{i}"), 10)).collect(), ..Snapshot::default() };
        assert_eq!(cards_of(&many).disks.len(), DISK_ROWS);
    }

    #[test]
    fn attention_lists_full_disks_and_zombies() {
        assert!(attention(None).is_empty());
        let clean = Snapshot { disks: vec![disk("/", 79)], processes: vec![process(1, 'S')], ..Snapshot::default() };
        assert!(attention(Some(&clean)).is_empty(), "79 % is fine");

        let snapshot = Snapshot {
            disks: vec![disk("/", 80), disk("/data", 82), disk("/var", 96), disk("/boot", 10)],
            processes: vec![process(5120, 'Z'), process(5121, 'Z'), process(1, 'S')],
            zombies: 2,
            ..Snapshot::default()
        };
        let items = attention(Some(&snapshot));
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            ["/var 已用 96%", "/data 已用 82%", "/ 已用 80%", "2 个僵尸进程"],
            "the fullest disk first, then the zombies"
        );
        assert_eq!((items[0].tone, items[1].tone, items[3].tone), (Tone::Err, Tone::Warn, Tone::Warn));
        assert_eq!(items[0].tag, "磁盘");
        assert_eq!(items[0].action_label, "分析目录");
        assert_eq!(items[0].action, AttentionAction::AnalyzeDisk("/var".into()));
        assert!(items[0].detail.starts_with("已用 192 GB / 200 GB · 可用 8.0 GB"), "{}", items[0].detail);
        assert_eq!(items[3].action, AttentionAction::ShowProcess(5120));
        assert!(
            items[3].detail.contains("5120 p5120") && items[3].detail.contains("5121 p5121"),
            "{}",
            items[3].detail
        );
        assert_eq!(attention_count(Some(&snapshot)), 4);
        // Many zombies: three are named.
        let many =
            Snapshot { processes: (1..=7).map(|pid| process(pid, 'Z')).collect(), zombies: 7, ..Snapshot::default() };
        assert!(attention(Some(&many))[0].detail.contains("等 7 个"), "{}", attention(Some(&many))[0].detail);
    }

    #[test]
    fn cores_are_shown_one_by_one_up_to_eight_and_then_merged() {
        let bars = core_bars(&[Some(52.0), Some(34.0), None, Some(20.4)]);
        assert_eq!(
            bars.iter().map(|b| (b.name.as_str(), b.text.as_str())).collect::<Vec<_>>(),
            [("cpu0", "52%"), ("cpu1", "34%"), ("cpu2", "—"), ("cpu3", "20%")]
        );
        assert_eq!(bars[2].fraction, None, "no bar before there is a rate");
        assert!((bars[0].fraction.unwrap() - 0.52).abs() < 1e-6);
        assert_eq!(core_bars(&[Some(1.0); 8]).len(), 8);
        // 16 cores: eight bars of two. 12 cores: six bars of two. 9 cores: five bars of two (and one of one).
        let sixteen: Vec<Option<f32>> = (0..16).map(|i| Some(i as f32 * 6.0)).collect();
        let bars = core_bars(&sixteen);
        assert_eq!(bars.len(), 8);
        assert_eq!((bars[0].name.as_str(), bars[0].text.as_str()), ("cpu0–1", "3%"));
        assert_eq!((bars[7].name.as_str(), bars[7].text.as_str()), ("cpu14–15", "87%"));
        assert_eq!(core_bars(&[None; 9]).len(), 5);
        assert_eq!(core_bars(&[None; 9]).last().unwrap().name, "cpu8");
        assert!(core_bars(&[]).is_empty());
        // A group with some cores unknown averages the known ones.
        assert_eq!(
            core_bars(&[
                Some(10.0),
                None,
                Some(30.0),
                Some(50.0),
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(0.0)
            ])[0]
                .text,
            "10%"
        );
    }

    #[test]
    fn temperature_only_when_there_is_a_sensor() {
        assert_eq!(temperature_text(None), None);
        assert_eq!(temperature_text(Some(&Snapshot::default())), None);
        assert_eq!(
            temperature_text(Some(&Snapshot { temperature: Some(52.4), ..Snapshot::default() })).as_deref(),
            Some("CPU 温度 52°C")
        );
    }

    #[test]
    fn the_busiest_processes() {
        let mut busy = process(2207, 'R');
        busy.cpu_percent = Some(11.5);
        busy.command = "node /var/www/app/dist/server.js".into();
        let mut new = process(3000, 'S');
        new.cpu_percent = None;
        let mut own = process(99, 'S');
        own.cpu_percent = Some(50.0);
        own.is_collector = true;
        let snapshot = Snapshot { processes: vec![process(1, 'S'), busy, own, new], ..Snapshot::default() };
        let top = top_processes(Some(&snapshot), 4);
        assert_eq!(
            top.iter().map(|t| t.pid).collect::<Vec<_>>(),
            [2207, 1, 3000],
            "the monitor's own collector is not among them, and a process without a rate is last"
        );
        assert_eq!((top[0].cpu.as_str(), top[2].cpu.as_str()), ("11.5%", "—"));
        assert_eq!(top_processes(Some(&snapshot), 1).len(), 1);
        assert!(top_processes(None, 4).is_empty());
    }

    #[test]
    fn the_pages_of_the_next_version_say_what_is_true_of_the_server() {
        let mut ubuntu = probe(Sudo::Passwordless);
        ubuntu.systemd = Some(Systemd { version: Some(255), running: true });
        ubuntu.containers = vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::User,
            version: Some("29.1.3".into()),
        }];
        let services = services_page(Some(&ubuntu));
        assert_eq!(services.title, "服务管理将在下一版本提供");
        assert!(services.detail.contains("systemd 255"));
        let docker = docker_page(Some(&ubuntu));
        assert_eq!(docker.title, "Docker 管理将在下一版本提供");
        assert!(docker.detail.contains("Docker 29.1.3") && docker.detail.contains("直接使用"), "{}", docker.detail);
        assert!(!docker.title.contains("未安装") && !docker.detail.contains("未检测到"), "Ubuntu has Docker");

        let alpine = probe(Sudo::NeedsPassword);
        assert_eq!(services_page(Some(&alpine)).title, "此主机没有 systemd");
        let docker = docker_page(Some(&alpine));
        assert_eq!(docker.title, "此主机未安装 Docker");
        assert_eq!(
            docker.detail, "未检测到 docker / podman 命令，也没有 /var/run/docker.sock\n安装后重新进入此页会自动识别",
            "the prototype's words"
        );

        let mut sudo_only = probe(Sudo::Passwordless);
        sudo_only.containers = vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::Sudo,
            version: Some("27".into()),
        }];
        assert!(docker_page(Some(&sudo_only)).detail.contains("sudo -n"));
        let mut denied = probe(Sudo::Unavailable);
        denied.containers = vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::Denied,
            version: None,
        }];
        assert_eq!(docker_page(Some(&denied)).title, "没有权限使用 Docker");
        denied.containers[0].access = ContainerAccess::NoDaemon;
        assert_eq!(docker_page(Some(&denied)).title, "Docker 没有在运行");
        assert_eq!(docker_page(None).title, "正在检测这台主机");
        assert_eq!(services_page(None).title, "正在检测这台主机");
    }

    #[test]
    fn sparklines_scale_to_what_they_hold() {
        assert_eq!(scale_to(&[1.0, 5.0, 3.0], 2.0), 5.0);
        assert_eq!(scale_to(&[0.1], 2.0), 2.0, "a trickle does not fill the card");
        assert!(scale_to(&[], 0.0) > 0.0, "never a zero to divide by");
    }
}
