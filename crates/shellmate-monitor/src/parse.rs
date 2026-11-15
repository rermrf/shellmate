//! Parsers for what the collector reads on the server: `/proc` and `/sys` files, `df -P`, `ps`. All of
//! them are pure functions of text, tested on output captured from real systems (procps and BusyBox
//! flavours) and on hand-made edge cases. None of them panics on garbage: what cannot be read is left
//! out.

use std::collections::HashMap;

fn number<T: std::str::FromStr>(word: &str) -> Option<T> {
    word.parse().ok()
}

// ---- /proc/stat --------------------------------------------------------------------------------

/// The CPU time counters of one `cpu` line of `/proc/stat`, in clock ticks since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Normal processes in user mode (includes guest time).
    pub user: u64,
    /// Niced processes in user mode.
    pub nice: u64,
    /// Kernel mode.
    pub system: u64,
    /// Idle.
    pub idle: u64,
    /// Waiting for I/O.
    pub iowait: u64,
    /// Servicing interrupts.
    pub irq: u64,
    /// Servicing soft interrupts.
    pub softirq: u64,
    /// Stolen by the hypervisor.
    pub steal: u64,
}

impl CpuTimes {
    /// All the time that passed (guest time is part of `user` and `nice` already).
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }

    /// The time the CPU had nothing to do (waiting for I/O counts as idle).
    pub fn idle_time(&self) -> u64 {
        self.idle + self.iowait
    }
}

/// `/proc/stat` as far as the monitor cares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuStat {
    /// The `cpu` line: all cores together.
    pub total: CpuTimes,
    /// One entry per online core: its number (`cpu3` is 3) and its counters.
    pub cores: Vec<(u32, CpuTimes)>,
}

fn cpu_times(fields: &[&str]) -> Option<CpuTimes> {
    // Kernels before 2.6.11 have four fields, 2.6.33 added guest, 3.2 guest_nice: the missing are zero.
    if fields.len() < 4 {
        return None;
    }
    let get = |i: usize| fields.get(i).map_or(Some(0), |word| number::<u64>(word));
    Some(CpuTimes {
        user: get(0)?,
        nice: get(1)?,
        system: get(2)?,
        idle: get(3)?,
        iowait: get(4)?,
        irq: get(5)?,
        softirq: get(6)?,
        steal: get(7)?,
    })
}

/// Reads the `cpu` and `cpuN` lines (other lines of `/proc/stat` are ignored).
pub fn parse_cpu_stat(text: &str) -> Option<CpuStat> {
    let mut total = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else { continue };
        let Some(rest) = name.strip_prefix("cpu") else { continue };
        let fields: Vec<&str> = words.collect();
        let Some(times) = cpu_times(&fields) else { continue };
        if rest.is_empty() {
            total = Some(times);
        } else if let Some(index) = number::<u32>(rest) {
            cores.push((index, times));
        }
    }
    // A machine whose `cpu` line is missing still has its cores to add up.
    let total = total.or_else(|| {
        (!cores.is_empty()).then(|| {
            cores.iter().fold(CpuTimes::default(), |sum, (_, core)| CpuTimes {
                user: sum.user + core.user,
                nice: sum.nice + core.nice,
                system: sum.system + core.system,
                idle: sum.idle + core.idle,
                iowait: sum.iowait + core.iowait,
                irq: sum.irq + core.irq,
                softirq: sum.softirq + core.softirq,
                steal: sum.steal + core.steal,
            })
        })
    })?;
    Some(CpuStat { total, cores })
}

// ---- /proc/meminfo -----------------------------------------------------------------------------

/// `/proc/meminfo`, in kibibytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfo {
    /// `MemTotal`
    pub total_kb: u64,
    /// `MemFree`
    pub free_kb: u64,
    /// `MemAvailable` (Linux 3.14 and later).
    pub available_kb: Option<u64>,
    /// `Buffers`
    pub buffers_kb: u64,
    /// `Cached` (includes shared memory).
    pub cached_kb: u64,
    /// `SReclaimable`: reclaimable kernel memory (since 2.6.19).
    pub sreclaimable_kb: u64,
    /// `Shmem`
    pub shmem_kb: u64,
    /// `SwapTotal`
    pub swap_total_kb: u64,
    /// `SwapFree`
    pub swap_free_kb: u64,
}

impl MemInfo {
    /// What `free` shows as buff/cache: memory the system gives back when programs ask for it.
    pub fn cache_kb(&self) -> u64 {
        self.buffers_kb + self.cached_kb + self.sreclaimable_kb
    }

    /// What programs can get without swapping: `MemAvailable`, or on kernels that do not have it
    /// free memory plus the cache (what it was introduced to estimate), never more than the total.
    pub fn available_or_estimate_kb(&self) -> u64 {
        self.available_kb.unwrap_or_else(|| self.free_kb + self.cache_kb()).min(self.total_kb)
    }

