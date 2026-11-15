//! Debug-only review data: a server made up for the screenshots of every state the monitor page can be in,
//! without a connection to it (`SHELLMATE_DEMO=monitor:<host>:<scenario>`, see `demo::run`).

use std::sync::Arc;

use shellmate_monitor::parse::LoadAvg;
use shellmate_monitor::probe::{ContainerAccess, ContainerRuntime, RuntimeKind, Systemd};
use shellmate_monitor::snapshot::{CpuCard, Disk, HostFacts, MemoryCard, NetworkCard, Process};
use shellmate_monitor::{Phase, Probe, Snapshot, Sudo};

use super::model::MonitorData;

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
        is_collector: false,
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
            return MonitorData { phase: Phase::Starting, probe: Some(Arc::new(probe)), snapshot: None };
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
                snapshot: None,
            };
        }
        "nosudo" => probe.sudo = Sudo::NeedsPassword,
        "alpine" => {
            probe.os_name = Some("Alpine Linux v3.24".into());
            probe.systemd = None;
            probe.containers.clear();
            probe.sudo = Sudo::NeedsPassword;
        }
        _ => {}
    }
    MonitorData { phase, probe: Some(Arc::new(probe)), snapshot: Some(Arc::new(snapshot)) }
}
