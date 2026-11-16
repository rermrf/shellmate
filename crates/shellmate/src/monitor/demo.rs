//! Debug-only review data: a server made up for the screenshots of every state the monitor page can be in,
//! without a connection to it (`SHELLMATE_DEMO=monitor:<host>:<scenario>`, see `demo::run`).

use std::sync::Arc;

use shellmate_monitor::docker::{
    Container, ContainerNetwork, ContainerState, Dangling, DockerView, PortMapping, Usage,
};
use shellmate_monitor::parse::LoadAvg;
use shellmate_monitor::probe::{ContainerAccess, ContainerRuntime, RuntimeKind, Systemd};
use shellmate_monitor::services::{ActiveState, Autostart, LogLevel, LogLine, Service, ServiceList};
use shellmate_monitor::snapshot::{CpuCard, Disk, HostFacts, MemoryCard, NetworkCard, Process};
use shellmate_monitor::{Phase, Probe, Snapshot, Sudo};

use super::model::{ForwardInfo, MonitorData};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The probe of the prototype's server (Ubuntu with sudo, docker and systemd).
pub fn probe(sudo: Sudo) -> Probe {
    Probe {
        os_name: Some("Ubuntu 22.04.4 LTS".into()),
        kernel: "Linux 6.8.0-45".into(),
        is_linux: true,
        uptime_secs: Some(41.0 * 86_400.0),
        uid: Some(1001),
        user: "deploy".into(),
        sudo,
        systemd: Some(Systemd { version: Some(249), running: true }),
        containers: vec![ContainerRuntime {
            kind: RuntimeKind::Docker,
            path: "/usr/bin/docker".into(),
            access: ContainerAccess::User,
            version: Some("27.3.1".into()),
        }],
        ..Probe::unknown()
    }
}

fn process(pid: u32, ppid: u32, user: &str, cpu: f32, rss_mb: u64, state: char, command: &str) -> Process {
    let rss = rss_mb * MIB;
    Process {
        pid,
        ppid,
        user: user.into(),
        name: command
            .split_whitespace()
            .next()
            .unwrap_or(command)
            .rsplit('/')
            .next()
            .unwrap_or(command)
            .trim_end_matches(':')
            .into(),
        command: command.into(),
        state,
        cpu_percent: Some(cpu),
        mem_percent: Some((rss as f64 / (15.6 * GIB as f64) * 100.0) as f32),
        rss_bytes: Some(rss),
        threads: 1,
        nice: 0,
        start_time: u64::from(pid) * 100,
    }
}

/// The processes of the prototype's table.
pub fn processes() -> Vec<Process> {
    let mut list = vec![
        process(1, 0, "root", 0.7, 12, 'S', "/sbin/init"),
        process(402, 1, "root", 1.1, 58, 'S', "/lib/systemd/systemd-journald"),
        process(648, 1, "root", 0.1, 9, 'S', "/usr/sbin/sshd -D"),
        process(912, 1, "root", 1.8, 188, 'S', "/usr/bin/containerd"),
        process(1020, 1, "root", 2.3, 280, 'S', "/usr/bin/dockerd -H fd://"),
        process(1184, 1, "root", 0.2, 14, 'S', "nginx: master process /usr/sbin/nginx"),
        process(1185, 1184, "www-data", 10.5, 96, 'S', "nginx: worker process"),
        process(1186, 1184, "www-data", 13.0, 91, 'S', "nginx: worker process"),
        process(2207, 1, "deploy", 11.5, 532, 'R', "node /var/www/app/dist/server.js"),
        process(2290, 3381, "deploy", 0.0, 148, 'S', "node /var/www/app/dist/queue.js"),
        process(3381, 1, "deploy", 2.0, 70, 'S', "PM2 v5.3.1: God Daemon"),
        process(4410, 648, "root", 0.1, 11, 'S', "sshd: deploy [priv]"),
        process(4415, 4410, "deploy", 0.0, 7, 'S', "sshd: deploy@pts/0"),
        process(4416, 4415, "deploy", 0.2, 5, 'S', "-bash"),
        process(5120, 912, "root", 0.0, 0, 'Z', "[php-fpm8.2] <defunct>"),
    ];
    list.sort_by_key(|p| p.pid);
    list
}

fn wave(base: f64, amplitude: f64, phase: f64) -> Vec<f64> {
    (0..60)
        .map(|i| (base + amplitude * ((i as f64 * 0.21 + phase).sin() + 0.4 * (i as f64 * 0.7 + phase).sin())).max(0.0))
        .collect()
}

