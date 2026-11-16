//! Map the prototype data into the real monitoring model.
use super::{DATA, bytes, reading};
use crate::monitor::{demo, model::MonitorData};
use shellmate_monitor::{
    services::{ActiveState, Autostart, Service, ServiceList},
    snapshot::Process,
};
use std::sync::Arc;
fn seconds(text: &str) -> u64 {
    text.split_whitespace()
        .map(|s| {
            let n = s
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0);
            n * if s.contains("min") {
                60
            } else if s.contains('h') {
                3600
            } else if s.contains('d') {
                86400
            } else {
                1
            }
        })
        .sum()
}
pub fn data(scene: &str, dark: bool) -> MonitorData {
    let readings = reading(scene, dark);
    let mut data = demo::data("overview");
    let mut snapshot = demo::snapshot();
    snapshot.memory.as_mut().unwrap().total = (15.6 * (1u64 << 30) as f64) as u64;
    snapshot.memory.as_mut().unwrap().used = ((readings.memory_gib - 0.04) * (1u64 << 30) as f64) as u64;
    snapshot.memory.as_mut().unwrap().cache = (2.8 * (1u64 << 30) as f64) as u64;
    snapshot.cpu.as_mut().unwrap().percent = Some(readings.cpu);
    snapshot.cpu.as_mut().unwrap().per_core = readings.cores.into_iter().map(Some).collect();
    snapshot.network.as_mut().unwrap().rx_per_sec = Some(readings.net[0] * (1u64 << 20) as f64);
    snapshot.network.as_mut().unwrap().tx_per_sec = Some(readings.net[1] * (1u64 << 20) as f64);
    snapshot.disks[0].percent = 67;
    snapshot.disks[2].percent = 24;
    snapshot.processes = DATA["PROCS"]
        .a()
        .iter()
        .map(|p| {
            let pid = p["pid"].n() as u32;
            Process {
                pid,
                ppid: p["pp"].n() as u32,
                user: p["u"].s().into(),
                name: p["cmd"].s().split_whitespace().next().unwrap_or("").into(),
                command: p["cmd"].s().into(),
                state: p["st"].s().chars().next().unwrap_or('S'),
                cpu_percent: Some(
                    readings
                        .processes
                        .iter()
                        .find(|(id, _)| *id == pid)
                        .map_or(p["cpu"].n() as f32, |(_, cpu)| *cpu),
                ),
                mem_percent: Some(p["mem"].n() as f32),
                rss_bytes: Some(bytes(p["rss"].s())),
                threads: 1,
                nice: 0,
                start_time: u64::from(pid) * 100,
            }
        })
        .collect();
    snapshot.zombies = 0; // The captured attention list has services, containers, and disk only.
    data.snapshot = Some(Arc::new(snapshot));
    let services = DATA["SERVICES"]
        .a()
        .iter()
        .map(|v| Service {
            name: v["n"].s().into(),
            description: v["d"].s().into(),
            load: "loaded".into(),
            active: ActiveState::parse(v["a"].s()),
            sub: v["sub"].s().into(),
            autostart: if v["en"].b() {
                Autostart::Enabled
            } else {
                Autostart::Disabled
            },
            main_pid: v["pid"].s().parse().ok(),
            memory: (!matches!(v["mem"].s(), "" | "—")).then(|| bytes(v["mem"].s())),
            cpu_nsec: (!matches!(v["cpu"].s(), "" | "—")).then(|| seconds(v["cpu"].s()) * 1_000_000_000),
            since_monotonic_us: None,
            since_text: Some(v["since"].s().into()),
            result: None,
            exit_status: None,
            last_error: if v["a"].s() == "failed" {
                Some("ERROR: [pool www] cannot get uid for user 'www-data2'".into())
            } else {
                None
            },
        })
        .collect();
    data.services = Some(Arc::new(ServiceList { services }));
    let mut docker = demo::containers();
    for container in &mut docker.containers {
        if let Some(v) = DATA["CTRS"].a().iter().find(|v| v["n"].s() == container.name) {
            container.id = v["id"].s().into();
            container.image = v["img"].s().into();
            container.status = v["up"].s().into();
            container.cpu_percent = (!matches!(v["cpu"], super::V::Null)).then(|| {
                readings
                    .docker_cpu
                    .iter()
                    .find(|(name, _)| *name == container.name)
                    .map_or(v["cpu"].n(), |(_, cpu)| *cpu)
            });
            container.mem_used = (!matches!(v["mem"].s(), "" | "—")).then(|| bytes(v["mem"].s()));
        }
    }
    data.docker = Some(Arc::new(docker));
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_monitor_fixture_has_the_prototype_lists_and_fixed_readings() {
        for dark in [false, true] {
            for scene in ["mon-ov", "mon-proc", "mon-svc", "mon-dk"] {
                let model = data(scene, dark);
                let snapshot = model.snapshot.unwrap();
                assert_eq!(snapshot.processes.len(), 15);
                assert_eq!(snapshot.host.cores, 4);
                assert_eq!(model.services.unwrap().services.len(), DATA["SERVICES"].a().len());
                assert_eq!(model.docker.unwrap().containers.len(), 6);
                assert_eq!(
                    snapshot.cpu.as_ref().unwrap().percent,
                    Some(reading(scene, dark).cpu)
                );
                assert_eq!(
                    crate::monitor::format::percent_whole(Some(
                        snapshot.memory.as_ref().unwrap().used_percent()
                    )),
                    crate::monitor::format::percent_whole(Some(
                        ((reading(scene, dark).memory_gib - 0.04) / 15.6 * 100.) as f32
                    ))
                );
            }
        }
    }
}