    /// Memory in use, as `free` counts it: the total without the free memory and the cache.
    pub fn used_kb(&self) -> u64 {
        self.total_kb.saturating_sub(self.free_kb).saturating_sub(self.cache_kb())
    }

    /// Swap in use.
    pub fn swap_used_kb(&self) -> u64 {
        self.swap_total_kb.saturating_sub(self.swap_free_kb)
    }
}

/// Reads the lines of `/proc/meminfo` that matter (the unit, if there is one, is always kB). `None`
/// without `MemTotal`.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut info = MemInfo::default();
    let mut total = false;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else { continue };
        let Some(value) = rest.split_whitespace().next().and_then(number::<u64>) else { continue };
        match key.trim() {
            "MemTotal" => {
                info.total_kb = value;
                total = true;
            }
            "MemFree" => info.free_kb = value,
            "MemAvailable" => info.available_kb = Some(value),
            "Buffers" => info.buffers_kb = value,
            "Cached" => info.cached_kb = value,
            "SReclaimable" => info.sreclaimable_kb = value,
            "Shmem" => info.shmem_kb = value,
            "SwapTotal" => info.swap_total_kb = value,
            "SwapFree" => info.swap_free_kb = value,
            _ => {}
        }
    }
    total.then_some(info)
}

// ---- /proc/loadavg, /proc/uptime ----------------------------------------------------------------

/// `/proc/loadavg`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoadAvg {
    /// The 1 minute average.
    pub one: f32,
    /// The 5 minute average.
    pub five: f32,
    /// The 15 minute average.
    pub fifteen: f32,
    /// How many threads are running right now.
    pub running: Option<u32>,
    /// How many threads there are.
    pub threads: Option<u32>,
}

/// Reads `0.52 0.58 0.59 1/466 12345`.
pub fn parse_loadavg(text: &str) -> Option<LoadAvg> {
    let mut words = text.split_whitespace();
    let (one, five, fifteen) = (number(words.next()?)?, number(words.next()?)?, number(words.next()?)?);
    let (running, threads) = words
        .next()
        .and_then(|pair| pair.split_once('/'))
        .map_or((None, None), |(running, threads)| (number(running), number(threads)));
    Some(LoadAvg { one, five, fifteen, running, threads })
}

/// Seconds since boot, from `190057.84 1518756.62`.
pub fn parse_uptime(text: &str) -> Option<f64> {
    number::<f64>(text.split_whitespace().next()?).filter(|secs| secs.is_finite() && *secs >= 0.0)
}

// ---- /proc/net/dev, /proc/net/route ---------------------------------------------------------------

/// The byte counters of one network interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetCounters {
    /// `eth0`
    pub interface: String,
    /// Bytes received since boot.
    pub rx_bytes: u64,
    /// Bytes sent since boot.
    pub tx_bytes: u64,
}

/// Reads the interface lines of `/proc/net/dev` (`  eth0: 7452403667 935362 ...`, also when the
/// numbers follow the colon directly); the two header lines are skipped.
pub fn parse_net_dev(text: &str) -> Vec<NetCounters> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            // rx: bytes packets errs drop fifo frame compressed multicast; tx: bytes ...
            let (rx_bytes, tx_bytes) = (number(fields.first()?)?, number(fields.get(8)?)?);
            Some(NetCounters { interface: name.trim().to_string(), rx_bytes, tx_bytes })
        })
        .collect()
}

/// The interface of the default route in `/proc/net/route`: the route to `0.0.0.0/0` that is up and has
/// the lowest metric (the first of equals).
pub fn default_interface(route: &str) -> Option<String> {
    let mut best: Option<(u32, &str)> = None;
    for line in route.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[1] != "00000000" || fields[7] != "00000000" {
            continue;
        }
        let up = u32::from_str_radix(fields[3], 16).is_ok_and(|flags| flags & 1 != 0);
        let metric = number::<u32>(fields[6]).unwrap_or(u32::MAX);
        if up && best.is_none_or(|(least, _)| metric < least) {
            best = Some((metric, fields[0]));
        }
    }
    best.map(|(_, name)| name.to_string())
}

// ---- df -P ---------------------------------------------------------------------------------------

/// One line of `df -P`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskUsage {
    /// The device or file system name (`/dev/vda1`, `tmpfs`).
    pub filesystem: String,
    /// Where it is mounted.
    pub mount: String,
    /// Size, in kibibytes.
    pub total_kb: u64,
    /// Used, in kibibytes.
    pub used_kb: u64,
    /// Available to ordinary users, in kibibytes.
    pub available_kb: u64,
    /// The `Capacity` column.
    pub percent: u8,
}