/// The numbers of the prototype's overview.
pub fn snapshot() -> Snapshot {
    let cpu: Vec<f32> = wave(24.0, 9.0, 0.5).into_iter().map(|v| v.min(100.0) as f32).collect();
    Snapshot {
        host: HostFacts {
            os_name: Some("Ubuntu 22.04.4 LTS".into()),
            kernel: "Linux 6.8.0-45".into(),
            uptime_secs: 41.0 * 86_400.0,
            load: Some(LoadAvg { one: 0.42, five: 0.38, fifteen: 0.31, running: Some(1), threads: Some(310) }),
            cores: 4,
        },
        cpu: Some(CpuCard {
            percent: Some(19.0),
            per_core: vec![Some(52.0), Some(34.0), Some(42.0), Some(20.0)],
            history: cpu,
        }),
        memory: Some(MemoryCard {
            total: 15_974 * MIB,
            used: 9_830 * MIB,
            cache: 2_870 * MIB,
            available: 5_000 * MIB,
            swap_total: 2 * GIB,
            swap_used: 128 * MIB,
        }),
        disks: vec![
            Disk {
                mount: "/".into(),
                filesystem: "/dev/vda1".into(),
                total: 79 * GIB,
                used: 51 * GIB,
                available: 28 * GIB,
                percent: 65,
            },
            Disk {
                mount: "/data".into(),
                filesystem: "/dev/vdb1".into(),
                total: 200 * GIB,
                used: 164 * GIB,
                available: 36 * GIB,
                percent: 82,
            },
            Disk {
                mount: "/boot".into(),
                filesystem: "/dev/vda15".into(),
                total: GIB,
                used: 200 * MIB,
                available: 800 * MIB,
                percent: 20,
            },
        ],
        network: Some(NetworkCard {
            interface: "eth0".into(),
            rx_per_sec: Some(0.4 * MIB as f64),
            tx_per_sec: Some(2.4 * MIB as f64),
            rx_history: wave(0.8 * MIB as f64, 0.5 * MIB as f64, 1.0),
            tx_history: wave(0.5 * MIB as f64, 0.25 * MIB as f64, 2.0),
        }),
        temperature: Some(52.0),
        processes: processes(),
        zombies: 1,
    }
}

/// The data for a scenario: `overview` (the prototype's server), `waiting` (the connection is gone and
/// the last numbers are grey), `failed` (the monitor gave up), `first` (the first sample: no rates yet),
/// `starting` (nothing yet), `quiet` (nothing needs attention), `many-cores` (16 cores), `notlinux`,
/// `nosudo` (a server that wants a password for sudo) and `alpine` (no systemd, no docker).
pub fn data(scenario: &str) -> MonitorData {
    let mut probe = probe(Sudo::Passwordless);
    let mut snapshot = snapshot();
    let mut phase = Phase::Collecting;
    match scenario {
        "waiting" => phase = Phase::WaitingReconnect,
        "failed" => phase = Phase::Failed("采集脚本意外结束：sh: 1: cut: not found".into()),
        "first" => {
            snapshot.cpu = Some(CpuCard { percent: None, per_core: vec![None; 4], history: Vec::new() });
            snapshot.network = snapshot.network.map(|n| NetworkCard {
                rx_per_sec: None,
                tx_per_sec: None,
                rx_history: Vec::new(),
                tx_history: Vec::new(),
                ..n
            });
            for p in &mut snapshot.processes {
                p.cpu_percent = None;
            }
        }
        "starting" => {
            return MonitorData { phase: Phase::Starting, probe: Some(Arc::new(probe)), ..MonitorData::empty() };
        }
        "quiet" => {
            snapshot.disks[1] = Disk { percent: 40, used: 80 * GIB, available: 120 * GIB, ..snapshot.disks[1].clone() };
            snapshot.processes.retain(|p| p.state != 'Z');
            snapshot.zombies = 0;
        }
        "many-cores" => {
            snapshot.host.cores = 16;
            snapshot.cpu =
                snapshot.cpu.map(|c| CpuCard { per_core: (0..16).map(|i| Some(5.0 + i as f32 * 5.5)).collect(), ..c });
        }
        "notlinux" => {
            probe.is_linux = false;
            probe.kernel = "Darwin 23.1.0".into();
            probe.os_name = None;
            return MonitorData {
                phase: Phase::Unsupported("暂只支持 Linux 主机的监控（Darwin 23.1.0）".into()),
                probe: Some(Arc::new(probe)),
                ..MonitorData::empty()
            };
        }
        "nosudo" => probe.sudo = Sudo::NeedsPassword,
        "alpine" => {
            probe.os_name = Some("Alpine Linux v3.24".into());
            probe.systemd = None;
            probe.containers.clear();
            probe.sudo = Sudo::NeedsPassword;
            return MonitorData {
                phase,
                probe: Some(Arc::new(probe)),
                snapshot: Some(Arc::new(snapshot)),
                ..MonitorData::empty()
            };
        }
        _ => {}
    }
    let forwards = match scenario {
        // A forward to the port of the cache container exists already.
        "forwarded" => vec![ForwardInfo {
            listen: "127.0.0.1:6379".into(),
            target_host: "172.18.0.3".into(),
            target_port: 6379,
            enabled: true,
        }],
        _ => Vec::new(),
    };
    let (services, docker) = match scenario {
        "quiet" => (Some(Arc::new(quiet_services())), Some(Arc::new(quiet_containers()))),
        "services-only" => (Some(Arc::new(services())), None),
        _ => (Some(Arc::new(services())), Some(Arc::new(containers()))),
    };
    MonitorData {
        phase,
        probe: Some(Arc::new(probe)),
        snapshot: Some(Arc::new(snapshot)),
        services,
        docker,
        forwards: Arc::new(forwards),
    }
}

