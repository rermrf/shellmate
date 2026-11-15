use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use shellmate_monitor::engine::Engine;
use shellmate_monitor::frame::{Frame, FrameKind, FrameParser, Item};
use shellmate_monitor::probe::Probe;
use shellmate_monitor::script::{CollectorConfig, collector_script};

use crate::fakeroot::{FakeRoot, ProbeBin, Ps};

const TOKEN: &str = "tok123";

/// Shells to run the script under: the ones that exist on this machine.
fn shells() -> Vec<&'static str> {
    ["/bin/dash", "/bin/sh", "/bin/bash"].into_iter().filter(|shell| std::path::Path::new(shell).exists()).collect()
}

fn config(root: &FakeRoot) -> CollectorConfig {
    let mut config = CollectorConfig::new(TOKEN);
    config.tick = "0.03".into();
    config.proc_root = root.proc_root();
    config.sys_root = root.sys_root();
    config
}

/// The output of the script run by `shell` for `ticks` ticks, then killed (the way a closed channel ends
/// it, but without waiting for the next write).
fn run(shell: &str, root: &FakeRoot, config: &CollectorConfig, wanted_ticks: u64) -> Vec<String> {
    run_hooked(shell, root, config, wanted_ticks, &mut |_| {})
}

/// Like [`run`], and `on_tick` is called with the number of every tick line as it arrives.
fn run_hooked(
    shell: &str,
    root: &FakeRoot,
    config: &CollectorConfig,
    wanted_ticks: u64,
    on_tick: &mut dyn FnMut(u64),
) -> Vec<String> {
    let mut child = Command::new(shell)
        .arg("-c")
        .arg(collector_script(config))
        .env("PATH", format!("{}:{}", root.bin().display(), std::env::var("PATH").unwrap()))
        .env("CALLS", root.calls_log())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut lines = Vec::new();
    let marker = format!("@@{TOKEN} tick {wanted_ticks}");
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if let Some(tick) = line.strip_prefix(&format!("@@{TOKEN} tick ")).and_then(|n| n.parse().ok()) {
            on_tick(tick);
        }
        let done = line == marker;
        lines.push(line);
        if done {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lines
}

fn frames(lines: &[String]) -> (Vec<Frame>, usize) {
    let mut parser = FrameParser::new(TOKEN);
    let mut frames = Vec::new();
    let mut ticks = 0;
    for line in lines {
        match parser.push(line) {
            Some(Item::Frame(frame)) => frames.push(frame),
            Some(Item::Tick(_)) => ticks += 1,
            None => {}
        }
    }
    (frames, ticks)
}

fn engine() -> Engine {
    let mut probe = Probe::unknown();
    probe.users.insert(1000, "tester".into());
    probe.users.insert(0, "root".into());
    Engine::new(Arc::new(probe))
}

#[test]
fn the_script_writes_frames_on_schedule_under_every_shell() {
    let shells = shells();
    assert!(!shells.is_empty());
    for shell in shells {
        let root = FakeRoot::new(Ps::Procps);
        let lines = run(shell, &root, &config(&root), 40);
        let (frames, ticks) = frames(&lines);
        assert_eq!(ticks, 41, "{shell}: a tick line every tick, 0 to 40");
        let count = |kind| frames.iter().filter(|frame| frame.kind == kind).count();
        // Ticks 0, 2, ..., 40 (21 system frames); 0, 3, ..., 39 (14 process frames); tick 1 (the first slow one).
        assert_eq!(count(FrameKind::Stats), 20, "{shell}: every 2nd tick (the last one is cut off by the stop marker)");
        assert_eq!(count(FrameKind::Procs), 14, "{shell}: every 3rd tick");
        assert_eq!(count(FrameKind::Slow), 2, "{shell}: ticks 1 and 31");
        assert!(frames.iter().all(|f| f.stamp > 190000.0), "{shell}: the stamp is the uptime of the fake");
        // The frames are in tick order: a stats frame first, as the first thing the engine needs (memory
        // total) comes before the processes.
        assert_eq!(frames[0].kind, FrameKind::Stats, "{shell}");
        assert_eq!(frames[1].kind, FrameKind::Procs, "{shell}");
    }
}

#[test]
fn what_the_frames_hold_after_the_engine_has_read_them() {
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let lines = run(shell, &root, &config(&root), 8);
        let (frames, _) = frames(&lines);
        let mut engine = engine();
        let mut snapshot = None;
        for frame in &frames {
            snapshot = Some(engine.apply(frame));
        }
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.host.cores, 8, "{shell}");
        assert_eq!(snapshot.host.load.unwrap().threads, Some(322), "{shell}");
        assert_eq!(snapshot.memory.unwrap().total, 4083300 * 1024, "{shell}");
        assert_eq!(snapshot.memory.unwrap().swap_total, 5131868 * 1024, "{shell}");
        let network = snapshot.network.clone().unwrap();
        assert_eq!(network.interface, "eth0", "{shell}: only the interface of the default route is sent");
        assert_eq!(snapshot.temperature, Some(45.0), "{shell}");
        // Disks: only local file systems were asked about (see the call log below), and the stand-in df
        // answers with the fixture.
        assert_eq!(snapshot.disks.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(), ["/config"], "{shell}");
        // Processes: the fixture's, with the users of the procps ps (uid 0 and 1000).
        assert_eq!(snapshot.processes.len(), 19, "{shell}: not /proc/sys, not /proc/self");
        assert_eq!(snapshot.process(1).unwrap().user, "root", "{shell}");
        assert_eq!(
            snapshot.process(1).unwrap().command,
            "/package/admin/s6/command/s6-svscan -d4 -- /run/service",
            "{shell}"
        );
        assert_eq!(snapshot.processes.iter().find(|p| p.command == "sleep 1234").unwrap().user, "tester", "{shell}");

        // The raw frames carry only what is needed.
        let stats = frames.iter().find(|f| f.kind == FrameKind::Stats).unwrap();
        assert_eq!(
            stats.section("cpu").unwrap().lines.len(),
            9,
            "{shell}: cpu and the 8 cores, nothing of intr and ctxt"
        );
        assert!(
            stats.section("mem").unwrap().lines.iter().all(|l| l.split(' ').count() == 2 && !l.contains("kB")),
            "{shell}: {:?}",
            stats.section("mem")
        );
        assert!(stats.section("mem").unwrap().lines.iter().any(|l| l.starts_with("MemAvailable:")), "{shell}");
        let net = stats.section("net").unwrap();
        assert_eq!(net.lines[0], "defaultif eth0", "{shell}");
        assert_eq!(net.lines.len(), 2, "{shell}: the default interface only, not lo, docker0 or veth: {:?}", net.lines);
        assert_eq!(
            stats.section("thermal").unwrap().lines,
            ["thermal thermal_zone0 45000 x86_pkg_temp", "thermal thermal_zone1 38500 acpitz"],
            "{shell}"
        );
        let procs = frames.iter().find(|f| f.kind == FrameKind::Procs).unwrap();
        assert!(
            procs.section("stat").unwrap().lines.iter().all(|l| l.split(' ').count() <= 40 + 1),
            "{shell}: cut after 40 fields"
        );
        let kept = procs
            .section("stat")
            .unwrap()
            .lines
            .iter()
            .filter_map(|l| shellmate_monitor::parse::parse_proc_stat(l))
            .count();
        assert_eq!(kept, 19, "{shell}: and every line still parses, memory included");
    }
}

