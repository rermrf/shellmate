//! From frames to snapshots: the rates (CPU, network, per process) are the differences of two samples
//! over the time between them, the histories keep the last minutes, and everything the pages show is
//! put together in a [`Snapshot`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::frame::{Frame, FrameKind};
use crate::parse::{
    self, CpuStat, LoadAvg, MemInfo, NetCounters, ProcStat, PsOwner, parse_cpu_stat, parse_df, parse_loadavg,
    parse_meminfo, parse_net_dev, parse_proc_stat, parse_ps, parse_thermal, real_disks,
};
use crate::probe::Probe;
use crate::snapshot::{CpuCard, Disk, HISTORY_POINTS, HostFacts, MemoryCard, NetworkCard, Process, Snapshot};

/// Commands longer than this are cut for display.
const MAX_COMMAND_CHARS: usize = 2048;
/// A network counter can only be believed to have wrapped if the traffic it implies is at most this many
/// bytes per second (2 Gbit/s: the 32-bit counters of old kernels and small routers). Anything more is
/// taken for a reset, so that a counter that restarted from zero is not shown as a spike of gigabytes.
const PLAUSIBLE_BYTES_PER_SEC: u64 = 250_000_000;

/// How far a counter moved between two samples. A counter that went down was reset (an interface that
/// came back, a reboot between two samples): there is no rate for that interval, `None`, unless it is a
/// 32-bit counter that wrapped (the old value is below 2^32 and the new one is small) and the traffic that
/// implies is plausible for the time that passed.
pub fn counter_delta(prev: u64, cur: u64, secs: f64) -> Option<u64> {
    if cur >= prev {
        return Some(cur - prev);
    }
    const WRAP: u64 = 1 << 32;
    if prev < WRAP && cur < WRAP {
        let moved = WRAP - prev + cur;
        let believable = (PLAUSIBLE_BYTES_PER_SEC as f64 * secs.max(1.0)) as u64;
        return (moved <= believable).then_some(moved);
    }
    None
}

/// Percent of the time between two samples that a CPU was busy; `None` if no time passed or the counters
/// were reset.
fn busy_percent(prev: &parse::CpuTimes, cur: &parse::CpuTimes) -> Option<f32> {
    let total = cur.total().checked_sub(prev.total())?;
    let idle = cur.idle_time().checked_sub(prev.idle_time())?;
    if total == 0 || idle > total {
        return None;
    }
    Some(((total - idle) as f64 / total as f64 * 100.0) as f32)
}

fn push_history<T>(history: &mut VecDeque<T>, value: T) {
    if history.len() == HISTORY_POINTS {
        history.pop_front();
    }
    history.push_back(value);
}

/// Takes the collector script out of the list, with everything it started (`cut`, `ps`, `sleep`) and the
/// shell that was started for it, if the login shell did not replace itself with the script: the monitor
/// is not what the user wants to see on the server. `collector` is the script's own process id (what the
/// process frames say); the command line `# shellmate-collector ...` finds the shells around it, and
/// servers whose frames do not name the pid still lose the script itself.
fn hide_the_collector(processes: &mut Vec<Process>, collector: Option<u32>) {
    let mut hidden: std::collections::HashSet<u32> = processes
        .iter()
        .filter(|process| Some(process.pid) == collector || process.command.contains("shellmate-collector"))
        .map(|process| process.pid)
        .collect();
    if hidden.is_empty() {
        return;
    }
    // Children, grandchildren, and so on.
    loop {
        let more: Vec<u32> = processes
            .iter()
            .filter(|process| hidden.contains(&process.ppid) && !hidden.contains(&process.pid))
            .map(|process| process.pid)
            .collect();
        if more.is_empty() {
            break;
        }
        hidden.extend(more);
    }
    processes.retain(|process| !hidden.contains(&process.pid));
}

/// What `ps` said about a process (kept between frames that do not repeat it).
struct CachedPs {
    /// The start time of the process when `ps` ran: another start time under the same number is another
    /// process. `None` until the process is seen in a stat line.
    start: Option<u64>,
    entry: parse::PsEntry,
}

/// What the engine remembers of the processes of the previous sample.
struct ProcessMemory {
    stamp: f64,
    /// pid -> (start time, CPU ticks)
    ticks: HashMap<u32, (u64, u64)>,
}

/// Turns frames into snapshots.
pub struct Engine {
    probe: Arc<Probe>,
    cpu_prev: Option<(f64, CpuStat)>,
    net_prev: Option<(f64, NetCounters)>,
    procs_prev: Option<ProcessMemory>,
    ps_cache: HashMap<u32, CachedPs>,

    cpu_history: VecDeque<f32>,
    rx_history: VecDeque<f64>,
    tx_history: VecDeque<f64>,

    host: HostFacts,
    cpu: Option<CpuCard>,
    mem: Option<MemInfo>,
    network: Option<NetworkCard>,
    temperature: Option<f32>,
    disks: Vec<Disk>,
    processes: Vec<Process>,
    zombies: usize,
    seen: bool,
}

