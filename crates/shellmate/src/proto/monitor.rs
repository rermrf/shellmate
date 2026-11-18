//! Map the prototype data into the real monitoring model.
use super::{DATA, bytes, reading};
use crate::monitor::{demo, model::MonitorData};
use shellmate_monitor::{
    services::{ActiveState, Autostart, Service, ServiceList},
    snapshot::Process,
};
use std::sync::Arc;

pub fn local() -> MonitorData {
    use shellmate_monitor::{CpuCard, Disk, MemoryCard, Phase, Probe};
    let gib = 1u64 << 30;
    let probe = Probe { local_hardware: Some("Apple M4 · arm64".into()), os_name: Some("macOS 15.1".into()),
        kernel: "Darwin 24.1.0".into(), user: "chen".into(), uid: Some(501), ..Probe::unknown() };
    let mut snapshot = demo::snapshot();
    snapshot.host.os_name.clone_from(&probe.os_name); snapshot.host.kernel.clone_from(&probe.kernel);
    snapshot.host.cores = 10; snapshot.host.uptime_secs = 3.5 * 86400.;
    snapshot.cpu = Some(CpuCard { percent: Some(21.), per_core: [45.,38.,35.,30.,16.,15.,12.,10.,5.,4.].map(Some).to_vec(),
        history: snapshot.cpu.unwrap().history });
    snapshot.memory = Some(MemoryCard { total: 24*gib, used: 14*gib, available: 10*gib, cache: 5*gib, swap_used: gib/4, swap_total: gib });
    snapshot.disks = [("/", "/dev/disk3s5", 500, 310),("/Volumes/Studio SSD", "/dev/disk5s1", 1000, 420)].map(|(mount,device,total,used)| Disk {
        mount: mount.into(), filesystem: device.into(), total: total*gib, used: used*gib, available: (total-used)*gib, percent: (100*used/total) as u8,
    }).into();
    snapshot.network.as_mut().unwrap().interface = "非回环接口合计".into();
    snapshot.temperature = None; snapshot.zombies = 0;
    snapshot.processes = [(0,0,"root","kernel_task",12.,300), (1,0,"root","launchd",1.,24),
        (141,1,"_windowserver","WindowServer",23.,380), (2207,1,"chen","Shellmate",7.,210), (3550,2207,"chen","node",34.,420)]
        .map(|(pid,ppid,user,name,cpu,mib)| Process { pid,ppid,user:user.into(),name:name.into(),command:match name {
            "Shellmate" => "/Applications/Shellmate.app/Contents/MacOS/Shellmate", "node" => "node dev server.js", other => other
        }.into(), state:'S', cpu_percent:Some(cpu), rss_bytes:Some(mib<<20), mem_percent:Some(100.*mib as f32 / (24.*1024.)), threads:0, nice:0, start_time:u64::from(pid)+1 }).into();
    MonitorData { phase: Phase::Collecting, probe: Some(Arc::new(probe)), snapshot: Some(Arc::new(snapshot)), ..MonitorData::empty() }
}

#[test]
fn local_fixture_has_three_pages_and_no_linux_actions() {
    use crate::monitor::{model, process};
    use model::Page;
    let data = local();
    let header = model::header(&data, Page::Overview, "本机");
    assert_eq!(header.tabs.iter().map(|tab| tab.page).collect::<Vec<_>>(), [Page::Overview, Page::Processes, Page::Docker]);
    assert!(header.chips.iter().any(|chip| chip.text.contains("Apple M4")));
    assert!(model::docker_page(data.probe.as_deref()).unwrap().title.contains("未检测到 Docker"));
    let snapshot = data.snapshot.as_ref().unwrap();
    assert_eq!(snapshot.host.cores, snapshot.cpu.as_ref().unwrap().per_core.len());
    let buttons = process::buttons(snapshot.process(141).unwrap(), data.probe.as_deref());
    assert!(buttons.iter().all(|b| b.action != process::ProcessAction::Strace && b.enabled));
    assert!(data.services.is_none());
}
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
    snapshot.processes.sort_by_key(|process| process.pid); // Snapshot::process uses binary search.
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
                assert!(snapshot.process(2207).is_some(), "the fixture must support binary PID lookup for selection");
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