#[test]
fn busybox_style_ps_is_found_by_trying_and_without_ps_the_short_names_are_used() {
    for shell in shells() {
        let root = FakeRoot::new(Ps::Busybox);
        let (frames_bb, _) = frames(&run(shell, &root, &config(&root), 4));
        let mut engine = engine();
        let mut last = None;
        for frame in &frames_bb {
            last = Some(engine.apply(frame));
        }
        let snapshot = last.unwrap();
        assert_eq!(snapshot.process(1).unwrap().user, "root", "{shell}: names straight from ps");
        assert_eq!(snapshot.processes.iter().find(|p| p.command == "sleep 1234").unwrap().user, "tester", "{shell}");

        let root = FakeRoot::new(Ps::Missing);
        let (frames_none, _) = frames(&run(shell, &root, &config(&root), 4));
        let procs = frames_none.iter().find(|f| f.kind == FrameKind::Procs).unwrap();
        assert!(procs.section("ps").is_none(), "{shell}: no ps section without ps");
        let mut engine = self::engine();
        let snapshot = engine.apply(procs);
        assert_eq!(snapshot.process(1).unwrap().command, "s6-svscan", "{shell}: the short name from the stat line");
        assert_eq!(snapshot.process(1).unwrap().user, "", "{shell}");
    }
}