impl Engine {
    /// An engine for a server that was probed as `probe`.
    pub fn new(probe: Arc<Probe>) -> Self {
        let host = HostFacts { os_name: probe.os_name.clone(), kernel: probe.kernel.clone(), ..HostFacts::default() };
        Self {
            probe,
            cpu_prev: None,
            net_prev: None,
            procs_prev: None,
            ps_cache: HashMap::new(),
            cpu_history: VecDeque::new(),
            rx_history: VecDeque::new(),
            tx_history: VecDeque::new(),
            host,
            cpu: None,
            mem: None,
            network: None,
            temperature: None,
            disks: Vec::new(),
            processes: Vec::new(),
            zombies: 0,
            seen: false,
        }
    }

    /// Forgets the previous samples (the connection was lost and came back: the time in between must not
    /// be a rate). The histories and the last numbers stay, so that the pages keep showing something.
    pub fn reset_baselines(&mut self) {
        self.cpu_prev = None;
        self.net_prev = None;
        self.procs_prev = None;
    }

    /// Takes a frame and returns the snapshot as it is now.
    pub fn apply(&mut self, frame: &Frame) -> Arc<Snapshot> {
        self.seen = true;
        self.host.uptime_secs = frame.stamp;
        match frame.kind {
            FrameKind::Stats => self.apply_stats(frame),
            FrameKind::Procs => self.apply_procs(frame),
            FrameKind::Slow => self.apply_slow(frame),
        }
        self.snapshot()
    }