fn service(name: &str, description: &str, active: &str, sub: &str, enabled: bool, memory_mb: Option<u64>) -> Service {
    Service {
        name: name.into(),
        description: description.into(),
        active: ActiveState::parse(active),
        sub: sub.into(),
        load: "loaded".into(),
        autostart: if enabled { Autostart::Enabled } else { Autostart::Disabled },
        main_pid: memory_mb.map(|mb| 1000 + mb as u32),
        memory: memory_mb.map(|mb| mb * MIB),
        cpu_nsec: memory_mb.map(|mb| mb * 61_000_000),
        since_monotonic_us: None,
        since_text: Some("09:49:12".into()),
        result: None,
        exit_status: None,
        last_error: None,
    }
}

/// The services of the prototype's server.
pub fn services() -> ServiceList {
    let mut php = service("php8.2-fpm.service", "The PHP 8.2 FastCGI Process Manager", "failed", "failed", true, None);
    php.result = Some("exit-code".into());
    php.exit_status = Some(78);
    php.last_error = Some("ERROR: [pool www] cannot get uid for user 'www-data2'".into());
    let mut nginx = service(
        "nginx.service",
        "A high performance web server and a reverse proxy server",
        "active",
        "running",
        true,
        Some(38),
    );
    nginx.main_pid = Some(1185);
    nginx.memory = Some(38 * MIB + 200 * 1024);
    let mut snapd = service("snapd.service", "Snap Daemon", "inactive", "dead", false, None);
    snapd.autostart = Autostart::Fixed;
    ServiceList {
        services: vec![
            php,
            service("cron.service", "Regular background program processing daemon", "active", "running", true, Some(2)),
            service("containerd.service", "containerd container runtime", "active", "running", true, Some(188)),
            service("docker.service", "Docker Application Container Engine", "active", "running", true, Some(412)),
            service("fail2ban.service", "Fail2Ban Service", "active", "running", true, Some(31)),
            nginx,
            service("pm2-deploy.service", "PM2 process manager", "active", "running", true, Some(70)),
            snapd,
            service("ssh.service", "OpenBSD Secure Shell server", "active", "running", true, Some(9)),
            service("unattended-upgrades.service", "Unattended Upgrades Shutdown", "inactive", "dead", true, None),
        ],
    }
}

fn quiet_services() -> ServiceList {
    let mut list = services();
    list.services.retain(|service| !service.is_failed());
    list
}

fn port(host_ip: Option<&str>, host_port: Option<u16>, container_port: u16) -> PortMapping {
    PortMapping { host_ip: host_ip.map(str::to_string), host_port, container_port, protocol: "tcp".into() }
}