/// Reads `df -P` (1024-byte blocks; `-k` is the same). File system names and mount points may contain
/// spaces, so the line is cut at the three numbers and the percentage in the middle.
pub fn parse_df(text: &str) -> Vec<DiskUsage> {
    text.lines().filter_map(parse_df_line).collect()
}

fn parse_df_line(line: &str) -> Option<DiskUsage> {
    let words: Vec<&str> = line.split_whitespace().collect();
    (1..words.len().saturating_sub(4)).find_map(|i| {
        let numbers = [number::<u64>(words[i])?, number::<u64>(words[i + 1])?, number::<u64>(words[i + 2])?];
        let percent = words[i + 3].strip_suffix('%').and_then(number::<u8>)?;
        let mount = words.get(i + 4..)?.join(" ");
        (!mount.is_empty()).then(|| DiskUsage {
            filesystem: words[..i].join(" "),
            mount,
            total_kb: numbers[0],
            used_kb: numbers[1],
            available_kb: numbers[2],
            percent,
        })
    })
}

/// File systems that are not disks: memory, kernel interfaces, containers' layers, read-only images.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "overlay",
    "squashfs",
    "shm",
    "none",
    "udev",
    "proc",
    "sysfs",
    "devpts",
    "ramfs",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "pstore",
    "mqueue",
    "hugetlbfs",
    "binfmt_misc",
    "nsfs",
    "bpf",
    "autofs",
    "rootfs",
    "efivarfs",
    "rpc_pipefs",
    "lxcfs",
    "gvfsd-fuse",
    "none",
];

/// Mount points that belong to the system and its containers, never to the user's data.
const PSEUDO_MOUNTS: &[&str] =
    &["/proc", "/sys", "/dev", "/run", "/snap", "/var/lib/docker", "/var/lib/containers", "/var/lib/kubelet"];

/// Whether `disk` is a real storage volume, not a pseudo file system.
pub fn is_real_disk(disk: &DiskUsage) -> bool {
    let name = disk.filesystem.as_str();
    if PSEUDO_FILESYSTEMS.contains(&name) || name.starts_with("cgroup") || name.starts_with("/dev/loop") {
        return false;
    }
    let mount = disk.mount.as_str();
    if PSEUDO_MOUNTS.iter().any(|prefix| mount == *prefix || mount.starts_with(&format!("{prefix}/"))) {
        return false;
    }
    disk.total_kb > 0
}

/// The disks worth showing: the real ones, one per volume (a volume mounted several times, as docker
/// does with `/etc/hosts`, is listed once, at its shortest mount point), in the order they came.
pub fn real_disks(disks: Vec<DiskUsage>) -> Vec<DiskUsage> {
    let mut kept: Vec<DiskUsage> = Vec::new();
    for disk in disks.into_iter().filter(is_real_disk) {
        let same = |other: &DiskUsage| {
            other.filesystem == disk.filesystem && other.total_kb == disk.total_kb && other.used_kb == disk.used_kb
        };
        match kept.iter_mut().find(|other| same(other)) {
            Some(other) if disk.mount.len() < other.mount.len() => *other = disk,
            Some(_) => {}
            None => kept.push(disk),
        }
    }
    kept
}

// ---- thermal zones ---------------------------------------------------------------------------------

/// A temperature sensor from `/sys/class/thermal`.
#[derive(Clone, Debug, PartialEq)]
pub struct Thermal {
    /// `thermal_zone0`
    pub zone: String,
    /// What it measures (`x86_pkg_temp`, `acpitz`, `cpu-thermal`); may be empty.
    pub kind: String,
    /// Degrees Celsius.
    pub celsius: f32,
}

/// Reads the collector's `thermal thermal_zone0 45000 x86_pkg_temp` lines (millidegrees; the kind may
/// have spaces or be missing). Readings that are not plausible (below -50 or above 150 degrees, which
/// is what a broken sensor reports) are left out.
pub fn parse_thermal(text: &str) -> Vec<Thermal> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("thermal ")?;
            let mut words = rest.splitn(3, ' ');
            let zone = words.next()?.to_string();
            let millidegrees: i64 = number(words.next()?)?;
            let kind = words.next().unwrap_or("").trim().to_string();
            let celsius = millidegrees as f32 / 1000.0;
            (-50.0..=150.0).contains(&celsius).then_some(Thermal { zone, kind, celsius })
        })
        .collect()
}

/// The temperature to show as the CPU's: a sensor that says it is the CPU or the package, otherwise the
/// hottest one.
pub fn cpu_temperature(sensors: &[Thermal]) -> Option<f32> {
    let cpu = |sensor: &&Thermal| {
        let kind = sensor.kind.to_ascii_lowercase();
        ["x86_pkg_temp", "cpu", "coretemp", "k10temp", "soc"].iter().any(|name| kind.contains(name))
    };
    sensors
        .iter()
        .filter(cpu)
        .map(|sensor| sensor.celsius)
        .reduce(f32::max)
        .or_else(|| sensors.iter().map(|sensor| sensor.celsius).reduce(f32::max))
}