#[test]
fn at_most_three_forks_per_tick_and_the_disk_check_only_asks_about_local_file_systems() {
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let _ = run(shell, &root, &config(&root), 65);
        let calls = std::fs::read_to_string(root.calls_log()).unwrap();
        // The log is in order; every tick ends with its `sleep`. Count what runs between two sleeps.
        let mut per_tick = vec![0usize];
        for call in calls.lines() {
            *per_tick.last_mut().unwrap() += 1;
            if call.starts_with("sleep ") {
                per_tick.push(0);
            }
        }
        assert!(per_tick.len() > 60, "{shell}: {}", per_tick.len());
        // Before the first tick the script asks `ps` once which of its two forms it understands (the
        // stand-in understands the first): that call is not part of a tick.
        per_tick[0] -= 1;
        let busiest = per_tick.iter().copied().max().unwrap();
        assert_eq!(busiest, 3, "{shell}: sleep + cut + ps on a process tick that asks for ps: {per_tick:?}");
        for (tick, forks) in per_tick.iter().enumerate().take(64) {
            // `ps` only when the list of processes changed (the first frame) or after 10 frames without it:
            // process frames 0 and 11, ticks 0 and 33.
            let process = tick % 3 == 0;
            let ps = process && (tick == 0 || tick == 33);
            let expected = 1 + usize::from(process) + usize::from(ps) + usize::from(tick % 30 == 1);
            assert_eq!(*forks, expected, "{shell}: tick {tick}");
        }
        // df runs at tick 1 and 31, and gets the mount points of real local file systems: not /, which is
        // an overlay, not /proc, /run, /snap, /sys, not the NFS and sshfs mounts, not a name with a backslash.
        let df: Vec<&str> = calls.lines().filter(|line| line.starts_with("df ")).collect();
        assert_eq!(df, ["df -P -k /config /data /boot"; 3], "{shell}");
        // Fewer processes are asked about at all: ps ran at tick 0 and 33 only.
        assert_eq!(
            calls.lines().filter(|line| line.starts_with("ps ")).count(),
            3,
            "{shell}: tick 0, 33 and the detection"
        );
        // Every forked process is one of ours.
        assert!(
            calls.lines().all(|line| ["sleep ", "cut ", "ps ", "df "].iter().any(|name| line.starts_with(name))),
            "{shell}"
        );
    }
}

#[test]
fn the_script_ends_by_itself_when_its_output_is_closed() {
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let mut child = Command::new(shell)
            .arg("-c")
            .arg(collector_script(&config(&root)))
            .env("PATH", format!("{}:{}", root.bin().display(), std::env::var("PATH").unwrap()))
            .env("CALLS", root.calls_log())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("@@tok123 tick 0"), "{line}");
        // The reader goes away, as the SSH channel does when it is closed.
        drop(reader);
        let started = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "{shell}: still running 5 s after the pipe closed");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn the_script_also_ends_when_sigpipe_is_ignored() {
    // Some environments start sshd's children with SIGPIPE ignored: then the write fails instead, and the
    // script has to notice that itself.
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("trap '' PIPE; exec {shell} -c \"$SCRIPT\""))
            .env("SCRIPT", collector_script(&config(&root)))
            .env("PATH", format!("{}:{}", root.bin().display(), std::env::var("PATH").unwrap()))
            .env("CALLS", root.calls_log())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        drop(reader);
        let started = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "{shell}: SIGPIPE ignored and the script runs on");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_process_that_vanishes_while_the_script_reads_the_files_does_not_break_the_frame() {
    // A directory without a stat file stands for a process that exited between the glob and the read.
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        std::fs::create_dir_all(std::path::Path::new(&root.proc_root()).join("31337")).unwrap();
        let (frames, _) = frames(&run(shell, &root, &config(&root), 4));
        let procs = frames.iter().find(|f| f.kind == FrameKind::Procs).unwrap();
        assert_eq!(procs.section("stat").unwrap().lines.len(), 19, "{shell}");
    }
}

#[test]
fn nothing_from_the_server_can_pass_for_a_marker() {
    // A process whose command line contains the marker of the frames cannot end or start a frame: the
    // token is not known to it.
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let ps = std::path::Path::new(root.path()).join("ps_procps.txt");
        let mut text = std::fs::read_to_string(&ps).unwrap();
        text.push_str("  999     0 @@end @@stats 1 @@tok12 end\n");
        std::fs::write(&ps, text).unwrap();
        let (frames, _) = frames(&run(shell, &root, &config(&root), 4));
        let procs = frames.iter().find(|f| f.kind == FrameKind::Procs).unwrap();
        assert!(procs.section("ps").unwrap().lines.iter().any(|l| l.contains("@@end @@stats 1")), "{shell}");
    }
}

