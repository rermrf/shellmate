//! Adversarial but valid server values, always in memory with inert backends.
use crate::monitor::model::MonitorData;
use std::sync::Arc;

pub const HOST: &str = "production-eu-west-01-observability-and-payments-primary.example.internal";
pub const USER: &str = "serviceaccount16";
pub const SERVICE: &str = "systemd-networkd-wait-online-production-infrastructure.service";
pub const CONTAINER: &str = "payments-production-background-reconciliation-worker-20261112";
pub const PATH: &str =
    "/srv/production/客户数据平台/releases/2026.11.12-abcdef0123456789/configuration/services/observability";
pub const FILE: &str = "production-observability-客户数据平台-configuration-2026.11.12-abcdef0123456789.yaml";

pub fn monitor(scene: &str, dark: bool) -> MonitorData {
    let mut model = super::monitor::data(scene, dark);
    let snapshot = Arc::make_mut(model.snapshot.as_mut().unwrap());
    snapshot.host.os_name = Some("Ubuntu 22.04.4 LTS — Enterprise Infrastructure Long Term Support".into());
    snapshot.host.kernel = "Linux 5.15.0-198-generic-production-hardened".into();
    if let Some(probe) = model.probe.as_mut() {
        let probe = Arc::make_mut(probe);
        probe.os_name = snapshot.host.os_name.clone();
        probe.kernel = snapshot.host.kernel.clone();
    }
    snapshot.host.uptime_secs = 391. * 86400.;
    snapshot.host.cores = 16;
    if let Some(load) = snapshot.host.load.as_mut() {
        load.one = 12.95;
        load.five = 10.36;
        load.fifteen = 8.13;
    }
    if let Some(cpu) = snapshot.cpu.as_mut() {
        cpu.per_core = (0..16).map(|n| Some(5. + n as f32 * 6.)).collect();
    }
    if let Some(mem) = snapshot.memory.as_mut() {
        mem.total = 2 << 40;
        mem.used = 1_319_413_953_331;
        mem.cache = 300 << 30;
    }
    if let Some(net) = snapshot.network.as_mut() {
        net.rx_per_sec = Some(12.5 * (1u64 << 30) as f64);
        net.tx_per_sec = Some(123456.);
    }
    snapshot.disks[1].mount = "/srv/production/客户数据平台/persistent-volumes/database-primary".into();
    snapshot.disks[1].total = 8 << 40;
    snapshot.disks[1].used = 7 << 40;
    if scene == "mon-ov" {
        snapshot.disks[1].available = 1 << 40;
        snapshot.disks[1].percent = 88;
        snapshot.disks.extend([("/var/lib/containers", 94), ("/srv/backups", 82), ("/opt", 35)]
            .map(|(mount, percent)| shellmate_monitor::Disk {
                filesystem: format!("/dev/mapper/vol-{percent}"), mount: mount.into(), percent,
                total: 100 << 30, used: u64::from(percent) << 30, available: u64::from(100-percent) << 30,
            }));
    }
    for (i, p) in snapshot.processes.iter_mut().enumerate() {
        p.pid = 4_194_304 - i as u32;
        p.ppid = 1;
        p.start_time = u64::from(p.pid) * 100;
        p.user = if i % 2 == 0 { USER.into() } else { "an-even-longer-service-account-name".into() };
        p.command = format!(
            "/opt/production/releases/2026.11.12-abcdef0123456789/bin/reconciliation-worker --config {PATH}/{FILE} --tenant customer-{i}"
        );
        p.cpu_percent = Some(1600. - i as f32 * 80.);
        p.rss_bytes = Some(1_319_413_953_331);
        if i == 2 {
            p.state = 'Z';
        }
    }
    snapshot.zombies = 1;
    snapshot.processes.sort_by_key(|p| p.pid);
    let services = Arc::make_mut(model.services.as_mut().unwrap());
    for (i, s) in services.services.iter_mut().enumerate() {
        s.name = if i == 0 {
            SERVICE.into()
        } else {
            format!("production-very-long-service-{i}-customer-data-platform.service")
        };
        s.description="Wait for network connectivity and initialize the production customer data reconciliation and observability platform".into();
        s.main_pid = Some(4_194_304);
        s.memory = Some(1_319_413_953_331);
        s.cpu_nsec = Some(9_876_543_210_000_000);
    }
    let docker = Arc::make_mut(model.docker.as_mut().unwrap());
    for (i, c) in docker.containers.iter_mut().enumerate() {
        c.name = if i == 0 { CONTAINER.into() } else { format!("production-infrastructure-very-long-container-{i}") };
        c.image = "registry.example.com:5000/team/customer-data-platform/app:2026.11.12-abcdef0123456789".into();
        c.project = Some("production-infrastructure-observability-customer-data-platform".into());
        c.config_files = Some(format!("{PATH}/docker-compose.production.override.yaml"));
        c.mem_used = Some(1_319_413_953_331);
        c.memory_limit = Some(2 << 40);
        c.cpu_percent = Some(1599.9);
        c.ports = (0..8)
            .map(|n| shellmate_monitor::docker::PortMapping {
                host_ip: Some("2001:db8:abcd:1234::100".into()),
                host_port: Some(60000 + n),
                container_port: 8000 + n,
                protocol: "tcp".into(),
            })
            .collect();
    }
    model
}

pub fn forward() -> crate::forward::model::ForwardView {
    let mut view = crate::forward::demo::view("states", HOST);
    for (i, card) in view.cards.iter_mut().enumerate() {
        card.name = format!("Production database replication and observability tunnel — region-{i} 客户数据平台");
        card.route =
            "[2001:db8:abcd:1234::100]:65535 → database-primary.production.eu-west.example.internal:65535".into();
        card.traffic = Some("↓ 1.2 TB · ↑ 999.9 GB · 123456 个连接".into());
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stress_fixture_contains_the_reported_edge_cases() {
        let model = monitor("mon-ov", false);
        let snapshot = model.snapshot.unwrap();
        assert_eq!(snapshot.host.cores, 16);
        let cards = crate::monitor::model::cards(Some(&snapshot));
        assert_eq!((snapshot.disks.len(), cards.hidden_disks), (6, 3));
        assert_eq!(cards.disks[1].mount, "/var/lib/containers");
        assert!(snapshot.process(4194304).is_some());
        assert!(snapshot.processes.iter().all(|p| p.pid >= 1_000_000 && p.command.len() > 200));
        assert!(model.services.unwrap().get(SERVICE).is_some());
        assert!(model.docker.unwrap().containers.iter().all(|c| c.ports.len() == 8 && c.image.len() > 70));
    }
}