// ---- processes -----------------------------------------------------------------------------------------

/// A line of `/proc/<pid>/stat`, as far as the first 22 fields (the collector cuts the rest off).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    /// Process id.
    pub pid: u32,
    /// The executable's name, up to 15 characters (may contain spaces and parentheses).
    pub comm: String,
    /// `R`, `S`, `D`, `Z`, `T`, `I`, ...
    pub state: char,
    /// Parent process id.
    pub ppid: u32,
    /// CPU time spent in user mode, in clock ticks.
    pub utime: u64,
    /// CPU time spent in kernel mode, in clock ticks.
    pub stime: u64,
    /// The nice value, -20 to 19.
    pub nice: i32,
    /// How many threads.
    pub threads: u32,
    /// When the process started, in ticks after boot (tells apart two processes with the same id).
    pub starttime: u64,
    /// Resident set size in pages, if the line reaches that far.
    pub rss_pages: Option<u64>,
}

/// Reads one line of `/proc/<pid>/stat`. The name (field 2) is cut out at the first `(` and the *last*
/// `)`: it may contain both, and spaces, which is why the line cannot be split on spaces from the start.
pub fn parse_proc_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let pid = number::<u32>(line[..open].trim())?;
    let comm = line[open + 1..close].to_string();
    let fields: Vec<&str> = line[close + 1..].split_whitespace().collect();
    // After the name: state(0) ppid(1) pgrp session tty tpgid flags minflt cminflt majflt cmajflt
    // utime(11) stime(12) cutime cstime priority nice(16) threads(17) itrealvalue starttime(19) vsize rss(21).
    Some(ProcStat {
        pid,
        comm,
        state: fields.first()?.chars().next()?,
        ppid: number(fields.get(1)?)?,
        utime: number(fields.get(11)?)?,
        stime: number(fields.get(12)?)?,
        nice: number(fields.get(16)?)?,
        threads: number(fields.get(17)?)?,
        starttime: number(fields.get(19)?)?,
        rss_pages: fields.get(21).and_then(|word| number::<i64>(word)).map(|rss| rss.max(0) as u64),
    })
}

/// Who owns a process, as `ps` printed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PsOwner {
    /// A numeric user id (procps `ps -o uid`).
    Uid(u32),
    /// A user name (BusyBox `ps -o user`, which has no numeric column; it cuts names at 8 characters).
    Name(String),
}

/// A line of `ps -eo pid=,uid=,args=` (procps) or `ps -o pid=,user=,args=` (BusyBox).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsEntry {
    /// Process id.
    pub pid: u32,
    /// The owner.
    pub owner: PsOwner,
    /// The command line (kernel threads: `[kworker/0:1]`).
    pub args: String,
}

/// Reads the output of `ps` in either flavour. A header line (older BusyBox prints one even with empty
/// headers) is skipped; a line that does not start with a process id continues the command line of the
/// process before it (an argument with a newline in it).
pub fn parse_ps(text: &str) -> Vec<PsEntry> {
    let mut entries: Vec<PsEntry> = Vec::new();
    for line in text.lines() {
        match parse_ps_line(line) {
            Some(entry) => entries.push(entry),
            None => {
                if let Some(last) = entries.last_mut()
                    && !line.trim().is_empty()
                {
                    last.args.push('\n');
                    last.args.push_str(line);
                }
            }
        }
    }
    entries
}