#[test]
fn ps_is_asked_again_when_the_list_of_processes_changes() {
    for shell in shells() {
        let root = FakeRoot::new(Ps::Procps);
        let proc = std::path::PathBuf::from(root.proc_root());
        // A process starts after tick 10.
        let mut start = |tick: u64| {
            if tick == 10 {
                std::fs::create_dir_all(proc.join("777")).unwrap();
                std::fs::write(
                    proc.join("777/stat"),
                    "777 (newcomer) S 1 777 777 0 -1 4194560 0 0 0 0 1 1 0 0 20 0 1 0 999 1000 5 0\n",
                )
                .unwrap();
            }
        };
        let (frames, _) = frames(&run_hooked(shell, &root, &config(&root), 45, &mut start));
        let procs: Vec<&Frame> = frames.iter().filter(|f| f.kind == FrameKind::Procs).collect();
        let with_ps: Vec<bool> = procs.iter().map(|f| f.section("ps").is_some()).collect();
        let newcomer: Vec<bool> = procs.iter().map(|f| f.text("stat").contains("(newcomer)")).collect();
        assert!(with_ps[0], "{shell}: the first frame has ps: {with_ps:?}");
        assert!(!with_ps[1] && !with_ps[2], "{shell}: nothing changed, nothing asked: {with_ps:?}");
        let first_newcomer = newcomer.iter().position(|seen| *seen).expect("the new process shows up");
        assert!(
            with_ps[first_newcomer],
            "{shell}: the frame that has the new process has ps again: {with_ps:?} {newcomer:?}"
        );
        assert!(!with_ps[first_newcomer + 1], "{shell}: and the next one does not: {with_ps:?}");
    }
}

#[test]
fn the_command_line_survives_every_login_shell_the_server_may_hand_it_to() {
    // The server's login shell reads `sh -c '<script>'` before `sh` does. The scripts have no single quote
    // in them, so one pair of quotes is all there is; bash, zsh, dash and fish (where installed) must hand
    // the script on untouched.
    let logins: Vec<&str> =
        ["/bin/bash", "/bin/zsh", "/bin/dash", "/bin/sh", "/opt/homebrew/bin/fish", "/usr/local/bin/fish"]
            .into_iter()
            .filter(|shell| std::path::Path::new(shell).exists())
            .collect();
    assert!(logins.len() >= 3, "{logins:?}");
    for login in logins {
        let root = FakeRoot::new(Ps::Procps);
        let command = shellmate_monitor::script::collector_command(&config(&root));
        let mut child = Command::new(login)
            .arg("-c")
            .arg(&command)
            .env("PATH", format!("{}:{}", root.bin().display(), std::env::var("PATH").unwrap()))
            .env("CALLS", root.calls_log())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = Vec::new();
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line.unwrap();
            let done = line == format!("@@{TOKEN} tick 8");
            lines.push(line);
            if done {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let (frames, ticks) = frames(&lines);
        assert_eq!(ticks, 9, "{login}");
        assert!(frames.iter().any(|f| f.kind == FrameKind::Procs && f.section("ps").is_some()), "{login}: {lines:?}");
        assert!(frames.iter().any(|f| f.kind == FrameKind::Stats), "{login}");

        // The probe too: here it must write its sections and end (on a Mac as "not Linux"). It runs with a PATH
        // that has only the tools it needs and stand-ins for sudo, docker and podman that fail: a test never
        // runs the developer's own sudo or docker.
        let bin = ProbeBin::new();
        let probe = shellmate_monitor::script::probe_command("ptok");
        let output = Command::new(login)
            .arg("-c")
            .arg(&probe)
            .env("PATH", bin.path())
            .env("CALLS", bin.calls_log())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("@@ptok kernel") && text.contains("@@ptok end"), "{login}: {text}");
        let parsed = shellmate_monitor::probe::parse_probe("ptok", &text);
        assert!(parsed.kernel.starts_with("Darwin") && !parsed.can_collect(), "{login}: {parsed:?}");
        // The stand-ins were what the probe found: sudo was asked (and refused), docker and podman failed.
        let calls = std::fs::read_to_string(bin.calls_log()).unwrap();
        assert!(calls.contains("sudo -n true"), "{login}: {calls}");
        assert!(calls.contains("docker version") && calls.contains("podman version"), "{login}: {calls}");
        assert_eq!(parsed.sudo, shellmate_monitor::Sudo::NeedsPassword, "{login}");
        assert!(parsed.containers.iter().all(|c| c.access == shellmate_monitor::ContainerAccess::Error), "{login}: {parsed:?}");
        assert!(parsed.uid.is_some() && !parsed.user.is_empty(), "{login}");
    }
}