#[allow(clippy::too_many_arguments)]
fn container(
    name: &str,
    image: &str,
    project: Option<(&str, &str)>,
    state: ContainerState,
    status: &str,
    ports: Vec<PortMapping>,
    cpu: Option<f64>,
    mem_mb: Option<u64>,
    ip: &str,
) -> Container {
    Container {
        id: format!("{name}-id"),
        name: name.into(),
        image: image.into(),
        state,
        status: status.into(),
        ports,
        project: project.map(|(name, _)| name.to_string()),
        config_files: project.map(|(_, files)| files.to_string()),
        service: None,
        restart_count: 0,
        oom_killed: false,
        restarting: false,
        memory_limit: None,
        networks: vec![ContainerNetwork { name: "bridge".into(), ip: ip.into() }],
        up_secs: None,
        cpu_percent: cpu,
        mem_used: mem_mb.map(|mb| mb * MIB),
    }
}

/// The containers of the prototype's server.
pub fn containers() -> DockerView {
    let app = ("app", "/var/www/app/docker-compose.yml");
    let monitoring = ("monitoring", "/opt/monitoring/compose.yml");
    let mut web = container(
        "app",
        "app:2026.11.12",
        Some(app),
        ContainerState::Running,
        "Up 11 hours",
        vec![port(Some("127.0.0.1"), Some(3000), 3000)],
        Some(4.8),
        Some(412),
        "172.18.0.2",
    );
    web.memory_limit = Some(GIB);
    web.up_secs = Some(11 * 3600);
    let cache = container(
        "cache",
        "redis:7-alpine",
        Some(app),
        ContainerState::Running,
        "Up 6 days",
        vec![port(None, None, 6379)],
        Some(1.2),
        Some(24),
        "172.18.0.3",
    );
    let mut worker = container(
        "worker",
        "app:2026.11.12",
        Some(app),
        ContainerState::Restarting,
        "Restarting (137) 3 seconds ago",
        Vec::new(),
        None,
        None,
        "",
    );
    worker.restart_count = 6;
    worker.oom_killed = true;
    worker.restarting = true;
    worker.memory_limit = Some(256 * MIB);
    worker.networks.clear();
    let grafana = container(
        "grafana",
        "grafana/grafana:11.3.0",
        Some(monitoring),
        ContainerState::Running,
        "Up 6 days",
        vec![port(Some("127.0.0.1"), Some(3001), 3000)],
        Some(1.0),
        Some(96),
        "172.19.0.2",
    );
    let prometheus = container(
        "prometheus",
        "prom/prometheus:v2.55",
        Some(monitoring),
        ContainerState::Running,
        "Up 6 days",
        vec![port(None, None, 9090)],
        Some(1.2),
        Some(188),
        "172.19.0.3",
    );
    let old = container(
        "old-api",
        "app:2026.09.30",
        None,
        ContainerState::Exited,
        "Exited (0) 2 weeks ago",
        Vec::new(),
        None,
        None,
        "",
    );
    DockerView {
        runtime: RuntimeKind::Docker,
        version: Some("27.3.1".into()),
        containers: vec![old, web, cache, worker, grafana, prometheus],
        usage: Usage { images: Some(14), images_size: Some(3_200_000_000), volumes: Some(6) },
        dangling: Dangling { count: 4, bytes: 1_240_000_000 },
        error: None,
    }
}

fn quiet_containers() -> DockerView {
    let mut view = containers();
    view.containers.retain(|c| c.name != "worker");
    view
}

fn line(time: &str, text: &str, level: LogLevel) -> LogLine {
    LogLine { time: time.into(), text: text.into(), level }
}

/// The journal of the failed unit of the prototype.
pub fn service_log() -> Vec<LogLine> {
    vec![
        line("09:49:11", "Starting php8.2-fpm.service...", LogLevel::Plain),
        line("09:49:12", "ERROR: [pool www] cannot get uid for user 'www-data2'", LogLevel::Error),
        line("09:49:12", "ERROR: FPM initialization failed", LogLevel::Error),
        line("09:49:12", "Main process exited, code=exited, status=78/CONFIG", LogLevel::Error),
        line("09:49:12", "Failed to start php8.2-fpm.service.", LogLevel::Error),
    ]
}

/// The last lines of the container that is killed for its memory.
pub fn container_log() -> Vec<LogLine> {
    vec![
        line("09:51:40", "[queue] consuming jobs from redis://cache:6379", LogLevel::Plain),
        line("09:51:52", "[export] building report 2026-11 (48,211 rows)", LogLevel::Plain),
        line("09:51:58", "<--- Last few GCs --->  heap limit reached", LogLevel::Warning),
        line(
            "09:51:58",
            "FATAL ERROR: Reached heap limit Allocation failed - JavaScript heap out of memory",
            LogLevel::Error,
        ),
        line("09:51:59", "container killed: OOMKilled (exit 137)", LogLevel::Error),
    ]
}