fn parse_ps_line(line: &str) -> Option<PsEntry> {
    let rest = line.trim_start();
    let (pid, rest) = rest.split_once(char::is_whitespace)?;
    let pid = number::<u32>(pid)?;
    let rest = rest.trim_start();
    let (owner, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if owner.is_empty() {
        return None;
    }
    let owner = number::<u32>(owner).map_or_else(|| PsOwner::Name(owner.to_string()), PsOwner::Uid);
    // One space separates the columns; the rest is the command line as it is.
    Some(PsEntry { pid, owner, args: args.strip_prefix(' ').unwrap_or(args).trim_start().to_string() })
}

/// Reads `/etc/passwd` / `getent passwd` into a map from user id to name (the first name of an id wins).
pub fn parse_passwd(text: &str) -> HashMap<u32, String> {
    let mut users = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split(':');
        let (Some(name), Some(_), Some(uid)) = (fields.next(), fields.next(), fields.next()) else { continue };
        if name.is_empty() || name.starts_with(['#', '+', '-']) {
            continue;
        }
        if let Some(uid) = number::<u32>(uid) {
            users.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    users
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|err| panic!("{name}: {err}"))
    }

    #[test]
    fn cpu_stat_of_a_real_machine() {
        let stat = parse_cpu_stat(&fixture("procps/proc_stat_cpu.txt")).unwrap();
        assert_eq!(
            stat.total,
            CpuTimes {
                user: 40493,
                nice: 0,
                system: 84524,
                idle: 151884385,
                iowait: 2150,
                irq: 0,
                softirq: 12783,
                steal: 0
            }
        );
        assert_eq!(stat.cores.len(), 8);
        assert_eq!(
            stat.cores[3],
            (
                3,
                CpuTimes {
                    user: 5669,
                    nice: 0,
                    system: 10440,
                    idle: 18985880,
                    iowait: 276,
                    irq: 0,
                    softirq: 209,
                    steal: 0
                }
            )
        );
        assert_eq!(stat.total.idle_time(), 151884385 + 2150);
        assert_eq!(stat.total.total(), 40493 + 84524 + 151884385 + 2150 + 12783);
    }

    #[test]
    fn cpu_stat_of_old_and_odd_kernels() {
        // Four fields (Linux 2.4), no `cpu` total line, cores out of order, a junk line.
        let stat = parse_cpu_stat("cpu1 1 2 3 4\ncpu0 10 20 30 40\nintr 5\nbtime 7\ncpuX 1 2 3 4\ncpu9 1 2\n").unwrap();
        assert_eq!(
            stat.cores,
            [
                (1, CpuTimes { user: 1, nice: 2, system: 3, idle: 4, ..Default::default() }),
                (0, CpuTimes { user: 10, nice: 20, system: 30, idle: 40, ..Default::default() })
            ]
        );
        assert_eq!(stat.total, CpuTimes { user: 11, nice: 22, system: 33, idle: 44, ..Default::default() }, "added up");
        // Ten fields: guest and guest_nice are part of user and nice already and are not counted twice.
        let stat = parse_cpu_stat("cpu  1 2 3 4 5 6 7 8 9 10\n").unwrap();
        assert_eq!(stat.total.total(), 36);
        assert_eq!(parse_cpu_stat(""), None);
        assert_eq!(parse_cpu_stat("garbage\n"), None);
    }

    #[test]
    fn meminfo_of_a_real_machine() {
        let info = parse_meminfo(&fixture("procps/meminfo.txt")).unwrap();
        assert_eq!(info.total_kb, 4083300);
        assert_eq!(info.free_kb, 3604356);
        assert_eq!(info.available_kb, Some(3660664));
        assert_eq!((info.buffers_kb, info.cached_kb), (108, 198148));
        assert_eq!((info.swap_total_kb, info.swap_free_kb), (5131868, 5131868));
        assert_eq!(info.swap_used_kb(), 0);
        assert_eq!(info.available_or_estimate_kb(), 3660664);
        assert_eq!(info.used_kb(), 4083300 - 3604356 - info.cache_kb());
    }

    #[test]
    fn meminfo_without_memavailable_estimates_it() {
        // A 2.6.32 kernel: no MemAvailable, no SReclaimable, no swap.
        let info = parse_meminfo("MemTotal:       1000 kB\nMemFree:         100 kB\nBuffers:          50 kB\nCached:          250 kB\nSwapTotal:         0 kB\nSwapFree:          0 kB\n").unwrap();
        assert_eq!(info.available_kb, None);
        assert_eq!(info.available_or_estimate_kb(), 400);
        assert_eq!(info.used_kb(), 600);
        assert_eq!(info.swap_used_kb(), 0);
        // Never more than the total, however the numbers add up.
        let odd = MemInfo { total_kb: 10, free_kb: 9, cached_kb: 9, ..Default::default() };
        assert_eq!(odd.available_or_estimate_kb(), 10);
        assert_eq!(odd.used_kb(), 0);
        // The collector's own lines have no unit.
        assert_eq!(parse_meminfo("MemTotal: 5\nMemFree: 1\n").unwrap().total_kb, 5);
        assert_eq!(parse_meminfo("MemFree: 1\n"), None);
    }

    #[test]
    fn loadavg_and_uptime() {
        let load = parse_loadavg(&fixture("procps/loadavg.txt")).unwrap();
        assert_eq!(
            (load.one, load.five, load.fifteen, load.running, load.threads),
            (0.34, 0.08, 0.03, Some(4), Some(322))
        );
        assert_eq!(parse_loadavg("1.50 2.25 0.05\n").unwrap().five, 2.25);
        assert_eq!(parse_loadavg("1.5 x 3"), None);
        assert_eq!(parse_uptime(&fixture("procps/uptime.txt")), Some(190070.99));
        assert_eq!(parse_uptime("-5 3"), None);
        assert_eq!(parse_uptime("nan 3"), None);
        assert_eq!(parse_uptime(""), None);
    }

    #[test]
    fn network_counters_and_the_default_route() {
        let dev = parse_net_dev(&fixture("procps/net_dev.txt"));
        assert_eq!(dev.len(), 2);
        assert_eq!(dev[0], NetCounters { interface: "lo".into(), rx_bytes: 4971599306, tx_bytes: 4971599306 });
        assert_eq!(dev[1], NetCounters { interface: "eth0".into(), rx_bytes: 7452403667, tx_bytes: 8826320896 });
        // Large counters on a busy interface glue the name and the first number together.
        let glued = parse_net_dev("eth1:1234567890123 1 0 0 0 0 0 0 99 1 0 0 0 0 0 0\n");
        assert_eq!(glued, [NetCounters { interface: "eth1".into(), rx_bytes: 1234567890123, tx_bytes: 99 }]);
        assert!(parse_net_dev("Inter-|   Receive\n face |bytes\n").is_empty());

        assert_eq!(default_interface(&fixture("procps/net_route.txt")), Some("eth0".into()));
        // A VPN with a lower metric wins; a down route and the default route's header do not count.
        let route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                     eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                     wg0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\t0\t0\t0\n\
                     eth1\t00000000\t0100A8C0\t0002\t0\t0\t1\t00000000\t0\t0\t0\n\
                     eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_interface(route), Some("wg0".into()));
        assert_eq!(default_interface("Iface\tDestination\n"), None);
        assert_eq!(default_interface(""), None);
    }

    #[test]
    fn df_in_both_flavours() {
        let gnu = parse_df(&fixture("procps/df.txt"));
        assert_eq!(gnu.len(), 6);
        assert_eq!(
            gnu[0],
            DiskUsage {
                filesystem: "overlay".into(),
                mount: "/".into(),
                total_kb: 30457744,
                used_kb: 9083808,
                available_kb: 21373936,
                percent: 30
            }
        );
        let busybox = parse_df(&fixture("busybox/df.txt"));
        assert_eq!(busybox.len(), 12);
        assert_eq!(busybox[7].mount, "/proc/asound");
        // Names and mount points with spaces.
        let spaced = parse_df(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n//nas/My Share 1000 400 600 40% /mnt/my share\n",
        );
        assert_eq!(spaced.len(), 1);
        assert_eq!(
            (spaced[0].filesystem.as_str(), spaced[0].mount.as_str(), spaced[0].percent),
            ("//nas/My Share", "/mnt/my share", 40)
        );
        // A name that wrapped onto its own line (old df without -P) and other junk give nothing.
        assert!(parse_df("/dev/mapper/very-long-name\n").is_empty());
        assert!(parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\n").is_empty());
        assert_eq!(parse_df("x 1 2 3 -% /").len(), 0);
    }

    #[test]
    fn pseudo_file_systems_are_not_disks() {
        let real = real_disks(parse_df(&fixture("procps/df.txt")));
        assert_eq!(
            real.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(),
            ["/config"],
            "overlay, tmpfs and shm are not disks"
        );
        // BusyBox lists a volume once per mount; it is shown once, at its shortest mount point.
        let real = real_disks(parse_df(&fixture("busybox/df.txt")));
        assert_eq!(real.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(), ["/config"]);

        let table = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
            udev 100 0 100 0% /dev\n\
            tmpfs 100 1 99 1% /run\n\
            /dev/sda1 1000 500 500 50% /\n\
            /dev/sda2 2000 1900 100 95% /data\n\
            /dev/loop3 100 100 0 100% /snap/core/123\n\
            squashfs 100 100 0 100% /mnt/image\n\
            cgroup2 0 0 0 0% /sys/fs/cgroup\n\
            cgroup_root 100 0 100 0% /weird\n\
            overlay 1000 500 500 50% /var/lib/docker/overlay2/abc/merged\n\
            /dev/sdb1 0 0 0 0% /empty\n\
            /dev/sda1 1000 500 500 50% /var/bind\n\
            /dev/sdc1 5000 10 4990 1% /run/media/me/usb\n";
        let mounts: Vec<String> = real_disks(parse_df(table)).into_iter().map(|d| d.mount).collect();
        assert_eq!(mounts, ["/", "/data"]);
    }

    #[test]
    fn thermal_zones() {
        let text = "thermal thermal_zone0 45000 x86_pkg_temp\nthermal thermal_zone1 38500 acpitz\nthermal thermal_zone2 -274000 broken\nthermal thermal_zone3 99999999 junk\nthermal thermal_zone4 41000 \nthermal thermal_zone5 notanumber x\nnot thermal\n";
        let sensors = parse_thermal(text);
        assert_eq!(sensors.len(), 3, "{sensors:?}");
        assert_eq!(sensors[0], Thermal { zone: "thermal_zone0".into(), kind: "x86_pkg_temp".into(), celsius: 45.0 });
        assert_eq!(sensors[2].kind, "", "a sensor without a type");
        assert_eq!(cpu_temperature(&sensors), Some(45.0));
        // Without a CPU sensor the hottest one stands in; without any there is nothing to show.
        assert_eq!(cpu_temperature(&sensors[1..]), Some(41.0));
        assert_eq!(cpu_temperature(&parse_thermal("")), None);
        assert_eq!(parse_thermal("thermal thermal_zone0 52000 cpu thermal sensor\n")[0].kind, "cpu thermal sensor");
    }

    #[test]
    fn proc_stat_lines_of_a_real_machine() {
        let text = fixture("procps/proc_stat_lines.txt");
        let stats: Vec<ProcStat> = text.lines().filter_map(parse_proc_stat).collect();
        assert_eq!(
            stats.len(),
            text.lines().count(),
            "every line parses, also the ones cut in the middle of the fields"
        );
        assert_eq!(
            stats[0],
            ProcStat {
                pid: 1,
                comm: "s6-svscan".into(),
                state: 'S',
                ppid: 0,
                utime: 4,
                stime: 21,
                nice: 0,
                threads: 1,
                starttime: 192404,
                rss_pages: Some(0)
            }
        );
        assert_eq!(stats[3].comm, "busybox");
        assert_eq!(stats[3].utime + stats[3].stime, 112 + 679);
    }

    #[test]
    fn lines_cut_by_the_collector_after_40_fields_still_have_what_is_needed() {
        // What `cut -d" " -f1-40` leaves of a real container's stat files.
        let text = fixture("procps/proc_stat_fields.txt");
        let stats: Vec<ProcStat> = text.lines().filter_map(parse_proc_stat).collect();
        assert_eq!(stats.len(), text.lines().count());
        assert!(stats.iter().all(|stat| stat.rss_pages.is_some()), "memory is there for every process");
        assert!(text.lines().all(|line| line.split(' ').count() <= 40));

        // A name of 15 characters can hold 15 spaces (16 words, the first and last with the parentheses on
        // them) and pushes the memory field to the 39th: the cut keeps it for every name.
        let text = fixture("procps/proc_stat_spaces.txt");
        let names = ["init", "Web Content", "tmux: server", "a b c d e f g", "a b c d e f g h", "a  b  c  d  e", "  x  ", "               ", "a b c d e f g h"];
        let stats: Vec<ProcStat> = text.lines().filter_map(parse_proc_stat).collect();
        assert_eq!(stats.len(), names.len());
        for (stat, name) in stats.iter().zip(names) {
            assert_eq!(stat.comm, name);
            assert_eq!((stat.utime, stat.stime, stat.nice, stat.threads, stat.starttime), (7, 9, 5, 3, 777), "{name:?}");
            assert_eq!(stat.rss_pages, Some(4242), "the memory of {name:?} is not lost");
        }
        assert!(text.lines().all(|line| line.split(' ').count() <= 40), "and the lines really are cut");

        // The reason for the number: every name of up to 15 characters, with any of its characters a space,
        // keeps the memory field within the first 40.
        let tail = "S 1 2 3 0 -1 4194560 10 0 0 0 7 9 0 0 20 5 3 0 777 12345678 4242 18446744073709551615 4194304 4194305 140737488 0 0 0";
        for spaces in 0..=15 {
            let comm = format!("{}{}", "x".repeat(15 - spaces), " ".repeat(spaces));
            let cut = format!("4321 ({comm}) {tail}").split(' ').take(40).collect::<Vec<_>>().join(" ");
            let stat = parse_proc_stat(&cut).unwrap_or_else(|| panic!("{comm:?}"));
            assert_eq!((stat.comm.as_str(), stat.rss_pages), (comm.as_str(), Some(4242)), "{spaces} spaces");
        }
        // With the old cut of 28 fields a name with six spaces lost it.
        let old = format!("4321 (a b c d e f g) {tail}").split(' ').take(28).collect::<Vec<_>>().join(" ");
        assert_eq!(parse_proc_stat(&old).unwrap().rss_pages, None);
    }

    #[test]
    fn a_process_name_with_spaces_and_parentheses_does_not_confuse_the_parser() {
        let rest = "S 1 2 3 0 -1 4194560 10 0 0 0 7 9 0 0 20 5 3 0 777 12345678 4242 18446744073709551615";
        for comm in ["a b", "x) S 99 (y", "(", ")", "))((", "weird) (name", "日本語", "", "a)b)c"] {
            let line = format!("4321 ({comm}) {rest}");
            let stat = parse_proc_stat(&line).unwrap_or_else(|| panic!("{comm:?}"));
            assert_eq!(stat.comm, comm);
            assert_eq!((stat.pid, stat.state, stat.ppid, stat.utime, stat.stime), (4321, 'S', 1, 7, 9), "{comm:?}");
            assert_eq!((stat.nice, stat.threads, stat.starttime, stat.rss_pages), (5, 3, 777, Some(4242)), "{comm:?}");
        }
    }

    #[test]
    fn broken_proc_stat_lines_give_nothing() {
        for line in [
            "",
            "garbage",
            "1 (init",
            "1 init) S",
            ") 1 (",
            "x (init) S 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0",
            "1 (init) S 0 1",
            "1 (init) S 0 1 1 0 -1 4194560 1 0 0 0 a b 0 0 20 0 1 0 5 0 0",
        ] {
            assert_eq!(parse_proc_stat(line), None, "{line:?}");
        }
        // Cut off after the 20 fields the monitor needs, without rss: still a process.
        let short = "7 (sh) R 1 7 7 0 -1 0 0 0 0 0 11 22 0 0 20 -3 2 0 99";
        let stat = parse_proc_stat(short).unwrap();
        assert_eq!(
            (stat.utime, stat.stime, stat.nice, stat.threads, stat.starttime, stat.rss_pages),
            (11, 22, -3, 2, 99, None)
        );
    }

    #[test]
    fn ps_output_in_the_procps_style() {
        let entries = parse_ps(&fixture("procps/ps.txt"));
        assert_eq!(entries.len(), 20);
        assert_eq!(
            entries[0],
            PsEntry {
                pid: 1,
                owner: PsOwner::Uid(0),
                args: "/package/admin/s6/command/s6-svscan -d4 -- /run/service".into()
            }
        );
        let sleeper = entries.iter().find(|e| e.args == "sleep 1234").unwrap();
        assert_eq!(sleeper.owner, PsOwner::Uid(1000));
    }

    #[test]
    fn ps_output_in_the_busybox_style_with_and_without_a_header() {
        for name in ["busybox/ps_noheader.txt", "busybox/ps_header.txt"] {
            let entries = parse_ps(&fixture(name));
            assert_eq!(entries.len(), 20, "{name}: the header is not a process");
            assert_eq!(entries[0].pid, 1, "{name}");
            assert_eq!(entries[0].owner, PsOwner::Name("root".into()), "{name}");
            assert_eq!(entries[0].args, "/package/admin/s6/command/s6-svscan -d4 -- /run/service", "{name}");
            let sleeper = entries.iter().find(|e| e.args == "sleep 1234").unwrap();
            assert_eq!(sleeper.owner, PsOwner::Name("tester".into()), "{name}");
        }
    }

    #[test]
    fn command_lines_with_odd_contents() {
        // Spaces inside, kernel threads, nothing at all, a very long one, non-UTF-8 bytes replaced earlier,
        // an argument that contains a newline (the next line continues the command), a pid-looking argument.
        let long = format!("java -cp {} Main", "x".repeat(20_000));
        let text = format!(
            "    1     0 /sbin/init  --two  spaces\n   2     0 [kthreadd]\n   3  1000 \n 4 1000 {long}\n  5 33 sh -c echo one\ntwo\n 6 33 echo 7 8 9\n  7 root 日本語 ünï 🙂\n"
        );
        let entries = parse_ps(&text);
        assert_eq!(entries.len(), 7);
        assert_eq!(entries[0].args, "/sbin/init  --two  spaces", "inner spacing is kept");
        assert_eq!(entries[1].args, "[kthreadd]");
        assert_eq!(entries[2].args, "");
        assert_eq!(entries[3].args.len(), long.len());
        assert_eq!(entries[4].args, "sh -c echo one\ntwo");
        assert_eq!(entries[5], PsEntry { pid: 6, owner: PsOwner::Uid(33), args: "echo 7 8 9".into() });
        assert_eq!(entries[6].owner, PsOwner::Name("root".into()));
        assert_eq!(entries[6].args, "日本語 ünï 🙂");
        assert!(parse_ps("").is_empty());
        assert!(parse_ps("PID USER COMMAND\n").is_empty());
    }

    #[test]
    fn passwd_maps_user_ids_to_names() {
        let users = parse_passwd(&fixture("procps/passwd.txt"));
        assert_eq!(users.get(&0).map(String::as_str), Some("root"));
        assert_eq!(users.get(&1000).map(String::as_str), Some("tester"));
        let users = parse_passwd(
            "# comment\n\n+@netgroup\nroot:x:0:0::/root:/bin/sh\ntoor:x:0:0::/:/bin/sh\nbad line\nnouid:x:abc:1\n-hidden:x:5:5\n me:x:7:7\n",
        );
        assert_eq!(users.len(), 2, "{users:?}");
        assert_eq!(users[&0], "root", "the first name of an id wins");
    }
}