    /// The snapshot as it is now.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::new(Snapshot {
            host: self.host.clone(),
            cpu: self.cpu.clone(),
            memory: self.mem.map(|mem| MemoryCard {
                total: mem.total_kb * 1024,
                used: mem.used_kb() * 1024,
                cache: mem.cache_kb().min(mem.total_kb) * 1024,
                available: mem.available_or_estimate_kb() * 1024,
                swap_total: mem.swap_total_kb * 1024,
                swap_used: mem.swap_used_kb() * 1024,
            }),
            disks: self.disks.clone(),
            network: self.network.clone(),
            temperature: self.temperature,
            processes: self.processes.clone(),
            zombies: self.zombies,
        })
    }

    fn apply_stats(&mut self, frame: &Frame) {
        // CPU
        if let Some(stat) = parse_cpu_stat(&frame.text("cpu")) {
            let secs_ok = |prev_stamp: f64| frame.stamp > prev_stamp;
            let mut card = self.cpu.take().unwrap_or_default();
            card.percent = None;
            card.per_core = stat.cores.iter().map(|_| None).collect();
            if let Some((prev_stamp, prev)) = self.cpu_prev.as_ref().filter(|(stamp, _)| secs_ok(*stamp)) {
                card.percent = busy_percent(&prev.total, &stat.total);
                card.per_core = stat
                    .cores
                    .iter()
                    .map(|(index, times)| {
                        prev.cores.iter().find(|(i, _)| i == index).and_then(|(_, before)| busy_percent(before, times))
                    })
                    .collect();
                let _ = prev_stamp;
            }
            if let Some(percent) = card.percent {
                push_history(&mut self.cpu_history, percent);
            }
            card.history = self.cpu_history.iter().copied().collect();
            self.host.cores = stat.cores.len().max(1);
            self.cpu = Some(card);
            self.cpu_prev = Some((frame.stamp, stat));
        }
        // load
        if let Some(load) = parse_loadavg(&frame.text("load")) {
            self.host.load = Some::<LoadAvg>(load);
        }
        // memory
        if let Some(mem) = parse_meminfo(&frame.text("mem")) {
            self.mem = Some(mem);
        }
        // network
        self.apply_net(frame);
        // temperature
        let sensors = parse_thermal(&frame.text("thermal"));
        self.temperature = parse::cpu_temperature(&sensors);
    }

    fn apply_net(&mut self, frame: &Frame) {
        let Some(section) = frame.section("net") else { return };
        let named = section
            .lines
            .iter()
            .find_map(|line| line.strip_prefix("defaultif "))
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let counters = parse_net_dev(&section.text());
        let chosen = match named {
            Some(name) => counters.iter().find(|c| c.interface == name),
            // No default route: the busiest interface that is not the loopback.
            None => {
                counters.iter().filter(|c| c.interface != "lo").max_by_key(|c| c.rx_bytes.saturating_add(c.tx_bytes))
            }
        };
        let Some(current) = chosen.cloned() else { return };
        let mut card = NetworkCard { interface: current.interface.clone(), ..NetworkCard::default() };
        if let Some((prev_stamp, prev)) =
            self.net_prev.as_ref().filter(|(stamp, prev)| frame.stamp > *stamp && prev.interface == current.interface)
        {
            let secs = frame.stamp - prev_stamp;
            card.rx_per_sec = counter_delta(prev.rx_bytes, current.rx_bytes, secs).map(|bytes| bytes as f64 / secs);
            card.tx_per_sec = counter_delta(prev.tx_bytes, current.tx_bytes, secs).map(|bytes| bytes as f64 / secs);
            if let (Some(rx), Some(tx)) = (card.rx_per_sec, card.tx_per_sec) {
                push_history(&mut self.rx_history, rx);
                push_history(&mut self.tx_history, tx);
            }
        } else if self.network.as_ref().is_some_and(|old| old.interface != current.interface) {
            // Another interface: its history is somebody else's.
            self.rx_history.clear();
            self.tx_history.clear();
        }
        card.rx_history = self.rx_history.iter().copied().collect();
        card.tx_history = self.tx_history.iter().copied().collect();
        self.network = Some(card);
        self.net_prev = Some((frame.stamp, current));
    }

    fn apply_procs(&mut self, frame: &Frame) {
        let stats: Vec<ProcStat> = frame.text("stat").lines().filter_map(parse_proc_stat).collect();
        // The collector only runs `ps` when the list of processes changed: a frame without the section
        // means "as before".
        if let Some(section) = frame.section("ps") {
            let started: HashMap<u32, u64> = stats.iter().map(|stat| (stat.pid, stat.starttime)).collect();
            self.ps_cache = parse_ps(&section.text())
                .into_iter()
                .map(|entry| (entry.pid, CachedPs { start: started.get(&entry.pid).copied(), entry }))
                .collect();
        }
        let clk = self.probe.clk_tck.max(1) as f64;
        let page = self.probe.page_size;
        let mem_total = self.mem.map(|mem| mem.total_kb * 1024).filter(|total| *total > 0);

        let mut ticks = HashMap::with_capacity(stats.len());
        let mut processes: Vec<Process> = Vec::with_capacity(stats.len());
        for stat in &stats {
            let used = stat.utime.saturating_add(stat.stime);
            ticks.insert(stat.pid, (stat.starttime, used));
            let cpu_percent = self.procs_prev.as_ref().and_then(|prev| {
                let secs = frame.stamp - prev.stamp;
                let (start, before) = *prev.ticks.get(&stat.pid)?;
                // The same number with another start time is another process.
                if secs <= 0.0 || start != stat.starttime {
                    return None;
                }
                Some((used.checked_sub(before)? as f64 / (clk * secs) * 100.0) as f32)
            });
            let rss_bytes = stat.rss_pages.map(|pages| pages.saturating_mul(page));
            let entry = self.ps_cache.get_mut(&stat.pid).and_then(|cached| {
                if cached.start.is_none() {
                    cached.start = Some(stat.starttime);
                }
                (cached.start == Some(stat.starttime)).then_some(&cached.entry)
            });
            let user = entry
                .map(|entry| match &entry.owner {
                    PsOwner::Uid(uid) => self.probe.user_name(*uid),
                    PsOwner::Name(name) => name.clone(),
                })
                .unwrap_or_default();
            let command = entry.map(|entry| entry.args.as_str()).filter(|args| !args.is_empty()).unwrap_or(&stat.comm);
            processes.push(Process {
                pid: stat.pid,
                ppid: stat.ppid,
                user,
                name: stat.comm.clone(),
                command: command.chars().take(MAX_COMMAND_CHARS).collect(),
                state: stat.state,
                cpu_percent,
                mem_percent: rss_bytes.zip(mem_total).map(|(rss, total)| (rss as f64 / total as f64 * 100.0) as f32),
                rss_bytes,
                threads: stat.threads,
                nice: stat.nice,
                start_time: stat.starttime,
            });
        }
        hide_the_collector(&mut processes, frame.pid);
        processes.sort_by_key(|process| process.pid);
        self.zombies = processes.iter().filter(|process| process.state == 'Z').count();
        self.processes = processes;
        self.procs_prev = Some(ProcessMemory { stamp: frame.stamp, ticks });
    }

    fn apply_slow(&mut self, frame: &Frame) {
        self.disks = real_disks(parse_df(&frame.text("df")))
            .into_iter()
            .map(|disk| Disk {
                mount: disk.mount,
                filesystem: disk.filesystem,
                total: disk.total_kb * 1024,
                used: disk.used_kb * 1024,
                available: disk.available_kb * 1024,
                percent: disk.percent,
            })
            .collect();
    }

    /// Whether any frame has been applied.
    pub fn has_data(&self) -> bool {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Section;
    use crate::parse::CpuTimes;

    fn probe() -> Arc<Probe> {
        let mut probe = Probe::unknown();
        probe.users.insert(1000, "alice".into());
        probe.users.insert(0, "root".into());
        Arc::new(probe)
    }

    fn frame(kind: FrameKind, stamp: f64, sections: &[(&str, &str)]) -> Frame {
        Frame {
            kind,
            stamp,
            pid: None,
            sections: sections
                .iter()
                .map(|(name, text)| Section { name: (*name).into(), lines: text.lines().map(str::to_string).collect() })
                .collect(),
        }
    }

    fn cpu_lines(total_busy: u64, total_idle: u64, cores: &[(u64, u64)]) -> String {
        let mut text = format!("cpu  {total_busy} 0 0 {total_idle} 0 0 0 0 0 0\n");
        for (index, (busy, idle)) in cores.iter().enumerate() {
            text.push_str(&format!("cpu{index} {busy} 0 0 {idle} 0 0 0 0 0 0\n"));
        }
        text
    }

    #[test]
    fn counters_that_wrap_or_reset() {
        assert_eq!(counter_delta(100, 250, 2.0), Some(150));
        assert_eq!(counter_delta(7, 7, 2.0), Some(0));
        // A 32-bit counter that wrapped: 4294967200 -> 100 is 196 bytes.
        assert_eq!(counter_delta(4_294_967_200, 100, 2.0), Some(196));
        // 295 MB in 2 s is 1.2 Gbit/s: believable on a counter close to the end.
        assert_eq!(counter_delta(4_000_000_000, 5, 2.0), Some(294_967_301));
        assert_eq!(counter_delta(4_100_000_000, 5, 0.1), Some(194_967_301), "a time span counts as at least a second");
        assert_eq!(counter_delta(4_000_000_000, 5, 0.1), None, "295 MB in a second is more than 2 Gbit/s");
        // From a counter far from 2^32 down to nearly nothing would be 4 GB in 2 s: it was reset.
        assert_eq!(counter_delta(1_000_000, 3, 2.0), None);
        assert_eq!(counter_delta(100, 50, 2.0), None);
        // A 64-bit counter cannot wrap in a lifetime: a decrease is a reset.
        assert_eq!(counter_delta(10_000_000_000, 5, 2.0), None);
    }

    #[test]
    fn the_first_sample_has_no_rates_and_the_second_has() {
        let mut engine = Engine::new(probe());
        let first = engine.apply(&frame(
            FrameKind::Stats,
            100.0,
            &[("cpu", &cpu_lines(100, 900, &[(50, 450), (50, 450)])), ("load", "0.50 0.25 0.10 2/100 999\n")],
        ));
        let cpu = first.cpu.as_ref().unwrap();
        assert_eq!(cpu.percent, None);
        assert_eq!(cpu.per_core, [None, None]);
        assert!(cpu.history.is_empty());
        assert_eq!(first.host.cores, 2);
        assert_eq!(first.host.load.unwrap().five, 0.25);

        // 2 s later: 100 more ticks busy out of 200 (50 %), core 0 fully busy, core 1 idle.
        let second =
            engine.apply(&frame(FrameKind::Stats, 102.0, &[("cpu", &cpu_lines(200, 1000, &[(150, 450), (50, 550)]))]));
        let cpu = second.cpu.as_ref().unwrap();
        assert_eq!(cpu.percent, Some(50.0));
        assert_eq!(cpu.per_core, [Some(100.0), Some(0.0)]);
        assert_eq!(cpu.history, [50.0]);
    }

    #[test]
    fn iowait_counts_as_idle_and_steal_as_busy_time_that_passed() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 0.0, &[("cpu", "cpu  0 0 0 0 0 0 0 0\n")]));
        // 25 user, 25 iowait, 50 idle: 25 % busy; steal is time that was spent, not idle.
        let snapshot = engine.apply(&frame(FrameKind::Stats, 2.0, &[("cpu", "cpu  25 0 0 50 25 0 0 0\n")]));
        assert_eq!(snapshot.cpu.clone().unwrap().percent, Some(25.0));
        let snapshot = engine.apply(&frame(FrameKind::Stats, 4.0, &[("cpu", "cpu  25 0 0 50 25 0 0 100\n")]));
        assert_eq!(snapshot.cpu.clone().unwrap().percent, Some(100.0));
    }

    #[test]
    fn a_cpu_counter_that_went_down_or_stood_still_gives_no_rate_for_that_interval() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 10.0, &[("cpu", &cpu_lines(500, 500, &[(500, 500)]))]));
        // The host rebooted between two samples: everything started from zero again.
        let reset = engine.apply(&frame(FrameKind::Stats, 12.0, &[("cpu", &cpu_lines(5, 10, &[(5, 10)]))]));
        assert_eq!(reset.cpu.as_ref().unwrap().percent, None);
        assert_eq!(reset.cpu.as_ref().unwrap().per_core, [None]);
        // The next interval is fine again.
        let next = engine.apply(&frame(FrameKind::Stats, 14.0, &[("cpu", &cpu_lines(15, 20, &[(15, 20)]))]));
        assert_eq!(next.cpu.as_ref().unwrap().percent, Some(50.0));
        // Nothing moved at all (the same sample twice): no rate either, and no division by zero.
        let same = engine.apply(&frame(FrameKind::Stats, 16.0, &[("cpu", &cpu_lines(15, 20, &[(15, 20)]))]));
        assert_eq!(same.cpu.clone().unwrap().percent, None);
    }

    #[test]
    fn a_stamp_that_did_not_move_forward_is_not_a_time_span() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 10.0, &[("cpu", &cpu_lines(0, 0, &[]))]));
        let back = engine.apply(&frame(FrameKind::Stats, 9.0, &[("cpu", &cpu_lines(10, 10, &[]))]));
        assert_eq!(back.cpu.clone().unwrap().percent, None);
    }

    #[test]
    fn the_history_keeps_sixty_points() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 0.0, &[("cpu", &cpu_lines(0, 0, &[]))]));
        let mut last = None;
        for i in 1..=75u64 {
            // The busy share grows from 1 % to 75 %.
            last = Some(engine.apply(&frame(
                FrameKind::Stats,
                i as f64 * 2.0,
                &[("cpu", &cpu_lines(i * (i + 1) / 2, i * 100 - i * (i + 1) / 2, &[]))],
            )));
        }
        let history = last.unwrap().cpu.as_ref().unwrap().history.clone();
        assert_eq!(history.len(), HISTORY_POINTS);
        assert_eq!(history.last().copied().unwrap().round(), 75.0, "the newest is last");
        assert_eq!(history[0].round(), 16.0, "the 15 oldest of 75 are gone");
    }

    #[test]
    fn reset_baselines_keeps_what_was_shown_but_the_next_sample_has_no_rate() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 0.0, &[("cpu", &cpu_lines(0, 0, &[]))]));
        engine.apply(&frame(FrameKind::Stats, 2.0, &[("cpu", &cpu_lines(50, 50, &[]))]));
        engine.reset_baselines();
        // An hour later, on a new connection: the difference over an hour is no rate for "now".
        let after = engine.apply(&frame(FrameKind::Stats, 3600.0, &[("cpu", &cpu_lines(50, 5000, &[]))]));
        assert_eq!(after.cpu.as_ref().unwrap().percent, None);
        assert_eq!(after.cpu.as_ref().unwrap().history, [50.0], "the history is still there");
    }

    const MEM: &str = "MemTotal: 1000\nMemFree: 200\nMemAvailable: 600\nBuffers: 50\nCached: 250\nSReclaimable: 0\nSwapTotal: 100\nSwapFree: 40\n";

    #[test]
    fn memory_in_bytes() {
        let mut engine = Engine::new(probe());
        let snapshot = engine.apply(&frame(FrameKind::Stats, 1.0, &[("mem", MEM)]));
        let mem = snapshot.memory.unwrap();
        assert_eq!(mem.total, 1000 * 1024);
        assert_eq!(mem.used, 500 * 1024, "total - free - cache");
        assert_eq!(mem.cache, 300 * 1024);
        assert_eq!(mem.available, 600 * 1024);
        assert_eq!((mem.swap_total, mem.swap_used), (100 * 1024, 60 * 1024));
        assert_eq!(mem.used_percent(), 50.0);
        assert_eq!(mem.used_and_cache_percent(), 80.0);
        let none = Engine::new(probe()).apply(&frame(FrameKind::Stats, 1.0, &[]));
        assert_eq!(none.memory, None, "nothing was sent");
    }

    fn net(interface: &str, rx: u64, tx: u64) -> String {
        format!("defaultif {interface}\n{interface}: {rx} 1 0 0 0 0 0 0 {tx} 1 0 0 0 0 0 0\n")
    }

    #[test]
    fn network_rates_of_the_default_interface() {
        let mut engine = Engine::new(probe());
        let first = engine.apply(&frame(FrameKind::Stats, 10.0, &[("net", &net("eth0", 1000, 5000))]));
        let card = first.network.clone().unwrap();
        assert_eq!((card.interface.as_str(), card.rx_per_sec, card.tx_per_sec), ("eth0", None, None));
        let second = engine.apply(&frame(FrameKind::Stats, 12.0, &[("net", &net("eth0", 3000, 5400))]));
        let card = second.network.clone().unwrap();
        assert_eq!((card.rx_per_sec, card.tx_per_sec), (Some(1000.0), Some(200.0)));
        assert_eq!((card.rx_history.clone(), card.tx_history.clone()), (vec![1000.0], vec![200.0]));
        // The interface came back with zeroed counters: no rate for that interval, not a spike of
        // gigabytes, and nothing added to the history.
        let reset = engine.apply(&frame(FrameKind::Stats, 14.0, &[("net", &net("eth0", 50, 5))]));
        assert_eq!(
            (reset.network.as_ref().unwrap().rx_per_sec, reset.network.as_ref().unwrap().tx_per_sec),
            (None, None)
        );
        assert_eq!(reset.network.as_ref().unwrap().rx_history, [1000.0]);
        let next = engine.apply(&frame(FrameKind::Stats, 16.0, &[("net", &net("eth0", 250, 45))]));
        assert_eq!(
            (next.network.as_ref().unwrap().rx_per_sec, next.network.as_ref().unwrap().tx_per_sec),
            (Some(100.0), Some(20.0))
        );
        assert_eq!(next.network.as_ref().unwrap().rx_history, [1000.0, 100.0]);
    }

    #[test]
    fn a_wrapped_32_bit_counter_is_a_rate_and_another_interface_starts_over() {
        let mut engine = Engine::new(probe());
        engine.apply(&frame(FrameKind::Stats, 0.0, &[("net", &net("eth0", 4_294_967_000, 10))]));
        let wrapped = engine.apply(&frame(FrameKind::Stats, 2.0, &[("net", &net("eth0", 504, 30))]));
        assert_eq!(wrapped.network.as_ref().unwrap().rx_per_sec, Some(400.0), "800 bytes in 2 s across the wrap");
        // The default route moved to the VPN: the old interface's numbers are not the new one's.
        let moved = engine.apply(&frame(FrameKind::Stats, 4.0, &[("net", &net("wg0", 10, 10))]));
        let card = moved.network.clone().unwrap();
        assert_eq!(card.interface, "wg0");
        assert_eq!(card.rx_per_sec, None);
        assert!(card.rx_history.is_empty());
    }

    #[test]
    fn without_a_default_route_the_busiest_interface_that_is_not_loopback_is_shown() {
        let mut engine = Engine::new(probe());
        let text = "defaultif \nlo: 999999999 1 0 0 0 0 0 0 999999999 1 0 0 0 0 0 0\nwlan0: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0\neth1: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n";
        let snapshot = engine.apply(&frame(FrameKind::Stats, 1.0, &[("net", text)]));
        assert_eq!(snapshot.network.clone().unwrap().interface, "eth1");
        // Only the loopback: there is no card.
        let mut engine = Engine::new(probe());
        let snapshot = engine.apply(&frame(
            FrameKind::Stats,
            1.0,
            &[("net", "defaultif \nlo: 1 1 0 0 0 0 0 0 1 1 0 0 0 0 0 0\n")],
        ));
        assert_eq!(snapshot.network, None);
    }

    #[allow(clippy::too_many_arguments)] // a stat line is its fields
    fn stat_line(pid: u32, comm: &str, state: char, ppid: u32, utime: u64, stime: u64, start: u64, rss: u64) -> String {
        format!(
            "{pid} ({comm}) {state} {ppid} {pid} {pid} 0 -1 4194560 0 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start} 1000000 {rss} 18446744073709551615"
        )
    }

    fn procs_frame(stamp: f64, stats: &[String], ps: &str) -> Frame {
        frame(FrameKind::Procs, stamp, &[("stat", &stats.join("\n")), ("ps", ps)])
    }

    #[test]
    fn process_rates_come_from_the_difference_of_ticks() {
        let mut engine = Engine::new(probe());
        // Memory first, as in every tick of the collector: 1 GiB in total.
        engine.apply(&frame(FrameKind::Stats, 0.0, &[("mem", "MemTotal: 1048576\nMemFree: 1\n")]));
        let ps = "    1     0 /sbin/init splash\n  200  1000 /usr/bin/worker --threads 4\n";
        let first = engine.apply(&procs_frame(
            10.0,
            &[stat_line(1, "init", 'S', 0, 10, 5, 100, 256), stat_line(200, "worker", 'R', 1, 1000, 500, 5000, 25600)],
            ps,
        ));
        assert!(first.processes.iter().all(|p| p.cpu_percent.is_none()), "no rates after the first sample");
        let worker = first.process(200).unwrap();
        assert_eq!(
            (worker.user.as_str(), worker.command.as_str(), worker.name.as_str()),
            ("alice", "/usr/bin/worker --threads 4", "worker")
        );
        assert_eq!(worker.rss_bytes, Some(25600 * 4096));
        assert!((worker.mem_percent.unwrap() - 9.765625).abs() < 0.01, "100 MiB of 1 GiB: {:?}", worker.mem_percent);
        assert_eq!(first.process(1).unwrap().user, "root");

        // 3 s later the worker used 300 ticks (3 s of CPU: a whole core), init nothing.
        let second = engine.apply(&procs_frame(
            13.0,
            &[stat_line(1, "init", 'S', 0, 10, 5, 100, 256), stat_line(200, "worker", 'R', 1, 1200, 600, 5000, 25600)],
            ps,
        ));
        assert_eq!(second.process(200).unwrap().cpu_percent, Some(100.0));
        assert_eq!(second.process(1).unwrap().cpu_percent, Some(0.0));
        assert_eq!(second.top_by_cpu(1)[0].pid, 200);
    }

    #[test]
    fn processes_that_vanish_appear_or_reuse_a_pid_between_samples() {
        let mut engine = Engine::new(probe());
        engine.apply(&procs_frame(
            0.0,
            &[
                stat_line(10, "gone", 'S', 1, 100, 0, 50, 1),
                stat_line(11, "stays", 'S', 1, 100, 0, 60, 1),
                stat_line(12, "reused", 'S', 1, 500, 0, 70, 1),
            ],
            "",
        ));
        let next = engine.apply(&procs_frame(
            3.0,
            &[
                // 10 vanished; 11 stays and used 150 ticks of 300 (50 %); 12's number belongs to a new
                // process (another start time): its 5 ticks are not "5 - 500"; 13 is new.
                stat_line(11, "stays", 'S', 1, 250, 0, 60, 1),
                stat_line(12, "reused", 'R', 1, 5, 0, 2000, 1),
                stat_line(13, "new", 'R', 1, 30, 0, 2900, 1),
            ],
            "",
        ));
        assert!(next.process(10).is_none());
        assert_eq!(next.process(11).unwrap().cpu_percent, Some(50.0));
        assert_eq!(next.process(12).unwrap().cpu_percent, None);
        assert_eq!(next.process(13).unwrap().cpu_percent, None);
        assert_eq!(next.processes.iter().map(|p| p.pid).collect::<Vec<_>>(), [11, 12, 13]);
        // A tick counter that went down (same start time) is a reset, not a huge rate.
        let weird = engine.apply(&procs_frame(6.0, &[stat_line(11, "stays", 'S', 1, 3, 0, 60, 1)], ""));
        assert_eq!(weird.process(11).unwrap().cpu_percent, None);
    }

    #[test]
    fn zombies_and_the_tree() {
        let mut engine = Engine::new(probe());
        let ps = "  5   0 sh -c work\n  6 root [defunct]\n";
        let snapshot = engine.apply(&procs_frame(
            1.0,
            &[
                stat_line(5, "sh", 'S', 1, 1, 1, 9, 1),
                stat_line(6, "child", 'Z', 5, 0, 0, 10, 0),
                stat_line(7, "unknown", 'S', 5, 0, 0, 11, 0),
            ],
            ps,
        ));
        assert_eq!(snapshot.zombies, 1);
        assert_eq!(
            snapshot.processes.iter().map(|p| (p.pid, p.ppid)).collect::<Vec<_>>(),
            [(5, 1), (6, 5), (7, 5)],
            "the parents for the tree"
        );
        let unknown = snapshot.process(7).unwrap();
        assert_eq!(
            (unknown.user.as_str(), unknown.command.as_str()),
            ("", "unknown"),
            "a process ps did not list shows its short name"
        );
        assert_eq!(snapshot.process(6).unwrap().user, "root", "a name from busybox is used as it is");
    }

    #[test]
    fn warm_process_rate_uses_half_a_second_of_remote_uptime() {
        let mut engine = Engine::new(probe());
        engine.apply(&procs_frame(10.0, &[stat_line(11, "work", 'S', 1, 100, 0, 60, 1)], ""));
        let next = engine.apply(&procs_frame(10.5, &[stat_line(11, "work", 'S', 1, 125, 0, 60, 1)], ""));
        assert_eq!(next.process(11).unwrap().cpu_percent, Some(50.0));
    }

    #[test]
    fn the_collector_and_what_it_started_are_not_listed_counted_or_busiest() {
        let mut engine = Engine::new(probe());
        let ps = "  4   0 bash -c sh -c # shellmate-collector abc\n  5   0 sh -c # shellmate-collector abc\n  7  1000 /usr/bin/editor\n";
        let mut frame = procs_frame(
            1.0,
            &[
                stat_line(4, "bash", 'S', 1, 1, 1, 8, 1),
                stat_line(5, "sh", 'S', 4, 1, 1, 9, 1),
                // `cut` and a zombie `ps` that the collector started.
                stat_line(6, "cut", 'R', 5, 90, 0, 10, 0),
                stat_line(8, "ps", 'Z', 5, 0, 0, 11, 0),
                stat_line(7, "editor", 'S', 1, 0, 0, 12, 1),
            ],
            ps,
        );
        frame.pid = Some(5);
        let snapshot = engine.apply(&frame);
        assert_eq!(
            snapshot.processes.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [7],
            "only the user's process is left"
        );
        assert_eq!(snapshot.zombies, 0, "the collector's own zombie is not counted");
        assert_eq!(snapshot.top_by_cpu(10).len(), 1);

        // The script's pid alone is enough when ps said nothing (no ps on the server): its children go too.
        let mut frame = procs_frame(
            2.0,
            &[
                stat_line(5, "sh", 'S', 1, 1, 1, 9, 1),
                stat_line(6, "cut", 'R', 5, 90, 0, 10, 0),
                stat_line(9, "w", 'S', 1, 0, 0, 13, 1),
            ],
            "",
        );
        frame.pid = Some(5);
        let snapshot = Engine::new(probe()).apply(&frame);
        assert_eq!(snapshot.processes.iter().map(|p| p.pid).collect::<Vec<_>>(), [9]);
    }

    #[test]
    fn frames_without_ps_mean_as_before_and_a_reused_process_number_is_not_the_old_process() {
        let mut engine = Engine::new(probe());
        let ps = "  100  1000 /usr/bin/editor file.txt\n  200     0 /sbin/daemon\n";
        engine.apply(&procs_frame(
            0.0,
            &[stat_line(100, "editor", 'S', 1, 5, 5, 700, 1), stat_line(200, "daemon", 'S', 1, 5, 5, 800, 1)],
            ps,
        ));
        // The next frames come without a ps section: the same users and command lines.
        let later = engine.apply(&frame(
            FrameKind::Procs,
            3.0,
            &[(
                "stat",
                &[stat_line(100, "editor", 'S', 1, 6, 5, 700, 1), stat_line(200, "daemon", 'S', 1, 5, 5, 800, 1)]
                    .join("\n"),
            )],
        ));
        assert_eq!(
            (later.process(100).unwrap().user.as_str(), later.process(100).unwrap().command.as_str()),
            ("alice", "/usr/bin/editor file.txt")
        );
        assert_eq!(later.process(200).unwrap().user, "root");
        assert!(later.process(100).unwrap().cpu_percent.is_some(), "rates do not depend on ps");
        // Process 100 exited and its number went to another process (another start time) while the list of
        // processes looked the same to the script: nobody knows its owner until the next ps.
        let reused = engine.apply(&frame(
            FrameKind::Procs,
            6.0,
            &[(
                "stat",
                &[stat_line(100, "newproc", 'R', 1, 1, 1, 9999, 1), stat_line(200, "daemon", 'S', 1, 5, 5, 800, 1)]
                    .join("\n"),
            )],
        ));
        let process = reused.process(100).unwrap();
        assert_eq!((process.user.as_str(), process.command.as_str()), ("", "newproc"));
        assert_eq!(reused.process(200).unwrap().user, "root", "the others are fine");
        // A frame with ps teaches it again.
        let taught = engine.apply(&procs_frame(
            9.0,
            &[stat_line(100, "newproc", 'R', 1, 2, 2, 9999, 1)],
            "  100  1000 /bin/newproc\n",
        ));
        assert_eq!(taught.process(100).unwrap().command, "/bin/newproc");
        // A process that is in ps but was not in the stat lines yet takes the start time of its first stat line.
        engine.apply(&procs_frame(12.0, &[], "  300     0 /bin/late\n"));
        let late =
            engine.apply(&frame(FrameKind::Procs, 15.0, &[("stat", &stat_line(300, "late", 'S', 1, 0, 0, 5555, 1))]));
        assert_eq!(late.process(300).unwrap().command, "/bin/late");
    }

    #[test]
    fn a_very_long_command_is_cut_for_display() {
        let mut engine = Engine::new(probe());
        let long = "x".repeat(50_000);
        let snapshot =
            engine.apply(&procs_frame(1.0, &[stat_line(9, "java", 'S', 1, 1, 1, 9, 1)], &format!("9 0 {long}\n")));
        assert_eq!(snapshot.process(9).unwrap().command.len(), MAX_COMMAND_CHARS);
    }

    #[test]
    fn disks_are_filtered_and_converted() {
        let mut engine = Engine::new(probe());
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 1000 850 150 85% /\ntmpfs 100 0 100 0% /dev/shm\n/dev/sdb1 2000 100 1900 5% /data\n";
        let snapshot = engine.apply(&frame(FrameKind::Slow, 30.0, &[("df", df)]));
        assert_eq!(snapshot.disks.len(), 2);
        assert_eq!(
            snapshot.disks[0],
            Disk {
                mount: "/".into(),
                filesystem: "/dev/sda1".into(),
                total: 1000 * 1024,
                used: 850 * 1024,
                available: 150 * 1024,
                percent: 85
            }
        );
        assert_eq!(snapshot.disks_over(80).len(), 1);
        assert_eq!(snapshot.disks_over(80)[0].mount, "/");
        assert_eq!(snapshot.host.uptime_secs, 30.0);
    }

    #[test]
    fn temperature_when_there_is_a_sensor() {
        let mut engine = Engine::new(probe());
        let snapshot =
            engine.apply(&frame(FrameKind::Stats, 1.0, &[("thermal", "thermal thermal_zone0 61000 x86_pkg_temp\n")]));
        assert_eq!(snapshot.temperature, Some(61.0));
        let snapshot = engine.apply(&frame(FrameKind::Stats, 3.0, &[("thermal", "")]));
        assert_eq!(snapshot.temperature, None);
    }

    #[test]
    fn a_frame_with_garbage_changes_nothing_and_panics_nowhere() {
        let mut engine = Engine::new(probe());
        let snapshot = engine.apply(&frame(
            FrameKind::Stats,
            1.0,
            &[("cpu", "garbage\n"), ("mem", "x\n"), ("net", "y\n"), ("load", "z"), ("thermal", "q")],
        ));
        assert_eq!(snapshot.cpu, None);
        assert_eq!(snapshot.memory, None);
        assert_eq!(snapshot.network, None);
        let snapshot = engine.apply(&procs_frame(2.0, &["not a stat line".to_string()], "nor ps\n"));
        assert!(snapshot.processes.is_empty());
        let _ = CpuTimes::default();
    }
}
